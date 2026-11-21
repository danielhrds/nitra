//! STUN Binding requests from one socket to several servers at once: each
//! answer says which public address (IP and port) the NAT gave the socket
//! towards that server. Comparing them shows how the NAT maps.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use transport::stun;

const RETRANSMIT: Duration = Duration::from_millis(300);

/// One STUN server, resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub addr: SocketAddr,
}

/// What one server said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub target: Target,
    pub mapped: Option<SocketAddr>,
    pub rtt: Option<Duration>,
}

/// The first address of the wanted family for each server name; names that
/// do not resolve (or have no such address) are left out.
pub fn resolve(names: &[&str], ipv6: bool) -> Vec<Target> {
    names
        .iter()
        .filter_map(|name| {
            let addr = name.to_socket_addrs().ok()?.find(|a| a.is_ipv6() == ipv6)?;
            Some(Target {
                name: (*name).to_owned(),
                addr,
            })
        })
        .collect()
}

/// Asks every target from `socket`, retransmitting, until all answered or
/// `timeout` passed. Answers come back in the targets' order.
pub fn query(socket: &UdpSocket, targets: &[Target], timeout: Duration) -> io::Result<Vec<Answer>> {
    struct Pending {
        answer: Answer,
        transaction: stun::TransactionId,
        first_sent: Option<Instant>,
        last_sent: Option<Instant>,
    }
    let mut pending: Vec<Pending> = targets
        .iter()
        .map(|target| Pending {
            answer: Answer {
                target: target.clone(),
                mapped: None,
                rtt: None,
            },
            transaction: random_transaction(),
            first_sent: None,
            last_sent: None,
        })
        .collect();
    socket.set_read_timeout(Some(Duration::from_millis(20)))?;
    let started = Instant::now();
    let mut buf = [0_u8; 1024];
    while started.elapsed() < timeout {
        let now = Instant::now();
        let mut waiting = false;
        for p in pending.iter_mut().filter(|p| p.answer.mapped.is_none()) {
            waiting = true;
            if p.last_sent
                .is_none_or(|t| now.duration_since(t) >= RETRANSMIT)
            {
                // A send error (e.g. no route to that family) just leaves
                // the target unanswered.
                let _ =
                    socket.send_to(&stun::binding_request(&p.transaction), p.answer.target.addr);
                p.first_sent.get_or_insert(now);
                p.last_sent = Some(now);
            }
        }
        if !waiting {
            break;
        }
        let (n, from) = match socket.recv_from(&mut buf) {
            Ok(received) => received,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            // Windows reports an ICMP "port unreachable" from an earlier
            // send as a reset on the next receive: not fatal.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => continue,
            Err(e) => return Err(e),
        };
        let datagram = &buf[..n];
        let (Some(id), Some(mapped)) =
            (stun::transaction(datagram), stun::mapped_address(datagram))
        else {
            continue;
        };
        if let Some(p) = pending
            .iter_mut()
            .find(|p| p.transaction == id && p.answer.target.addr == from)
        {
            p.answer.mapped = Some(mapped);
            p.answer.rtt = p.first_sent.map(|t| t.elapsed());
        }
    }
    Ok(pending.into_iter().map(|p| p.answer).collect())
}

/// A fresh random transaction id (public STUN has no authentication, so it
/// only needs to be unpredictable enough to match answers to requests).
fn random_transaction() -> stun::TransactionId {
    let a = RandomState::new().hash_one(Instant::now()).to_le_bytes();
    let b = RandomState::new()
        .hash_one(std::process::id())
        .to_le_bytes();
    let mut id = [0_u8; 12];
    id[..8].copy_from_slice(&a);
    id[8..].copy_from_slice(&b[..4]);
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local fake STUN server answering every request with `mapped`.
    fn fake_server(mapped: SocketAddr) -> (Target, std::thread::JoinHandle<()>) {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buf = [0_u8; 512];
            if let Ok((n, from)) = server.recv_from(&mut buf) {
                let id = stun::transaction(&buf[..n]).unwrap();
                server
                    .send_to(&stun::binding_response(&id, mapped), from)
                    .unwrap();
            }
        });
        let target = Target {
            name: format!("fake {addr}"),
            addr,
        };
        (target, handle)
    }

    #[test]
    fn answers_from_several_servers_on_one_socket() {
        let (a, ha) = fake_server("203.0.113.5:6000".parse().unwrap());
        let (b, hb) = fake_server("203.0.113.5:6001".parse().unwrap());
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let answers = query(&socket, &[a.clone(), b.clone()], Duration::from_secs(2)).unwrap();
        ha.join().unwrap();
        hb.join().unwrap();
        assert_eq!(answers[0].target, a);
        assert_eq!(answers[0].mapped, Some("203.0.113.5:6000".parse().unwrap()));
        assert_eq!(answers[1].mapped, Some("203.0.113.5:6001".parse().unwrap()));
        assert!(answers.iter().all(|a| a.rtt.is_some()));
    }

    #[test]
    fn a_silent_server_times_out() {
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = Target {
            name: "silent".into(),
            addr: silent.local_addr().unwrap(),
        };
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let started = Instant::now();
        let answers = query(&socket, &[target], Duration::from_millis(400)).unwrap();
        let took = started.elapsed();
        assert_eq!(answers[0].mapped, None);
        assert!(
            took >= Duration::from_millis(400) && took < Duration::from_secs(1),
            "{took:?}"
        );
    }

    #[test]
    fn transactions_differ() {
        assert_ne!(random_transaction(), random_transaction());
    }
}
