//! `boxme claude`: run Claude Code inside the sandbox, then copy exactly what it
//! changed back out.
//!
//! The model reuses the review run's integrity core: the project is bind-mounted
//! read-only as an overlay lower, the agent's writes land in a throwaway guest
//! upper, and the host is never touched during the run. Instead of the file
//! review TUI, the agent's net changeset — a manifest diff against the guest
//! baseline — is staged out while the VM is alive and, once the VM is gone,
//! copied into your working tree as plain uncommitted edits you review with
//! `git diff`. If applying would overwrite files you've also edited locally,
//! boxme asks first (overwrite / branch / abort); a headless run, which can't
//! ask, lands the work on a fresh `boxme/claude-<n>` branch instead, leaving your
//! edits untouched. Nothing reaches your machine until after teardown.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use microsandbox::{NetworkPolicy, Sandbox};
use microsandbox_network::secrets::config::SecretEntry;
use owo_colors::OwoColorize;

use crate::allowlist::{self, Scope};
use crate::auth;
use crate::cli::Cli;
use crate::composer_auth;
use crate::copyback::{self, CopyPlan};
use crate::detect;
use crate::manifest::{self, Change};
use crate::netcap::{self, NetworkContact};
use crate::run::{
    claude_policy, cleanup, ensure_cache_volumes, observe_policy, resolve_env, vm_name,
};
use crate::scripts;
use crate::setup::{base_snapshot_exists, BASE_SNAPSHOT};
use crate::util::{shell_capture, stream_shell_stderr};

/// Auth credentials boxme forwards from the host shell when the user didn't pass
/// one explicitly via `-e`.
const AUTH_VARS: [&str; 2] = ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"];

/// Everything the in-guest run produced that teardown + copy-out need.
struct ClaudeRun {
    staged: copyback::Staged,
    contacts: Vec<NetworkContact>,
    change_count: usize,
    /// Every path in the manifest diff (changed or deleted) — used host-side to
    /// detect which the user has also edited before copying out.
    changed_paths: Vec<String>,
    exit_code: i32,
}

pub async fn claude(cli: &Cli, prompt_parts: &[String]) -> Result<()> {
    if cli.json {
        bail!(
            "--json isn't supported for `boxme claude` yet — a headless run \
             (`boxme claude 'prompt'`) is already non-interactive"
        );
    }
    if !base_snapshot_exists().await? {
        bail!("base snapshot missing — run `boxme setup` first");
    }
    let project_dir = std::env::current_dir()?;

    let prompt = (!prompt_parts.is_empty()).then(|| prompt_parts.join(" "));

    let php = detect::php_version(&project_dir).await;
    let node = detect::node_major(&project_dir).await;
    let has_composer = project_dir.join("composer.json").exists();
    eprintln!(
        "{} php {php}, node {}",
        ">> detected:".dimmed(),
        node.map(|n| n.to_string())
            .unwrap_or_else(|| format!("{} (default)", scripts::BASE_NODE_MAJOR)),
    );

    let mut env = resolve_claude_env(cli)?;
    // The agent runs composer itself, so `--composer-auth` lets it install your
    // private packages — the credentials stay placeholder-only inside the box.
    let secrets = if cli.composer_auth {
        composer_auth::inject(&mut env)?
    } else {
        Vec::new()
    };
    ensure_cache_volumes().await?;
    let policy = net_policy(cli, &project_dir);

    let name = vm_name(&project_dir);
    let sb = boot(cli, &project_dir, policy, &env, &secrets, &name).await?;

    let outcome = run_in_guest(
        &sb,
        &project_dir,
        prompt.as_deref(),
        &php,
        node,
        has_composer,
    )
    .await;

    cleanup(cli, sb, &name).await;
    let run = outcome?;

    report_contacts(&run.contacts);
    if cli.learn {
        learn_hosts(&project_dir, &run.contacts)?;
    }
    if run.exit_code != 0 {
        eprintln!(
            "{}",
            format!(">> claude exited with code {}", run.exit_code).yellow()
        );
    }

    if run.change_count == 0 {
        eprintln!(
            "{}",
            "the agent made no file changes — nothing to copy out".dimmed()
        );
        return Ok(());
    }

    deliver(&project_dir, prompt.as_deref(), run)
}

