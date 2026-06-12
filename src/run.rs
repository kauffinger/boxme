use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use microsandbox::{NetworkPolicy, Sandbox, Volume};
use microsandbox_network::policy::{
    Action, Destination, DestinationGroup, Direction, DomainName, PortRange, Protocol, Rule,
};
use owo_colors::OwoColorize;

use crate::allowlist;
use crate::cli::Cli;
use crate::copyback::{self, CopyPlan};
use crate::detect;
use crate::manifest::{self, Change, WriteSet};
use crate::netcap::{self, NetworkContact};
use crate::review::{self, Decision, FileItem, FileKind, NetRow, Review};
use crate::scripts;
use crate::setup::{base_snapshot_exists, BASE_SNAPSHOT};
use crate::util::{shell_capture, shell_quote, slugify, stream_shell_stderr, tar_directory};

/// Fetching a unified diff per unexpected file is one guest exec each — cap it.
const MAX_DIFFS: usize = 100;

/// Everything one in-guest command run produces that the review and copy-back
/// need. Built by `run_command`.
struct CommandRun {
    command: String,
    exit_code: i32,
    files: Vec<FileItem>,
    network: Vec<NetworkContact>,
    network_banner: Option<String>,
    changes: Vec<(String, Change)>,
    expected_files: Vec<String>,
    unexpected: Vec<(String, Change)>,
}

pub async fn run(cli: &Cli, args: &[String]) -> Result<()> {
    // 1. Validate + detect.
    let tool = args.first().map(String::as_str).unwrap_or("");
    if !matches!(tool, "composer" | "npm") {
        bail!("boxme only wraps `composer` and `npm` (got `{tool}`)");
    }
    let write_set = manifest::expected_write_set(tool, &args[1..]);

    if !base_snapshot_exists().await? {
        bail!("base snapshot missing — run `boxme setup` first");
    }

    let project_dir = std::env::current_dir()?;
    let php = detect::php_version(&project_dir).await;
    let node = detect::node_major(&project_dir).await;
    eprintln!(
        "{} php {php}, node {}",
        ">> detected:".dimmed(),
        node.map(|n| n.to_string())
            .unwrap_or_else(|| format!("{} (default)", scripts::BASE_NODE_MAJOR)),
    );

    let env = resolve_env(&cli.env)?;
    ensure_cache_volumes().await?;

    // A run learns (observe + pick) when asked to, or the first time a project
    // has no allowlist yet. Otherwise it enforces straight away: `--strict` is
    // registries-only, an existing allowlist is registries + its entries.
    if cli.learn || (!cli.strict && !allowlist::exists(&project_dir)) {
        learn_run(cli, &project_dir, tool, args, &write_set, &php, node, &env).await
    } else {
        let policy = if cli.strict {
            strict_policy()
        } else {
            enforced_policy(&allowlist::load(&project_dir))
        };
        enforced_run(cli, &project_dir, tool, args, &write_set, &php, node, &env, policy).await
    }
}

