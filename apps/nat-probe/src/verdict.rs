//! What the measurements mean: how the NAT maps (from the STUN answers),
//! whether a carrier-grade NAT is likely (from the router's own external
//! address over UPnP), how IPv6 behaves, and the overall chance of a direct
//! connection. Pure functions, so every case is unit tested.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::Serialize;

use crate::stun_probe::Answer;

/// How the NAT in front of IPv4 maps one local socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mapping {
    /// No server answered: UDP looks blocked.
    NoAnswer,
    /// Answers from a single server address: nothing to compare.
    OneServer,
    /// The public address is one of this PC's own: no NAT.
    NoNat,
    /// The same public address and port towards every server
    /// (endpoint-independent mapping): hole punching usually works.
    SameForAll,
    /// A different public port (or address) per server: a symmetric NAT.
    PerDestination,
}

/// The mapping from one socket's answers; `own` are this PC's addresses.
pub fn mapping(answers: &[Answer], own: &[IpAddr]) -> Mapping {
    let answered: Vec<(&Answer, SocketAddr)> = answers
        .iter()
        .filter_map(|a| Some((a, a.mapped?)))
        .collect();
    let Some((_, first)) = answered.first() else {
        return Mapping::NoAnswer;
    };
    if answered.iter().any(|(_, m)| own.contains(&m.ip())) {
        return Mapping::NoNat;
    }
    let servers: HashSet<IpAddr> = answered.iter().map(|(a, _)| a.target.addr.ip()).collect();
    if servers.len() < 2 {
        return Mapping::OneServer;
    }
    if answered.iter().all(|(_, m)| m == first) {
        Mapping::SameForAll
    } else {
        Mapping::PerDestination
    }
}

/// The router's answer to UPnP `GetExternalIPAddress`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Router {
    /// No UPnP gateway answered on the local network.
    NotFound,
    /// A gateway answered but gave no usable address (UPnP control off,
    /// WAN down, or an error).
    NoAddress(String),
    External(Ipv4Addr),
}

/// Whether a carrier-grade NAT (or another NAT after the router) is likely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cgnat {
    /// The PC itself has the public address.
    PcIsPublic,
    /// The PC's own address is in 100.64.0.0/10 (e.g. a phone hotspot or a
    /// modem in bridge mode behind the carrier's NAT).
    PcInSharedRange,
    /// The router's external address is the public one: no CGNAT.
    RouterIsPublic,
    /// The router got an address in 100.64.0.0/10: CGNAT.
    RouterInSharedRange,
    /// The router got a private address: another router or modem in front
    /// (double NAT), maybe also CGNAT further out.
    RouterPrivate,
    /// The router has a public address that is not the one the servers see:
    /// another NAT after it.
    RouterDiffers,
    /// No UPnP answer (or no address): cannot tell.
    Unknown,
}

impl Cgnat {
    /// Whether this rules a carrier or double NAT in.
    pub fn likely(self) -> bool {
        matches!(
            self,
            Self::PcInSharedRange
                | Self::RouterInSharedRange
                | Self::RouterPrivate
                | Self::RouterDiffers
        )
    }
}

/// `pc`: the PC's own IPv4 towards the internet; `public`: the address the
/// STUN servers saw; `router`: the UPnP answer.
pub fn cgnat(pc: Option<Ipv4Addr>, public: Option<Ipv4Addr>, router: &Router) -> Cgnat {
    if let Some(pc) = pc {
        if Some(pc) == public {
            return Cgnat::PcIsPublic;
        }
        if is_shared(pc) {
            return Cgnat::PcInSharedRange;
        }
    }
    let Router::External(external) = router else {
        return Cgnat::Unknown;
    };
    let external = *external;
    if external.is_unspecified() {
        return Cgnat::Unknown;
    }
    if is_shared(external) {
        Cgnat::RouterInSharedRange
    } else if external.is_private() || external.is_link_local() {
        Cgnat::RouterPrivate
    } else if Some(external) == public {
        Cgnat::RouterIsPublic
    } else if public.is_some() {
        Cgnat::RouterDiffers
    } else {
        Cgnat::Unknown
    }
}

/// 100.64.0.0/10, the range carriers use behind their NAT (RFC 6598).
pub fn is_shared(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// How IPv6 behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ipv6 {
    /// No native (non-tunnel) IPv6 address on any physical adapter.
    NoAddress,
    /// A native address, but no STUN server answered over IPv6.
    NoAnswer,
    /// Answers came only through a tunnel (Teredo, 6to4).
    TunnelOnly,
    /// The servers saw this PC's own address: reachable without NAT (the
    /// firewalls still decide).
    Direct,
    /// The servers saw another address: NAT on IPv6 too (rare).
    Nat,
}

