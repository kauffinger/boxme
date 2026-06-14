//! Per-project network allowlist: domains the user has decided package code is
//! allowed to reach. Stored as `.boxme/allow` in the project dir, one entry per
//! line, `#` comments and blank lines ignored. Its presence flips a normal run
//! from observe-by-default to deny-by-default (see `run::choose_policy`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const HEADER: &str = "\
# boxme network allowlist — hosts package code in this project may reach.
# Deny-by-default is in effect whenever this file exists (alongside the package
# registries, which are always allowed). One entry per line:
#   example.com     a bare domain matches it and every subdomain
#   =api.example.com  a '=' prefix matches that exact host only
# '#' comments and blank lines are ignored. Edit freely.
";

pub fn path(project_dir: &Path) -> PathBuf {
    project_dir.join(".boxme").join("allow")
}

/// Whether the project has an allowlist file. Its mere existence flips a run to
/// deny-by-default, even if it lists no extra hosts (registries only).
pub fn exists(project_dir: &Path) -> bool {
    path(project_dir).exists()
}

/// Whether `host` is permitted by allowlist `entry`. A `=`-prefixed entry is an
/// exact host match; a bare entry matches the domain and all of its subdomains.
pub fn entry_matches(entry: &str, host: &str) -> bool {
    match entry.strip_prefix('=') {
        Some(exact) => host == exact,
        None => {
            host == entry
                || host
                    .strip_suffix(entry)
                    .is_some_and(|rest| rest.ends_with('.'))
        }
    }
}

/// Load the allowlist, returning a deduplicated, sorted list of entries. A
/// missing file is simply an empty allowlist.
pub fn load(project_dir: &Path) -> Vec<String> {
    let raw = match std::fs::read_to_string(path(project_dir)) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    entries(raw.lines())
}

/// Merge `additions` into the existing allowlist and write it back, creating
/// `.boxme/` if needed. Returns the merged list.
pub fn save_merged(project_dir: &Path, additions: &[String]) -> Result<Vec<String>> {
    let merged = entries(
        load(project_dir)
            .iter()
            .map(String::as_str)
            .chain(additions.iter().map(String::as_str)),
    );

    let file = path(project_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {} failed", parent.display()))?;
    }
    let body = format!("{HEADER}{}\n", merged.join("\n"));
    std::fs::write(&file, body).with_context(|| format!("writing {} failed", file.display()))?;
    Ok(merged)
}

/// Normalize a set of lines into clean entries: trimmed, comments and blanks
/// dropped, deduplicated and sorted.
fn entries<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_dedupes_sorts_and_strips_comments() {
        let got = entries(
            [
                "# a comment",
                "  packagist.org ",
                "",
                "github.com",
                "packagist.org",
            ]
            .into_iter(),
        );
        assert_eq!(got, vec!["github.com", "packagist.org"]);
    }

    #[test]
    fn suffix_entry_matches_domain_and_subdomains() {
        assert!(entry_matches("npmjs.org", "npmjs.org"));
        assert!(entry_matches("npmjs.org", "registry.npmjs.org"));
        assert!(!entry_matches("npmjs.org", "evilnpmjs.org"));
        assert!(!entry_matches("npmjs.org", "npmjs.org.evil.com"));
    }

    #[test]
    fn exact_entry_matches_only_that_host() {
        assert!(entry_matches("=registry.npmjs.org", "registry.npmjs.org"));
        assert!(!entry_matches("=registry.npmjs.org", "npmjs.org"));
        assert!(!entry_matches("=registry.npmjs.org", "other.npmjs.org"));
    }
}
