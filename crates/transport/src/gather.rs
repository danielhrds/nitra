//! ICE candidate gathering: which local addresses to bind (by network
//! scope), their host candidates, and server-reflexive candidates (the
//! public address a NAT gives each socket) from STUN servers, plus a hint
//! of the NAT behaviour. Done once before the offer/answer (no trickle).

use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use str0m::Candidate;
use str0m::CandidateKind;
use str0m::net::Protocol;

use crate::interfaces::{InterfaceKind, LocalAddress, local_addresses};
use crate::net::{NetSocket, Woke};
use crate::stun;

/// Public STUN servers: Cloudflare first, Google as a second opinion (the
/// NAT hint needs two servers).
pub const DEFAULT_STUN_SERVERS: [&str; 2] = ["stun.cloudflare.com:3478", "stun.l.google.com:19302"];
/// How long gathering waits for STUN answers.
pub const STUN_TIMEOUT: Duration = Duration::from_millis(1000);
const STUN_RETRANSMIT: Duration = Duration::from_millis(250);

/// Which addresses a session offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// 127.0.0.1 only (tests, one PC).
    Loopback,
    /// Every adapter that is up (LAN, IPv6, virtual), no STUN.
    Lan,
    /// As `Lan`, plus public addresses from STUN servers.
    Internet,
}

#[derive(Clone, Debug)]
pub struct NetworkConfig {
    pub scope: Scope,
    pub stun_servers: Vec<String>,
}

impl NetworkConfig {
    pub fn loopback() -> Self {
        Self {
            scope: Scope::Loopback,
            stun_servers: Vec::new(),
        }
    }

    pub fn internet() -> Self {
        Self {
            scope: Scope::Internet,
            stun_servers: DEFAULT_STUN_SERVERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// One local candidate to offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCandidate {
    /// The address the peer sends to.
    pub addr: SocketAddr,
    /// The local socket it arrives on.
    pub base: SocketAddr,
    pub reflexive: bool,
    pub interface: InterfaceKind,
}

/// What one STUN server said about one local socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StunAnswer {
    pub server: String,
    pub server_addr: SocketAddr,
    pub base: SocketAddr,
    pub mapped: Option<SocketAddr>,
    pub rtt: Option<Duration>,
}

/// How the NAT in front of the main IPv4 address maps it, from two STUN
/// servers' answers. A hint, not a full NAT test (see `nat-probe`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatHint {
    /// STUN was not tried (scope) or nothing answered.
    Unknown,
    /// The public address is the local one: no NAT.
    NoNat,
    /// Both servers saw the same public address and port: direct P2P
    /// usually works (endpoint-independent mapping).
    SameMapping,
    /// The servers saw different ports: a symmetric NAT, where direct P2P
    /// usually fails unless the other side is open.
    DifferentMappings,
    /// Only one server answered.
    OneAnswer,
}

#[derive(Clone, Debug)]
pub struct Gathered {
    pub candidates: Vec<LocalCandidate>,
    pub stun: Vec<StunAnswer>,
    pub nat: NatHint,
    pub took: Duration,
}

impl Gathered {
    /// The candidates as str0m candidates, with explicit priorities:
    /// physical adapters (cable, Wi-Fi) > public addresses from STUN >
    /// virtual adapters (WSL, VPNs, IPv6 tunnels like Teredo).
    ///
    /// str0m's default local preference ranks every IPv6 host candidate
    /// above IPv4 ones, so a Teredo tunnel beat the Ethernet address even
    /// between two processes on one PC (measured); hence the explicit values.
    pub fn str0m_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for (i, c) in self.candidates.iter().enumerate() {
            if c.reflexive {
                // str0m's own server-reflexive priority (type preference 100).
                match Candidate::server_reflexive(c.addr, c.base, "udp") {
                    Ok(candidate) => out.push(candidate),
                    Err(error) => tracing::debug!(addr = %c.addr, %error, "candidate skipped"),
                }
                continue;
            }
            out.push(Candidate::from_parts(
                format!("h{i}"),
                1,
                Protocol::Udp,
                host_priority(c.interface, i),
                c.addr,
                CandidateKind::Host,
                None,
                None,
                None,
            ));
        }
        out
    }
}

