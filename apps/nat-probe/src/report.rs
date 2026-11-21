//! The findings in words, for the friend who ran the probe (Portuguese:
//! they read it) and as JSON. Neither shows a public IP address unless
//! `show_ip` is set (only for the developer's own PC).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde_json::{Value, json};
use transport::interfaces::{InterfaceKind, LocalAddress};

use crate::stun_probe::Answer;
use crate::verdict::{self, Cgnat, Chance, Ipv6, Mapping, Router};

/// Everything measured.
pub struct Measurements {
    pub adapters: Vec<LocalAddress>,
    /// The PC's own IPv4 towards the internet.
    pub pc_ipv4: Option<Ipv4Addr>,
    /// The local port of the socket that asked the IPv4 servers.
    pub local_port: u16,
    pub ipv4: Vec<Answer>,
    /// The same socket asking one server again after `waited`.
    pub recheck: Option<Answer>,
    pub waited: Duration,
    /// `None`: not asked (the PC has a public address itself).
    pub router: Option<Router>,
    pub ipv6: Vec<Answer>,
    pub took: Duration,
}

/// What the measurements mean.
pub struct Findings {
    pub mapping: Mapping,
    pub port_preserved: Option<bool>,
    pub stable: Option<bool>,
    pub cgnat: Cgnat,
    pub native_ipv6: bool,
    pub ipv6: Ipv6,
    pub chance: Chance,
    pub public_ipv4: Option<Ipv4Addr>,
}

pub fn findings(m: &Measurements) -> Findings {
    let mut own: Vec<IpAddr> = m.adapters.iter().map(|a| a.ip).collect();
    own.extend(m.pc_ipv4.map(IpAddr::V4));
    let mapping = verdict::mapping(&m.ipv4, &own);
    let first = m.ipv4.iter().find(|a| a.mapped.is_some());
    let public_ipv4 = first.and_then(|a| match a.mapped?.ip() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    });
    let port_preserved = first
        .and_then(|a| a.mapped)
        .map(|m2| m2.port() == m.local_port);
    let stable = m.recheck.as_ref().and_then(|again| {
        let before = m.ipv4.iter().find(|a| a.target == again.target)?.mapped?;
        Some(again.mapped? == before)
    });
    let native_ipv6 = m
        .adapters
        .iter()
        .any(|a| a.kind == InterfaceKind::Physical && a.ip.is_ipv6());
    let ipv6 = verdict::ipv6(native_ipv6, &m.ipv6, &own);
    let router = m.router.clone().unwrap_or(Router::NotFound);
    Findings {
        mapping,
        port_preserved,
        stable,
        cgnat: verdict::cgnat(m.pc_ipv4, public_ipv4, &router),
        native_ipv6,
        ipv6,
        chance: verdict::chance(mapping, ipv6),
        public_ipv4,
    }
}

fn chance_text(chance: Chance) -> &'static str {
    match chance {
        Chance::Likely => "PROVÁVEL",
        Chance::Hard => "DIFÍCIL",
        Chance::Unlikely => "IMPROVÁVEL",
        Chance::Unknown => "INCONCLUSIVO",
    }
}

fn reason(f: &Findings) -> &'static str {
    match f.mapping {
        Mapping::NoNat => "o seu PC tem endereço público, sem NAT na frente.",
        Mapping::SameForAll => {
            "o seu NAT usa a mesma porta pública para qualquer destino (NAT bom), então dá para abrir um caminho direto entre os dois PCs."
        }
        Mapping::PerDestination => {
            "o seu NAT troca a porta pública para cada destino (NAT simétrico). A conexão direta só funciona se o outro lado tiver um NAT bom."
        }
        Mapping::NoAnswer => {
            "nenhum servidor STUN respondeu por IPv4: o UDP parece bloqueado nesta rede (firewall, antivírus ou rede de empresa)."
        }
        Mapping::OneServer => {
            "só um servidor respondeu, então não deu para comparar as portas. Rode o teste de novo."
        }
    }
}

fn answered(m: &Measurements) -> usize {
    m.ipv4.iter().filter(|a| a.mapped.is_some()).count()
}

fn mapping_text(m: &Measurements, f: &Findings) -> String {
    let total = m.ipv4.len();
    match f.mapping {
        Mapping::NoAnswer => format!("nenhum de {total} servidores respondeu"),
        Mapping::OneServer => format!("só 1 servidor respondeu (de {total})"),
        Mapping::NoNat => "sem NAT (o PC tem o endereço público)".into(),
        Mapping::SameForAll => format!(
            "NAT bom: a mesma porta pública em {} de {total} servidores",
            answered(m)
        ),
        Mapping::PerDestination => {
            let ports: Vec<String> = m
                .ipv4
                .iter()
                .filter_map(|a| a.mapped)
                .map(|p| p.port().to_string())
                .collect();
            format!(
                "NAT simétrico: uma porta por servidor ({})",
                ports.join(", ")
            )
        }
    }
}

