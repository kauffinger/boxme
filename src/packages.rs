//! What a run changed, package by package: the versions in each lockfile before
//! and after, diffed into a readable list ("laravel/framework v12.1.0 → v12.3.1,
//! minor"). The raw lockfile diff stays the source of truth; this is the summary
//! a reviewer actually reads.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

/// The lockfiles boxme summarizes, with the package manager each belongs to.
pub const LOCKFILES: &[(&str, &str)] =
    &[("composer.lock", "composer"), ("package-lock.json", "npm")];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageChange {
    /// "composer" or "npm".
    pub manager: &'static str,
    pub name: String,
    /// `None` when the package was added.
    pub from: Option<String>,
    /// `None` when the package was removed.
    pub to: Option<String>,
    pub change: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Major,
    Minor,
    Patch,
    Downgrade,
    /// A version change that isn't plain semver: a dev branch moving to a new
    /// commit, a pre-release, or an npm package installed at several versions.
    Changed,
    Added,
    Removed,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Major => "major",
            ChangeKind::Minor => "minor",
            ChangeKind::Patch => "patch",
            ChangeKind::Downgrade => "downgrade",
            ChangeKind::Changed => "changed",
            ChangeKind::Added => "added",
            ChangeKind::Removed => "removed",
        }
    }
}

/// Every package whose version differs between two versions of a lockfile.
/// An empty side counts as "no lockfile" (everything added or removed); `None`
/// when either side isn't a lockfile we can read, so a corrupt file can't pass
/// for "every package removed".
pub fn diff_lockfiles(
    manager: &'static str,
    before: &str,
    after: &str,
) -> Option<Vec<PackageChange>> {
    let before = versions(manager, before)?;
    let after = versions(manager, after)?;
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    Some(
        names
            .into_iter()
            .filter_map(|name| {
                let from = before.get(name);
                let to = after.get(name);
                (from != to).then(|| PackageChange {
                    manager,
                    name: name.clone(),
                    from: from.cloned(),
                    to: to.cloned(),
                    change: classify(from, to),
                })
            })
            .collect(),
    )
}

/// "12 composer packages changed: 1 major, 3 minor, 7 patch, 1 added".
pub fn summary(manager: &str, changes: &[PackageChange]) -> String {
    let mut counts: BTreeMap<ChangeKind, usize> = BTreeMap::new();
    for change in changes {
        *counts.entry(change.change).or_default() += 1;
    }
    let breakdown = counts
        .iter()
        .map(|(kind, count)| format!("{count} {}", kind.label()))
        .collect::<Vec<_>>()
        .join(", ");
    let noun = if changes.len() == 1 {
        "package"
    } else {
        "packages"
    };
    format!("{} {manager} {noun} changed: {breakdown}", changes.len())
}

/// Package name → installed version, from a composer.lock or package-lock.json.
fn versions(manager: &str, lockfile: &str) -> Option<BTreeMap<String, String>> {
    if lockfile.trim().is_empty() {
        return Some(BTreeMap::new());
    }
    let lock: Value = serde_json::from_str(lockfile).ok()?;
    match manager {
        "composer" => Some(composer_versions(&lock)),
        "npm" => Some(npm_versions(&lock)),
        _ => None,
    }
}

fn composer_versions(lock: &Value) -> BTreeMap<String, String> {
    ["packages", "packages-dev"]
        .iter()
        .filter_map(|section| lock.get(section)?.as_array())
        .flatten()
        .filter_map(|package| {
            let name = package.get("name")?.as_str()?;
            let version = package.get("version")?.as_str()?;
            Some((name.to_string(), composer_version(version, package)))
        })
        .collect()
}

/// A dev branch keeps its version ("dev-main") while its commit moves, so the
/// commit is part of what changed.
fn composer_version(version: &str, package: &Value) -> String {
    if !version.starts_with("dev-") && !version.ends_with("-dev") {
        return version.to_string();
    }
    let reference = ["source", "dist"]
        .iter()
        .find_map(|key| package.get(key)?.get("reference")?.as_str());
    match reference {
        Some(reference) => format!("{version}@{}", &reference[..reference.len().min(7)]),
        None => version.to_string(),
    }
}

/// npm installs one package at several versions when ranges conflict, and
/// moves copies around the tree between runs; keying by name over the set of
/// installed versions ignores the moves and keeps the real version changes.
fn npm_versions(lock: &Value) -> BTreeMap<String, String> {
    let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    if let Some(packages) = lock.get("packages").and_then(Value::as_object) {
        for (path, entry) in packages {
            let Some((_, name)) = path.rsplit_once("node_modules/") else {
                continue;
            };
            if let Some(version) = entry.get("version").and_then(Value::as_str) {
                found
                    .entry(name.to_string())
                    .or_default()
                    .insert(version.to_string());
            }
        }
    } else if let Some(dependencies) = lock.get("dependencies") {
        collect_v1_dependencies(dependencies, &mut found);
    }
    found
        .into_iter()
        .map(|(name, versions)| (name, versions.into_iter().collect::<Vec<_>>().join(", ")))
        .collect()
}