/// RFC 8445 priority: type preference << 24 | local preference << 8 |
/// (256 - component). Type preference by kind of adapter: physical 126
/// (the standard host value) and virtual 90 (below the standard
/// server-reflexive 100), so ICE uses virtual ones only when nothing better
/// works.
/// `index` keeps local preferences unique.
fn host_priority(kind: InterfaceKind, index: usize) -> u32 {
    let type_preference: u32 = match kind {
        InterfaceKind::Physical | InterfaceKind::Loopback => 126,
        InterfaceKind::Virtual => 90,
    };
    let local_preference = 65_535 - (index as u32).min(65_535);
    type_preference << 24 | local_preference << 8 | 255
}

impl Gathered {
    /// Not used by the sessions: keeps `impl` blocks apart from helpers.
    /// What was gathered, without any address (for logs: a public address
    /// identifies the user's connection). E.g. "3 candidates: 1 physical,
    /// 1 virtual, 1 public from STUN, IPv6 yes; NAT SameMapping".
    pub fn summary(&self) -> String {
        let count =
            |f: &dyn Fn(&LocalCandidate) -> bool| self.candidates.iter().filter(|c| f(c)).count();
        let host = |kind| count(&|c| !c.reflexive && c.interface == kind);
        format!(
            "{} candidates: {} physical, {} virtual, {} public from STUN, IPv6 {}; NAT {:?}",
            self.candidates.len(),
            host(InterfaceKind::Physical),
            host(InterfaceKind::Virtual),
            count(&|c| c.reflexive),
            if count(&|c| c.addr.is_ipv6()) > 0 {
                "yes"
            } else {
                "no"
            },
            self.nat
        )
    }

    #[cfg(test)]
    fn priorities(&self) -> Vec<(std::net::IpAddr, u32)> {
        self.str0m_candidates()
            .iter()
            .map(|c| (c.addr().ip(), c.prio()))
            .collect()
    }
}

/// The local addresses to bind for `scope` (port chosen by the OS).
pub fn bind_addresses(scope: Scope) -> std::io::Result<Vec<LocalAddress>> {
    if scope == Scope::Loopback {
        return Ok(vec![LocalAddress {
            ip: "127.0.0.1".parse().expect("valid"),
            kind: InterfaceKind::Loopback,
            adapter: "loopback".into(),
        }]);
    }
    Ok(local_addresses()?
        .into_iter()
        .filter(|a| a.kind != InterfaceKind::Loopback)
        .collect())
}

/// Gathers host candidates for every bound socket and, for `Internet`,
/// server-reflexive ones from the STUN servers. Uses the sockets directly:
/// call it before the session starts.
pub fn gather(
    socket: &mut NetSocket,
    addresses: &[LocalAddress],
    config: &NetworkConfig,
) -> Gathered {
    let started = Instant::now();
    let kind_of = |base: SocketAddr| {
        addresses
            .iter()
            .find(|a| a.ip == base.ip())
            .map_or(InterfaceKind::Physical, |a| a.kind)
    };
    let mut candidates: Vec<LocalCandidate> = socket
        .local_addrs()
        .into_iter()
        .map(|base| LocalCandidate {
            addr: base,
            base,
            reflexive: false,
            interface: kind_of(base),
        })
        .collect();
    let mut answers = Vec::new();
    if config.scope == Scope::Internet && !config.stun_servers.is_empty() {
        // STUN from real networks only: not loopback.
        let bases: Vec<SocketAddr> = candidates
            .iter()
            .filter(|c| {
                matches!(
                    c.interface,
                    InterfaceKind::Physical | InterfaceKind::Virtual
                )
            })
            .map(|c| c.base)
            .collect();
        // Waiting stops once the physical adapters are answered: virtual
        // ones (e.g. WSL's switch) often never get an answer.
        let required: Vec<SocketAddr> = candidates
            .iter()
            .filter(|c| c.interface == InterfaceKind::Physical)
            .map(|c| c.base)
            .collect();
        answers = query_stun(socket, &bases, &required, &config.stun_servers);
        for answer in &answers {
            let Some(mapped) = answer.mapped else {
                continue;
            };
            let known = candidates.iter().any(|c| c.addr == mapped);
            if !known {
                candidates.push(LocalCandidate {
                    addr: mapped,
                    base: answer.base,
                    reflexive: true,
                    interface: kind_of(answer.base),
                });
            }
        }
    }
    let nat = nat_hint(&answers);
    Gathered {
        candidates,
        stun: answers,
        nat,
        took: started.elapsed(),
    }
}

