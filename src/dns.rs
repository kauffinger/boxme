//! A host-side DNS relay that raises short answer TTLs.
//!
//! microsandbox lets a connection to an allowed host through only while the
//! dialled IP is bound to that name by a DNS answer, and the binding lasts
//! exactly the answer's TTL. Clients cache longer than that — curl (so
//! composer) keeps an address for 60 s, npm for 5 min — and resolvers hand out
//! TTLs that have already counted down, so a long install dials an IP the
//! sandbox has forgotten and the download dies mid-run. Every boot points
//! microsandbox at this relay instead: it forwards each query to the host's
//! resolvers unchanged and raises every answer TTL to at least
//! `TTL_FLOOR_SECS`, outlasting any client cache. That widens nothing — whoever
//! runs an allowed domain's DNS can already pick any TTL they like.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::OnceCell;

const TTL_FLOOR_SECS: u32 = 600;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

static RELAY: OnceCell<Option<SocketAddr>> = OnceCell::const_new();

/// The relay's loopback address, started on first use and kept for the life
/// of the process. `None` when the host has no usable resolver in
/// /etc/resolv.conf: the sandbox then asks the host's resolvers itself.
pub async fn relay() -> Option<SocketAddr> {
    *RELAY.get_or_init(|| async { start().await.ok() }).await
}

async fn start() -> Result<SocketAddr> {
    let resolv_conf =
        std::fs::read_to_string("/etc/resolv.conf").context("reading /etc/resolv.conf")?;
    let upstreams = nameservers(&resolv_conf);
    ensure!(
        !upstreams.is_empty(),
        "no usable nameserver in /etc/resolv.conf"
    );

    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = udp.local_addr()?;
    let tcp = TcpListener::bind(addr).await?;
    tokio::spawn(serve_udp(udp, upstreams.clone()));
    tokio::spawn(serve_tcp(tcp, upstreams));
    Ok(addr)
}

