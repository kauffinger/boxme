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

/// Parse the output of `scripts::MANIFEST`: NUL-delimited records, a `#FILES`
/// section of `size\ttype\tpath` then a `#MD5` section of `hash  /workspace/path`
/// (from `md5sum -z`). The path is the record's last field, so tabs or newlines
/// in a filename stay part of the path instead of forging extra records.
pub fn parse(output: &str) -> Manifest {
    let mut manifest = Manifest::new();
    let mut in_md5 = false;
    for record in output.split('\0') {
        match record {
            "#FILES" => in_md5 = false,
            "#MD5" => in_md5 = true,
            "" => {}
            _ if in_md5 => {
                // `md5sum` output is a 32-char hex digest, two spaces, then the
                // verbatim path — split on the fixed digest width rather than a
                // delimiter the path itself could contain.
                if record.len() < 34 {
                    continue;
                }
                let (hash, rest) = record.split_at(32);
                if !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    continue;
                }
                let path = rest.trim_start_matches(' ');
                let path = path.strip_prefix("/workspace/").unwrap_or(path);
                if let Some(entry) = manifest.get_mut(path) {
                    entry.md5 = Some(hash.to_string());
                }
            }
            _ => {
                let mut parts = record.splitn(3, '\t');
                let (Some(size), Some(kind), Some(path)) =
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            || self.dirs.iter().any(|d| {
                path == *d
                    || path
                        .strip_prefix(*d)
                        .is_some_and(|rest| rest.starts_with('/'))
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_files_and_joins_md5() {
        let output = "#FILES\0\
            12\tf\tcomposer.json\0\
            4096\td\tsrc\0\
            #MD5\0\
            d41d8cd98f00b204e9800998ecf8427e  /workspace/composer.json\0";
        let manifest = parse(output);
        let json = manifest.get("composer.json").unwrap();
        assert_eq!(json.kind, 'f');
        assert_eq!(json.size, 12);
        assert_eq!(
            json.md5.as_deref(),
            Some("d41d8cd98f00b204e9800998ecf8427e")
        );
        assert_eq!(manifest.get("src").unwrap().kind, 'd');
    }

    #[test]
    fn filename_with_tab_and_newline_stays_one_entry() {
        // A name containing a tab and a newline must not forge extra records or
        // truncate the path — it is the record's last field, NUL-delimited.
        let nasty = "weird\tname\nwith-breaks.txt";
        let output = format!("#FILES\05\tf\t{nasty}\0");
        let manifest = parse(&output);
        assert_eq!(manifest.len(), 1);
        assert!(manifest.contains_key(nasty));
    }
}
