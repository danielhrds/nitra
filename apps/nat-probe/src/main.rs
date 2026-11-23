//! nat-probe: a one-minute test a friend runs (double click) to see whether
//! a direct P2P connection can work from their PC. It asks public STUN
//! servers which public address and port they see, compares them, asks the
//! router over UPnP (read only) for its external address to spot a carrier
//! NAT, checks IPv6, and prints a verdict in Portuguese with a short summary
//! copied to the clipboard and a JSON file next to the program.

mod clipboard;
mod report;
mod stun_probe;
mod upnp;
mod verdict;

use std::io::{BufRead, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;

use report::Measurements;
use stun_probe::Target;

/// IPv4 STUN servers on different addresses (Cloudflare and Google).
const IPV4_SERVERS: [&str; 4] = [
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
];
const IPV6_SERVERS: [&str; 2] = ["stun.cloudflare.com:3478", "stun.l.google.com:19302"];
const STUN_TIMEOUT: Duration = Duration::from_millis(2500);
const RESULT_FILE: &str = "nat-probe-resultado.json";

#[derive(Parser)]
#[command(
    version,
    about = "Tests whether direct P2P connections can work from this PC."
)]
struct Args {
    /// Skip the consent question and the final "press Enter".
    #[arg(long)]
    yes: bool,
    /// Show and save the public IP addresses (for your own PC only).
    #[arg(long)]
    show_ip: bool,
    /// Seconds to wait before asking again, to see if the mapping holds.
    #[arg(long, default_value_t = 25)]
    wait: u64,
    /// Where to save the JSON result (default: next to the program).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Do not copy the summary to the clipboard.
    #[arg(long)]
    no_clipboard: bool,
}

fn main() {
    let args = Args::parse();
    let interactive = !args.yes && std::io::stdin().is_terminal();
    println!(
        "nitra - teste de conexão (nat-probe {})\n",
        env!("CARGO_PKG_VERSION")
    );
    if !args.yes && !consent() {
        println!("Teste cancelado. Nada foi enviado.");
        pause(interactive);
        return;
    }
    let measurements = run(Duration::from_secs(args.wait));
    let findings = report::findings(&measurements);
    println!("\n== Resultado ==\n");
    println!(
        "{}",
        report::details(&measurements, &findings, args.show_ip)
    );

    let summary = report::summary(&measurements, &findings);
    if !args.no_clipboard {
        match clipboard::set_text(&summary) {
            Ok(()) => println!("O resumo foi copiado. Cole na conversa com quem pediu o teste."),
            Err(e) => println!("Não deu para copiar o resumo ({e}). Ele está aqui:\n\n{summary}"),
        }
    }
    let path = args.out.unwrap_or_else(default_result_path);
    let json = report::to_json(&measurements, &findings, args.show_ip);
    let text = serde_json::to_string_pretty(&json).expect("JSON values serialize");
    match std::fs::write(&path, text + "\n") {
        Ok(()) => println!("Resultado completo salvo em: {}", path.display()),
        Err(e) => println!("Não deu para salvar {} ({e}).", path.display()),
    }
    pause(interactive);
}

fn consent() -> bool {
    println!(
        "Este teste verifica se o seu PC consegue se conectar direto a outro PC
pelo nitra, sem um servidor no meio. Ele vai:
  - enviar pacotes STUN para servidores públicos da Cloudflare e do Google,
    que respondem com o endereço público que veem;
  - perguntar ao seu roteador (UPnP), só para ler, qual endereço externo ele
    tem. Nada é alterado no roteador;
  - levar cerca de 30 segundos.
Nenhum dado pessoal é enviado. O resultado não mostra o seu IP público.
Se o Windows perguntar sobre o firewall, pode negar: o teste funciona mesmo assim.
"
    );
    print!("Continuar? [s/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(
        line.trim().to_lowercase().as_str(),
        "s" | "sim" | "y" | "yes"
    )
}

