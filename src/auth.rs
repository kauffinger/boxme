//! Storage for the Claude Code OAuth token `boxme claude` authenticates with, so
//! the credential lives in the OS keychain (macOS) or a `0600` file (Linux)
//! instead of an exported shell variable. The token is read only at boot and
//! injected into the *guest* environment — it never touches your host shell or a
//! dotfile. `claude setup-token` mints a scoped, year-long token for exactly this;
//! `boxme login` captures it, `boxme logout` removes it.

use std::io::{self, Write};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use owo_colors::OwoColorize;

/// The env var the stored token is injected as inside the guest. Matches the name
/// Claude Code reads (`claude setup-token`'s output).
pub const TOKEN_VAR: &str = "CLAUDE_CODE_OAUTH_TOKEN";

/// Keychain item coordinates (macOS) / file name stem (Linux).
const SERVICE: &str = "boxme-claude-oauth";
const ACCOUNT: &str = "claude";

/// `boxme login`: prompt for the token and store it. Reads a single line from
/// stdin with echo disabled (so a pasted token never lands in the terminal
/// scrollback); a piped line works too, for scripting.
pub fn login() -> Result<()> {
    let token = read_token()?;
    if token.is_empty() {
        bail!("no token entered — nothing saved");
    }
    store(&token)?;
    eprintln!("{}", "saved the Claude token to your keychain.".green());
    eprintln!(
        "{}",
        format!(
            "boxme claude will use it automatically — you can drop any `export {TOKEN_VAR}=…` \
             from your shell rc now."
        )
        .dimmed()
    );
    Ok(())
}

/// `boxme logout`: remove the stored token.
pub fn logout() -> Result<()> {
    if delete()? {
        eprintln!("{}", "removed the stored Claude token.".green());
    } else {
        eprintln!("{}", "no stored Claude token to remove.".dimmed());
    }
    Ok(())
}

fn read_token() -> Result<String> {
    eprint!(
        "{}",
        "paste your Claude token (from `claude setup-token`), then press Enter: ".bold()
    );
    io::stderr().flush().ok();

    let echo_off = set_echo(false);
    let mut line = String::new();
    let n = io::stdin()
        .read_line(&mut line)
        .context("reading the token from stdin")?;
    if echo_off {
        set_echo(true);
        eprintln!();
    }
    if n == 0 {
        bail!("no input received");
    }
    Ok(line.trim().to_string())
}

/// Toggle terminal echo via `stty`, operating on our inherited stdin. Returns
/// whether it took effect — `false` when stdin isn't a tty (e.g. a pipe), in
/// which case the caller just reads normally.
fn set_echo(on: bool) -> bool {
    let arg = if on { "echo" } else { "-echo" };
    Command::new("stty")
        .arg(arg)
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn store(token: &str) -> Result<()> {
    // -U updates the item in place if it already exists. `security` exposes the
    // token in argv briefly (visible to `ps`); acceptable on a personal machine,
    // and there is no stdin path for add-generic-password.
    let status = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-a",
            ACCOUNT,
            "-s",
            SERVICE,
            "-w",
            token,
        ])
        .status()
        .context("running `security add-generic-password`")?;
    if !status.success() {
        bail!("storing the token in the keychain failed");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn load() -> Result<Option<String>> {
    let out = Command::new("security")
        .args(["find-generic-password", "-a", ACCOUNT, "-s", SERVICE, "-w"])
        .output()
        .context("running `security find-generic-password`")?;
    if !out.status.success() {
        return Ok(None); // not found
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!token.is_empty()).then_some(token))
}

#[cfg(target_os = "macos")]
fn delete() -> Result<bool> {
    let out = Command::new("security")
        .args(["delete-generic-password", "-a", ACCOUNT, "-s", SERVICE])
        .output()
        .context("running `security delete-generic-password`")?;
    Ok(out.status.success())
}

#[cfg(not(target_os = "macos"))]
fn store(token: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = token_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, token).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting 0600 on {}", path.display()))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn load() -> Result<Option<String>> {
    let path = token_file()?;
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let token = s.trim().to_string();
            Ok((!token.is_empty()).then_some(token))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(not(target_os = "macos"))]
fn delete() -> Result<bool> {
    let path = token_file()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(not(target_os = "macos"))]
fn token_file() -> Result<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .context("cannot locate a config dir — set HOME or XDG_CONFIG_HOME")?;
    Ok(base.join("boxme").join("claude-oauth-token"))
}
