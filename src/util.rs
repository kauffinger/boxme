use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use microsandbox::sandbox::exec::ExecEvent;
use microsandbox::Sandbox;

/// Lowercase + hyphenate down to `[a-z0-9-]`, collapsing runs of separators.
/// Sandbox VM names must match this shape.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !out.is_empty() && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Single-quote a string for safe interpolation into a shell command line.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// Decode a stream of UTF-8 byte chunks where a codepoint may be split across a
/// chunk boundary. Complete bytes are returned immediately; an incomplete
/// trailing sequence is held in `carry` until the next chunk completes it.
/// Genuinely invalid bytes are replaced with U+FFFD so the stream can't stall.
pub fn decode_utf8_stream(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    carry.extend_from_slice(chunk);
    let mut out = String::new();
    loop {
        match std::str::from_utf8(carry) {
            Ok(valid) => {
                out.push_str(valid);
                carry.clear();
                return out;
            }
            Err(e) => {
                let good = e.valid_up_to();
                if good > 0 {
                    out.push_str(std::str::from_utf8(&carry[..good]).unwrap());
                }
                match e.error_len() {
                    // Incomplete sequence at the end: keep it for the next chunk.
                    None => {
                        carry.drain(..good);
                        return out;
                    }
                    // Invalid bytes mid-stream: emit a replacement char and skip.
                    Some(bad) => {
                        out.push('\u{FFFD}');
                        carry.drain(..good + bad);
                    }
                }
            }
        }
    }
}

/// Run a bash script inside the sandbox and forward stdout/stderr to the host's
/// stderr as it arrives. Returns the script's exit code.
pub async fn stream_shell_stderr(sb: &Sandbox, script: &str) -> Result<i32> {
    let mut handle = sb
        .exec_stream_with("bash", |e| e.args(["-lc", script]))
        .await?;

    let mut code = -1;
    let mut carry: Vec<u8> = Vec::new();
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes) => {
                let data = decode_utf8_stream(&mut carry, &bytes);
                if !data.is_empty() {
                    let mut err = std::io::stderr();
                    let _ = err.write_all(data.as_bytes());
                    let _ = err.flush();
                }
            }
            ExecEvent::Exited { code: c } => code = c,
            ExecEvent::Failed(f) => return Err(anyhow!("command failed to start: {f:?}")),
            _ => {}
        }
    }

    Ok(code)
}

/// Run a bash script inside the sandbox quietly, collecting stdout. Errors with
/// the captured stderr when the script exits nonzero.
pub async fn shell_capture(sb: &Sandbox, script: &str) -> Result<String> {
    let output = sb
        .shell_with(script, |e| e.timeout(std::time::Duration::from_secs(600)))
        .await?;
    if output.status().code != 0 {
        return Err(anyhow!(
            "guest script exited with code {}:\n{}",
            output.status().code,
            output.stderr().unwrap_or_default()
        ));
    }
    Ok(output.stdout().unwrap_or_default())
}

/// What `tar_directory` leaves out of the project tarball.
#[derive(Clone, Copy, Default)]
pub struct CopyFilter {
    /// Skip the top-level `.git` directory (the guest rebuilds its baseline).
    pub without_git: bool,
    /// Skip image/video/audio/archive assets anywhere in the tree.
    pub without_media: bool,
    /// Skip the top-level `vendor/`/`node_modules/`. Off by default so
    /// incremental commands start from the existing install; the `dev` path
    /// always sets it (it installs Linux-native in-guest and never copies back).
    pub without_deps: bool,
}

/// What the copy filter dropped, for the run summary.
#[derive(Default)]
pub struct SkipStats {
    pub files: u64,
    pub bytes: u64,
}

/// Heavy binary assets composer/npm install scripts don't read. Matched
/// case-insensitively against the file extension when `--without-media` is set.
const MEDIA_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif", "ico", "avif", "heic", "heif", "svg",
    "psd", "ai", "eps", // video
    "mp4", "mov", "avi", "mkv", "webm", "m4v", "wmv", "flv", "mpg", "mpeg", // audio
    "mp3", "wav", "flac", "ogg", "aac", "m4a", "wma", // archives / disk images
    "zip", "gz", "tgz", "bz2", "xz", "7z", "rar", "tar", "dmg", "iso",
];

