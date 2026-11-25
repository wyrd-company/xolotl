use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::net::TcpStream;

use super::Refusal;

#[derive(Clone)]
pub(super) struct Destination {
    pub host: String,
    pub port: u16,
    ip: Option<IpAddr>,
}

impl Destination {
    pub fn parse(authority: &str) -> Option<Self> {
        let (host, port) = authority.rsplit_once(':')?;
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
        let (host, ip) = if let Some(bracketed) = host.strip_prefix('[') {
            let ip = bracketed.strip_suffix(']')?.parse::<Ipv6Addr>().ok()?;
            (ip.to_string(), Some(ip.into()))
        } else if let Ok(ip) = host.parse::<Ipv4Addr>() {
            (ip.to_string(), Some(ip.into()))
        } else {
            (dns_name(host)?, None)
        };
        Some(Self { host, port, ip })
    }

    pub async fn checked_addresses(&self, dialer: &dyn Dialer) -> Result<Vec<SocketAddr>, Refusal> {
        let addresses = match self.ip {
            Some(ip) => vec![SocketAddr::new(ip, self.port)],
            None => dialer.resolve(&self.host, self.port).await.map_err(|_| Refusal::Resolve)?,
        };
        if addresses.is_empty() {
            return Err(Refusal::Resolve);
        }
        if addresses.iter().any(|address| !is_public(address.ip())) {
            return Err(Refusal::Address);
        }
        Ok(addresses)
    }
}

fn dns_name(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    (host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }))
    .then(|| host.to_ascii_lowercase())
}

/// The IANA special-purpose registries, under the listener's public-address rules.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let segments = ip.segments();
            if let [0x64, 0xff9b, 0, 0, 0, 0, high, low] = segments {
                return is_public_v4(Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)));
            }
            matches!(segments[0], 0x2000..=0x3fff)
                && !matches!(
                    segments,
                    [0x2001, 0..=0x01ff | 0x0db8, ..] // IETF protocol assignments and documentation
                        | [0x2002, ..] // 6to4
                        | [0x2620, 0x4f, 0x8000, ..] // Direct Delegation AS112
                        | [0x3fff, 0..=0x0fff, ..] // Documentation
                )
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    !matches!(
        ip.octets(),
        [0 | 10 | 127 | 224..=255, ..]
            | [100, 64..=127, ..]
            | [169, 254, ..]
            | [172, 16..=31, ..]
            | [192, 0, 0, 0..=8 | 11..=255] // .9 and .10 are globally reachable
            | [192, 0, 2, _]
            | [192, 88, 99, _]
            | [192, 168, ..]
            | [198, 18 | 19, ..]
            | [198, 51, 100, _]
            | [203, 0, 113, _]
    )
}

/// Keep resolution and dialing separate so the same address check runs in tests.
#[async_trait::async_trait]
pub(super) trait Dialer: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>>;
    async fn connect(&self, addresses: &[SocketAddr]) -> io::Result<TcpStream>;
}

pub(super) struct TcpDialer;

#[async_trait::async_trait]
impl Dialer for TcpDialer {
    async fn resolve(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        Ok(tokio::net::lookup_host((host, port)).await?.collect())
    }

    async fn connect(&self, addresses: &[SocketAddr]) -> io::Result<TcpStream> {
        TcpStream::connect(addresses).await
    }
}
