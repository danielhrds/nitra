//! `bench net`: what ICE would offer from this PC — local addresses by
//! kind, public addresses from STUN servers (with round-trip times) and a
//! hint of the NAT's behaviour. Sends only STUN requests; no media.

use std::error::Error;
use std::net::SocketAddr;

use transport::gather::{self, NatHint, NetworkConfig, Scope};
use transport::net::NetSocket;

pub struct NetCommand {
    pub scope: Scope,
    pub stun_servers: Vec<String>,
}

pub fn run(command: NetCommand) -> Result<(), Box<dyn Error>> {
    let addresses = gather::bind_addresses(command.scope)?;
    println!("local addresses ({}):", addresses.len());
    for a in &addresses {
        println!("  {:<42} {:?} ({})", a.ip.to_string(), a.kind, a.adapter);
    }
    let addrs: Vec<SocketAddr> = addresses.iter().map(|a| SocketAddr::new(a.ip, 0)).collect();
    let mut socket = NetSocket::bind_all(&addrs)?;
    let config = NetworkConfig {
        scope: command.scope,
        stun_servers: command.stun_servers,
    };
    let gathered = gather::gather(&mut socket, &addresses, &config);
    println!(
        "\ncandidates ({}), gathered in {:.0} ms:",
        gathered.candidates.len(),
        gathered.took.as_secs_f64() * 1000.0
    );
    for c in &gathered.candidates {
        let kind = if c.reflexive {
            "public (STUN)"
        } else {
            "local"
        };
        println!(
            "  {:<48} {kind:<14} {:?}, via {}",
            c.addr.to_string(),
            c.interface,
            c.base
        );
    }
    if !gathered.stun.is_empty() {
        println!("\nSTUN answers:");
        for a in &gathered.stun {
            match (a.mapped, a.rtt) {
                (Some(mapped), Some(rtt)) => println!(
                    "  {:<28} from {:<44} -> {mapped} in {:.1} ms",
                    a.server,
                    a.base.to_string(),
                    rtt.as_secs_f64() * 1000.0
                ),
                _ => println!(
                    "  {:<28} from {:<44} -> no answer",
                    a.server,
                    a.base.to_string()
                ),
            }
        }
    }
    let meaning = match gathered.nat {
        NatHint::Unknown => "unknown (no STUN answer, or STUN not used)",
        NatHint::NoNat => "no NAT: the public address is this PC's own",
        NatHint::SameMapping => {
            "both servers saw the same public port: direct P2P usually works (endpoint-independent mapping)"
        }
        NatHint::DifferentMappings => {
            "the servers saw different public ports (symmetric NAT): direct P2P usually fails unless the other side is open"
        }
        NatHint::OneAnswer => "only one server answered: not enough to tell",
    };
    println!("\nNAT hint: {:?} — {meaning}", gathered.nat);
    // Tunnels (Teredo, 6to4) are classified virtual: not real IPv6.
    let ipv6 = gathered
        .candidates
        .iter()
        .any(|c| c.addr.is_ipv6() && c.interface == transport::interfaces::InterfaceKind::Physical);
    println!(
        "native IPv6: {}",
        if ipv6 {
            "yes"
        } else {
            "no (tunnels like Teredo do not count)"
        }
    );
    Ok(())
}