fn nameservers(resolv_conf: &str) -> Vec<SocketAddr> {
    resolv_conf
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .filter_map(|address| address.trim().parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

async fn serve_udp(socket: UdpSocket, upstreams: Vec<SocketAddr>) {
    let socket = Arc::new(socket);
    let mut buf = vec![0; 65535];
    loop {
        let Ok((len, client)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let query = buf[..len].to_vec();
        let (socket, upstreams) = (socket.clone(), upstreams.clone());
        tokio::spawn(async move {
            if let Some(mut response) = ask_udp(&upstreams, &query).await {
                raise_ttls(&mut response);
                let _ = socket.send_to(&response, client).await;
            }
        });
    }
}

async fn ask_udp(upstreams: &[SocketAddr], query: &[u8]) -> Option<Vec<u8>> {
    for upstream in upstreams {
        let local: SocketAddr = match upstream.ip() {
            IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let Ok(socket) = UdpSocket::bind(local).await else {
            continue;
        };
        if socket.connect(upstream).await.is_err() || socket.send(query).await.is_err() {
            continue;
        }
        let mut buf = vec![0; 65535];
        let Ok(Ok(len)) = tokio::time::timeout(UPSTREAM_TIMEOUT, socket.recv(&mut buf)).await
        else {
            continue;
        };
        buf.truncate(len);
        if buf.get(..2) == query.get(..2) {
            return Some(buf);
        }
    }
    None
}

async fn serve_tcp(listener: TcpListener, upstreams: Vec<SocketAddr>) {
    loop {
        let Ok((client, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(relay_tcp(client, upstreams.clone()));
    }
}

async fn relay_tcp(mut client: TcpStream, upstreams: Vec<SocketAddr>) {
    while let Some(query) = read_frame(&mut client).await {
        let Some(mut response) = ask_tcp(&upstreams, &query).await else {
            return;
        };
        raise_ttls(&mut response);
        if write_frame(&mut client, &response).await.is_err() {
            return;
        }
    }
}

async fn ask_tcp(upstreams: &[SocketAddr], query: &[u8]) -> Option<Vec<u8>> {
    for upstream in upstreams {
        let attempt = async {
            let mut stream = TcpStream::connect(upstream).await.ok()?;
            write_frame(&mut stream, query).await.ok()?;
            read_frame(&mut stream).await
        };
        if let Ok(Some(response)) = tokio::time::timeout(UPSTREAM_TIMEOUT, attempt).await {
            return Some(response);
        }
    }
    None
}

async fn read_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let len = stream.read_u16().await.ok()?;
    let mut message = vec![0; usize::from(len)];
    stream.read_exact(&mut message).await.ok()?;
    Some(message)
}

async fn write_frame(stream: &mut TcpStream, message: &[u8]) -> std::io::Result<()> {
    let len = u16::try_from(message.len()).map_err(std::io::Error::other)?;
    stream.write_u16(len).await?;
    stream.write_all(message).await
}

/// Raise every answer record's TTL to `TTL_FLOOR_SECS`. A message that
/// doesn't parse is left exactly as it arrived.
fn raise_ttls(message: &mut [u8]) {
    let Some(ttl_offsets) = answer_ttl_offsets(message) else {
        return;
    };
    for offset in ttl_offsets {
        let ttl = &mut message[offset..offset + 4];
        if u32::from_be_bytes([ttl[0], ttl[1], ttl[2], ttl[3]]) < TTL_FLOOR_SECS {
            ttl.copy_from_slice(&TTL_FLOOR_SECS.to_be_bytes());
        }
    }
}

fn answer_ttl_offsets(message: &[u8]) -> Option<Vec<usize>> {
    let questions = read_u16(message, 4)?;
    let answers = read_u16(message, 6)?;
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(message, offset)? + 4;
    }
    let mut ttl_offsets = Vec::new();
    for _ in 0..answers {
        let ttl_offset = skip_name(message, offset)? + 4;
        message.get(ttl_offset..ttl_offset + 4)?;
        let data_len = read_u16(message, ttl_offset + 4)?;
        offset = ttl_offset + 6 + usize::from(data_len);
        message.get(..offset)?;
        ttl_offsets.push(ttl_offset);
    }
    Some(ttl_offsets)
}

fn skip_name(message: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let len = *message.get(offset)?;
        match len & 0xC0 {
            0xC0 => return message.get(offset + 1).map(|_| offset + 2),
            0x00 if len == 0 => return Some(offset + 1),
            0x00 => offset += 1 + usize::from(len),
            _ => return None,
        }
    }
}

fn read_u16(message: &[u8], offset: usize) -> Option<u16> {
    let bytes = message.get(offset..offset + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response for `api.github.com`: a CNAME (TTL 30) to `lb.github.com`
    /// whose A record (TTL 18) is a compression pointer back into the CNAME.
    fn github_response(cname_ttl: u32, a_ttl: u32) -> Vec<u8> {
        let mut message = vec![0x12, 0x34, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
        message.extend(b"\x03api\x06github\x03com\x00");
        message.extend([0, 1, 0, 1]);
        message.extend([0xC0, 12, 0, 5, 0, 1]);
        message.extend(cname_ttl.to_be_bytes());
        message.extend([0, 5]);
        message.extend(b"\x02lb\xC0\x10");
        message.extend([0xC0, 44, 0, 1, 0, 1]);
        message.extend(a_ttl.to_be_bytes());
        message.extend([0, 4, 140, 82, 121, 6]);
        message
    }

    fn ttls(message: &[u8]) -> Vec<u32> {
        answer_ttl_offsets(message)
            .unwrap()
            .into_iter()
            .map(|offset| u32::from_be_bytes(message[offset..offset + 4].try_into().unwrap()))
            .collect()
    }

    #[test]
    fn short_answer_ttls_are_raised_to_the_floor() {
        let mut message = github_response(30, 18);
        assert_eq!(ttls(&message), vec![30, 18]);

        raise_ttls(&mut message);

        assert_eq!(ttls(&message), vec![TTL_FLOOR_SECS, TTL_FLOOR_SECS]);
        assert_eq!(&message[message.len() - 4..], [140, 82, 121, 6]);
    }

    #[test]
    fn ttls_above_the_floor_are_kept() {
        let mut message = github_response(3600, 18);
        raise_ttls(&mut message);
        assert_eq!(ttls(&message), vec![3600, TTL_FLOOR_SECS]);
    }

    #[test]
    fn a_message_that_does_not_parse_is_left_alone() {
        let full = github_response(30, 18);
        for len in 0..full.len() {
            let mut truncated = full[..len].to_vec();
            raise_ttls(&mut truncated);
            assert_eq!(truncated, full[..len]);
        }
    }

    #[test]
    fn nameservers_come_from_resolv_conf() {
        let conf = "# generated\nsearch fritz.box\nnameserver 100.100.100.100\n\
                    nameserver fd7a:115c:a1e0::53\nnameserver fe80::1%en0\n";
        assert_eq!(
            nameservers(conf),
            vec![
                "100.100.100.100:53".parse().unwrap(),
                "[fd7a:115c:a1e0::53]:53".parse().unwrap(),
            ]
        );
    }
}