/// Copy the agent's changeset out to the host. The default is an in-place apply
/// to the working tree — plain uncommitted edits the user reviews with `git
/// diff`. If any changed file is one the user has *also* edited locally, applying
/// would clobber their work, so boxme asks (interactive) or falls back to a
/// branch (headless). `headless` is true for a `-p` run, where there's no TTY to
/// prompt on.
fn deliver(project_dir: &Path, prompt: Option<&str>, run: ClaudeRun) -> Result<()> {
    let headless = prompt.is_some();
    let message = match prompt {
        Some(p) => format!("boxme claude: {p}"),
        None => "boxme claude session".to_string(),
    };

    let collisions = copyback::collisions(project_dir, &run.changed_paths)?;
    let to_branch = if collisions.is_empty() {
        false
    } else if headless {
        report_overwrite_skipped(&collisions);
        true
    } else {
        match ask_collision(&collisions) {
            CollisionChoice::Overwrite => false,
            CollisionChoice::Branch => true,
            CollisionChoice::Abort => {
                report_aborted(run.staged.tarball());
                return Ok(());
            }
        }
    };

    if to_branch {
        let branch = format!("boxme/claude-{}", epoch_secs());
        match copyback::commit_to_branch(project_dir, run.staged, &message, &branch)? {
            Some(branch) => {
                eprintln!(
                    "{}",
                    format!(
                        "committed {} change(s) to {branch} — your working tree is untouched",
                        run.change_count
                    )
                    .green()
                );
                eprintln!(
                    "{}",
                    format!(">> review: git diff {branch}~1 {branch}    merge: git merge {branch}")
                        .dimmed()
                );
            }
            None => eprintln!("{}", "nothing to commit".dimmed()),
        }
    } else {
        copyback::commit(project_dir, run.staged)?;
        eprintln!(
            "{}",
            format!(
                "copied {} change(s) into your working tree",
                run.change_count
            )
            .green()
        );
        if project_dir.join(".git").exists() {
            eprintln!(
                "{}",
                ">> review with `git diff`, then commit or discard as you like".dimmed()
            );
        }
    }
    Ok(())
}

enum CollisionChoice {
    Overwrite,
    Branch,
    Abort,
}

/// Ask what to do about files the agent changed that the user has also edited
/// locally. Defaults to the safe choice (branch) on EOF or anything unrecognized,
/// so a stray keypress never clobbers local work.
fn ask_collision(files: &[String]) -> CollisionChoice {
    eprintln!(
        "{}",
        format!(
            "{} file(s) you've edited locally would be overwritten by the agent's changes:",
            files.len()
        )
        .yellow()
    );
    for f in files.iter().take(20) {
        eprintln!("    {f}");
    }
    if files.len() > 20 {
        eprintln!("    … and {} more", files.len() - 20);
    }
    eprint!(
        "{}",
        "apply over them [o], put the agent's work on a branch [b], or abort [a]? (b) ".bold()
    );
    io::stderr().flush().ok();

    let mut line = String::new();
    if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return CollisionChoice::Branch;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "o" | "overwrite" => CollisionChoice::Overwrite,
        "a" | "abort" => CollisionChoice::Abort,
        _ => CollisionChoice::Branch,
    }
}

/// Non-interactive notice that boxme chose the branch over overwriting.
fn report_overwrite_skipped(files: &[String]) {
    eprintln!(
        "{}",
        format!(
            ">> {} file(s) you've edited would be overwritten — landing the agent's work on a \
             branch instead, your edits untouched",
            files.len()
        )
        .yellow()
    );
}

/// On abort, point the user at the staged tarball so the agent's work isn't lost.
fn report_aborted(tarball: Option<&Path>) {
    eprintln!("{}", "aborted — your working tree is unchanged.".yellow());
    if let Some(p) = tarball {
        eprintln!(
            "{}",
            format!(
                ">> the agent's changes are staged at {0} — `tar xzf {0} -C .` to apply by hand",
                p.display()
            )
            .dimmed()
        );
    }
}