pub fn ipv6(native_address: bool, answers: &[Answer], own: &[IpAddr]) -> Ipv6 {
    let mapped: Vec<IpAddr> = answers
        .iter()
        .filter_map(|a| a.mapped)
        .map(|m| m.ip())
        .collect();
    if mapped.is_empty() {
        return if native_address {
            Ipv6::NoAnswer
        } else {
            Ipv6::NoAddress
        };
    }
    if mapped
        .iter()
        .all(|ip| matches!(ip, IpAddr::V6(v6) if is_tunnel(*v6)))
    {
        return Ipv6::TunnelOnly;
    }
    if mapped.iter().any(|ip| own.contains(ip)) {
        Ipv6::Direct
    } else {
        Ipv6::Nat
    }
}

/// Teredo (2001:0::/32) and 6to4 (2002::/16): IPv6 carried over IPv4.
pub fn is_tunnel(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    (s[0] == 0x2001 && s[1] == 0) || s[0] == 0x2002
}

/// The overall chance of a direct connection with another PC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Chance {
    Likely,
    Hard,
    Unlikely,
    Unknown,
}

/// The chance from the IPv4 mapping, the deciding factor (most friends have
/// no IPv6). IPv6 only rescues a blocked IPv4: the text mentions it in the
/// other cases.
pub fn chance(mapping: Mapping, ipv6: Ipv6) -> Chance {
    match mapping {
        Mapping::NoNat | Mapping::SameForAll => Chance::Likely,
        Mapping::PerDestination => Chance::Hard,
        Mapping::NoAnswer if ipv6 == Ipv6::Direct => Chance::Unknown,
        Mapping::NoAnswer => Chance::Unlikely,
        Mapping::OneServer => Chance::Unknown,
    }
}