/// lockfileVersion 1 nests `dependencies` instead of listing install paths.
fn collect_v1_dependencies(dependencies: &Value, found: &mut BTreeMap<String, BTreeSet<String>>) {
    let Some(dependencies) = dependencies.as_object() else {
        return;
    };
    for (name, entry) in dependencies {
        if let Some(version) = entry.get("version").and_then(Value::as_str) {
            found
                .entry(name.clone())
                .or_default()
                .insert(version.to_string());
        }
        if let Some(nested) = entry.get("dependencies") {
            collect_v1_dependencies(nested, found);
        }
    }
}

fn classify(from: Option<&String>, to: Option<&String>) -> ChangeKind {
    let (from, to) = match (from, to) {
        (None, _) => return ChangeKind::Added,
        (_, None) => return ChangeKind::Removed,
        (Some(from), Some(to)) => (from, to),
    };
    let (Some(from), Some(to)) = (release(from), release(to)) else {
        return ChangeKind::Changed;
    };
    if to < from {
        return ChangeKind::Downgrade;
    }
    let Some(changed) = (0..3).find(|&index| to[index] != from[index]) else {
        return ChangeKind::Changed;
    };
    // Below 1.0 the first non-zero part is the breaking one, as composer's and
    // npm's `^` treat it (`^0.1.1` stops before 0.2.0), so 0.1 → 0.5 is major.
    let breaking = from.iter().position(|&part| part != 0).unwrap_or(2);
    if changed <= breaking {
        ChangeKind::Major
    } else if changed == 1 {
        ChangeKind::Minor
    } else {
        ChangeKind::Patch
    }
}