fn cgnat_text(cgnat: Cgnat) -> &'static str {
    match cgnat {
        Cgnat::PcIsPublic => "não (o PC tem o endereço público)",
        Cgnat::PcInSharedRange => {
            "provável (o PC está na faixa 100.64.0.0/10, usada pelas operadoras)"
        }
        Cgnat::RouterIsPublic => "não (o roteador tem o endereço público)",
        Cgnat::RouterInSharedRange => {
            "sim (o roteador recebeu um endereço 100.64.x.x da operadora)"
        }
        Cgnat::RouterPrivate => {
            "NAT duplo (o roteador está atrás de outro roteador ou modem; pode haver CGNAT além dele)"
        }
        Cgnat::RouterDiffers => "provável (existe outro NAT depois do roteador)",
        Cgnat::Unknown => "inconclusivo (o roteador não informou o endereço externo)",
    }
}

fn router_text(router: Option<&Router>) -> String {
    match router {
        None => "não consultado (o PC tem endereço público)".into(),
        Some(Router::NotFound) => {
            "nenhum roteador com UPnP respondeu (UPnP desligado ou ausente)".into()
        }
        Some(Router::NoAddress(_)) => {
            "o roteador respondeu, mas não informou o endereço (UPnP restrito)".into()
        }
        Some(Router::External(ip)) => format!(
            "ligado; endereço externo do roteador: {}",
            class_pt(IpAddr::V4(*ip))
        ),
    }
}

fn upnp_short(router: Option<&Router>) -> &'static str {
    match router {
        None => "não consultado",
        Some(Router::NotFound) => "desligado ou ausente",
        Some(Router::NoAddress(_)) => "restrito",
        Some(Router::External(_)) => "ligado",
    }
}

fn ipv6_text(ipv6: Ipv6) -> &'static str {
    match ipv6 {
        Ipv6::NoAddress => "não disponível",
        Ipv6::NoAnswer => "há endereço IPv6, mas nenhum servidor respondeu por IPv6",
        Ipv6::TunnelOnly => "só por túnel (Teredo ou 6to4), que não conta como IPv6 de verdade",
        Ipv6::Direct => "funcionando, sem NAT",
        Ipv6::Nat => "funcionando, com NAT",
    }
}

fn class_pt(ip: IpAddr) -> &'static str {
    match verdict::address_class(ip) {
        "public" => "público",
        "private" => "privado",
        "shared (100.64.0.0/10)" => "da operadora (100.64.0.0/10)",
        "tunnel" => "túnel",
        "loopback" => "local",
        _ => "local",
    }
}

fn yes_no(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "sim",
        Some(false) => "não",
        None => "não medido",
    }
}

fn adapter_counts(m: &Measurements) -> String {
    let mut names: Vec<(&str, InterfaceKind)> = Vec::new();
    for a in &m.adapters {
        if a.kind != InterfaceKind::Loopback && !names.iter().any(|(n, _)| *n == a.adapter) {
            names.push((&a.adapter, a.kind));
        }
    }
    let physical = names
        .iter()
        .filter(|(_, k)| *k == InterfaceKind::Physical)
        .count();
    let virtual_ = names.len() - physical;
    format!(
        "{physical} {}, {virtual_} {}",
        if physical == 1 { "físico" } else { "físicos" },
        if virtual_ == 1 { "virtual" } else { "virtuais" }
    )
}

/// Extra remarks that change how to read the verdict.
fn notes(m: &Measurements, f: &Findings) -> Vec<String> {
    let mut notes = Vec::new();
    if f.cgnat.likely() && f.chance == Chance::Likely {
        notes.push(
            "Mesmo com outro NAT no caminho, ele dá a mesma porta para todos os destinos: costuma funcionar.".into(),
        );
    }
    if f.chance == Chance::Hard {
        notes.push(
            "Se os dois lados tiverem NAT simétrico, será preciso um servidor de relay.".into(),
        );
    }
    if f.ipv6 == Ipv6::Direct && f.chance != Chance::Likely {
        notes.push(
            "O IPv6 funciona sem NAT: se o outro lado também tiver IPv6, a conexão direta deve funcionar por ele."
                .into(),
        );
    }
    if f.stable == Some(false) {
        notes.push(format!(
            "A porta pública mudou depois de {} s parado. O ziz manda pacotes a cada poucos segundos, então isso costuma não atrapalhar.",
            m.waited.as_secs()
        ));
    }
    notes
}

