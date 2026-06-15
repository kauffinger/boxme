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
use crate::outside::{self, OutsideScan};
use crate::review::{self, Decision, FileItem, FileKind, NetRow, NetStatus, Review};
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
    outside: OutsideScan,
    changes: Vec<(String, Change)>,
    expected_files: Vec<String>,
    unexpected: Vec<(String, Change)>,
}

/// What an enforced review resolves to: either we're finished (copied back or
/// aborted), or the user allowed blocked host(s) and wants a clean re-run under
/// the merged allowlist.
enum AfterReview {
    Done,
    Rerun(Vec<String>),
}

/// The per-run inputs every stage shares: which command, where, the detected
/// toolchain versions, and the resolved guest environment. Built once in `run`
/// and threaded through by reference.
struct RunCtx<'a> {
    cli: &'a Cli,
    project_dir: &'a Path,
    tool: &'a str,
    args: &'a [String],
    write_set: &'a WriteSet,
    php: &'a str,
    node: Option<u32>,
    env: Vec<(String, String)>,
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

    let ctx = RunCtx {
        cli,
        project_dir: &project_dir,
        tool,
        args,
        write_set: &write_set,
        php: &php,
        node,
        env,
    };

    // A run learns (observe + pick) when asked to, or the first time a project
    // has no allowlist yet. Otherwise it enforces straight away: `--strict` is
    // registries-only, an existing allowlist is registries + its entries.
    if cli.learn || (!cli.strict && !allowlist::exists(&project_dir)) {
        learn_run(&ctx).await
    } else {
        // --strict ignores the allowlist (registries only) and disables the
        // allow-and-re-run affordance, since allowlisting wouldn't change it.
        let (allow, can_trust) = if cli.strict {
            (Vec::new(), false)
        } else {
            (allowlist::load(&project_dir), true)
        };
        enforced_run(&ctx, allow, can_trust).await
    }
}

/// Observe the command with the network open, let the user trust hosts in the
/// review, save them, then either copy back directly (if nothing it touched
/// would be blocked) or re-run under enforcement for a clean result.
async fn learn_run(ctx: &RunCtx<'_>) -> Result<()> {
    eprintln!(
        "{}",
        ">> learn: observing this run to build the allowlist".dimmed()
    );
    let (sb, name) = boot(ctx, observe_policy(), vm_name(ctx.project_dir)).await?;

    // Everything that can fail before the teardown decision runs here, so the
    // observe VM is discarded in exactly one place on the error path. `None`
    // means the user aborted the review.
    let staged = async {
        let mut run = run_command(&sb, ctx).await?;
        let outcome = review::run(Review {
            files: std::mem::take(&mut run.files),
            network: net_rows(&run.network, learn_status),
            network_selectable: true,
            allow_rerun: false,
            network_banner: run.network_banner.clone(),
            outside_banner: run.outside.banner(),
            outside_truncated: run.outside.truncated,
            outside: std::mem::take(&mut run.outside.files),
            exit_code: run.exit_code,
            command: run.command.clone(),
        })?;
        if outcome.decision == Decision::Abort {
            return Ok(None);
        }
        // Persist the picks; this creates the file even with no extra hosts, so
        // future runs in this project enforce by default.
        let merged = allowlist::save_merged(ctx.project_dir, &outcome.allow)?;
        Ok::<_, anyhow::Error>(Some((run, merged)))
    }
    .await;

    let (run, merged) = match staged {
        Ok(Some(staged)) => staged,
        Ok(None) => {
            discard(sb, &name).await;
            eprintln!(
                "{}",
                "aborted — no allowlist written, nothing copied back".red()
            );
            return Ok(());
        }
        Err(e) => {
            discard(sb, &name).await;
            return Err(e);
        }
    };

    eprintln!(
        "{} {} extra host(s) → {}",
        ">> learn: allowlist saved,".dimmed(),
        merged.len(),
        allowlist::path(ctx.project_dir).display(),
    );

    // If everything the command contacted is allowed under enforcement, the
    // observe run already is the clean result — copy it back without a re-run.
    let blocked = blocked_hosts(&run.network, &merged);
    if blocked.is_empty() {
        let result = copy_back(&sb, ctx.project_dir, ctx.write_set, &run).await;
        cleanup(ctx.cli, sb, &name).await;
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
    enforced_run(ctx, merged, !ctx.cli.strict).await
}

/// Deny-by-default run: boot under the allowlist, run, review, and either copy
/// back on approval or — if the user allowed blocked hosts — re-run clean under
/// the updated allowlist. `can_trust` is false under `--strict`, where the
/// allowlist is ignored and allowing a host wouldn't change the next run.
async fn enforced_run(ctx: &RunCtx<'_>, allow: Vec<String>, can_trust: bool) -> Result<()> {
    let (sb, name) = boot(ctx, enforced_policy(&allow), vm_name(ctx.project_dir)).await?;
    let outcome = async {
        let run = run_command(&sb, ctx).await?;
        finish_review(&sb, ctx, run, &allow, can_trust).await
    }
    .await;

    // A re-run discards this VM unconditionally — like the observe VM, its result
    // is thrown away; otherwise tear down honoring --keep.
    match &outcome {
        Ok(AfterReview::Rerun(_)) => discard(sb, &name).await,
        _ => cleanup(ctx.cli, sb, &name).await,
    }

    match outcome? {
        AfterReview::Done => Ok(()),
        AfterReview::Rerun(merged) => {
            eprintln!(
                "{}",
                ">> re-running clean under the updated allowlist".dimmed()
            );
            Box::pin(enforced_run(ctx, merged, can_trust)).await
        }
    }
}

/// Run the package-manager command in an already-booted guest and gather the
/// before/after manifest diff and network capture. Does not review or copy back.
async fn run_command(sb: &Sandbox, ctx: &RunCtx<'_>) -> Result<CommandRun> {
    let (tool, args, write_set, php, node, project_dir) = (
        ctx.tool,
        ctx.args,
        ctx.write_set,
        ctx.php,
        ctx.node,
        ctx.project_dir,
    );
    let command_line = args.join(" ");

    // Unpack the project into /workspace and tag the guest baseline.
    eprintln!("{}", ">> packing project...".dimmed());
    let tarball = std::env::temp_dir().join(format!("boxme-{}.tgz", std::process::id()));
    tar_directory(project_dir, &tarball).await?;
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

    // Stamp the out-of-workspace baseline now that setup (unpack, version
    // switch) is done — from here, anything written outside /workspace is the
    // command's doing. Best-effort: a missing marker just disables the scan.
    let _ = shell_capture(sb, &format!("touch {}", scripts::BASELINE_MARKER)).await;

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
    let guest_cmd = format!(
        "{}; cd /workspace && exec {}",
        scripts::RAISE_FDS,
        quote_args(args)
    );
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

    // Sweep for anything the command wrote outside /workspace.
    let outside = match shell_capture(sb, &scripts::outside_scan()).await {
        Ok(out) => outside::parse(&out),
        Err(_) => OutsideScan::unavailable(),
    };

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
            unexpected.push((path.clone(), *change));
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
        outside,
        changes,
        expected_files,
        unexpected,
    })
}