/// Mount the project, match toolchain versions, run claude attached, then diff
/// the manifest and stage the agent's changeset out of the guest. Does not tear
/// the VM down or commit — the caller does both, in that order.
async fn run_in_guest(
    sb: &Sandbox,
    project_dir: &Path,
    prompt: Option<&str>,
    php: &str,
    node: Option<u32>,
    has_composer: bool,
) -> Result<ClaudeRun> {
    eprintln!("{}", ">> mounting project (overlay)...".dimmed());
    let code = stream_shell_stderr(sb, scripts::UNPACK).await?;
    if code != 0 {
        bail!("mounting the project in the guest failed (exit {code})");
    }

    // Match host toolchain versions so anything the agent runs (composer/npm)
    // behaves like it would on the host.
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

    let before = manifest::parse(&shell_capture(sb, scripts::MANIFEST).await?);

    let capture = netcap::start(sb).await;
    if capture.is_none() {
        eprintln!(
            "{}",
            "warning: tcpdump unavailable in the guest — network capture disabled".yellow()
        );
    }

    eprintln!(
        "{}\n",
        match prompt {
            Some(p) => format!(">> claude (headless): {p}"),
            None => ">> claude (interactive) — exit the session to copy out the result".to_string(),
        }
        .dimmed()
    );
    let guest_cmd = scripts::claude_run(prompt);
    let exit_code = sb
        .attach_with("bash", |a| a.args(["-lc", &guest_cmd]))
        .await?;

    let contacts = match capture {
        Some(cap) => {
            cap.stop().await;
            netcap::contacts(sb).await.unwrap_or_default()
        }
        None => Vec::new(),
    };

    // The whole diff is the result — no expected/unexpected partition. vendor,
    // node_modules and .git are excluded by the manifest, so an agent that runs
    // `composer require` commits the lockfile change but not the (Linux-native,
    // gitignored) dep tree, which is exactly what you'd want on the branch.
    let after = manifest::parse(&shell_capture(sb, scripts::MANIFEST).await?);
    let changes = manifest::diff(&before, &after);
    let plan = CopyPlan {
        dirs: Vec::new(),
        files: changes
            .iter()
            .filter(|(_, c)| !matches!(c, Change::Deleted))
            .map(|(p, _)| p.clone())
            .collect(),
        deletions: changes
            .iter()
            .filter(|(_, c)| matches!(c, Change::Deleted))
            .map(|(p, _)| p.clone())
            .collect(),
    };
    let changed_paths: Vec<String> = changes.iter().map(|(p, _)| p.clone()).collect();
    let staged = copyback::stage(sb, project_dir, &plan).await?;

    Ok(ClaudeRun {
        staged,
        contacts,
        change_count: changes.len(),
        changed_paths,
        exit_code,
    })
}

/// Pick the egress policy: `--learn` observes (open egress, recorded),
/// `--strict` is API + registries only (ignores `.boxme/claude-allow`), and the
/// default enforces API + registries + the saved claude allowlist.
fn net_policy(cli: &Cli, project_dir: &Path) -> NetworkPolicy {
    if cli.learn {
        observe_policy()
    } else if cli.strict {
        claude_policy(&[])
    } else {
        claude_policy(&allowlist::load(project_dir, Scope::Claude))
    }
}