/// The full text shown on screen.
pub fn details(m: &Measurements, f: &Findings, show_ip: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!("Conexão direta: {}\n", chance_text(f.chance)));
    out.push_str(&format!("  Motivo: {}\n", reason(f)));
    for note in notes(m, f) {
        out.push_str(&format!("  Obs.: {note}\n"));
    }
    out.push_str("\nDetalhes:\n");
    out.push_str(&format!("  IPv4: {}\n", mapping_text(m, f)));
    out.push_str(&format!(
        "  Porta local preservada: {}\n",
        yes_no(f.port_preserved)
    ));
    out.push_str(&format!(
        "  Mesma porta após {} s parado: {}\n",
        m.waited.as_secs(),
        yes_no(f.stable)
    ));
    out.push_str(&format!("  CGNAT: {}\n", cgnat_text(f.cgnat)));
    out.push_str(&format!(
        "  UPnP no roteador: {}\n",
        router_text(m.router.as_ref())
    ));
    out.push_str(&format!("  IPv6: {}\n", ipv6_text(f.ipv6)));
    out.push_str(&format!("  Adaptadores: {}\n", adapter_list(m)));
    for a in &m.ipv4 {
        let rtt = a
            .rtt
            .map_or("sem resposta".into(), |r| format!("{} ms", r.as_millis()));
        out.push_str(&format!("  Servidor {}: {rtt}\n", a.target.name));
    }
    if show_ip {
        if let Some(ip) = f.public_ipv4 {
            out.push_str(&format!("  IP público (IPv4): {ip}\n"));
        }
        if let Some(Router::External(ip)) = &m.router {
            out.push_str(&format!("  Endereço externo do roteador: {ip}\n"));
        }
    }
    out.push_str(&format!("  Tempo do teste: {} s\n", m.took.as_secs()));
    out
}

fn adapter_list(m: &Measurements) -> String {
    let mut seen: Vec<String> = Vec::new();
    for a in m
        .adapters
        .iter()
        .filter(|a| a.kind != InterfaceKind::Loopback)
    {
        let kind = match a.kind {
            InterfaceKind::Physical => "físico",
            _ => "virtual",
        };
        let entry = format!("{} ({kind})", a.adapter);
        if !seen.contains(&entry) {
            seen.push(entry);
        }
    }
    if seen.is_empty() {
        "nenhum".into()
    } else {
        seen.join(", ")
    }
}

/// The short text copied to the clipboard (no addresses, no adapter names).
pub fn summary(m: &Measurements, f: &Findings) -> String {
    let stability = match f.stable {
        Some(true) => format!(", estável {} s", m.waited.as_secs()),
        Some(false) => format!(", mudou após {} s", m.waited.as_secs()),
        None => String::new(),
    };
    let preserved = match f.port_preserved {
        Some(true) => ", porta preservada",
        Some(false) => ", porta trocada",
        None => "",
    };
    format!(
        "ziz nat-probe {}\nConexão direta: {}\nIPv4: {}{preserved}{stability}\nCGNAT: {}\nUPnP: {}\nIPv6: {}\nAdaptadores: {}\n",
        env!("CARGO_PKG_VERSION"),
        chance_text(f.chance).to_lowercase(),
        mapping_text(m, f),
        cgnat_text(f.cgnat),
        upnp_short(m.router.as_ref()),
        ipv6_text(f.ipv6),
        adapter_counts(m),
    )
}

