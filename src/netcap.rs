use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use microsandbox::sandbox::exec::{ExecControl, ExecEvent};
use microsandbox::Sandbox;
use tokio::task::JoinHandle;

use crate::scripts::{TCPDUMP_PARSE, TCPDUMP_START};
use crate::util::shell_capture;

/// Registry hosts a composer/npm run is expected to talk to. Suffix match, so
/// repo.packagist.org, codeload.github.com, raw.githubusercontent.com etc. are
/// covered.
const KNOWN_SUFFIXES: &[&str] = &[
    "packagist.org",
    "github.com",
    "githubusercontent.com",
    "npmjs.org",
    "npmjs.com",
    "nodejs.org",
];

/// Domains allowed in `--strict` mode (deny-by-default whitelist).
pub const STRICT_DOMAINS: &[&str] = &[
    "packagist.org",
    "github.com",
    "githubusercontent.com",
    "npmjs.org",
    "nodejs.org",
];

#[derive(Debug, Clone)]
pub struct NetworkContact {
    pub domain: Option<String>,
    pub ip: String,
    pub port: u16,
    pub known: bool,
}

impl NetworkContact {
    /// Identity for dedup and display: the resolved domain if there is one,
    /// otherwise the bare IP.
    pub fn host(&self) -> &str {
        self.domain.as_deref().unwrap_or(&self.ip)
    }
}

/// A running in-guest tcpdump. Kill it before parsing the capture.
pub struct Capture {
    control: ExecControl,
    task: JoinHandle<()>,
}

/// Start tcpdump in the guest and give it a moment to come up. `None` means
/// the spawn failed (stale pre-tcpdump base snapshot) — callers degrade to a
/// "capture unavailable" banner.
pub async fn start(sb: &Sandbox) -> Option<Capture> {
    let mut handle = sb
        .exec_stream_with("bash", |e| e.args(["-lc", TCPDUMP_START]))
        .await
        .ok()?;
    let control = handle.control();
    let exited = Arc::new(AtomicBool::new(false));
    let exited_flag = exited.clone();
    // Drain events in the background; the task ends when the stream closes (i.e.
    // tcpdump exits). The flag lets the startup probe below notice an early
    // death — missing binary, bad filter.
    let task = tokio::spawn(async move {
        while let Some(event) = handle.recv().await {
            if matches!(event, ExecEvent::Exited { .. } | ExecEvent::Failed(_)) {
                exited_flag.store(true, Ordering::SeqCst);
            }
        }
        exited_flag.store(true, Ordering::SeqCst);
    });
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    if exited.load(Ordering::SeqCst) {
        return None;
    }
    Some(Capture { control, task })
}

impl Capture {
    /// SIGTERM tcpdump (flushes the pcap thanks to -U) and wait for the drain
    /// task to finish, bounded so a stuck guest can't hang the run.
    pub async fn stop(self) {
        let _ = self.control.signal(15).await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.task).await;
    }
}

/// Parse the capture in the guest and classify destinations. `Err` means the
/// capture was unreadable — the review shows a banner instead of contacts.
pub async fn contacts(sb: &Sandbox) -> Result<Vec<NetworkContact>> {
    let text = shell_capture(sb, TCPDUMP_PARSE).await?;
    if let Some(path) = std::env::var_os("BOXME_DEBUG_NET") {
        let _ = std::fs::write(path, &text);
    }
    Ok(parse_tcpdump_text(&text))
}

