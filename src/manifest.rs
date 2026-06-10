use std::collections::BTreeMap;

/// One /workspace entry from the guest manifest. Files carry an md5 so mtime
/// noise can't show up as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: char, // f / d / l from find's %y
    pub size: u64,
    pub md5: Option<String>,
}

pub type Manifest = BTreeMap<String, Entry>;

/// Parse the output of `scripts::MANIFEST`: a `#FILES` section of
/// `path\tsize\ttype` lines, then a `#MD5` section of `hash  /workspace/path`.
pub fn parse(output: &str) -> Manifest {
    let mut manifest = Manifest::new();
    let mut in_md5 = false;
    for line in output.lines() {
        match line {
            "#FILES" => in_md5 = false,
            "#MD5" => in_md5 = true,
            "" => {}
            _ if in_md5 => {
                let Some((hash, path)) = line.split_once("  ") else {
                    continue;
                };
                let path = path.strip_prefix("/workspace/").unwrap_or(path);
                if let Some(entry) = manifest.get_mut(path) {
                    entry.md5 = Some(hash.to_string());
                }
            }
            _ => {
                let mut parts = line.split('\t');
                let (Some(path), Some(size), Some(kind)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if path.is_empty() {
                    continue;
                }
                manifest.insert(
                    path.to_string(),
                    Entry {
                        kind: kind.chars().next().unwrap_or('f'),
                        size: size.parse().unwrap_or(0),
                        md5: None,
                    },
                );
            }
        }
    }
    manifest
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

/// Host-side manifest diff. Files compare by md5 (or size when a hash is
/// missing); directories and symlinks by existence + type only.
pub fn diff(before: &Manifest, after: &Manifest) -> Vec<(String, Change)> {
    let mut changes = Vec::new();
    for (path, entry) in after {
        match before.get(path) {
            None => changes.push((path.clone(), Change::Added)),
            Some(prev) => {
                let modified = if entry.kind == 'f' && prev.kind == 'f' {
                    match (&prev.md5, &entry.md5) {
                        (Some(a), Some(b)) => a != b,
                        _ => prev.size != entry.size,
                    }
                } else {
                    prev.kind != entry.kind
                };
                if modified {
                    changes.push((path.clone(), Change::Modified));
                }
            }
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            changes.push((path.clone(), Change::Deleted));
        }
    }
    changes
}

/// What a given package-manager command is allowed to touch.
pub struct WriteSet {
    pub dirs: Vec<&'static str>,
    pub files: Vec<&'static str>,
}

impl WriteSet {
    pub fn contains(&self, path: &str) -> bool {
        self.files.contains(&path)
            || self
                .dirs
                .iter()
                .any(|d| path == *d || path.starts_with(&format!("{d}/")))
    }
}

/// Expected write-set per command. `tool` is "composer" or "npm";
/// `args` everything after it.
pub fn expected_write_set(tool: &str, args: &[String]) -> WriteSet {
    match tool {
        "composer" => {
            let mutates_json = args
                .first()
                .is_some_and(|sub| matches!(sub.as_str(), "require" | "remove" | "update" | "up"));
            let mut files = vec!["composer.lock"];
            if mutates_json {
                files.push("composer.json");
            }
            WriteSet {
                dirs: vec!["vendor"],
                files,
            }
        }
        "npm" => {
            let sub = args.first().map(String::as_str).unwrap_or("");
            let has_pkg_args = args.iter().skip(1).any(|a| !a.starts_with('-'));
            // `npm install <pkg>`, uninstall and update rewrite package.json;
            // a bare `npm install` / `npm ci` only the lockfile.
            let mutates_json =
                matches!(sub, "uninstall" | "remove" | "rm" | "un" | "update" | "up")
                    || (matches!(sub, "install" | "i" | "add") && has_pkg_args);
            let mut files = vec!["package-lock.json"];
            if mutates_json {
                files.push("package.json");
            }
            WriteSet {
                dirs: vec!["node_modules"],
                files,
            }
        }
        _ => WriteSet {
            dirs: vec![],
            files: vec![],
        },
    }
}
