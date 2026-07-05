//! The VM lifecycle surface: list boxme's VMs, run commands in them, attach a
//! shell, and remove strays. A kept VM (`--keep`, or a copy-out failure keeping
//! an agent's work alive) is only useful if you can get back into it — and only
//! tolerable if you can find and delete the ones you forgot about.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use microsandbox::sandbox::SandboxStatus;
use microsandbox::Sandbox;
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::run::quote_args;
use crate::setup::BUILDER;
use crate::util::{project_slug, stream_shell_split};

/// Which of boxme's name shapes a sandbox matches. Everything boxme boots is
/// classified here; sandboxes from other tools never are — which is what lets
/// `ps` and `kill --all` operate on "boxme's VMs" without a state file.
#[derive(Debug, PartialEq, Clone, Copy)]
enum VmKind {
    /// `boxme-dev-<slug>-<8 hex>` — a deterministic per-folder dev session.
    Dev,
    /// `boxme-<slug>-<4 hex>` — a throwaway run/claude VM (kept via `--keep`).
    Run,
    /// The one-off base-snapshot builder from `boxme setup`.
    Setup,
}

fn classify(name: &str) -> Option<VmKind> {
    if name == BUILDER {
        return Some(VmKind::Setup);
    }
    let rest = name.strip_prefix("boxme-")?;
    let (stem, suffix) = rest.rsplit_once('-')?;
    if stem.is_empty() || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The suffix length tells the shapes apart even for a folder literally
    // named `dev-…`: the dev hash is always 8 hex digits, the run nonce 4.
    match suffix.len() {
        8 if stem.strip_prefix("dev-").is_some_and(|s| !s.is_empty()) => Some(VmKind::Dev),
        4 => Some(VmKind::Run),
        _ => None,
    }
}

fn kind_label(kind: VmKind) -> &'static str {
    match kind {
        VmKind::Dev => "dev",
        VmKind::Run => "run",
        VmKind::Setup => "setup",
    }
}

fn status_label(status: SandboxStatus) -> &'static str {
    match status {
        SandboxStatus::Running => "running",
        SandboxStatus::Draining => "draining",
        SandboxStatus::Paused => "paused",
        SandboxStatus::Stopped => "stopped",
        SandboxStatus::Crashed => "crashed",
    }
}

#[derive(Serialize)]
struct VmRow {
    name: String,
    kind: &'static str,
    status: &'static str,
    created_at: Option<String>,
    age_seconds: Option<i64>,
}

/// List boxme's VMs with status and age. `json` prints a machine-readable
/// array to stdout instead of the table.
pub async fn ps(json: bool) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let rows: Vec<VmRow> = Sandbox::list()
        .await
        .context("listing sandboxes")?
        .iter()
        .filter_map(|h| {
            let kind = classify(h.name())?;
            Some(VmRow {
                name: h.name().to_string(),
                kind: kind_label(kind),
                status: status_label(h.status()),
                created_at: h.created_at().map(|t| t.to_rfc3339()),
                age_seconds: h.created_at().map(|t| (now - t.timestamp()).max(0)),
            })
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        eprintln!("no boxme VMs — `--keep` keeps one after a run, `boxme dev` starts one");
        return Ok(());
    }
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max(4);
    println!("{:<name_w$}  {:<5}  {:<8}  AGE", "NAME", "KIND", "STATUS");
    for r in &rows {
        println!(
            "{:<name_w$}  {:<5}  {:<8}  {}",
            r.name,
            r.kind,
            r.status,
            r.age_seconds.map(format_age).unwrap_or_else(|| "-".into()),
        );
    }
    Ok(())
}

