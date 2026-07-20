use std::path::Path;

use crate::scripts::{DEFAULT_PHP_VERSION, PHP_VERSIONS};

/// Resolve the PHP `X.Y` the guest should run. Host binary first — run from
/// the project dir so mise/asdf/herd shims resolve per-directory — then the
/// composer.json constraint, then the default. Always clamped to the versions
/// actually baked into the base image.
pub async fn php_version(project_dir: &Path) -> String {
    choose_php(
        host_php_version(project_dir).await.as_deref(),
        composer_php_constraint(project_dir).as_deref(),
    )
}

/// The host version wins only when it doesn't contradict the composer.json
/// constraint — booting a guest on the host's 8.3 is useless when the project
/// requires ^8.5, the install would just fail its platform check.
fn choose_php(host: Option<&str>, constraint: Option<&str>) -> String {
    let host_contradicts = match (host, constraint) {
        (Some(v), Some(c)) => satisfies(v, c) == Some(false),
        _ => false,
    };
    if !host_contradicts {
        if let Some(v) = host {
            if let Some(pick) = baked_or_clamped(v) {
                return pick;
            }
        }
    }
    if let Some(c) = constraint {
        if let Some(v) = PHP_VERSIONS.iter().find(|b| satisfies(b, c) == Some(true)) {
            if host_contradicts {
                let h = host.unwrap_or_default();
                eprintln!(
                    "note: host PHP {h} does not satisfy composer.json php \"{c}\"; using {v}"
                );
            }
            return v.to_string();
        }
        if let Some(v) = first_constraint_version(c) {
            if PHP_VERSIONS.contains(&v.as_str()) {
                return v;
            }
        }
    }
    // The constraint ruled the host out but no baked version satisfies it
    // either (e.g. ^7.4) — the host version is still the best boot we have.
    if let Some(v) = host {
        if let Some(pick) = baked_or_clamped(v) {
            return pick;
        }
    }
    DEFAULT_PHP_VERSION.to_string()
}