/// Resolve the guest environment and the auth credential to inject. There is no
/// browser login inside the box (network enforcement keeps the OAuth authorize
/// endpoints unreachable), so the agent authenticates from a token, and a run
/// with no credential can't succeed — we bail here, before booting, rather than
/// drop the user into a doomed login screen. The credential is resolved in
/// precedence order: an explicit `-e` flag, then the host shell env, then the
/// token saved by `boxme login` (keychain on macOS, a `0600` file on Linux) — so
/// the normal path keeps the token out of your shell entirely. Putting a token in
/// the box is mitigated by network enforcement: only Anthropic's own services
/// (`anthropic.com` / `claude.com`, plus the registries) are reachable over TCP,
/// so a leaked token can't be used against anything else.
fn resolve_claude_env(cli: &Cli) -> Result<Vec<(String, String)>> {
    let mut env = resolve_env(&cli.env)?;
    for var in AUTH_VARS {
        if !env.iter().any(|(k, _)| k == var) {
            if let Ok(val) = std::env::var(var) {
                eprintln!("{} forwarding {var} from your shell", ">> auth:".dimmed());
                env.push((var.to_string(), val));
            }
        }
    }
    if !env.iter().any(|(k, _)| AUTH_VARS.contains(&k.as_str())) {
        if let Some(token) = auth::load()? {
            eprintln!(
                "{} using the Claude token from your keychain",
                ">> auth:".dimmed()
            );
            env.push((auth::TOKEN_VAR.to_string(), token));
        }
    }
    if !env.iter().any(|(k, _)| AUTH_VARS.contains(&k.as_str())) {
        bail!(
            "no Claude credential found — boxme claude needs a token to authenticate, and there's \
             no browser login inside the box.\n\
             run `claude setup-token` on the host, then save it with:\n\
            \n    \
                boxme login\n\
            \n\
             boxme keeps it in your keychain and injects it into the sandbox at boot, where \
             network enforcement limits it to Anthropic's services. (or pass `-e ANTHROPIC_API_KEY`.)"
        );
    }
    Ok(env)
}

/// Boot a guest from the base snapshot with the project bound read-only as the
/// overlay lower (so the agent's writes stay in the guest upper and the host is
/// untouchable for the whole session) and the run's environment injected.
async fn boot(
    cli: &Cli,
    project_dir: &Path,
    policy: NetworkPolicy,
    env: &[(String, String)],
    secrets: &[SecretEntry],
    name: &str,
) -> Result<Sandbox> {
    eprintln!("{} '{name}' from {BASE_SNAPSHOT}...", ">> booting".dimmed());
    let project_dir =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let intercept = !secrets.is_empty();
    let mut builder = Sandbox::builder(name)
        .from_snapshot(BASE_SNAPSHOT)
        .memory(cli.memory)
        .cpus(cli.cpus)
        .replace()
        .volume("/root/.n", |m| m.named("boxme-node-versions"))
        .volume("/ws-lower", |m| m.bind(project_dir).readonly());
    for entry in secrets {
        builder = builder.secret_entry(entry.clone());
    }
    // Keep the agent's own Anthropic traffic out of interception — only the
    // composer-auth hosts need MITM, and the API is reached over its real cert.
    builder = builder.network(move |n| {
        let n = n.policy(policy);
        if intercept {
            n.tls(|t| {
                t.bypass("anthropic.com")
                    .bypass("*.anthropic.com")
                    .bypass("claude.com")
                    .bypass("*.claude.com")
            })
        } else {
            n
        }
    });
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    builder.create().await.map_err(Into::into)
}

/// Print the distinct hosts the agent contacted, so the run is honest about its
/// egress even without the file-review TUI.
fn report_contacts(contacts: &[NetworkContact]) {
    let hosts: BTreeSet<&str> = contacts.iter().map(NetworkContact::host).collect();
    if hosts.is_empty() {
        return;
    }
    let list = hosts.into_iter().collect::<Vec<_>>().join(", ");
    eprintln!("{} {list}", ">> network: contacted".dimmed());
}

/// In `--learn`, persist the named hosts the agent contacted — minus the
/// registries and the always-allowed API — to `.boxme/claude-allow`, turning the
/// open-egress observation into enforceable policy for the next run.
fn learn_hosts(project_dir: &Path, contacts: &[NetworkContact]) -> Result<()> {
    let mut hosts = BTreeSet::new();
    for c in contacts {
        if c.known {
            continue; // a package registry — always allowed
        }
        let Some(domain) = &c.domain else {
            continue; // a bare IP — nothing to write to the allowlist
        };
        if netcap::CLAUDE_DOMAINS
            .iter()
            .any(|d| allowlist::entry_matches(d, domain))
        {
            continue; // the Anthropic API — always allowed
        }
        hosts.insert(domain.clone());
    }
    let additions: Vec<String> = hosts.into_iter().collect();
    let merged = allowlist::save_merged(project_dir, Scope::Claude, &additions)?;
    eprintln!(
        "{} {} extra host(s) → {} (edit to trim)",
        ">> learn: claude allowlist saved,".dimmed(),
        merged.len(),
        allowlist::path(project_dir, Scope::Claude).display(),
    );
    Ok(())
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