/// The machine-readable result.
pub fn to_json(m: &Measurements, f: &Findings, show_ip: bool) -> Value {
    let ip = |ip: IpAddr| show_ip.then(|| ip.to_string());
    let answers = |answers: &[Answer]| -> Vec<Value> {
        answers
            .iter()
            .map(|a| {
                json!({
                    "server": a.target.name,
                    "answered": a.mapped.is_some(),
                    "mapped_port": a.mapped.map(|p| p.port()),
                    "mapped_class": a.mapped.map(|p| verdict::address_class(p.ip())),
                    "mapped_ip": a.mapped.and_then(|p| ip(p.ip())),
                    "rtt_ms": a.rtt.map(|r| r.as_secs_f64() * 1000.0),
                })
            })
            .collect()
    };
    let router = match &m.router {
        None => json!({ "asked": false }),
        Some(Router::NotFound) => json!({ "asked": true, "found": false }),
        Some(Router::NoAddress(error)) => json!({ "asked": true, "found": true, "error": error }),
        Some(Router::External(external)) => json!({
            "asked": true,
            "found": true,
            "external_class": verdict::address_class(IpAddr::V4(*external)),
            "external_ip": ip(IpAddr::V4(*external)),
        }),
    };
    json!({
        "tool": "ziz nat-probe",
        "version": env!("CARGO_PKG_VERSION"),
        "chance": f.chance,
        "took_secs": m.took.as_secs_f64(),
        "adapters": m.adapters.iter().filter(|a| a.kind != InterfaceKind::Loopback).map(|a| json!({
            "name": a.adapter,
            "kind": format!("{:?}", a.kind).to_lowercase(),
            "class": verdict::address_class(a.ip),
            "ip": ip(a.ip),
        })).collect::<Vec<_>>(),
        "ipv4": {
            "pc_class": m.pc_ipv4.map(|p| verdict::address_class(IpAddr::V4(p))),
            "local_port": m.local_port,
            "answers": answers(&m.ipv4),
            "mapping": f.mapping,
            "port_preserved": f.port_preserved,
            "recheck_after_secs": m.waited.as_secs(),
            "stable": f.stable,
            "public_ip": f.public_ipv4.and_then(|p| ip(IpAddr::V4(p))),
        },
        "cgnat": f.cgnat,
        "upnp": router,
        "ipv6": {
            "native_address": f.native_ipv6,
            "answers": answers(&m.ipv6),
            "result": f.ipv6,
        },
    })
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
            rtt: Some(Duration::from_millis(12)),
        }
    }

    fn measurements() -> Measurements {
        Measurements {
            adapters: vec![LocalAddress {
                ip: "192.168.0.10".parse().unwrap(),
                kind: InterfaceKind::Physical,
                adapter: "Ethernet".into(),
            }],
            pc_ipv4: Some(Ipv4Addr::new(192, 168, 0, 10)),
            local_port: 6000,
            ipv4: vec![
                answer("162.159.207.0:3478", Some("203.0.113.5:6000")),
                answer("74.125.250.129:19302", Some("203.0.113.5:6000")),
            ],
            recheck: Some(answer("162.159.207.0:3478", Some("203.0.113.5:6000"))),
            waited: Duration::from_secs(25),
            router: Some(Router::External(Ipv4Addr::new(203, 0, 113, 5))),
            ipv6: Vec::new(),
            took: Duration::from_secs(31),
        }
    }

    #[test]
    fn a_good_nat_reads_as_likely() {
        let m = measurements();
        let f = findings(&m);
        assert_eq!(f.mapping, Mapping::SameForAll);
        assert_eq!(f.port_preserved, Some(true));
        assert_eq!(f.stable, Some(true));
        assert_eq!(f.cgnat, Cgnat::RouterIsPublic);
        assert_eq!(f.chance, Chance::Likely);
        let text = summary(&m, &f);
        assert!(text.contains("Conexão direta: provável"), "{text}");
        assert!(text.contains("2 de 2 servidores"), "{text}");
    }

    #[test]
    fn a_changed_mapping_is_noted() {
        let mut m = measurements();
        m.recheck = Some(answer("162.159.207.0:3478", Some("203.0.113.5:6100")));
        let f = findings(&m);
        assert_eq!(f.stable, Some(false));
        assert!(details(&m, &f, false).contains("mudou depois de 25 s"));
    }

    /// The texts and the JSON carry no public address unless asked to.
    #[test]
    fn public_addresses_stay_out_by_default() {
        let m = measurements();
        let f = findings(&m);
        let hidden = [
            summary(&m, &f),
            details(&m, &f, false),
            to_json(&m, &f, false).to_string(),
        ];
        for text in &hidden {
            assert!(!text.contains("203.0.113.5"), "{text}");
            assert!(!text.contains("192.168.0.10"), "{text}");
        }
        assert!(details(&m, &f, true).contains("203.0.113.5"));
        assert!(to_json(&m, &f, true).to_string().contains("203.0.113.5"));
    }

    #[test]
    fn a_symmetric_nat_lists_its_ports() {
        let mut m = measurements();
        m.ipv4[1].mapped = Some("203.0.113.5:6013".parse().unwrap());
        let f = findings(&m);
        assert_eq!(f.chance, Chance::Hard);
        assert!(mapping_text(&m, &f).contains("6000, 6013"));
        assert!(details(&m, &f, false).contains("relay"));
    }
}