/// A rough kind of address, for reports that must not show the address.
pub fn address_class(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => "loopback",
        IpAddr::V4(v4) if is_shared(v4) => "shared (100.64.0.0/10)",
        IpAddr::V4(v4) if v4.is_private() => "private",
        IpAddr::V4(v4) if v4.is_link_local() => "link-local",
        IpAddr::V4(_) => "public",
        IpAddr::V6(v6) if is_tunnel(v6) => "tunnel",
        IpAddr::V6(v6) if v6.segments()[0] & 0xE000 == 0x2000 => "public",
        IpAddr::V6(_) => "local",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stun_probe::Target;

    fn answer(server: &str, mapped: Option<&str>) -> Answer {
        Answer {
            target: Target {
                name: server.into(),
                addr: server.parse().unwrap(),
            },
            mapped: mapped.map(|m| m.parse().unwrap()),
            rtt: None,
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const CLOUDFLARE: &str = "162.159.207.0:3478";
    const GOOGLE: &str = "74.125.250.129:19302";
    const GOOGLE_2: &str = "142.250.82.98:19302";

    #[test]
    fn mappings() {
        let own = [ip("192.168.0.10")];
        assert_eq!(mapping(&[], &own), Mapping::NoAnswer);
        assert_eq!(
            mapping(&[answer(CLOUDFLARE, None)], &own),
            Mapping::NoAnswer
        );
        assert_eq!(
            mapping(
                &[
                    answer(CLOUDFLARE, Some("203.0.113.5:6000")),
                    answer(GOOGLE, Some("203.0.113.5:6000")),
                    answer(GOOGLE_2, Some("203.0.113.5:6000")),
                    answer("1.2.3.4:3478", None),
                ],
                &own
            ),
            Mapping::SameForAll
        );
        assert_eq!(
            mapping(
                &[
                    answer(CLOUDFLARE, Some("203.0.113.5:6000")),
                    answer(GOOGLE, Some("203.0.113.5:6013")),
                ],
                &own
            ),
            Mapping::PerDestination
        );
        assert_eq!(
            mapping(
                &[
                    answer(CLOUDFLARE, Some("203.0.113.5:6000")),
                    answer(GOOGLE, Some("203.0.113.9:6000")),
                ],
                &own
            ),
            Mapping::PerDestination,
            "a different public address per server is symmetric too"
        );
        assert_eq!(
            mapping(&[answer(CLOUDFLARE, Some("203.0.113.5:6000"))], &own),
            Mapping::OneServer
        );
        assert_eq!(
            mapping(
                &[
                    answer(GOOGLE, Some("203.0.113.5:6000")),
                    answer(GOOGLE, Some("203.0.113.5:6000")),
                ],
                &own
            ),
            Mapping::OneServer,
            "two names for one server address compare nothing"
        );
        assert_eq!(
            mapping(
                &[answer(CLOUDFLARE, Some("203.0.113.5:6000"))],
                &[ip("203.0.113.5")]
            ),
            Mapping::NoNat
        );
    }

    #[test]
    fn carrier_grade_nat() {
        let public = Some(Ipv4Addr::new(203, 0, 113, 5));
        let lan = Some(Ipv4Addr::new(192, 168, 0, 10));
        let router = |s: &str| Router::External(s.parse().unwrap());
        assert_eq!(cgnat(public, public, &Router::NotFound), Cgnat::PcIsPublic);
        assert_eq!(
            cgnat(
                Some(Ipv4Addr::new(100, 72, 1, 2)),
                public,
                &Router::NotFound
            ),
            Cgnat::PcInSharedRange
        );
        assert_eq!(
            cgnat(lan, public, &router("203.0.113.5")),
            Cgnat::RouterIsPublic
        );
        assert_eq!(
            cgnat(lan, public, &router("100.64.0.1")),
            Cgnat::RouterInSharedRange
        );
        assert_eq!(
            cgnat(lan, public, &router("100.127.255.254")),
            Cgnat::RouterInSharedRange
        );
        assert_eq!(
            cgnat(lan, public, &router("192.168.1.2")),
            Cgnat::RouterPrivate
        );
        assert_eq!(
            cgnat(lan, public, &router("10.20.30.40")),
            Cgnat::RouterPrivate
        );
        assert_eq!(
            cgnat(lan, public, &router("198.51.100.7")),
            Cgnat::RouterDiffers
        );
        assert_eq!(cgnat(lan, public, &router("0.0.0.0")), Cgnat::Unknown);
        assert_eq!(cgnat(lan, public, &Router::NotFound), Cgnat::Unknown);
        assert_eq!(
            cgnat(lan, public, &Router::NoAddress("401".into())),
            Cgnat::Unknown
        );
        assert_eq!(cgnat(lan, None, &router("198.51.100.7")), Cgnat::Unknown);
        assert!(!is_shared(Ipv4Addr::new(100, 128, 0, 1)));
        assert!(!is_shared(Ipv4Addr::new(100, 63, 255, 255)));
    }

    #[test]
    fn ipv6_cases() {
        let own = [ip("2804:14c:1:2::10")];
        let v6 = "[2606:4700:49::]:3478";
        assert_eq!(ipv6(false, &[], &own), Ipv6::NoAddress);
        assert_eq!(ipv6(true, &[answer(v6, None)], &own), Ipv6::NoAnswer);
        assert_eq!(
            ipv6(true, &[answer(v6, Some("[2804:14c:1:2::10]:5000"))], &own),
            Ipv6::Direct
        );
        assert_eq!(
            ipv6(
                false,
                &[answer(v6, Some("[2001:0:14c9:cd04::1]:5000"))],
                &own
            ),
            Ipv6::TunnelOnly
        );
        assert_eq!(
            ipv6(true, &[answer(v6, Some("[2804:14c:9:9::1]:5000"))], &own),
            Ipv6::Nat
        );
    }

    #[test]
    fn chances() {
        assert_eq!(chance(Mapping::SameForAll, Ipv6::NoAddress), Chance::Likely);
        assert_eq!(chance(Mapping::NoNat, Ipv6::NoAddress), Chance::Likely);
        assert_eq!(chance(Mapping::PerDestination, Ipv6::Direct), Chance::Hard);
        assert_eq!(chance(Mapping::NoAnswer, Ipv6::NoAddress), Chance::Unlikely);
        assert_eq!(chance(Mapping::NoAnswer, Ipv6::Direct), Chance::Unknown);
        assert_eq!(chance(Mapping::OneServer, Ipv6::NoAddress), Chance::Unknown);
    }

    #[test]
    fn address_classes() {
        assert_eq!(address_class(ip("192.168.0.10")), "private");
        assert_eq!(address_class(ip("100.80.1.1")), "shared (100.64.0.0/10)");
        assert_eq!(address_class(ip("203.0.113.5")), "public");
        assert_eq!(address_class(ip("2001:0:14c9::1")), "tunnel");
        assert_eq!(address_class(ip("2804:14c::1")), "public");
        assert_eq!(address_class(ip("fe80::1")), "local");
    }
}