/// Token-scan `tcpdump -r -n` text output: join DNS answers to their query
/// names via the DNS transaction id, then label each outbound SYN destination
/// with the domain that resolved to it. Lenient on purpose — any line that
/// doesn't match is skipped.
fn parse_tcpdump_text(text: &str) -> Vec<NetworkContact> {
    let mut queries: BTreeMap<String, String> = BTreeMap::new(); // txn id -> name
    let mut ip_to_domain: BTreeMap<String, String> = BTreeMap::new();
    let mut syns: BTreeSet<(String, u16)> = BTreeSet::new();

    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();

        // DNS query: "... > 10.0.2.3.53: 12345+ [1au] A? example.com. (33)"
        if let Some(qpos) = tokens
            .iter()
            .position(|t| matches!(*t, "A?" | "AAAA?" | "HTTPS?" | "CNAME?"))
        {
            if let (Some(id), Some(name)) = (txn_id(&tokens), tokens.get(qpos + 1)) {
                queries.insert(id, name.trim_end_matches('.').to_string());
            }
            continue;
        }

        // DNS answer: "... > 10.0.2.15.51234: 12345 2/0/0 CNAME x., A 1.2.3.4 (60)"
        if tokens.iter().any(|t| is_answer_counts(t)) {
            let Some(id) = txn_id(&tokens) else { continue };
            let Some(name) = queries.get(&id).cloned() else {
                continue;
            };
            for window in tokens.windows(2) {
                if matches!(window[0], "A" | "AAAA") {
                    let candidate = window[1].trim_end_matches(',');
                    if candidate.parse::<IpAddr>().is_ok() {
                        ip_to_domain.insert(candidate.to_string(), name.clone());
                    }
                }
            }
            continue;
        }

        // Outbound SYN: "... > 1.2.3.4.443: Flags [S], ..." ([S.] is the
        // server's syn-ack — not an outbound contact).
        if tokens.contains(&"[S],") {
            if let Some(gt) = tokens.iter().position(|t| *t == ">") {
                if let Some(dst) = tokens.get(gt + 1) {
                    if let Some((ip, port)) = split_ip_port(dst.trim_end_matches(':')) {
                        if !is_local(&ip) {
                            syns.insert((ip, port));
                        }
                    }
                }
            }
        }
    }

    syns.into_iter()
        .map(|(ip, port)| {
            let domain = ip_to_domain.get(&ip).cloned();
            let known = domain.as_deref().is_some_and(|d| {
                KNOWN_SUFFIXES
                    .iter()
                    .any(|s| d == *s || d.strip_suffix(*s).is_some_and(|rest| rest.ends_with('.')))
            });
            NetworkContact {
                domain,
                ip,
                port,
                known,
            }
        })
        .collect()
}

/// The DNS transaction id is the first token after the `dst:` token — strip
/// the flag suffixes tcpdump appends ("12345+", "12345*-").
fn txn_id(tokens: &[&str]) -> Option<String> {
    let gt = tokens.iter().position(|t| *t == ">")?;
    let colon = gt + 1 + tokens[gt + 1..].iter().position(|t| t.ends_with(':'))?;
    let raw = tokens.get(colon + 1)?;
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then_some(digits)
}

/// "1/0/0"-shaped token marking a DNS answer line.
fn is_answer_counts(token: &str) -> bool {
    let parts: Vec<&str> = token.split('/').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
}

/// "1.2.3.4.443" / "2607:f8b0::200a.443" -> (ip, port). tcpdump joins ip and
/// port with a dot; the port is the part after the last dot.
fn split_ip_port(s: &str) -> Option<(String, u16)> {
    let (ip, port) = s.rsplit_once('.')?;
    let port: u16 = port.parse().ok()?;
    ip.parse::<IpAddr>().ok()?;
    Some((ip.to_string(), port))
}

/// The guest's own loopback and the slirp gateway range (10.0.2.0/24) aren't
/// real outbound contacts — filter them out of the SYN list.
fn is_local(ip: &str) -> bool {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback() || matches!(v4.octets(), [10, 0, 2, _]),
        Ok(IpAddr::V6(v6)) => v6.is_loopback(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dns_join_and_syn() {
        let text = "\
12:00:00.000000 eth0 Out IP 10.0.2.15.51234 > 10.0.2.3.53: 12345+ A? packagist.org. (31)
12:00:00.010000 eth0 In  IP 10.0.2.3.53 > 10.0.2.15.51234: 12345 1/0/0 A 142.44.161.219 (47)
12:00:00.020000 eth0 Out IP 10.0.2.15.40000 > 142.44.161.219.443: Flags [S], seq 1, win 64240, length 0
12:00:00.030000 eth0 Out IP 10.0.2.15.40001 > 6.6.6.6.443: Flags [S], seq 1, win 64240, length 0
12:00:00.040000 eth0 In  IP 142.44.161.219.443 > 10.0.2.15.40000: Flags [S.], seq 2, ack 2, length 0
";
        let contacts = parse_tcpdump_text(text);
        assert_eq!(contacts.len(), 2);
        let known = contacts.iter().find(|c| c.ip == "142.44.161.219").unwrap();
        assert_eq!(known.domain.as_deref(), Some("packagist.org"));
        assert!(known.known);
        let unknown = contacts.iter().find(|c| c.ip == "6.6.6.6").unwrap();
        assert!(unknown.domain.is_none());
        assert!(!unknown.known);
    }
}