/// Observe the command with the network open, let the user trust hosts in the
/// review, save them, then either copy back directly (if nothing it touched
/// would be blocked) or re-run under enforcement for a clean result.
#[allow(clippy::too_many_arguments)]
async fn learn_run(
    cli: &Cli,
    project_dir: &Path,
    tool: &str,
    args: &[String],
    write_set: &WriteSet,
    php: &str,
    node: Option<u32>,
    env: &[(String, String)],
) -> Result<()> {
    eprintln!(
        "{}",
        ">> learn: observing this run to build the allowlist".dimmed()
    );
    let (sb, name) = boot(cli, env, observe_policy(), vm_name(project_dir)).await?;

    let mut run = match run_command(&sb, project_dir, tool, args, write_set, php, node).await {
        Ok(run) => run,
        Err(e) => {
            discard(sb, &name).await;
            return Err(e);
        }
    };

    let review = review::run(Review {
        files: std::mem::take(&mut run.files),
        network: net_rows(&run.network, true),
        network_selectable: true,
        network_banner: run.network_banner.clone(),
        exit_code: run.exit_code,
        command: run.command.clone(),
    });
    let outcome = match review {
        Ok(outcome) => outcome,
        Err(e) => {
            discard(sb, &name).await;
            return Err(e);
        }
    };

    if outcome.decision == Decision::Abort {
        discard(sb, &name).await;
        eprintln!("{}", "aborted — no allowlist written, nothing copied back".red());
        return Ok(());
    }

    // Persist the picks; this creates the file even with no extra hosts, so
    // future runs in this project enforce by default.
    let merged = match allowlist::save_merged(project_dir, &outcome.allow) {
        Ok(merged) => merged,
        Err(e) => {
            discard(sb, &name).await;
            return Err(e);
        }
    };
    eprintln!(
        "{} {} extra host(s) → {}",
        ">> learn: allowlist saved,".dimmed(),
        merged.len(),
        allowlist::path(project_dir).display(),
    );

    // If everything the command contacted is allowed under enforcement, the
    // observe run already is the clean result — copy it back without a re-run.
    let blocked = blocked_hosts(&run.network, &merged);
    if blocked.is_empty() {
        let result = copy_back(&sb, project_dir, write_set, &run).await;
        cleanup(cli, sb, &name).await;
        result?;
        eprintln!("{}", "approved — results copied into the project".green());
        return Ok(());
    }

    eprintln!(
        "{} {}",
        ">> re-running clean; these contacted host(s) will be blocked:".dimmed(),
        blocked.join(", ").dimmed(),
    );
    discard(sb, &name).await;
    enforced_run(
        cli,
        project_dir,
        tool,
        args,
        write_set,
        php,
        node,
        env,
        enforced_policy(&merged),
    )
    .await
}

/// Single-pass run under a fixed policy: boot, run, review (read-only), copy
/// back on approval.
#[allow(clippy::too_many_arguments)]
async fn enforced_run(
    cli: &Cli,
    project_dir: &Path,
    tool: &str,
    args: &[String],
    write_set: &WriteSet,
    php: &str,
    node: Option<u32>,
    env: &[(String, String)],
    policy: NetworkPolicy,
) -> Result<()> {
    let (sb, name) = boot(cli, env, policy, vm_name(project_dir)).await?;
    let outcome = async {
        let run = run_command(&sb, project_dir, tool, args, write_set, php, node).await?;
        finish_review(&sb, project_dir, write_set, run).await
    }
    .await;
    cleanup(cli, sb, &name).await;
    outcome
}

