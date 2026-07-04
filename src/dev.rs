//! `boxme dev`: run a dev-server stack (e.g. `composer run dev`) entirely inside
//! the sandbox, with one-way host→guest file sync so HMR works while the guest
//! can never write back to your machine.
//!
//! The model: boot a writable guest, copy the project in, install dependencies
//! *in the guest* (so they get Linux-native binaries), then run the dev command
//! attached while a host file watcher pushes every edit into the guest's real
//! ext4 via the agent filesystem API. Writes only ever flow host→guest, so the
//! integrity guarantee ("guest code can't touch your machine") holds: nothing the
//! guest writes is copied back. Guest ports are forwarded to the host so you hit
//! the app in your browser as usual.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use microsandbox::sandbox::SandboxStatus;
use microsandbox::{NetworkPolicy, Sandbox};
use microsandbox_network::secrets::config::SecretEntry;
use owo_colors::OwoColorize;

use crate::allowlist::{self, Scope};
use crate::cli::Cli;
use crate::composer_auth;
use crate::detect;
use crate::run::{
    cleanup, enforced_policy, ensure_cache_volumes, observe_policy, quote_args, resolve_env,
    strict_policy,
};
use crate::scripts;
use crate::setup::{base_snapshot_exists, BASE_SNAPSHOT};
use crate::util::{slugify, stream_shell_stderr, tar_directory};

/// Forwarded by default when no `--port` is given: artisan serve + Vite.
const DEFAULT_PORTS: &[(u16, u16)] = &[(8000, 8000), (5173, 5173)];

/// How long to keep batching a burst of filesystem events before flushing them
/// to the guest. macOS FSEvents coalesces internally too; this just collapses a
/// save-all or branch-switch into one pass instead of a flood of per-file pushes.
const COALESCE: Duration = Duration::from_millis(80);

pub async fn dev(cli: &Cli, cmd: &[String], port_specs: &[String]) -> Result<()> {
    if cli.json {
        bail!("--json applies to package-manager runs, not `boxme dev`");
    }
    if !base_snapshot_exists().await? {
        bail!("base snapshot missing — run `boxme setup` first");
    }
    let project_dir = std::env::current_dir()?;
    let ports = parse_ports(port_specs)?;
    let command: Vec<String> = if cmd.is_empty() {
        vec!["composer".into(), "run".into(), "dev".into()]
    } else {
        cmd.to_vec()
    };

    let has_composer = project_dir.join("composer.json").exists();
    let has_npm = project_dir.join("package.json").exists();
    let php = detect::php_version(&project_dir).await;
    let node = detect::node_major(&project_dir).await;
    eprintln!(
        "{} php {php}, node {}",
        ">> detected:".dimmed(),
        node.map(|n| n.to_string())
            .unwrap_or_else(|| format!("{} (default)", scripts::BASE_NODE_MAJOR)),
    );

    let mut env = resolve_env(&cli.env)?;
    // Dependencies install in-guest, so `--composer-auth` is what lets a dev
    // session pull private composer packages — placeholder-only inside the box.
    let secrets = if cli.composer_auth && has_composer {
        composer_auth::inject(&mut env)?
    } else {
        Vec::new()
    };
    let policy = dev_policy(cli, &project_dir);
    ensure_cache_volumes().await?;

    // One dev VM per folder, named deterministically so `boxme attach` finds it.
    let name = dev_vm_name(&project_dir);
    if dev_session_running(&name).await {
        bail!(
            "a boxme dev session is already running for this folder\n\
             open another shell with `boxme attach`, or stop the running session first"
        );
    }
    // Bump any host port that's already taken (e.g. another repo's dev session)
    // to the next free one before publishing.
    let ports = resolve_free_ports(&ports)?;
    let sb = boot_dev(cli, &name, &ports, policy, &env, &secrets).await?;

    let session = dev_session(
        &sb,
        &project_dir,
        &command,
        &ports,
        &php,
        node,
        has_composer,
        has_npm,
    )
    .await;

    cleanup(cli, sb, &name).await;
    session
}