/// Map a host version onto the baked set: exact when baked, else clamped to
/// the nearest edge (with a note).
fn baked_or_clamped(v: &str) -> Option<String> {
    if PHP_VERSIONS.contains(&v) {
        return Some(v.to_string());
    }
    eprintln!("note: host PHP {v} is not in the base image (8.3-8.5); picking closest");
    clamp_php(v)
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

/// The raw composer.json `require.php` constraint, e.g. "^8.3" or ">=8.3 <8.5".
fn composer_php_constraint(dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join("composer.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(json.get("require")?.get("php")?.as_str()?.to_string())
}

/// The first X.Y that appears in a constraint — the pre-satisfier fallback for
/// shapes `satisfies` can't parse.
fn first_constraint_version(constraint: &str) -> Option<String> {
    let start = constraint.find(|c: char| c.is_ascii_digit())?;
    major_minor(&constraint[start..])
}

/// Does PHP `X.Y` satisfy a composer version constraint? Understands the
/// common forms (`^8.3`, `~8.3`, `>=8.1 <8.5`, `8.3.*`, `||` alternatives) at
/// major.minor granularity. `None` when the constraint can't be parsed — the
/// caller then trusts the host version, as before.
fn satisfies(version: &str, constraint: &str) -> Option<bool> {
    let (major, minor) = parse_req(version)?;
    let v = (major, minor?);
    let mut any = false;
    let mut alternatives = 0;
    for alt in constraint
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        alternatives += 1;
        let mut all = true;
        let mut primitives = 0;
        for prim in alt
            .split([' ', ',', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            primitives += 1;
            if !primitive_satisfied(v, prim)? {
                all = false;
            }
        }
        if primitives == 0 {
            return None;
        }
        if all {
            any = true;
        }
    }
    if alternatives == 0 {
        return None;
    }
    Some(any)
}

fn primitive_satisfied(v: (u32, u32), prim: &str) -> Option<bool> {
    // Strip a stability suffix like `^8.4@beta`.
    let prim = prim.split('@').next().unwrap_or(prim);
    if let Some(rest) = prim.strip_prefix('^') {
        let (major, minor) = parse_req(rest)?;
        return Some(v.0 == major && v >= (major, minor.unwrap_or(0)));
    }
    if let Some(rest) = prim.strip_prefix('~') {
        let (major, minor) = parse_req(rest)?;
        // ~8.3.1 pins the minor; ~8.3 (and ~8) allow minor bumps.
        return Some(if rest.split('.').count() >= 3 {
            v == (major, minor.unwrap_or(0))
        } else {
            v.0 == major && v >= (major, minor.unwrap_or(0))
        });
    }
    if let Some(rest) = prim.strip_prefix(">=") {
        let (major, minor) = parse_req(rest)?;
        return Some(v >= (major, minor.unwrap_or(0)));
    }
    if let Some(rest) = prim.strip_prefix("<=") {
        let (major, minor) = parse_req(rest)?;
        return Some(v <= (major, minor.unwrap_or(u32::MAX)));
    }
    if let Some(rest) = prim.strip_prefix('>') {
        // At X.Y granularity `>8.3` still admits an 8.3.x patch release.
        let (major, minor) = parse_req(rest)?;
        return Some(v >= (major, minor.unwrap_or(0)));
    }
    if let Some(rest) = prim.strip_prefix('<') {
        let (major, minor) = parse_req(rest)?;
        return Some(v < (major, minor.unwrap_or(0)));
    }
    let rest = prim.strip_prefix('=').unwrap_or(prim);
    let (major, minor) = parse_req(rest)?;
    Some(match minor {
        Some(minor) => v == (major, minor),
        None => v.0 == major, // "8", "8.*"
    })
}

/// "8", "8.3", "8.3.14", "8.*" → (major, minor). A `None` minor means "any"
/// ("8", "8.*"); anything non-numeric fails the parse.
fn parse_req(s: &str) -> Option<(u32, Option<u32>)> {
    let s = s.trim().trim_start_matches('v');
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.trim().parse().ok()?;
    let minor = match parts.next() {
        None => None,
        Some("*") | Some("x") => None,
        Some(m) => {
            let digits: String = m.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            Some(digits.parse().ok()?)
        }
    };
    Some((major, minor))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_wins_when_it_satisfies_the_constraint() {
        assert_eq!(choose_php(Some("8.4"), Some("^8.3")), "8.4");
        assert_eq!(choose_php(Some("8.3"), Some(">=8.1")), "8.3");
    }

    #[test]
    fn constraint_beats_an_incompatible_host() {
        // The reported case: project requires ^8.5, host runs 8.3.
        assert_eq!(choose_php(Some("8.3"), Some("^8.5")), "8.5");
        // A pin below the host also wins.
        assert_eq!(choose_php(Some("8.5"), Some("8.3.*")), "8.3");
    }

    #[test]
    fn no_host_resolves_from_the_constraint() {
        assert_eq!(choose_php(None, Some("^8.4")), "8.4");
        assert_eq!(choose_php(None, Some(">=8.4 <8.6")), "8.4");
        assert_eq!(choose_php(None, None), DEFAULT_PHP_VERSION);
    }

    #[test]
    fn unparseable_constraint_trusts_the_host() {
        assert_eq!(choose_php(Some("8.3"), Some("dev-main")), "8.3");
    }

    #[test]
    fn unsatisfiable_constraint_falls_back_to_the_host() {
        // ^7.4 rules the host out but nothing baked satisfies it either.
        assert_eq!(choose_php(Some("8.3"), Some("^7.4")), "8.3");
    }

    #[test]
    fn out_of_range_host_still_clamps() {
        assert_eq!(choose_php(Some("8.2"), None), "8.3");
        assert_eq!(choose_php(Some("9.0"), None), "8.5");
    }

    #[test]
    fn satisfies_common_constraint_shapes() {
        assert_eq!(satisfies("8.3", "^8.3"), Some(true));
        assert_eq!(satisfies("8.3", "^8.5"), Some(false));
        assert_eq!(satisfies("8.4", ">=8.1 <8.5"), Some(true));
        assert_eq!(satisfies("8.5", ">=8.1 <8.5"), Some(false));
        assert_eq!(satisfies("8.4", "8.3.* || 8.4.*"), Some(true));
        assert_eq!(satisfies("8.5", "8.3.*|8.4.*"), Some(false));
        assert_eq!(satisfies("8.3", "~8.3.2"), Some(true));
        assert_eq!(satisfies("8.4", "~8.3.2"), Some(false));
        assert_eq!(satisfies("8.4", "~8.3"), Some(true));
        assert_eq!(satisfies("8.4", "8.*"), Some(true));
        assert_eq!(satisfies("8.4", "^8.4@beta"), Some(true));
        assert_eq!(satisfies("8.4", ">=7.2.5"), Some(true));
        assert_eq!(satisfies("8.4", "dev-main"), None);
    }
}
