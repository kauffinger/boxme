//! The out-of-workspace change scan: what the command wrote anywhere on the
//! guest besides /workspace. Informational only — these are never copied back;
//! they're a supply-chain / indicator-of-compromise signal for the review (a
//! postinstall script dropping a binary in /usr/local/bin, a key in /root/.ssh,
//! a cron entry in /etc, ...).

/// Largest scan we'll render. A clean run touches nothing out here, so hitting
/// this is itself a signal; we cap rather than flood the review.
const MAX_FILES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysKind {
    File,
    Symlink,
}

/// A guest path outside /workspace that the command created or modified.
#[derive(Debug, Clone)]
pub struct SysFile {
    pub path: String,
    pub kind: SysKind,
    /// Bytes for a file; the target length for a symlink.
    pub size: u64,
}

/// Result of the out-of-workspace sweep. `available` is false only when the
/// baseline marker is missing (the touch failed) — then the scan couldn't run
/// and the review says so rather than claiming nothing changed.
pub struct OutsideScan {
    pub files: Vec<SysFile>,
    pub available: bool,
    pub truncated: bool,
}

impl OutsideScan {
    /// The scan couldn't run (no marker / guest error) — shown as a banner.
    pub fn unavailable() -> Self {
        OutsideScan {
            files: Vec::new(),
            available: false,
            truncated: false,
        }
    }

    /// Banner text when the sweep couldn't run; `None` when it did.
    pub fn banner(&self) -> Option<String> {
        (!self.available).then(|| {
            "filesystem baseline unavailable — changes outside /workspace were not scanned"
                .to_string()
        })
    }
}

/// Parse `scripts::outside_scan` output: `size\ttype\tpath` lines, sorted by
/// path so related entries (e.g. everything under /usr/local/bin) cluster. The
/// sentinel `#NOMARKER` means the baseline was never recorded.
pub fn parse(out: &str) -> OutsideScan {
    if out.trim() == "#NOMARKER" {
        return OutsideScan::unavailable();
    }
    let mut files: Vec<SysFile> = out.lines().filter_map(parse_line).collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    OutsideScan {
        files,
        available: true,
        truncated,
    }
}

fn parse_line(line: &str) -> Option<SysFile> {
    let mut parts = line.splitn(3, '\t');
    let size = parts.next()?.parse().ok()?;
    let kind = match parts.next()? {
        "f" => SysKind::File,
        "l" => SysKind::Symlink,
        _ => return None,
    };
    let path = parts.next()?.to_string();
    Some(SysFile { path, kind, size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_marker_is_unavailable() {
        let scan = parse("#NOMARKER\n");
        assert!(!scan.available);
        assert!(scan.files.is_empty());
    }

    #[test]
    fn parses_sorts_and_keeps_files_and_symlinks() {
        // Intentionally out of order; directories (`d`) and junk lines drop out.
        let out = "1024\tf\t/usr/local/bin/evil\n\
                   42\tl\t/root/.ssh/authorized_keys\n\
                   0\td\t/usr/local/bin\n\
                   garbage line\n";
        let scan = parse(out);
        assert!(scan.available);
        assert!(!scan.truncated);
        let paths: Vec<_> = scan.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["/root/.ssh/authorized_keys", "/usr/local/bin/evil"]);
        assert_eq!(scan.files[0].kind, SysKind::Symlink);
        assert_eq!(scan.files[1].size, 1024);
    }
}