/// Run the package-manager command in an already-booted guest and gather the
/// before/after manifest diff and network capture. Does not review or copy back.
#[allow(clippy::too_many_arguments)]
async fn run_command(
    sb: &Sandbox,
    project_dir: &Path,
    tool: &str,
    args: &[String],
    write_set: &WriteSet,
    php: &str,
    node: Option<u32>,
) -> Result<CommandRun> {
    let command_line = args.join(" ");

    // Unpack the project into /workspace and tag the guest baseline.
    eprintln!("{}", ">> packing project...".dimmed());
    let tarball = std::env::temp_dir().join(format!("boxme-{}.tgz", std::process::id()));
    let tarball_str = tarball.to_string_lossy().to_string();
    tar_directory(&project_dir.to_string_lossy(), &tarball_str).await?;
    sb.fs()
        .copy_from_host(&tarball, "/tmp/repo.tgz")
        .await
        .context("copying project into the sandbox failed")?;
    let _ = tokio::fs::remove_file(&tarball).await;
    let code = stream_shell_stderr(sb, scripts::UNPACK).await?;
    if code != 0 {
        bail!("unpacking the project in the guest failed (exit {code})");
    }

    // Match host versions.
    if tool == "composer" {
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

    // Manifest before (after unpack, so extraction artifacts can't pollute).
    let before = manifest::parse(&shell_capture(sb, scripts::MANIFEST).await?);

    // Network capture must be live before the command starts.
    let capture = netcap::start(sb).await;
    if capture.is_none() {
        eprintln!(
            "{}",
            "warning: tcpdump unavailable in the guest — network capture disabled \
             (rebuild with `boxme setup --force`)"
                .yellow()
        );
    }

    // The actual command, fully interactive on the host terminal.
    eprintln!("{} {command_line}\n", ">> running:".dimmed());
    let guest_cmd = format!("cd /workspace && exec {}", quote_args(args));
    let exit_code = sb
        .attach_with("bash", |a| a.args(["-lc", &guest_cmd]))
        .await?;

    // Stop capture, parse contacts.
    let mut network_banner = None;
    let network = match capture {
        Some(cap) => {
            cap.stop().await;
            match netcap::contacts(sb).await {
                Ok(contacts) => contacts,
                Err(_) => {
                    network_banner =
                        Some("network capture unreadable — contacts unknown".to_string());
                    Vec::new()
                }
            }
        }
        None => {
            network_banner = Some(
                "network capture unavailable (no tcpdump in base image) — \
                 run `boxme setup --force` to enable it"
                    .to_string(),
            );
            Vec::new()
        }
    };

    // Manifest after + diff, partitioned expected vs unexpected.
    let after = manifest::parse(&shell_capture(sb, scripts::MANIFEST).await?);
    let changes = manifest::diff(&before, &after);

    let mut files: Vec<FileItem> = Vec::new();
    for dir in &write_set.dirs {
        let count = shell_capture(sb, &scripts::count_files(dir))
            .await
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if count > 0 {
            files.push(FileItem {
                label: format!("{dir}/: {count} files"),
                kind: FileKind::ExpectedSummary,
                diff: None,
            });
        }
    }

    let mut expected_files: Vec<String> = Vec::new();
    let mut unexpected: Vec<(String, Change)> = Vec::new();
    for (path, change) in &changes {
        if write_set.contains(path) {
            expected_files.push(path.clone());
            files.push(FileItem {
                label: path.clone(),
                kind: FileKind::ExpectedFile,
                diff: fetch_diff(sb, path, change).await,
            });
        } else {
            unexpected.push((path.clone(), change.clone()));
        }
    }

    for (i, (path, change)) in unexpected.iter().enumerate() {
        let diff = if i < MAX_DIFFS {
            fetch_diff(sb, path, change).await
        } else {
            None
        };
        let is_binary = diff
            .as_deref()
            .is_some_and(|d| d.contains("Binary files") && d.lines().count() <= 2);
        let kind = if is_binary {
            FileKind::Binary
        } else {
            match change {
                Change::Added => FileKind::Added,
                Change::Modified => FileKind::Modified,
                Change::Deleted => FileKind::Deleted,
            }
        };
        files.push(FileItem {
            label: path.clone(),
            kind,
            diff,
        });
    }

    Ok(CommandRun {
        command: format!("boxme {command_line}"),
        exit_code,
        files,
        network,
        network_banner,
        changes,
        expected_files,
        unexpected,
    })
}

/// Read-only review of a finished run, then copy back on approval.
async fn finish_review(
    sb: &Sandbox,
    project_dir: &Path,
    write_set: &WriteSet,
    mut run: CommandRun,
) -> Result<()> {
    let outcome = review::run(Review {
        files: std::mem::take(&mut run.files),
        network: net_rows(&run.network, false),
        network_selectable: false,
        network_banner: run.network_banner.clone(),
        exit_code: run.exit_code,
        command: run.command.clone(),
    })?;

    match outcome.decision {
        Decision::Abort => {
            eprintln!("{}", "aborted — nothing copied back".red());
            Ok(())
        }
        Decision::Approve => {
            copy_back(sb, project_dir, write_set, &run).await?;
            eprintln!("{}", "approved — results copied into the project".green());
            Ok(())
        }
    }
}

/// Copy the approved write-set out of the guest into the project.
async fn copy_back(
    sb: &Sandbox,
    project_dir: &Path,
    write_set: &WriteSet,
    run: &CommandRun,
) -> Result<()> {
    let plan = CopyPlan {
        dirs: write_set.dirs.iter().map(|d| d.to_string()).collect(),
        files: run
            .expected_files
            .iter()
            .filter(|p| !matches!(lookup(&run.changes, p), Some(Change::Deleted)))
            .cloned()
            .chain(
                run.unexpected
                    .iter()
                    .filter(|(_, c)| !matches!(c, Change::Deleted))
                    .map(|(p, _)| p.clone()),
            )
            .collect(),
        deletions: run
            .changes
            .iter()
            .filter(|(_, c)| matches!(c, Change::Deleted))
            .map(|(p, _)| p.clone())
            .collect(),
    };
    copyback::apply(sb, project_dir, &plan)
        .await
        .context("copy-back failed")
}

/// One review row per distinct host. In a learn run, unexpected named hosts are
/// selectable (the user opts in to trust them); known registries and bare-IP
/// contacts are not — registries are always allowed, and an IP with no resolved
/// name is itself worth leaving blocked.
fn net_rows(contacts: &[NetworkContact], learn: bool) -> Vec<NetRow> {
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for c in contacts {
        let host = c.domain.clone().unwrap_or_else(|| c.ip.clone());
        if !seen.insert(host) {
            continue;
        }
        let selectable = learn && !c.known && c.domain.is_some();
        rows.push(NetRow {
            contact: c.clone(),
            selectable,
            selected: false,
        });
    }
    rows
}

/// Distinct hosts the command contacted that the enforced policy (registries +
/// `allow`) would block — i.e. the reason a clean re-run is needed.
fn blocked_hosts(contacts: &[NetworkContact], allow: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut blocked = Vec::new();
    for c in contacts {
        if contact_allowed(c, allow) {
            continue;
        }
        let host = c.domain.clone().unwrap_or_else(|| c.ip.clone());
        if seen.insert(host.clone()) {
            blocked.push(host);
        }
    }
    blocked
}

/// Whether a contacted host is reachable under enforcement: registries always
/// are; a named host is if some allowlist entry matches; a bare IP never is.
fn contact_allowed(c: &NetworkContact, allow: &[String]) -> bool {
    if c.known {
        return true;
    }
    match &c.domain {
        Some(domain) => allow.iter().any(|e| allowlist::entry_matches(e, domain)),
        None => false,
    }
}

fn lookup<'a>(changes: &'a [(String, Change)], path: &str) -> Option<&'a Change> {
    changes.iter().find(|(p, _)| p == path).map(|(_, c)| c)
}