/// The `major.minor.patch` of `v1.2.3`, `1.2.3-beta.1` or `1.2` (missing parts
/// are zero). `None` for anything else: branches, commits, several versions.
fn release(version: &str) -> Option<[u64; 3]> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut release = [0; 3];
    for (index, slot) in release.iter_mut().enumerate() {
        match parts.next() {
            Some(part) => *slot = part.parse().ok()?,
            None if index > 0 => break,
            None => return None,
        }
    }
    Some(release)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(name: &str, from: Option<&str>, to: Option<&str>, kind: ChangeKind) -> PackageChange {
        PackageChange {
            manager: "composer",
            name: name.to_string(),
            from: from.map(str::to_string),
            to: to.map(str::to_string),
            change: kind,
        }
    }

    #[test]
    fn composer_lock_diff_classifies_every_kind_of_change() {
        let before = r#"{
            "packages": [
                {"name": "laravel/framework", "version": "v9.19.0"},
                {"name": "symfony/console", "version": "v6.1.3"},
                {"name": "guzzlehttp/guzzle", "version": "7.4.5"},
                {"name": "league/flysystem", "version": "3.2.0"},
                {"name": "acme/old", "version": "1.0.0"},
                {"name": "acme/branch", "version": "dev-main", "source": {"reference": "aaaaaaaaaaaa"}},
                {"name": "acme/same", "version": "2.0.0"}
            ],
            "packages-dev": [
                {"name": "phpunit/phpunit", "version": "9.5.20"}
            ]
        }"#;
        let after = r#"{
            "packages": [
                {"name": "laravel/framework", "version": "v9.52.16"},
                {"name": "symfony/console", "version": "v6.1.2"},
                {"name": "guzzlehttp/guzzle", "version": "7.4.6"},
                {"name": "league/flysystem", "version": "4.0.0"},
                {"name": "acme/new", "version": "0.1.0"},
                {"name": "acme/branch", "version": "dev-main", "source": {"reference": "bbbbbbbbbbbb"}},
                {"name": "acme/same", "version": "2.0.0"}
            ],
            "packages-dev": [
                {"name": "phpunit/phpunit", "version": "9.6.22"}
            ]
        }"#;

        let changes = diff_lockfiles("composer", before, after).unwrap();
        assert_eq!(
            changes,
            vec![
                change(
                    "acme/branch",
                    Some("dev-main@aaaaaaa"),
                    Some("dev-main@bbbbbbb"),
                    ChangeKind::Changed
                ),
                change("acme/new", None, Some("0.1.0"), ChangeKind::Added),
                change("acme/old", Some("1.0.0"), None, ChangeKind::Removed),
                change(
                    "guzzlehttp/guzzle",
                    Some("7.4.5"),
                    Some("7.4.6"),
                    ChangeKind::Patch
                ),
                change(
                    "laravel/framework",
                    Some("v9.19.0"),
                    Some("v9.52.16"),
                    ChangeKind::Minor
                ),
                change(
                    "league/flysystem",
                    Some("3.2.0"),
                    Some("4.0.0"),
                    ChangeKind::Major
                ),
                change(
                    "phpunit/phpunit",
                    Some("9.5.20"),
                    Some("9.6.22"),
                    ChangeKind::Minor
                ),
                change(
                    "symfony/console",
                    Some("v6.1.3"),
                    Some("v6.1.2"),
                    ChangeKind::Downgrade
                ),
            ]
        );
    }

    #[test]
    fn npm_lock_diff_ignores_moves_and_tracks_every_installed_version() {
        let before = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "app", "version": "1.0.0"},
                "node_modules/vite": {"version": "4.5.0"},
                "node_modules/@scope/pkg": {"version": "1.0.0"},
                "node_modules/a/node_modules/semver": {"version": "6.3.0"},
                "node_modules/semver": {"version": "7.5.0"},
                "node_modules/moved": {"version": "2.0.0"},
                "packages/workspace": {"version": "0.0.1"}
            }
        }"#;
        let after = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "app", "version": "1.0.0"},
                "node_modules/vite": {"version": "4.5.14"},
                "node_modules/@scope/pkg": {"version": "1.2.0"},
                "node_modules/semver": {"version": "7.7.1"},
                "node_modules/b/node_modules/moved": {"version": "2.0.0"}
            }
        }"#;

        let changes = diff_lockfiles("npm", before, after).unwrap();
        let summary: Vec<(&str, Option<&str>, Option<&str>, ChangeKind)> = changes
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.from.as_deref(),
                    c.to.as_deref(),
                    c.change,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "@scope/pkg",
                    Some("1.0.0"),
                    Some("1.2.0"),
                    ChangeKind::Minor
                ),
                (
                    "semver",
                    Some("6.3.0, 7.5.0"),
                    Some("7.7.1"),
                    ChangeKind::Changed
                ),
                ("vite", Some("4.5.0"), Some("4.5.14"), ChangeKind::Patch),
            ]
        );
        assert!(changes.iter().all(|c| c.manager == "npm"));
    }

    #[test]
    fn npm_v1_lockfile_reads_nested_dependencies() {
        let before = r#"{
            "lockfileVersion": 1,
            "dependencies": {
                "axios": {"version": "0.27.2", "dependencies": {"form-data": {"version": "4.0.0"}}}
            }
        }"#;
        let after = r#"{
            "lockfileVersion": 3,
            "packages": {
                "node_modules/axios": {"version": "0.30.0"},
                "node_modules/form-data": {"version": "4.0.0"}
            }
        }"#;
        let changes = diff_lockfiles("npm", before, after).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "axios");
        assert_eq!(changes[0].change, ChangeKind::Major);
    }

    #[test]
    fn missing_lockfile_side_means_added_or_removed_and_garbage_means_unknown() {
        let lock = r#"{"packages": [{"name": "a/b", "version": "1.0.0"}]}"#;
        let added = diff_lockfiles("composer", "", lock).unwrap();
        assert_eq!(added[0].change, ChangeKind::Added);
        let removed = diff_lockfiles("composer", lock, "").unwrap();
        assert_eq!(removed[0].change, ChangeKind::Removed);

        assert!(diff_lockfiles("composer", lock, "not json").is_none());
        assert_eq!(diff_lockfiles("composer", lock, lock).unwrap(), vec![]);
    }

    #[test]
    fn summary_counts_by_kind_in_severity_order() {
        let changes = vec![
            change("a", Some("1.0.0"), Some("1.0.1"), ChangeKind::Patch),
            change("b", Some("1.0.0"), Some("2.0.0"), ChangeKind::Major),
            change("c", Some("1.0.0"), Some("1.0.2"), ChangeKind::Patch),
            change("d", None, Some("1.0.0"), ChangeKind::Added),
        ];
        assert_eq!(
            summary("composer", &changes),
            "4 composer packages changed: 1 major, 2 patch, 1 added"
        );
        assert_eq!(
            summary("npm", &changes[..1]),
            "1 npm package changed: 1 patch"
        );
    }

    #[test]
    fn pre_one_versions_break_at_their_first_non_zero_part() {
        let kind = |from: &str, to: &str| classify(Some(&from.into()), Some(&to.into()));

        assert_eq!(kind("v0.1.1", "v0.5.9"), ChangeKind::Major);
        assert_eq!(kind("0.3.6", "0.3.26"), ChangeKind::Patch);
        assert_eq!(kind("0.0.3", "0.0.4"), ChangeKind::Major);
        assert_eq!(kind("0.9.0", "1.0.0"), ChangeKind::Major);
        assert_eq!(kind("1.2.3", "1.3.0"), ChangeKind::Minor);
        assert_eq!(kind("1.2.3", "1.2.3-beta"), ChangeKind::Changed);
    }

    #[test]
    fn release_parses_semver_shapes_and_rejects_the_rest() {
        assert_eq!(release("v1.2.3"), Some([1, 2, 3]));
        assert_eq!(release("1.2"), Some([1, 2, 0]));
        assert_eq!(release("2.0.0-beta.1"), Some([2, 0, 0]));
        assert_eq!(release("1.2.3.4"), Some([1, 2, 3]));
        assert_eq!(release("dev-main@abc1234"), None);
        assert_eq!(release("6.3.0, 7.5.0"), None);
    }
}
