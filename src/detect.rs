use std::path::Path;

use crate::scripts::{DEFAULT_PHP_VERSION, PHP_VERSIONS};

/// Resolve the PHP `X.Y` the guest should run. Host binary first — run from
/// the project dir so mise/asdf/herd shims resolve per-directory — then the
/// composer.json constraint, then the default. Always clamped to the versions
/// actually baked into the base image.
pub async fn php_version(project_dir: &Path) -> String {
    if let Some(v) = host_php_version(project_dir).await {
        if PHP_VERSIONS.contains(&v.as_str()) {
            return v;
        }
        eprintln!("note: host PHP {v} is not in the base image (8.3-8.5); picking closest");
        if let Some(clamped) = clamp_php(&v) {
            return clamped;
        }
    }
    if let Some(v) = composer_json_php(project_dir) {
        if PHP_VERSIONS.contains(&v.as_str()) {
            return v;
        }
    }
    DEFAULT_PHP_VERSION.to_string()
}

/// Resolve the Node major for the guest: host `node -v` from the project dir,
/// then .nvmrc, then package.json engines.node.  None means "whatever the
/// base image ships".
pub async fn node_major(project_dir: &Path) -> Option<u32> {
    if let Some(v) = host_node_major(project_dir).await {
        return Some(v);
    }
    if let Ok(nvmrc) = std::fs::read_to_string(project_dir.join(".nvmrc")) {
        if let Some(v) = first_version_number(&nvmrc) {
            return Some(v);
        }
    }
    if let Some(v) = package_json_node(project_dir) {
        return Some(v);
    }
    None
}

async fn host_php_version(dir: &Path) -> Option<String> {
    let output = tokio::process::Command::new("php")
        .arg("-v")
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // "PHP 8.3.14 (cli) ..." -> "8.3"
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.split_whitespace().nth(1)?;
    major_minor(version)
}

async fn host_node_major(dir: &Path) -> Option<u32> {
    let output = tokio::process::Command::new("node")
        .arg("-v")
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // "v22.11.0" -> 22
    first_version_number(&String::from_utf8_lossy(&output.stdout))
}

/// composer.json `require.php` like "^8.3", ">=8.3 <8.5", "8.3.*" — take the
/// first X.Y that appears.
fn composer_json_php(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let constraint = json.get("require")?.get("php")?.as_str()?;
    let start = constraint.find(|c: char| c.is_ascii_digit())?;
    major_minor(&constraint[start..])
}

/// package.json `engines.node` like ">=20", "^22.1" — take the first number.
fn package_json_node(dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let constraint = json.get("engines")?.get("node")?.as_str()?;
    first_version_number(constraint)
}

/// "8.3.14-dev" -> "8.3"
fn major_minor(version: &str) -> Option<String> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.trim().parse().ok()?;
    let minor: u32 = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    Some(format!("{major}.{minor}"))
}

/// First integer in a string like "v22.11.0" or ">=20 <23" -> 22 / 20.
fn first_version_number(s: &str) -> Option<u32> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let digits: String = s[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// A host PHP outside 8.3-8.5 still has to map to something bootable.
fn clamp_php(v: &str) -> Option<String> {
    let (major, minor) = {
        let mut it = v.split('.');
        (
            it.next()?.parse::<u32>().ok()?,
            it.next()?.parse::<u32>().ok()?,
        )
    };
    let known: Vec<(u32, u32)> = PHP_VERSIONS
        .iter()
        .filter_map(|s| {
            let mut it = s.split('.');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect();
    let (lo, hi) = (known.first()?, known.last()?);
    let pick = if (major, minor) < *lo { lo } else { hi };
    Some(format!("{}.{}", pick.0, pick.1))
}