/// Unified diff for one changed path. New files are untracked (the baseline
/// committed everything), so they diff against /dev/null; everything else
/// diffs against the boxme-baseline tag. `None` on any failure — the review
/// then shows the path without a diff.
async fn fetch_diff(sb: &Sandbox, path: &str, change: &Change) -> Option<String> {
    let quoted = shell_quote(path);
    let script = match change {
        Change::Added => format!("cd /workspace && diff -u /dev/null {quoted} || true"),
        _ => format!("cd /workspace && git diff boxme-baseline -- {quoted}"),
    };
    let out = shell_capture(sb, &script).await.ok()?;
    (!out.trim().is_empty()).then_some(out)
}

/// The user's command tokens, re-quoted for the guest shell.
fn quote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `-e KEY=VALUE` is taken verbatim; bare `-e KEY` copies the host value and
/// errors if the host doesn't have it (a silent skip would surface later as a
/// confusing auth failure in the guest).
fn resolve_env(specs: &[String]) -> Result<Vec<(String, String)>> {
    specs
        .iter()
        .map(|spec| match spec.split_once('=') {
            Some((key, value)) => Ok((key.to_string(), value.to_string())),
            None => std::env::var(spec)
                .map(|value| (spec.clone(), value))
                .map_err(|_| anyhow!("-e {spec}: not set in the host environment")),
        })
        .collect()
}

/// Boot a guest from the base snapshot under `policy`, mounting the shared
/// caches and injecting `env`.
async fn boot(
    cli: &Cli,
    env: &[(String, String)],
    policy: NetworkPolicy,
    name: String,
) -> Result<(Sandbox, String)> {
    eprintln!("{} '{name}' from {BASE_SNAPSHOT}...", ">> booting".dimmed());
    let mut builder = Sandbox::builder(name.as_str())
        .from_snapshot(BASE_SNAPSHOT)
        .memory(cli.memory)
        .cpus(cli.cpus)
        .replace()
        .volume("/root/.composer/cache", |m| m.named("boxme-composer-cache"))
        .volume("/root/.npm", |m| m.named("boxme-npm-cache"))
        .volume("/root/.n", |m| m.named("boxme-node-versions"))
        .network(|n| n.policy(policy));
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    let sb = builder.create().await?;
    Ok((sb, name))
}

