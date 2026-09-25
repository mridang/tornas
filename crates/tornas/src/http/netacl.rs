//! Source-address access control. By default tornas answers only clients on the
//! loopback, private (RFC1918 and unique-local) and Tailscale ranges, so a box
//! that is accidentally exposed to the internet serves nothing. Players on the
//! LAN and devices on your tailnet need no credentials.

use std::net::{IpAddr, Ipv6Addr};

use anyhow::Context;
use ipnet::IpNet;

/// Loopback, link-local, RFC1918, unique-local, and Tailscale's CGNAT range.
pub const DEFAULT_ALLOW: &str = "127.0.0.0/8,::1/128,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,169.254.0.0/16,fe80::/10,fc00::/7,100.64.0.0/10";

#[derive(Debug, Clone)]
pub struct Acl {
    allow: Vec<IpNet>,
    proxies: Vec<IpNet>,
}

fn parse(list: &[String], what: &str) -> anyhow::Result<Vec<IpNet>> {
    let mut out = Vec::new();
    for entry in list.iter().flat_map(|s| s.split(',')) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Accept bare addresses as well as CIDRs.
        let net: IpNet = match entry.parse() {
            Ok(n) => n,
            Err(_) => entry
                .parse::<IpAddr>()
                .with_context(|| format!("{what}: {entry:?} is not an IP address or CIDR"))?
                .into(),
        };
        out.push(net);
    }
    Ok(out)
}

/// An IPv4 client reaching a dual-stack listener arrives as `::ffff:a.b.c.d`;
/// compare against the IPv4 form so IPv4 ranges match.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

impl Acl {
    pub fn new(allow: &[String], proxies: &[String]) -> anyhow::Result<Self> {
        Ok(Self {
            allow: parse(allow, "allow-from")?,
            proxies: parse(proxies, "trusted-proxy")?,
        })
    }

    /// True when every address is permitted, i.e. the ACL is effectively off.
    pub fn allows_everything(&self) -> bool {
        self.allow.iter().any(|n| n.prefix_len() == 0)
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = unmap(ip);
        self.allow.iter().any(|n| n.contains(&ip))
    }

    fn is_proxy(&self, ip: IpAddr) -> bool {
        let ip = unmap(ip);
        self.proxies.iter().any(|n| n.contains(&ip))
    }

    /// Resolve the real client address. `X-Forwarded-For` is honoured only when the
    /// peer is a configured trusted proxy, walking right to left past further
    /// trusted hops, so a client cannot spoof its way in with a header.
    pub fn client_ip(&self, peer: IpAddr, forwarded_for: Option<&str>) -> IpAddr {
        if !self.is_proxy(peer) {
            return unmap(peer);
        }
        let Some(xff) = forwarded_for else {
            return unmap(peer);
        };
        for hop in xff.rsplit(',') {
            let hop = hop.trim().trim_matches('"');
            // Strip a port if present, and brackets from [v6]:port forms.
            let candidate = hop
                .parse::<IpAddr>()
                .ok()
                .or_else(|| hop.parse::<std::net::SocketAddr>().ok().map(|s| s.ip()))
                .or_else(|| {
                    hop.trim_matches(|c| c == '[' || c == ']')
                        .parse::<Ipv6Addr>()
                        .ok()
                        .map(IpAddr::V6)
                });
            let Some(ip) = candidate else { continue };
            if !self.is_proxy(ip) {
                return unmap(ip);
            }
        }
        unmap(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acl(allow: &str, proxies: &str) -> Acl {
        Acl::new(&[allow.to_owned()], &[proxies.to_owned()]).unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn defaults_allow_lan_loopback_and_tailscale() {
        let a = acl(DEFAULT_ALLOW, "");
        for ok in [
            "127.0.0.1",
            "::1",
            "192.168.1.50",
            "10.4.4.4",
            "172.16.0.9",
            "100.101.102.103",
            "fd7a:115c:a1e0::1",
        ] {
            assert!(a.allows(ip(ok)), "{ok} should be allowed");
        }
        for no in ["8.8.8.8", "1.2.3.4", "2606:4700::1111", "172.32.0.1"] {
            assert!(!a.allows(ip(no)), "{no} should be refused");
        }
    }

    #[test]
    fn ipv4_mapped_clients_match_ipv4_ranges() {
        let a = acl(DEFAULT_ALLOW, "");
        assert!(a.allows(ip("::ffff:192.168.1.50")));
        assert!(!a.allows(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn bare_addresses_and_wildcards() {
        let a = acl("203.0.113.7", "");
        assert!(a.allows(ip("203.0.113.7")));
        assert!(!a.allows(ip("203.0.113.8")));
        assert!(!a.allows_everything());
        let open = acl("0.0.0.0/0,::/0", "");
        assert!(open.allows_everything());
        assert!(open.allows(ip("8.8.8.8")));
    }

    #[test]
    fn forwarded_for_only_trusted_from_a_proxy() {
        let a = acl(DEFAULT_ALLOW, "192.168.1.2");
        // Untrusted peer: the header is ignored, so spoofing fails.
        assert_eq!(
            a.client_ip(ip("8.8.8.8"), Some("192.168.1.9")),
            ip("8.8.8.8")
        );
        // Trusted proxy: the header names the client.
        assert_eq!(
            a.client_ip(ip("192.168.1.2"), Some("8.8.8.8")),
            ip("8.8.8.8")
        );
        // Chained proxies: walk past trusted hops.
        assert_eq!(
            a.client_ip(ip("192.168.1.2"), Some("203.0.113.5, 192.168.1.2")),
            ip("203.0.113.5")
        );
        // Ports and quoting are tolerated.
        assert_eq!(
            a.client_ip(ip("192.168.1.2"), Some("\"203.0.113.5:4711\"")),
            ip("203.0.113.5")
        );
        // No header: fall back to the peer.
        assert_eq!(a.client_ip(ip("192.168.1.2"), None), ip("192.168.1.2"));
    }

    #[test]
    fn rejects_nonsense() {
        assert!(Acl::new(&["not-an-ip".into()], &[]).is_err());
    }
}