/// Binding requests from every base to every server of its IP family,
/// retransmitted until the `required` bases are all answered (all bases,
/// if none is required) or `STUN_TIMEOUT`.
fn query_stun(
    socket: &mut NetSocket,
    bases: &[SocketAddr],
    required: &[SocketAddr],
    servers: &[String],
) -> Vec<StunAnswer> {
    struct Pending {
        answer: StunAnswer,
        transaction: stun::TransactionId,
        sent: Instant,
        last_sent: Instant,
    }
    let started = Instant::now();
    let mut pending = Vec::new();
    for server in servers {
        let resolved: Vec<SocketAddr> = match server.to_socket_addrs() {
            Ok(addrs) => addrs.collect(),
            Err(error) => {
                tracing::warn!(%server, %error, "STUN server not resolved");
                continue;
            }
        };
        for base in bases {
            // One server address per family is enough.
            let Some(server_addr) = resolved.iter().find(|a| a.is_ipv4() == base.is_ipv4()) else {
                continue;
            };
            pending.push(Pending {
                answer: StunAnswer {
                    server: server.clone(),
                    server_addr: *server_addr,
                    base: *base,
                    mapped: None,
                    rtt: None,
                },
                transaction: random_transaction(pending.len() as u64),
                sent: started,
                last_sent: started - STUN_RETRANSMIT,
            });
        }
    }
    let mut buf = vec![0_u8; 2048];
    loop {
        let now = Instant::now();
        let mut waiting = false;
        for p in pending.iter_mut().filter(|p| p.answer.mapped.is_none()) {
            waiting |= required.is_empty() || required.contains(&p.answer.base);
            if now.duration_since(p.last_sent) >= STUN_RETRANSMIT {
                if p.last_sent < p.sent {
                    p.sent = now;
                }
                p.last_sent = now;
                socket.send_to(
                    &stun::binding_request(&p.transaction),
                    p.answer.base,
                    p.answer.server_addr,
                );
            }
        }
        if !waiting || now.duration_since(started) >= STUN_TIMEOUT {
            break;
        }
        let deadline = (now + Duration::from_millis(20)).min(started + STUN_TIMEOUT);
        let Ok(Woke::Packet { n, .. }) = socket.wait(&mut buf, deadline) else {
            continue;
        };
        let datagram = &buf[..n];
        if let (Some(id), Some(mapped)) =
            (stun::transaction(datagram), stun::mapped_address(datagram))
            && let Some(p) = pending.iter_mut().find(|p| p.transaction == id)
        {
            p.answer.mapped = Some(mapped);
            p.answer.rtt = Some(Instant::now().duration_since(p.sent));
        }
    }
    pending.into_iter().map(|p| p.answer).collect()
}

/// The NAT hint from the answers for the first IPv4 base that has any.
pub fn nat_hint(answers: &[StunAnswer]) -> NatHint {
    let Some(base) = answers
        .iter()
        .find(|a| a.base.is_ipv4() && a.mapped.is_some())
        .map(|a| a.base)
    else {
        return NatHint::Unknown;
    };
    let mapped: Vec<SocketAddr> = answers
        .iter()
        .filter(|a| a.base == base)
        .filter_map(|a| a.mapped)
        .collect();
    match mapped.as_slice() {
        [] => NatHint::Unknown,
        [m, ..] if *m == base => NatHint::NoNat,
        [_] => NatHint::OneAnswer,
        [first, rest @ ..] if rest.iter().all(|m| m == first) => NatHint::SameMapping,
        _ => NatHint::DifferentMappings,
    }
}