/// Tear down the run VM, honoring `--keep`.
async fn cleanup(cli: &Cli, sb: Sandbox, name: &str) {
    if cli.keep {
        eprintln!("{} VM kept running as '{name}'", ">> --keep:".dimmed());
        sb.detach().await;
    } else {
        let _ = sb.stop().await;
        drop(sb);
        if let Err(e) = Sandbox::remove(name).await {
            eprintln!("warning: could not remove VM '{name}': {e}");
        }
    }
}

/// Unconditionally remove a VM (ignores `--keep`) — used for the throwaway
/// observe VM once a learn run decides to re-run under enforcement.
async fn discard(sb: Sandbox, name: &str) {
    let _ = sb.stop().await;
    drop(sb);
    if let Err(e) = Sandbox::remove(name).await {
        eprintln!("warning: could not remove observe VM '{name}': {e}");
    }
}

/// Named volumes must exist before a sandbox can mount them.
async fn ensure_cache_volumes() -> Result<()> {
    let existing: Vec<String> = Volume::list()
        .await?
        .iter()
        .map(|v| v.name().to_string())
        .collect();
    for name in [
        "boxme-composer-cache",
        "boxme-npm-cache",
        "boxme-node-versions",
    ] {
        if !existing.iter().any(|n| n == name) {
            Volume::builder(name).create().await?;
        }
    }
    Ok(())
}

fn vm_name(project_dir: &Path) -> String {
    let slug = slugify(
        &project_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string()),
    );
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("boxme-{slug}-{:04x}", nanos % 0x10000)
}

/// Default (non-strict) egress: DNS and any TCP destination on the public
/// internet are allowed and merely observed; everything else is denied. The
/// point of denying the rest is UDP — composer/npm need nothing beyond DNS over
/// UDP, so blocking it closes the QUIC/raw-UDP exfil channel that tcpdump's
/// SYN-based capture can't even see. Private/loopback/link-local/metadata stay
/// denied by falling through to the default, as with microsandbox's own
/// `public_only` default.
fn observe_policy() -> NetworkPolicy {
    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Allow,
        rules: vec![
            Rule::allow_dns(),
            Rule {
                direction: Direction::Egress,
                destination: Destination::Group(DestinationGroup::Public),
                protocols: vec![Protocol::Tcp],
                ports: vec![],
                action: Action::Allow,
            },
        ],
    }
}

/// Deny-by-default egress: DNS plus the package registries over HTTP(S).
fn strict_policy() -> NetworkPolicy {
    let mut rules = vec![Rule::allow_dns()];
    for host in netcap::STRICT_DOMAINS {
        rules.push(entry_rule(host).expect("builtin strict domain parses"));
    }
    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Allow,
        rules,
    }
}

/// Strict baseline plus the user's saved allowlist entries. Unparseable entries
/// are skipped (they can only get there by hand-editing the file).
fn enforced_policy(extra: &[String]) -> NetworkPolicy {
    let mut policy = strict_policy();
    for entry in extra {
        if let Some(rule) = entry_rule(entry) {
            policy.rules.push(rule);
        }
    }
    policy
}

/// An allow rule for one allowlist entry over HTTP(S). A `=`-prefixed entry
/// matches that exact host; a bare entry matches the domain and all subdomains.
fn entry_rule(entry: &str) -> Option<Rule> {
    let (exact, name) = match entry.strip_prefix('=') {
        Some(host) => (true, host),
        None => (false, entry),
    };
    let domain: DomainName = name.parse().ok()?;
    let destination = if exact {
        Destination::Domain(domain)
    } else {
        Destination::DomainSuffix(domain)
    };
    Some(Rule {
        direction: Direction::Egress,
        destination,
        protocols: vec![Protocol::Tcp],
        ports: vec![PortRange::single(443), PortRange::single(80)],
        action: Action::Allow,
    })
}
