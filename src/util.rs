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

/// Tar+gzip a host directory into `out_tgz` so it can be copied into a sandbox
/// in one shot and unpacked there. Uses the `tar`/`flate2` crates (no host `tar`
/// binary) and runs the compression on a blocking thread so it never stalls the
/// async runtime — a large project tree can take a while.
///
/// Used only by the `dev` path — the review run mounts the project read-only via
/// an overlay instead of taring it in. Top-level `vendor/`/`node_modules/` are
/// always skipped: `dev` installs them Linux-native in the guest and never
/// copies back, so shipping the host's (macOS) build in would be wrong as well
/// as wasteful. Top level only: a nested one (e.g. Laravel's `public/vendor`) is
/// real content.
pub async fn tar_directory(dir: &Path, out_tgz: &Path) -> Result<()> {
    let dir = dir.to_path_buf();
    let out = out_tgz.to_path_buf();
    let label = dir.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let file = std::fs::File::create(&out)?;
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        // Store symlinks as symlinks, like the `tar` CLI does by default. The
        // crate otherwise follows them, which turns `node_modules/.bin/*` links
        // into stray file copies whose relative `require()`s then break.
        builder.follow_symlinks(false);
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == "vendor" || name == "node_modules" {
                continue;
            }
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                builder.append_dir_all(&name, &path)?;
            } else {
                builder.append_path_with_name(&path, &name)?;
            }
        }
        builder.into_inner()?.finish()?;
        Ok(())
    })
    .await
    .with_context(|| format!("packing {} failed", label.display()))?
}
