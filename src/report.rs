//! The non-interactive (`--json`) surface: the machine-readable report a
//! `boxme --json <command>` prints to stdout instead of opening the review TUI,
//! and the pending-changeset lifecycle behind the two-step flow — the run stages
//! its result under `.boxme/pending` and a later `boxme apply` / `boxme discard`
//! decides what happens to it. Nothing touches the project tree until `apply`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;
use serde::Serialize;

use crate::copyback;

/// Where a `--json` run stages its changeset, relative to the project dir.
/// The dir carries a `*` .gitignore so it can never be committed by accident.
pub const PENDING_DIR: &str = ".boxme/pending";

/// The guest command exited non-zero (nothing was staged).
pub const EXIT_COMMAND_FAILED: i32 = 2;
/// The command succeeded but the report has findings to look at before applying.
pub const EXIT_FINDINGS: i32 = 3;

/// The full report a `--json` run prints to stdout. `finalize` derives
/// `findings` and `clean` from the raw fields; `exit_code_for` maps them onto
/// the process exit code.
#[derive(Serialize)]
pub struct Report {
    pub schema: u32,
    pub command: String,
    /// "enforced" (registries + allowlist) or "strict" (registries only).
    pub mode: &'static str,
    /// The allowlist entries the run enforced under.
    pub allowlist: Vec<String>,
    /// The guest command's own exit code.
    pub exit_code: i32,
    /// True when there is nothing that needs a second look before `boxme apply`.
    pub clean: bool,
    /// Machine-readable flags: "command_failed", "blocked_hosts",
    /// "unexpected_files", "outside_writes", "network_capture_unavailable",
    /// "outside_scan_unavailable".
    pub findings: Vec<&'static str>,
    /// Expected write-set dirs (vendor/, node_modules/) as counts — their
    /// contents are the point of the command, not worth listing file by file.
    pub expected_dirs: Vec<DirSummary>,
    /// Every other changed path, with its diff where one was captured.
    pub files: Vec<FileChange>,
    pub network: Network,
    pub outside: Outside,
    /// Present when a changeset was staged: where it is and how to decide on it.
    pub pending: Option<Pending>,
}

#[derive(Serialize)]
pub struct DirSummary {
    pub dir: String,
    pub files: u64,
}

#[derive(Serialize)]
pub struct FileChange {
    pub path: String,
    /// "added" / "modified" / "deleted".
    pub change: &'static str,
    /// Inside the command's expected write-set (lockfiles, manifests).
    pub expected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Serialize)]
pub struct Network {
    /// Set instead of contacts when the capture was unavailable/unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    pub contacts: Vec<NetContact>,
}

#[derive(Serialize)]
pub struct NetContact {
    pub host: String,
    pub ip: String,
    pub port: u16,
    /// "registry" / "allowed" / "blocked".
    pub status: &'static str,
}

#[derive(Serialize)]
pub struct Outside {
    /// Set when the out-of-workspace scan couldn't run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
    pub truncated: bool,
    pub files: Vec<OutsideFile>,
}

#[derive(Serialize)]
pub struct OutsideFile {
    pub path: String,
    /// "file" / "symlink".
    pub kind: &'static str,
    pub size: u64,
}

#[derive(Serialize)]
pub struct Pending {
    /// Project-relative dir holding the staged changeset and this report.
    pub dir: String,
    pub apply: &'static str,
    pub discard: &'static str,
    /// Shell command that lists every staged path — the changeset is a plain
    /// gzipped tar with project-relative paths, so any inspection works.
    pub list: String,
    /// Shell command that prints one staged file (substitute `<path>`).
    pub show: String,
}

impl Pending {
    pub fn new(dir: String) -> Self {
        let list = format!("tar tzf {dir}/changeset.tgz");
        let show = format!("tar xzf {dir}/changeset.tgz -O -- <path>");
        Pending {
            dir,
            apply: "boxme apply",
            discard: "boxme discard",
            list,
            show,
        }
    }
}

impl Report {
    /// Derive `findings` and `clean` from the raw fields. Call once, after all
    /// fields are set.
    pub fn finalize(&mut self) {
        let mut findings = Vec::new();
        if self.exit_code != 0 {
            findings.push("command_failed");
        }
        if self.network.contacts.iter().any(|c| c.status == "blocked") {
            findings.push("blocked_hosts");
        }
        if self.files.iter().any(|f| !f.expected) {
            findings.push("unexpected_files");
        }
        if !self.outside.files.is_empty() {
            findings.push("outside_writes");
        }
        if self.network.banner.is_some() {
            findings.push("network_capture_unavailable");
        }
        if self.outside.banner.is_some() {
            findings.push("outside_scan_unavailable");
        }
        self.findings = findings;
        self.clean = self.findings.is_empty();
    }

    /// The process exit code the report maps to: a failed guest command beats
    /// findings, findings beat clean.
    pub fn exit_code_for(&self) -> i32 {
        if self.exit_code != 0 {
            EXIT_COMMAND_FAILED
        } else if !self.findings.is_empty() {
            EXIT_FINDINGS
        } else {
            0
        }
    }
}

fn pending_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(PENDING_DIR)
}