/// A transaction id that differs per request and per run (not secret:
/// public STUN has no authentication).
fn random_transaction(salt: u64) -> stun::TransactionId {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut state = now ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(std::process::id());
    let mut id = [0_u8; 12];
    for chunk in id.chunks_mut(4) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(base: &str, mapped: Option<&str>) -> StunAnswer {
        StunAnswer {
            server: "s".into(),
            server_addr: "1.1.1.1:3478".parse().unwrap(),
            base: base.parse().unwrap(),
            mapped: mapped.map(|m| m.parse().unwrap()),
            rtt: None,
        }
    }

    #[test]
    fn nat_hints() {
        let lan = "192.168.0.10:5000";
        assert_eq!(nat_hint(&[]), NatHint::Unknown);
        assert_eq!(
            nat_hint(&[answer(lan, None), answer(lan, None)]),
            NatHint::Unknown
        );
        assert_eq!(
            nat_hint(&[
                answer(lan, Some("203.0.113.5:6000")),
                answer(lan, Some("203.0.113.5:6000"))
            ]),
            NatHint::SameMapping
        );
        assert_eq!(
            nat_hint(&[
                answer(lan, Some("203.0.113.5:6000")),
                answer(lan, Some("203.0.113.5:6001"))
            ]),
            NatHint::DifferentMappings
        );
        assert_eq!(
            nat_hint(&[answer(lan, Some("203.0.113.5:6000")), answer(lan, None)]),
            NatHint::OneAnswer
        );
        assert_eq!(
            nat_hint(&[answer("203.0.113.5:6000", Some("203.0.113.5:6000"))]),
            NatHint::NoNat
        );
    }

    /// The order ICE should try, whatever the IP version: a Teredo tunnel
    /// listed first must still rank below the Ethernet address (str0m's
    /// default ranked it above, and ICE picked it).
    #[test]
    fn physical_beats_public_beats_virtual() {
        let host = |addr: &str, interface| LocalCandidate {
            addr: addr.parse().unwrap(),
            base: addr.parse().unwrap(),
            reflexive: false,
            interface,
        };
        let gathered = Gathered {
            candidates: vec![
                host("[2001:0:14c9:cd04::1]:5003", InterfaceKind::Virtual),
                host("192.168.0.10:5000", InterfaceKind::Physical),
                LocalCandidate {
                    addr: "203.0.113.5:6000".parse().unwrap(),
                    base: "192.168.0.10:5000".parse().unwrap(),
                    reflexive: true,
                    interface: InterfaceKind::Physical,
                },
            ],
            stun: Vec::new(),
            nat: NatHint::Unknown,
            took: Duration::ZERO,
        };
        let prio = |ip: &str| {
            gathered
                .priorities()
                .into_iter()
                .find(|(a, _)| a.to_string() == ip)
                .unwrap()
                .1
        };
        let (ethernet, public) = (prio("192.168.0.10"), prio("203.0.113.5"));
        let teredo = prio("2001:0:14c9:cd04::1");
        assert!(ethernet > public && public > teredo);

        // The log summary names kinds, never an address.
        let summary = gathered.summary();
        assert_eq!(
            summary,
            "3 candidates: 1 physical, 1 virtual, 1 public from STUN, IPv6 yes; NAT Unknown"
        );
        for part in ["192.168", "203.0", "2001:", "5000", "6000"] {
            assert!(!summary.contains(part), "{summary}");
        }
    }

    #[test]
    fn transactions_differ() {
        assert_ne!(random_transaction(1), random_transaction(2));
    }

    /// A local fake STUN server answers with a made-up public address:
    /// gathering turns it into a server-reflexive candidate.
    #[test]
    fn a_stun_answer_becomes_a_reflexive_candidate() {
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let fake_public: SocketAddr = "203.0.113.7:40000".parse().unwrap();
        let answering = std::thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0_u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let id = stun::transaction(&buf[..n]).unwrap();
            server
                .send_to(&stun::binding_response(&id, fake_public), from)
                .unwrap();
        });
        let addresses = bind_addresses(Scope::Loopback).unwrap();
        let mut socket = NetSocket::bind_all(&[SocketAddr::new(addresses[0].ip, 0)]).unwrap();
        // Loopback is not a STUN base by default; ask from it explicitly.
        let base = socket.local_addr();
        let answers = query_stun(&mut socket, &[base], &[base], &[server_addr.to_string()]);
        answering.join().unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].mapped, Some(fake_public));
        assert!(answers[0].rtt.unwrap() < STUN_TIMEOUT);
    }

    #[test]
    fn a_silent_server_times_out() {
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut socket = NetSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let base = socket.local_addr();
        let started = Instant::now();
        let answers = query_stun(
            &mut socket,
            &[base],
            &[base],
            &[silent.local_addr().unwrap().to_string()],
        );
        assert_eq!(answers[0].mapped, None);
        let took = started.elapsed();
        assert!(took >= STUN_TIMEOUT && took < STUN_TIMEOUT * 2, "{took:?}");
    }
}
