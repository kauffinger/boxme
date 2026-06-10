use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use microsandbox::{NetworkPolicy, Sandbox, Volume};
use microsandbox_network::policy::{
    Action, Destination, Direction, DomainName, PortRange, Protocol, Rule,
};
use owo_colors::OwoColorize;

use crate::cli::Cli;
use crate::copyback::{self, CopyPlan};
use crate::detect;
use crate::manifest::{self, Change, WriteSet};
use crate::netcap;
use crate::review::{self, Decision, FileItem, FileKind, Review};
use crate::scripts;
use crate::setup::{base_snapshot_exists, BASE_SNAPSHOT};
use crate::util::{shell_capture, shell_quote, slugify, stream_shell_stderr, tar_directory};

/// Fetching a unified diff per unexpected file is one guest exec each — cap it.
const MAX_DIFFS: usize = 100;

pub async fn run(cli: &Cli, args: &[String]) -> Result<()> {
    // 1. Validate + detect.
    let tool = args.first().map(String::as_str).unwrap_or("");
    if !matches!(tool, "composer" | "npm") {
        bail!("boxme only wraps `composer` and `npm` (got `{tool}`)");
    }
    let tool_args = &args[1..];
    let write_set = manifest::expected_write_set(tool, tool_args);

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

    // 2. Boot — attached, so a boxme crash SIGTERMs the VM.
    ensure_cache_volumes().await?;
    let name = vm_name(&project_dir);
    eprintln!("{} '{name}' from {BASE_SNAPSHOT}...", ">> booting".dimmed());
    let mut builder = Sandbox::builder(name.as_str())
        .from_snapshot(BASE_SNAPSHOT)
        .memory(cli.memory)
        .cpus(cli.cpus)
        .replace()
        .volume("/root/.composer/cache", |m| m.named("boxme-composer-cache"))
        .volume("/root/.npm", |m| m.named("boxme-npm-cache"))
        .volume("/root/.n", |m| m.named("boxme-node-versions"));
    if cli.strict {
        builder = builder.network(|n| n.policy(strict_policy()));
    }
    for (key, value) in &env {
        builder = builder.env(key, value);
    }
    let sb = builder.create().await?;

    let outcome = run_inner(&sb, &project_dir, tool, args, &write_set, php, node).await;

    // 11. Cleanup — always, also on errors mid-flight.
    if cli.keep {
        eprintln!("{} VM kept running as '{name}'", ">> --keep:".dimmed());
        sb.detach().await;
    } else {
        let _ = sb.stop().await;
        drop(sb);
        if let Err(e) = Sandbox::remove(&name).await {
            eprintln!("warning: could not remove VM '{name}': {e}");
        }
    }

    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    sb: &Sandbox,
    project_dir: &Path,
    tool: &str,
    args: &[String],
    write_set: &WriteSet,
    php: String,
    node: Option<u32>,
) -> Result<()> {
    let command_line = args.join(" ");
    // 3. Unpack the project into /workspace and tag the guest baseline.
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

    // 4. Match host versions.
    if tool == "composer" {
        let code = stream_shell_stderr(sb, &scripts::php_switch(&php)).await?;
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

    // 5. Manifest before (after unpack, so extraction artifacts can't pollute).
    let before = manifest::parse(&shell_capture(sb, scripts::MANIFEST).await?);

    // 6. Network capture must be live before the command starts.
    let capture = netcap::start(sb).await;
    if capture.is_none() {
        eprintln!(
            "{}",
            "warning: tcpdump unavailable in the guest — network capture disabled \
             (rebuild with `boxme setup --force`)"
                .yellow()
        );
    }

    // 7. The actual command, fully interactive on the host terminal.
    eprintln!("{} {command_line}\n", ">> running:".dimmed());
    let guest_cmd = format!("cd /workspace && exec {}", quote_args(args));
    let exit_code = sb
        .attach_with("bash", |a| a.args(["-lc", &guest_cmd]))
        .await?;

    // 8. Stop capture, parse contacts.
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

    // 9. Manifest after + diff, partitioned expected vs unexpected.
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

    // 10. Review.
    let decision = review::run(Review {
        files,
        network,
        network_banner,
        exit_code,
        command: format!("boxme {command_line}"),
    })?;

    match decision {
        Decision::Abort => {
            eprintln!("{}", "aborted — nothing copied back".red());
            Ok(())
        }
        Decision::Approve => {
            let plan = CopyPlan {
                dirs: write_set.dirs.iter().map(|d| d.to_string()).collect(),
                files: expected_files
                    .into_iter()
                    .filter(|p| !matches!(lookup(&changes, p), Some(Change::Deleted)))
                    .chain(
                        unexpected
                            .iter()
                            .filter(|(_, c)| !matches!(c, Change::Deleted))
                            .map(|(p, _)| p.clone()),
                    )
                    .collect(),
                deletions: changes
                    .iter()
                    .filter(|(_, c)| matches!(c, Change::Deleted))
                    .map(|(p, _)| p.clone())
                    .collect(),
            };
            copyback::apply(sb, project_dir, &plan)
                .await
                .context("copy-back failed")?;
            eprintln!("{}", "approved — results copied into the project".green());
            Ok(())
        }
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

/// Deny-by-default egress: DNS plus the package registries over HTTP(S).
fn strict_policy() -> NetworkPolicy {
    let mut rules = vec![Rule::allow_dns()];
    for host in netcap::STRICT_DOMAINS {
        // `DomainSuffix` matches the apex domain itself and every subdomain.
        let suffix: DomainName = host
            .parse()
            .map_err(|_| anyhow!("bad builtin domain {host}"))
            .expect("builtin strict domains parse");
        rules.push(Rule {
            direction: Direction::Egress,
            destination: Destination::DomainSuffix(suffix),
            protocols: vec![Protocol::Tcp],
            ports: vec![PortRange::single(443), PortRange::single(80)],
            action: Action::Allow,
        });
    }
    NetworkPolicy {
        default_egress: Action::Deny,
        default_ingress: Action::Allow,
        rules,
    }
}
