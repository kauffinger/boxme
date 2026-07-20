//! Inject the host's global composer `auth.json` into the guest as microsandbox
//! *secrets*, so private-repo credentials are usable inside the box but cannot
//! be read or exfiltrated by anything running there.
//!
//! The mechanism is microsandbox's native placeholder-secret + TLS interception
//! (see `microsandbox_network::secrets`): the guest only ever sees an opaque
//! placeholder (e.g. `__BOXME_COMPOSER_SECRET_0__`), never the real value. We
//! build a `COMPOSER_AUTH` environment variable that is structurally identical
//! to the user's `auth.json` but with every password/token replaced by its
//! placeholder, and register one secret per credential. microsandbox runs a
//! host-side TLS proxy that substitutes the real value into the outgoing
//! request — basic-auth, headers, or query — but *only* on a TLS-intercepted
//! connection whose SNI matches that credential's host (`require_tls_identity`).
//! Its guest agent installs the interception CA into the guest trust store, so
//! composer (PHP-curl) and git trust the re-signed certs with no setup.
//!
//! Because the default violation action is `BlockAndLog`, if any in-box code
//! copies a placeholder and tries to send it to a *different* host, microsandbox
//! blocks the request — the real secret never travels anywhere but the one host
//! it authenticates. Network reachability is unchanged: a private repo still has
//! to be in `.boxme/allow` (or `.boxme/claude-allow`) to be contacted at all;
//! the credential simply rides along automatically once the host is allowed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use microsandbox_network::secrets::config::{HostPattern, SecretEntry, SecretInjection};
use owo_colors::OwoColorize;

/// Guest-side path where microsandbox's agent installs the TLS interception CA.
/// Mirrors `microsandbox_protocol::GUEST_TLS_CA_PATH`. Node ships its own CA
/// bundle and ignores the system trust store, so npm / Claude Code need this
/// pointed at the proxy CA via `NODE_EXTRA_CA_CERTS`; composer (PHP-curl) and
/// git use the system store the agent already updated.
pub const GUEST_TLS_CA_PATH: &str = "/.msb/tls/ca.pem";

/// The composer credentials rewritten for placeholder injection.
pub struct ComposerAuth {
    /// `("COMPOSER_AUTH", <auth.json with placeholders>)`, injected as a guest
    /// env var. composer reads it natively, sending the placeholders on the wire
    /// where the TLS proxy swaps in the real values for the matching host.
    pub env: (String, String),
    /// One microsandbox secret per credential — real value held host-side,
    /// placeholder in-guest, scoped to the credential's own host.
    pub secrets: Vec<SecretEntry>,
}

/// Load the host auth.json and, if it holds credentials, push `COMPOSER_AUTH`
/// (with placeholders) and `NODE_EXTRA_CA_CERTS` onto `env`, returning the
/// secret entries to register on the sandbox builder. Prints a one-line note.
/// Callers gate this on `--composer-auth` (and that composer is in play).
pub fn inject(env: &mut Vec<(String, String)>) -> Result<Vec<SecretEntry>> {
    match load()? {
        Some(auth) => {
            eprintln!(
                "{} {} credential(s) from auth.json as sandbox secrets",
                ">> composer-auth: injecting".dimmed(),
                auth.secrets.len(),
            );
            env.push(auth.env);
            // Node ignores the system trust store the guest agent updates, so
            // point npm / Claude Code at the proxy CA explicitly.
            env.push((
                "NODE_EXTRA_CA_CERTS".to_string(),
                GUEST_TLS_CA_PATH.to_string(),
            ));
            Ok(auth.secrets)
        }
        None => {
            eprintln!(
                "{}",
                ">> composer-auth: no global auth.json with credentials found — nothing injected"
                    .dimmed()
            );
            Ok(Vec::new())
        }
    }
}