/// Open another shell — or run a one-off command — inside the running dev
/// session for the current folder. Reaches the live VM by its deterministic name
/// (`dev_vm_name`), connects as a second client, and runs an interactive TTY
/// alongside the dev stack. Never tears the VM down: the `boxme dev` process owns
/// its lifecycle, this is just a guest.
pub async fn attach(cmd: &[String]) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let name = dev_vm_name(&project_dir);

    let handle = Sandbox::get(&name).await.map_err(|_| {
        anyhow!("no boxme dev session for this folder — start one with `boxme dev`")
    })?;
    if !matches!(handle.status(), SandboxStatus::Running) {
        bail!("the boxme dev session for this folder isn't running — start one with `boxme dev`");
    }
    let sb = handle
        .connect()
        .await
        .context("connecting to the dev session")?;

    let inner = if cmd.is_empty() {
        "cd /workspace && exec bash -l".to_string()
    } else {
        format!("cd /workspace && exec {}", quote_args(cmd))
    };
    eprintln!("{} {name}", ">> attached to".dimmed());
    let result = sb.attach_with("bash", |a| a.args(["-lc", &inner])).await;
    // Release the connection without stopping the VM — `boxme dev` owns it.
    sb.detach().await;
    result?;
    Ok(())
}

/// Boot a guest from the base snapshot with the dev ports published. Mirrors
/// `run::boot` (Node-version volume, injected env) but adds port forwarding;
/// guest-local caches still apply (see `run::boot` for why they aren't mounted).
async fn boot_dev(
    cli: &Cli,
    name: &str,
    ports: &[(u16, u16)],
    policy: NetworkPolicy,
    env: &[(String, String)],
    secrets: &[SecretEntry],
) -> Result<Sandbox> {
    eprintln!("{} '{name}' from {BASE_SNAPSHOT}...", ">> booting".dimmed());
    let mut builder = Sandbox::builder(name)
        .from_snapshot(BASE_SNAPSHOT)
        .memory(cli.memory)
        .cpus(cli.cpus)
        .replace()
        .volume("/root/.n", |m| m.named("boxme-node-versions"));
    // Composer-auth secrets before `.network` (validates them; auto-enables
    // 443 TLS interception with verify-upstream + QUIC blocking).
    for entry in secrets {
        builder = builder.secret_entry(entry.clone());
    }
    builder = builder.network(|n| n.policy(policy));
    for (host, guest) in ports {
        builder = builder.port(*host, *guest);
    }
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    builder.create().await.map_err(Into::into)
}