/// Review of a finished enforced run. Copies back on approval, aborts on quit,
/// or — when the user marked blocked hosts and confirmed — saves them to the
/// allowlist and signals a clean re-run.
async fn finish_review(
    sb: &Sandbox,
    ctx: &RunCtx<'_>,
    mut run: CommandRun,
    allow: &[String],
    can_trust: bool,
) -> Result<AfterReview> {
    let outcome = review::run(Review {
        files: std::mem::take(&mut run.files),
        network: net_rows(&run.network, |c| enforced_status(c, allow, can_trust)),
        network_selectable: can_trust,
        allow_rerun: can_trust,
        network_banner: run.network_banner.clone(),
        outside_banner: run.outside.banner(),
        outside_truncated: run.outside.truncated,
        outside: std::mem::take(&mut run.outside.files),
        exit_code: run.exit_code,
        command: run.command.clone(),
    })?;

    match outcome.decision {
        Decision::Abort => {
            eprintln!("{}", "aborted — nothing copied back".red());
            Ok(AfterReview::Done)
        }
        Decision::Approve => {
            copy_back(sb, ctx.project_dir, ctx.write_set, &run).await?;
            eprintln!("{}", "approved — results copied into the project".green());
            Ok(AfterReview::Done)
        }
        Decision::Rerun => {
            let merged = allowlist::save_merged(ctx.project_dir, &outcome.allow)?;
            eprintln!(
                "{} {} host(s) → {}",
                ">> allowed".dimmed(),
                outcome.allow.len(),
                allowlist::path(ctx.project_dir).display(),
            );
            Ok(AfterReview::Rerun(merged))
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

/// One review row per distinct host, classified by `classify` into a status and
/// whether it can be selected. Bare-IP contacts are never selectable — there's no
/// name to write to the allowlist.
fn net_rows(
    contacts: &[NetworkContact],
    classify: impl Fn(&NetworkContact) -> (NetStatus, bool),
) -> Vec<NetRow> {
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    for c in contacts {
        if !seen.insert(c.host().to_string()) {
            continue;
        }
        let (status, selectable) = classify(c);
        rows.push(NetRow {
            contact: c.clone(),
            status,
            selectable,
            selected: false,
        });
    }
    rows
}

/// Learn run: registries are known; every other contact was merely observed and —
/// if it resolved to a domain — can be trusted for future enforcement.
fn learn_status(c: &NetworkContact) -> (NetStatus, bool) {
    if c.known {
        (NetStatus::Known, false)
    } else {
        (NetStatus::Observed, c.domain.is_some())
    }
}

/// Enforced run: known registries and allowlisted hosts are reachable; everything
/// else was blocked, and a blocked named host can be selected to allow on a clean
/// re-run (never under `--strict`, where the allowlist is ignored).
fn enforced_status(c: &NetworkContact, allow: &[String], can_trust: bool) -> (NetStatus, bool) {
    if c.known {
        (NetStatus::Known, false)
    } else if contact_allowed(c, allow) {
        (NetStatus::Allowed, false)
    } else {
        (NetStatus::Blocked, can_trust && c.domain.is_some())
    }
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
        let host = c.host().to_string();
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

fn lookup(changes: &[(String, Change)], path: &str) -> Option<Change> {
    changes.iter().find(|(p, _)| p == path).map(|(_, c)| *c)
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
/// caches and injecting the run's environment.
async fn boot(ctx: &RunCtx<'_>, policy: NetworkPolicy, name: String) -> Result<(Sandbox, String)> {
    eprintln!("{} '{name}' from {BASE_SNAPSHOT}...", ">> booting".dimmed());
    let mut builder = Sandbox::builder(name.as_str())
        .from_snapshot(BASE_SNAPSHOT)
        .memory(ctx.cli.memory)
        .cpus(ctx.cli.cpus)
        .replace()
        .volume("/root/.composer/cache", |m| m.named("boxme-composer-cache"))
        .volume("/root/.npm", |m| m.named("boxme-npm-cache"))
        .volume("/root/.n", |m| m.named("boxme-node-versions"))
        .network(|n| n.policy(policy));
    for (key, value) in &ctx.env {
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
        remove_vm(sb, name, "VM").await;
    }
}

/// Unconditionally remove a VM (ignores `--keep`) — used for the throwaway
/// observe VM once a learn run decides to re-run under enforcement.
async fn discard(sb: Sandbox, name: &str) {
    remove_vm(sb, name, "observe VM").await;
}

/// Stop and remove a VM. `Sandbox::remove` operates by name, so the local
/// handle is dropped first to release it before removal.
async fn remove_vm(sb: Sandbox, name: &str, label: &str) {
    let _ = sb.stop().await;
    drop(sb);
    if let Err(e) = Sandbox::remove(name).await {
        eprintln!("warning: could not remove {label} '{name}': {e}");
    }
}

/// Named volumes must exist before a sandbox can mount them.
async fn ensure_cache_volumes() -> Result<()> {
    let existing = Volume::list().await?;
    for name in [
        "boxme-composer-cache",
        "boxme-npm-cache",
        "boxme-node-versions",
    ] {
        if !existing.iter().any(|v| v.name() == name) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(domain: Option<&str>, known: bool) -> NetworkContact {
        NetworkContact {
            domain: domain.map(str::to_string),
            ip: "1.2.3.4".to_string(),
            port: 443,
            known,
        }
    }

    #[test]
    fn enforced_status_marks_only_blocked_named_hosts_selectable() {
        let allow = vec!["example.com".to_string()];

        // Registry: reachable, not selectable.
        assert_eq!(
            enforced_status(&contact(Some("packagist.org"), true), &allow, true),
            (NetStatus::Known, false)
        );
        // Covered by the allowlist (subdomain): reachable, not selectable.
        assert_eq!(
            enforced_status(&contact(Some("api.example.com"), false), &allow, true),
            (NetStatus::Allowed, false)
        );
        // Blocked named host: selectable when trust is permitted.
        assert_eq!(
            enforced_status(&contact(Some("evil.test"), false), &allow, true),
            (NetStatus::Blocked, true)
        );
        // Same host under --strict: shown blocked, but not selectable.
        assert_eq!(
            enforced_status(&contact(Some("evil.test"), false), &allow, false),
            (NetStatus::Blocked, false)
        );
        // Bare IP: blocked, never selectable — nothing to add to the allowlist.
        assert_eq!(
            enforced_status(&contact(None, false), &allow, true),
            (NetStatus::Blocked, false)
        );
    }
}