fn is_media(name: &std::ffi::OsStr) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively append `disk_path`'s contents under `arc_path` in the archive,
/// dropping media files when asked. Symlinks are stored as symlinks and not
/// followed (matching `follow_symlinks(false)`), so symlink cycles can't loop.
fn append_filtered<W: Write>(
    builder: &mut tar::Builder<W>,
    disk_path: &Path,
    arc_path: &Path,
    without_media: bool,
    stats: &mut SkipStats,
) -> Result<()> {
    for entry in std::fs::read_dir(disk_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        let arc = arc_path.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            builder.append_path_with_name(&path, &arc)?;
        } else if file_type.is_dir() {
            builder.append_path_with_name(&path, &arc)?;
            append_filtered(builder, &path, &arc, without_media, stats)?;
        } else if without_media && is_media(&name) {
            stats.files += 1;
            stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
        } else {
            builder.append_path_with_name(&path, &arc)?;
        }
    }
    Ok(())
}

/// Tar+gzip a host directory into `out_tgz` so it can be copied into a sandbox
/// in one shot and unpacked there. Uses the `tar`/`flate2` crates (no host `tar`
/// binary) and runs the compression on a blocking thread so it never stalls the
/// async runtime — a large project tree can take a while. Returns what the
/// filter dropped.
pub async fn tar_directory(dir: &Path, out_tgz: &Path, filter: CopyFilter) -> Result<SkipStats> {
    let dir = dir.to_path_buf();
    let out = out_tgz.to_path_buf();
    let label = dir.clone();
    tokio::task::spawn_blocking(move || -> Result<SkipStats> {
        let file = std::fs::File::create(&out)?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        // Store symlinks as symlinks, like the `tar` CLI does by default. The
        // crate otherwise follows them, which turns `node_modules/.bin/*` links
        // into stray file copies whose relative `require()`s then break.
        builder.follow_symlinks(false);
        let mut stats = SkipStats::default();
        // Top-level vendor/ and node_modules/ are copied in by default so an
        // incremental command starts from the existing install; `--without-deps`
        // drops them for a lighter full-install transfer. Top level only: a
        // nested one (e.g. Laravel's `public/vendor`) is real content. `.git` is
        // top-level too, dropped only when asked.
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if filter.without_deps && (name == "vendor" || name == "node_modules") {
                continue;
            }
            if filter.without_git && name == ".git" {
                continue;
            }
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                builder.append_path_with_name(&path, &name)?;
            } else if file_type.is_dir() {
                builder.append_path_with_name(&path, &name)?;
                append_filtered(
                    &mut builder,
                    &path,
                    Path::new(&name),
                    filter.without_media,
                    &mut stats,
                )?;
            } else if filter.without_media && is_media(&name) {
                stats.files += 1;
                stats.bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            } else {
                builder.append_path_with_name(&path, &name)?;
            }
        }
        builder.into_inner()?.finish()?;
        Ok(stats)
    })
    .await
    .with_context(|| format!("packing {} failed", label.display()))?
}

/// Human-readable byte size for run summaries (e.g. "1.3 GiB").
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn media_match_is_case_insensitive_and_extension_only() {
        assert!(is_media(OsStr::new("logo.PNG")));
        assert!(is_media(OsStr::new("clip.Mp4")));
        assert!(is_media(OsStr::new("bundle.tar")));
        assert!(!is_media(OsStr::new("composer.json")));
        assert!(!is_media(OsStr::new("package-lock.json")));
        // No extension, or a name that merely contains a media word.
        assert!(!is_media(OsStr::new("Makefile")));
        assert!(!is_media(OsStr::new("png")));
    }

    #[test]
    fn human_bytes_scales_to_the_largest_fitting_unit() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
    }
}