fn pause(interactive: bool) {
    if interactive {
        println!("\nPressione Enter para fechar.");
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
}

fn step(text: &str) {
    println!("{text}");
    let _ = std::io::stdout().flush();
}

/// All measurements; failures become "no answer" rather than errors, so
/// the verdict can still say what was seen.
fn run(wait: Duration) -> Measurements {
    let started = Instant::now();
    step("[1/4] Adaptadores de rede...");
    let adapters = transport::interfaces::local_addresses().unwrap_or_default();
    let ipv4_targets = stun_probe::resolve(&IPV4_SERVERS, false);
    let ipv6_targets = stun_probe::resolve(&IPV6_SERVERS, true);
    let pc_ipv4 = ipv4_targets
        .first()
        .and_then(|t| source_ip(t.addr))
        .and_then(|ip| match ip {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        });

    step(&format!(
        "[2/4] Tipo de NAT ({} servidores STUN)...",
        ipv4_targets.len()
    ));
    let socket = UdpSocket::bind("0.0.0.0:0").ok();
    let local_port = socket
        .as_ref()
        .and_then(|s| s.local_addr().ok())
        .map_or(0, |a| a.port());
    let ipv4 = ask(socket.as_ref(), &ipv4_targets);
    let first_asked = Instant::now();

    step("[3/4] Roteador (UPnP) e IPv6...");
    let router = match pc_ipv4 {
        Some(pc) if pc.is_private() || verdict::is_shared(pc) => Some(upnp::query(pc)),
        Some(_) => None,
        None => Some(verdict::Router::NotFound),
    };
    let ipv6 = if ipv6_targets.is_empty() {
        Vec::new()
    } else {
        let socket6 = UdpSocket::bind("[::]:0").ok();
        ask(socket6.as_ref(), &ipv6_targets)
    };

    // Ask the first server that answered again from the same socket, after
    // the socket stayed quiet for `wait`.
    let again: Option<Target> = ipv4
        .iter()
        .find(|a| a.mapped.is_some())
        .map(|a| a.target.clone());
    let recheck = again.and_then(|target| {
        let left = wait.saturating_sub(first_asked.elapsed());
        step(&format!(
            "[4/4] Aguardando {} s para ver se a porta pública se mantém...",
            left.as_secs()
        ));
        std::thread::sleep(left);
        ask(socket.as_ref(), &[target]).pop()
    });
    Measurements {
        adapters,
        pc_ipv4,
        local_port,
        ipv4,
        recheck,
        waited: wait,
        router,
        ipv6,
        took: started.elapsed(),
    }
}

fn ask(socket: Option<&UdpSocket>, targets: &[Target]) -> Vec<stun_probe::Answer> {
    let unanswered = || {
        targets
            .iter()
            .map(|t| stun_probe::Answer {
                target: t.clone(),
                mapped: None,
                rtt: None,
            })
            .collect()
    };
    match socket {
        Some(socket) => {
            stun_probe::query(socket, targets, STUN_TIMEOUT).unwrap_or_else(|_| unanswered())
        }
        None => unanswered(),
    }
}

/// The local address the OS would send from towards `target` (a connected
/// UDP socket sends nothing until asked to).
fn source_ip(target: SocketAddr) -> Option<IpAddr> {
    let any: SocketAddr = if target.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        "[::]:0".parse().ok()?
    };
    let socket = UdpSocket::bind(any).ok()?;
    socket.connect(target).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn default_result_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(RESULT_FILE)))
        .unwrap_or_else(|| PathBuf::from(RESULT_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to the real STUN servers and the router.
    #[test]
    #[ignore = "uses the network: public STUN servers and the local router"]
    fn the_real_network_answers() {
        let m = run(Duration::ZERO);
        assert!(
            m.ipv4.iter().any(|a| a.mapped.is_some()),
            "no IPv4 STUN answer"
        );
        let f = report::findings(&m);
        assert_ne!(f.mapping, verdict::Mapping::NoAnswer);
    }
}