/// Everything between boot and teardown: copy in, install, then run the dev
/// stack attached while syncing host edits. Errors here still let the caller tear
/// the VM down.
#[allow(clippy::too_many_arguments)]
async fn dev_session(
    sb: &Sandbox,
    project_dir: &Path,
    command: &[String],
    ports: &[(u16, u16)],
    php: &str,
    node: Option<u32>,
    has_composer: bool,
    has_npm: bool,
) -> Result<()> {
    // 1. Copy the project in. vendor/node_modules are always skipped here: the
    //    guest installs them Linux-native and never copies back, so shipping the
    //    host's (macOS) build in would be wrong as well as wasteful.
    eprintln!("{}", ">> packing project...".dimmed());
    let tarball = std::env::temp_dir().join(format!("boxme-dev-{}.tgz", std::process::id()));
    tar_directory(project_dir, &tarball).await?;
    sb.fs()
        .copy_from_host(&tarball, "/tmp/repo.tgz")
        .await
        .context("copying project into the sandbox failed")?;
    let _ = tokio::fs::remove_file(&tarball).await;
    let code = stream_shell_stderr(sb, scripts::DEV_UNPACK).await?;
    if code != 0 {
        bail!("unpacking the project in the guest failed (exit {code})");
    }

    // 2. Match host toolchain versions.
    if has_composer {
        let code = stream_shell_stderr(sb, &scripts::php_switch(php)).await?;
        if code != 0 {
            bail!("switching the guest to PHP {php} failed");
        }
    }
    if let Some(major) = node {
        if major != scripts::BASE_NODE_MAJOR {
            let code = stream_shell_stderr(sb, &scripts::node_switch(major)).await?;
            if code != 0 {
                bail!("installing Node {major} in the guest failed");
            }
        }
    }

    // 3. Install dependencies in the guest — they run there, so they get the
    //    Linux-native artifacts (no host-platform retargeting).
    if has_composer {
        eprintln!("{}", ">> composer install (in guest)...".dimmed());
        let code = stream_shell_stderr(sb, scripts::COMPOSER_INSTALL).await?;
        if code != 0 {
            bail!("composer install in the guest failed (exit {code})");
        }
    }
    if has_npm {
        eprintln!("{}", ">> npm install (in guest)...".dimmed());
        let code = stream_shell_stderr(sb, scripts::NPM_INSTALL).await?;
        if code != 0 {
            bail!("npm install in the guest failed (exit {code})");
        }
    }

    // 4. Build the guest command: raise the fd/inotify ceilings (Vite watches the
    //    whole tree), start the port bridges, then exec the stack.
    let guest_ports: Vec<u16> = ports.iter().map(|(_, g)| *g).collect();
    let guest_cmd = format!(
        "{}\n{}\n{}\ncd /workspace && exec {}",
        scripts::RAISE_FDS,
        scripts::RAISE_INOTIFY,
        scripts::port_bridge(&guest_ports),
        quote_args(command),
    );

    let forwarded = ports
        .iter()
        .map(|(h, g)| {
            if h == g {
                h.to_string()
            } else {
                format!("{h}->{g}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "\n{} {}  |  {} {}",
        ">> dev:".dimmed(),
        command.join(" "),
        "ports".dimmed(),
        forwarded.dimmed(),
    );
    eprintln!(
        "{}",
        ">> syncing host edits → guest (one-way); your machine stays read-only".dimmed()
    );
    eprintln!(
        "{}\n",
        ">> stop the dev server (Ctrl-C) to shut the sandbox down".dimmed()
    );

    // 5. Run the dev stack attached while one-way syncing host edits. Both borrow
    //    `sb` immutably; when the user stops the dev server, `attach_with`
    //    returns and the sync future is dropped (its watcher stops with it).
    tokio::select! {
        res = sb.attach_with("bash", |a| a.args(["-lc", &guest_cmd])) => {
            let _ = res?;
        }
        // sync_loop only ever resolves on watcher-setup failure; otherwise it
        // runs until cancelled by the branch above completing.
        _ = sync_loop(sb, project_dir) => {}
    }
    Ok(())
}

/// Watch the project on the host and push every change into the guest's
/// `/workspace`. Never returns while syncing — it loops until the surrounding
/// `select!` cancels it. On watcher-setup failure it warns and parks (so the dev
/// session keeps running, just without live sync) rather than tearing down.
async fn sync_loop(sb: &Sandbox, root: &Path) {
    use notify::{RecursiveMode, Watcher};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => return park(format!("file sync disabled ({e})")).await,
        };
    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        return park(format!("file sync disabled ({e})")).await;
    }

    loop {
        // Block for the first event, then drain a short burst into one batch so a
        // save-all collapses to a single pass.
        let Some(first) = rx.recv().await else {
            return park("file sync stopped".into()).await;
        };
        let mut batch: HashSet<PathBuf> = HashSet::new();
        batch.insert(first);
        while let Ok(Some(path)) = tokio::time::timeout(COALESCE, rx.recv()).await {
            batch.insert(path);
        }
        for path in batch {
            reconcile(sb, root, &path).await;
        }
    }
}

/// Park forever after emitting a one-time warning, so a sync failure degrades the
/// dev session to "no live sync" instead of cancelling the attached command.
async fn park(reason: String) {
    eprintln!("{} {reason}", "warning:".yellow());
    std::future::pending::<()>().await;
}

/// Make the guest match the host for one changed path, reconciling by the path's
/// *current* state rather than the event kind — which handles creates, edits,
/// deletes, and renames (delivered as two paths) uniformly.
async fn reconcile(sb: &Sandbox, root: &Path, host_path: &Path) {
    let Ok(rel) = host_path.strip_prefix(root) else {
        return;
    };
    if is_excluded(rel) {
        return;
    }
    let Some(guest_rel) = to_guest_rel(rel) else {
        return;
    };
    let guest_path = format!("/workspace/{guest_rel}");

    match std::fs::symlink_metadata(host_path) {
        Ok(meta) if meta.file_type().is_dir() => {
            let _ = sb.fs().mkdir(&guest_path).await;
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            if let Ok(target) = std::fs::read_link(host_path) {
                if let Some(target) = target.to_str() {
                    let _ = sb.fs().remove(&guest_path).await;
                    let _ = sb.fs().symlink(target, &guest_path).await;
                }
            }
        }
        Ok(_) => {
            // Regular file: ensure the parent exists (events can arrive out of
            // order), then push the contents.
            if let Some((parent, _)) = guest_rel.rsplit_once('/') {
                let _ = sb.fs().mkdir(&format!("/workspace/{parent}")).await;
            }
            if let Err(e) = sb.fs().copy_from_host(host_path, &guest_path).await {
                eprintln!("{} sync {guest_rel}: {e}", "warning:".yellow());
            }
        }
        Err(_) => {
            // Gone on the host → remove from the guest (file first, then dir).
            if sb.fs().remove(&guest_path).await.is_err() {
                let _ = sb.fs().remove_dir(&guest_path).await;
            }
        }
    }
}

/// Paths the host must never push into the guest: the guest owns its own copies
/// of these (installed dependencies, build output, framework runtime state), and
/// overwriting them with the host's — possibly absent or wrong-platform — version
/// would break the running stack. This is the sync-side inverse of an
/// "enumerate writable paths" config: instead of listing where the guest may
/// write, we list what the host won't touch.
fn is_excluded(rel: &Path) -> bool {
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    let Some(&last) = comps.last() else {
        return true;
    };
    // Dependencies and VCS, at any depth.
    if comps.iter().any(|c| *c == "node_modules" || *c == ".git") {
        return true;
    }
    if last == ".DS_Store" {
        return true;
    }
    let first = comps[0];
    // Composer deps, boxme's own dir, and Laravel's runtime-writable trees.
    if first == "vendor" || first == ".boxme" || first == "storage" {
        return true;
    }
    if comps.len() >= 2 && first == "bootstrap" && comps[1] == "cache" {
        return true;
    }
    // Vite output (`public/build`) and its dev-server marker (`public/hot`),
    // both written by the guest.
    if comps.len() >= 2 && first == "public" && (comps[1] == "build" || comps[1] == "hot") {
        return true;
    }
    false
}

/// Relative host path → forward-slash guest path under `/workspace`. `None` if it
/// contains anything but plain components (`..`, a drive prefix) — which the
/// watcher shouldn't produce, but we won't forward into the guest if it does.
fn to_guest_rel(rel: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Deterministic VM name for a folder's dev session, so `boxme attach` in the
/// same folder finds the same VM with no state file. The slug keeps it readable;
/// the absolute-path hash keeps two same-named folders apart. One dev VM per
/// folder is intentional — `dev` refuses to clobber a running one, and `attach`
/// reuses it.
fn dev_vm_name(project_dir: &Path) -> String {
    let canonical =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let slug = slugify(
        &project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string()),
    );
    format!(
        "boxme-dev-{slug}-{:08x}",
        fnv1a(canonical.to_string_lossy().as_bytes())
    )
}

/// FNV-1a (32-bit) — a stable hash with no dependency, so the dev VM name is
/// identical across separate `dev` and `attach` invocations.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Whether a dev VM by this name is currently booted (vs. absent, or a stale
/// stopped one that `boot_dev`'s `.replace()` will clear).
async fn dev_session_running(name: &str) -> bool {
    matches!(
        Sandbox::get(name).await.map(|h| h.status()),
        Ok(SandboxStatus::Running)
    )
}

/// Pick the network policy for the dev session, the same way `run` does: strict
/// is registries-only, an existing allowlist enforces registries + its entries,
/// and a fresh project observes (open egress, DNS-only UDP). A dev server often
/// needs to reach a database or external API, so build the allowlist first with
/// `boxme <pm> install --learn` if you want enforcement.
fn dev_policy(cli: &Cli, project_dir: &Path) -> NetworkPolicy {
    if cli.strict {
        strict_policy()
    } else if allowlist::exists(project_dir, Scope::Packages) {
        enforced_policy(&allowlist::load(project_dir, Scope::Packages))
    } else {
        observe_policy()
    }
}

/// `--port HOST:GUEST` or a bare `--port PORT` (same on both sides); the default
/// set when none are given is artisan serve + Vite.
fn parse_ports(specs: &[String]) -> Result<Vec<(u16, u16)>> {
    if specs.is_empty() {
        return Ok(DEFAULT_PORTS.to_vec());
    }
    specs.iter().map(|s| parse_port(s)).collect()
}

fn parse_port(spec: &str) -> Result<(u16, u16)> {
    match spec.split_once(':') {
        Some((host, guest)) => Ok((parse_one(host)?, parse_one(guest)?)),
        None => {
            let p = parse_one(spec)?;
            Ok((p, p))
        }
    }
}

fn parse_one(s: &str) -> Result<u16> {
    s.trim()
        .parse::<u16>()
        .with_context(|| format!("invalid port `{s}`"))
}

/// Ensure each forwarded port's *host* side is free, bumping to the next open
/// port when it isn't — so a second `boxme dev` in another repo doesn't collide
/// on 8000/5173. Only the host port moves; the guest side is untouched (each app
/// still serves its normal port inside its own VM). `taken` keeps two auto-picks
/// in the same run from landing on the same port before the SDK binds them.
fn resolve_free_ports(ports: &[(u16, u16)]) -> Result<Vec<(u16, u16)>> {
    let mut chosen = Vec::with_capacity(ports.len());
    let mut taken: HashSet<u16> = HashSet::new();
    for &(host, guest) in ports {
        let mut candidate = host;
        while taken.contains(&candidate) || !host_port_free(candidate) {
            candidate = candidate
                .checked_add(1)
                .ok_or_else(|| anyhow!("no free host port available at or above {host}"))?;
        }
        if candidate != host {
            eprintln!(
                "{} host port {host} busy → using {candidate} (guest {guest})",
                ">> port:".dimmed()
            );
        }
        taken.insert(candidate);
        chosen.push((candidate, guest));
    }
    Ok(chosen)
}

/// Whether the host loopback port the SDK would publish on (`.port` binds
/// `127.0.0.1`) can be bound right now.
fn host_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn excluded(p: &str) -> bool {
        is_excluded(Path::new(p))
    }

    #[test]
    fn excludes_guest_owned_paths() {
        assert!(excluded("node_modules/vite/index.js"));
        assert!(excluded("packages/app/node_modules/x"));
        assert!(excluded("vendor/laravel/framework/x.php"));
        assert!(excluded(".git/HEAD"));
        assert!(excluded(".boxme/allow"));
        assert!(excluded("storage/logs/laravel.log"));
        assert!(excluded("bootstrap/cache/config.php"));
        assert!(excluded("public/build/manifest.json"));
        assert!(excluded("public/hot"));
        assert!(excluded(".DS_Store"));
        assert!(excluded("app/.DS_Store"));
    }

    #[test]
    fn syncs_real_source() {
        assert!(!excluded("app/Models/User.php"));
        assert!(!excluded("resources/js/app.js"));
        assert!(!excluded("routes/web.php"));
        assert!(!excluded(".env"));
        // A nested vendor that is real content (e.g. published assets) still
        // syncs — only a top-level vendor/ is the composer target.
        assert!(!excluded("public/vendor/telescope/app.js"));
        assert!(!excluded("public/index.php"));
    }

    #[test]
    fn guest_rel_rejects_traversal() {
        assert_eq!(
            to_guest_rel(Path::new("app/Http/Kernel.php")).as_deref(),
            Some("app/Http/Kernel.php")
        );
        assert_eq!(to_guest_rel(Path::new("../escape")), None);
        assert_eq!(to_guest_rel(Path::new("")), None);
    }

    #[test]
    fn parses_port_specs() {
        assert_eq!(parse_ports(&[]).unwrap(), DEFAULT_PORTS.to_vec());
        assert_eq!(
            parse_ports(&["3000".into(), "8080:80".into()]).unwrap(),
            vec![(3000, 3000), (8080, 80)]
        );
        assert!(parse_ports(&["nope".into()]).is_err());
        assert!(parse_ports(&["80:bad".into()]).is_err());
    }

    #[test]
    fn free_ports_bump_past_a_busy_host_port() {
        // Hold a real host port, then confirm the resolver moves off it while
        // keeping the guest side fixed and landing on something actually free.
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();

        let resolved = resolve_free_ports(&[(port, port)]).unwrap();
        assert_eq!(resolved.len(), 1);
        let (host, guest) = resolved[0];
        assert_ne!(host, port, "should bump off the busy port");
        assert_eq!(guest, port, "guest side stays put");
        assert!(host_port_free(host));
    }
}