fn format_age(secs: i64) -> String {
    match secs {
        s if s < 0 => "0s".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

/// Open an interactive shell — or run a one-off command on a TTY — inside a
/// running boxme VM. Connects as a second client and never owns teardown: the
/// process that booted the VM (or `boxme kill`) controls its lifecycle.
pub async fn attach(vm: Option<&str>, cmd: &[String]) -> Result<()> {
    let (name, sb) = connect_target(vm).await?;
    let inner = if cmd.is_empty() {
        "cd /workspace && exec bash -l".to_string()
    } else {
        format!("cd /workspace && exec {}", quote_args(cmd))
    };
    eprintln!("{} {name}", ">> attached to".dimmed());
    let result = sb.attach_with("bash", |a| a.args(["-lc", &inner])).await;
    sb.detach().await;
    result?;
    Ok(())
}

/// Run one command inside a running boxme VM without a TTY: guest stdout stays
/// stdout, guest stderr stays stderr, and the command's exit code becomes ours.
/// The scriptable counterpart of `attach`, for agents inspecting a kept VM.
pub async fn exec(vm: Option<&str>, cmd: &[String]) -> Result<i32> {
    let (_name, sb) = connect_target(vm).await?;
    let script = format!("cd /workspace && {}", quote_args(cmd));
    let result = stream_shell_split(&sb, &script).await;
    sb.detach().await;
    result
}

/// Stop and remove boxme VMs by name, or every one with `all`. Only boxme's
/// own name shapes are accepted — this is destructive and shouldn't be able to
/// delete another tool's sandbox on a typo.
pub async fn kill(names: &[String], all: bool) -> Result<()> {
    let targets: Vec<String> = if all {
        Sandbox::list()
            .await
            .context("listing sandboxes")?
            .iter()
            .map(|h| h.name().to_string())
            .filter(|n| classify(n).is_some())
            .collect()
    } else {
        if let Some(bad) = names.iter().find(|n| classify(n).is_none()) {
            bail!("'{bad}' isn't a boxme VM (see `boxme ps`) — boxme only removes its own");
        }
        names.to_vec()
    };
    if targets.is_empty() {
        eprintln!("no boxme VMs to remove");
        return Ok(());
    }

    let mut failed = 0;
    for name in &targets {
        match stop_and_remove(name).await {
            Ok(()) => eprintln!("{} {name}", ">> removed".dimmed()),
            Err(e) => {
                failed += 1;
                eprintln!("{} could not remove '{name}': {e:#}", "warning:".yellow());
            }
        }
    }
    if failed > 0 {
        bail!("{failed} of {} VM(s) could not be removed", targets.len());
    }
    Ok(())
}

async fn stop_and_remove(name: &str) -> Result<()> {
    let handle = Sandbox::get(name)
        .await
        .map_err(|_| anyhow!("no VM by this name"))?;
    // Graceful stop with a force-kill fallback; a no-op if already stopped.
    handle.stop().await?;
    handle.remove().await?;
    Ok(())
}

/// Resolve `--vm NAME` (any VM) or fall back to the current folder's single
/// running VM, and connect as a guest client.
async fn connect_target(vm: Option<&str>) -> Result<(String, Sandbox)> {
    let name = match vm {
        Some(name) => name.to_string(),
        None => pick_one(running_for_folder(&std::env::current_dir()?).await?)?,
    };
    let handle = Sandbox::get(&name)
        .await
        .map_err(|_| anyhow!("no VM named '{name}' — `boxme ps` lists boxme's VMs"))?;
    if !matches!(handle.status(), SandboxStatus::Running) {
        bail!(
            "VM '{name}' isn't running ({}) — `boxme kill {name}` removes it",
            status_label(handle.status())
        );
    }
    let sb = handle
        .connect()
        .await
        .with_context(|| format!("connecting to '{name}'"))?;
    Ok((name, sb))
}

async fn running_for_folder(dir: &Path) -> Result<Vec<String>> {
    let dev_name = crate::dev::dev_vm_name(dir);
    let slug = project_slug(dir);
    Ok(Sandbox::list()
        .await
        .context("listing sandboxes")?
        .iter()
        .filter(|h| matches!(h.status(), SandboxStatus::Running))
        .map(|h| h.name().to_string())
        .filter(|n| belongs_to_folder(n, &dev_name, &slug))
        .collect())
}

/// Whether a VM belongs to this folder: its deterministic dev session, or a
/// run/claude VM carrying the folder's slug. Two folders with the same name
/// collide on slug — acceptable, since `pick_one` lists the candidates and
/// `--vm` disambiguates.
fn belongs_to_folder(name: &str, dev_name: &str, slug: &str) -> bool {
    if name == dev_name {
        return true;
    }
    matches!(classify(name), Some(VmKind::Run))
        && name
            .strip_prefix("boxme-")
            .and_then(|rest| rest.rsplit_once('-'))
            .is_some_and(|(stem, _)| stem == slug)
}

fn pick_one(mut running: Vec<String>) -> Result<String> {
    match running.len() {
        1 => Ok(running.remove(0)),
        0 => bail!(
            "no running boxme VM for this folder — keep one after a run with --keep, \
             start a dev session with `boxme dev`, or name any VM with --vm (`boxme ps`)"
        ),
        _ => bail!(
            "several VMs are running for this folder — pick one with --vm:\n  {}",
            running.join("\n  ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_tells_boxmes_name_shapes_apart() {
        assert_eq!(classify("boxme-myapp-3f2a"), Some(VmKind::Run));
        assert_eq!(classify("boxme-my-app-3f2a"), Some(VmKind::Run));
        assert_eq!(classify("boxme-dev-myapp-0a1b2c3d"), Some(VmKind::Dev));
        assert_eq!(classify("boxme-base-builder"), Some(VmKind::Setup));
        // A folder literally named `dev-myapp` still makes a 4-hex run VM.
        assert_eq!(classify("boxme-dev-myapp-3f2a"), Some(VmKind::Run));
        assert_eq!(classify("boxme-myapp-xyz9"), None);
        assert_eq!(classify("boxme-myapp-12345"), None);
        assert_eq!(classify("boxme-3f2a"), None);
        assert_eq!(classify("someone-elses-vm-3f2a"), None);
    }

    #[test]
    fn folder_discovery_matches_only_this_folders_vms() {
        let dev = "boxme-dev-myapp-0a1b2c3d";
        assert!(belongs_to_folder(dev, dev, "myapp"));
        assert!(belongs_to_folder("boxme-myapp-3f2a", dev, "myapp"));
        assert!(!belongs_to_folder("boxme-other-3f2a", dev, "myapp"));
        // Another folder with the same name: different path hash, no match.
        assert!(!belongs_to_folder("boxme-dev-myapp-deadbeef", dev, "myapp"));
        assert!(!belongs_to_folder("boxme-base-builder", dev, "myapp"));
    }

    #[test]
    fn pick_one_wants_exactly_one_running_vm() {
        assert_eq!(
            pick_one(vec!["boxme-x-0001".into()]).unwrap(),
            "boxme-x-0001"
        );

        let none = pick_one(Vec::new()).unwrap_err().to_string();
        assert!(none.contains("--keep"));

        let many = pick_one(vec!["boxme-x-0001".into(), "boxme-x-0002".into()])
            .unwrap_err()
            .to_string();
        assert!(many.contains("--vm"));
        assert!(many.contains("boxme-x-0001") && many.contains("boxme-x-0002"));
    }

    #[test]
    fn format_age_uses_one_coarse_unit() {
        assert_eq!(format_age(-3), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3 * 3600 + 5), "3h");
        assert_eq!(format_age(49 * 3600), "2d");
    }
}