/// Stage a changeset under `.boxme/pending` for a later `boxme apply`. Replaces
/// any changeset a previous run left unapplied (with a warning — it is
/// reproducible by re-running, unlike the fresh one). Returns the
/// project-relative pending dir for the report.
pub fn save_pending(project_dir: &Path, staged: copyback::Staged) -> Result<String> {
    let dir = pending_dir(project_dir);
    if dir.exists() {
        eprintln!(
            "{}",
            "warning: replacing an unapplied staged changeset from a previous run".yellow()
        );
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing {} failed", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {} failed", dir.display()))?;
    std::fs::write(dir.join(".gitignore"), "*\n")?;
    copyback::persist(staged, &dir)?;
    Ok(PENDING_DIR.to_string())
}

/// Keep a copy of the printed report next to the staged changeset, so the
/// context for an `apply` decision survives the run that produced it.
pub fn save_report_copy(project_dir: &Path, json: &str) {
    let dir = pending_dir(project_dir);
    if dir.is_dir() {
        let _ = std::fs::write(dir.join("report.json"), json);
    }
}

/// `boxme apply` — copy the staged changeset into the project and drop the
/// pending dir. The explicit second step of the `--json` flow.
pub fn apply(json: bool) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let dir = pending_dir(&project_dir);
    if !dir.join("staged.json").exists() {
        bail!("nothing staged — run `boxme --json composer …` (or npm) first");
    }
    let staged = copyback::load_staged(&dir)?;
    copyback::commit(&project_dir, staged)?;
    let _ = std::fs::remove_dir_all(&dir);
    if json {
        println!("{}", serde_json::json!({ "applied": true }));
    } else {
        eprintln!(
            "{}",
            "applied — staged changeset copied into the project".green()
        );
    }
    Ok(())
}

/// `boxme discard` — drop the staged changeset without applying it. A no-op
/// (not an error) when nothing is staged.
pub fn discard(json: bool) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let dir = pending_dir(&project_dir);
    let existed = dir.exists();
    if existed {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing {} failed", dir.display()))?;
    }
    if json {
        println!("{}", serde_json::json!({ "discarded": existed }));
    } else if existed {
        eprintln!("{}", "discarded — staged changeset removed".yellow());
    } else {
        eprintln!("{}", "nothing staged".dimmed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_report() -> Report {
        Report {
            schema: 1,
            command: "boxme composer install".to_string(),
            mode: "enforced",
            allowlist: vec![],
            exit_code: 0,
            clean: false,
            findings: vec![],
            expected_dirs: vec![],
            files: vec![],
            network: Network {
                banner: None,
                contacts: vec![],
            },
            outside: Outside {
                banner: None,
                truncated: false,
                files: vec![],
            },
            pending: None,
        }
    }

    fn contact(status: &'static str) -> NetContact {
        NetContact {
            host: "example.com".to_string(),
            ip: "1.2.3.4".to_string(),
            port: 443,
            status,
        }
    }

    #[test]
    fn clean_run_finalizes_clean_with_exit_zero() {
        let mut r = base_report();
        r.network.contacts.push(contact("registry"));
        r.files.push(FileChange {
            path: "composer.lock".to_string(),
            change: "modified",
            expected: true,
            diff: None,
        });
        r.finalize();
        assert!(r.clean);
        assert!(r.findings.is_empty());
        assert_eq!(r.exit_code_for(), 0);
    }

    #[test]
    fn findings_surface_and_map_to_exit_three() {
        let mut r = base_report();
        r.network.contacts.push(contact("blocked"));
        r.files.push(FileChange {
            path: "app/Evil.php".to_string(),
            change: "added",
            expected: false,
            diff: Some("+ evil".to_string()),
        });
        r.outside.files.push(OutsideFile {
            path: "/usr/local/bin/x".to_string(),
            kind: "file",
            size: 10,
        });
        r.finalize();
        assert!(!r.clean);
        assert_eq!(
            r.findings,
            vec!["blocked_hosts", "unexpected_files", "outside_writes"]
        );
        assert_eq!(r.exit_code_for(), EXIT_FINDINGS);
    }

    #[test]
    fn failed_command_wins_over_findings() {
        let mut r = base_report();
        r.exit_code = 1;
        r.network.contacts.push(contact("blocked"));
        r.finalize();
        assert!(r.findings.contains(&"command_failed"));
        assert_eq!(r.exit_code_for(), EXIT_COMMAND_FAILED);
    }

    #[test]
    fn missing_capture_and_scan_are_findings() {
        let mut r = base_report();
        r.network.banner = Some("capture unavailable".to_string());
        r.outside.banner = Some("scan unavailable".to_string());
        r.finalize();
        assert_eq!(
            r.findings,
            vec!["network_capture_unavailable", "outside_scan_unavailable"]
        );
        assert_eq!(r.exit_code_for(), EXIT_FINDINGS);
    }

    #[test]
    fn json_omits_absent_diff_and_banners() {
        let mut r = base_report();
        r.files.push(FileChange {
            path: "composer.lock".to_string(),
            change: "modified",
            expected: true,
            diff: None,
        });
        r.finalize();
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"diff\""));
        assert!(!json.contains("\"banner\""));
        assert!(json.contains("\"clean\":true"));
    }
}