/// Load the host's global composer `auth.json` and build the injection. Returns
/// `None` when no auth file exists or it holds no credentials we can inject.
pub fn load() -> Result<Option<ComposerAuth>> {
    let Some(path) = auth_path() else {
        return Ok(None);
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {} as JSON", path.display()))?;
    Ok(build(value))
}

/// First existing global `auth.json`, honoring `COMPOSER_HOME` then the XDG /
/// legacy locations — the same precedence composer itself uses.
fn auth_path() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = std::env::var_os("COMPOSER_HOME") {
        candidates.push(PathBuf::from(home).join("auth.json"));
    }
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(base) = xdg {
        candidates.push(base.join("composer").join("auth.json"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".composer").join("auth.json"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// Walk the parsed `auth.json`, replacing each secret leaf with a placeholder
/// and emitting a matching secret entry. Returns `None` if nothing was found.
fn build(mut value: serde_json::Value) -> Option<ComposerAuth> {
    let obj = value.as_object_mut()?;
    let mut secrets = Vec::new();

    // Token-style sections: a flat `{ host: "token" }` map. The token can ride
    // in an Authorization header (`Bearer`/`token`), a Basic-auth username (git
    // over HTTPS), or a `?access_token=` query, so allow all three scopes.
    for section in ["github-oauth", "gitlab-oauth", "gitlab-token", "bearer"] {
        let is_github = section == "github-oauth";
        let Some(map) = obj.get_mut(section).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        for (host, token) in map.iter_mut() {
            let Some(real) = token.as_str() else { continue };
            let idx = secrets.len();
            secrets.push(secret(
                idx,
                real.to_string(),
                allowed_hosts(host, is_github),
                token_injection(),
            ));
            *token = serde_json::Value::String(placeholder(idx));
        }
    }

    // http-basic: `{ host: { username, password } }`. Both fields become
    // placeholders: the username is often just an email, but a common pattern
    // (GitHub PAT over HTTPS, Private Packagist, Satis) puts the real token in
    // `username` with a dummy password — treating only `password` as the secret
    // would serialize that token verbatim into the guest env.
    if let Some(map) = obj.get_mut("http-basic").and_then(|v| v.as_object_mut()) {
        for (host, creds) in map.iter_mut() {
            let host = host.clone();
            for field in ["username", "password"] {
                let Some(value) = creds.get_mut(field) else {
                    continue;
                };
                let Some(real) = value.as_str() else {
                    continue;
                };
                let idx = secrets.len();
                secrets.push(secret(
                    idx,
                    real.to_string(),
                    allowed_hosts(&host, false),
                    SecretInjection::default(),
                ));
                *value = serde_json::Value::String(placeholder(idx));
            }
        }
    }

    if secrets.is_empty() {
        return None;
    }
    let json = serde_json::to_string(&value).ok()?;
    Some(ComposerAuth {
        env: ("COMPOSER_AUTH".to_string(), json),
        secrets,
    })
}

/// Hosts the proxy may substitute this credential into. `*.host` matches the
/// host itself and any subdomain (microsandbox wildcard semantics), so a single
/// pattern covers e.g. `composer.fluxui.dev` plus a CDN subdomain. A github
/// token additionally needs the asset host `githubusercontent.com`.
fn allowed_hosts(host: &str, is_github: bool) -> Vec<HostPattern> {
    let mut hosts = vec![HostPattern::Wildcard(format!("*.{host}"))];
    if is_github && host.eq_ignore_ascii_case("github.com") {
        hosts.push(HostPattern::Wildcard("*.githubusercontent.com".to_string()));
    }
    hosts
}

fn token_injection() -> SecretInjection {
    SecretInjection {
        headers: true,
        basic_auth: true,
        query_params: true,
        body: false,
    }
}

fn placeholder(idx: usize) -> String {
    format!("__BOXME_COMPOSER_SECRET_{idx}__")
}

fn secret(
    idx: usize,
    value: String,
    allowed_hosts: Vec<HostPattern>,
    injection: SecretInjection,
) -> SecretEntry {
    SecretEntry {
        env_var: format!("BOXME_COMPOSER_AUTH_{idx}"),
        value: value.into(),
        source: None,
        placeholder: placeholder(idx),
        allowed_hosts,
        injection,
        on_violation: None,
        require_tls_identity: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built(json: &str) -> ComposerAuth {
        build(serde_json::from_str(json).unwrap()).expect("some credentials")
    }

    #[test]
    fn http_basic_username_and_password_both_become_placeholders() {
        let auth = built(
            r#"{"http-basic":{"composer.fluxui.dev":{"username":"me@x.de","password":"hunter2"}}}"#,
        );
        // Neither field lands in COMPOSER_AUTH verbatim.
        assert!(!auth.env.1.contains("hunter2"));
        assert!(!auth.env.1.contains("me@x.de"));
        assert!(auth.env.1.contains("__BOXME_COMPOSER_SECRET_0__"));
        assert!(auth.env.1.contains("__BOXME_COMPOSER_SECRET_1__"));

        assert_eq!(auth.secrets.len(), 2);
        let username = &auth.secrets[0];
        assert_eq!(username.value.as_str(), "me@x.de");
        let password = &auth.secrets[1];
        assert_eq!(password.value.as_str(), "hunter2");
        for s in &auth.secrets {
            assert_eq!(
                s.allowed_hosts,
                vec![HostPattern::Wildcard("*.composer.fluxui.dev".to_string())]
            );
            assert!(s.injection.basic_auth);
            assert!(s.require_tls_identity);
        }
    }

    #[test]
    fn http_basic_token_as_username_never_reaches_the_guest() {
        // PAT-over-HTTPS shape: the real credential rides in `username`.
        let auth = built(
            r#"{"http-basic":{"repo.packagist.com":{"username":"packagist-token-xyz","password":"x-oauth-basic"}}}"#,
        );
        assert!(!auth.env.1.contains("packagist-token-xyz"));
        let s = auth
            .secrets
            .iter()
            .find(|s| s.value.as_str() == "packagist-token-xyz")
            .expect("username registered as a secret");
        assert_eq!(
            s.allowed_hosts,
            vec![HostPattern::Wildcard("*.repo.packagist.com".to_string())]
        );
    }

    #[test]
    fn github_oauth_covers_github_and_asset_host() {
        let auth = built(r#"{"github-oauth":{"github.com":"github_pat_xxx"}}"#);
        assert!(!auth.env.1.contains("github_pat_xxx"));
        let s = &auth.secrets[0];
        assert_eq!(s.value.as_str(), "github_pat_xxx");
        assert_eq!(
            s.allowed_hosts,
            vec![
                HostPattern::Wildcard("*.github.com".to_string()),
                HostPattern::Wildcard("*.githubusercontent.com".to_string()),
            ]
        );
        // A token can travel as a header, basic-auth username, or query param.
        assert!(s.injection.headers && s.injection.basic_auth && s.injection.query_params);
    }

    #[test]
    fn multiple_credentials_get_distinct_placeholders() {
        let auth = built(
            r#"{
                "github-oauth":{"github.com":"tok"},
                "http-basic":{
                    "a.example.com":{"username":"alice@x.de","password":"p1"},
                    "b.example.com":{"username":"bob@x.de","password":"p2"}
                }
            }"#,
        );
        assert_eq!(auth.secrets.len(), 5);
        let mut placeholders: Vec<&str> = auth
            .secrets
            .iter()
            .map(|s| s.placeholder.as_str())
            .collect();
        placeholders.sort();
        placeholders.dedup();
        assert_eq!(placeholders.len(), 5, "placeholders must be unique");
        for s in &auth.secrets {
            assert!(!auth.env.1.contains(s.value.as_str()));
            assert!(auth.env.1.contains(&s.placeholder));
        }
    }

    #[test]
    fn empty_or_credential_free_auth_yields_none() {
        assert!(build(serde_json::from_str("{}").unwrap()).is_none());
        assert!(build(serde_json::from_str(r#"{"http-basic":{}}"#).unwrap()).is_none());
    }
}
