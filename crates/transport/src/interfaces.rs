//! The local IP addresses a peer can be reached at (ICE host candidates),
//! from the network adapters that are up, classified so the ICE priorities
//! prefer real networks: physical (cable, Wi-Fi) first, virtual adapters
//! (Hyper-V, VPNs, WSL) later. Radmin VPN addresses are never offered (the
//! project does not use Radmin).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InterfaceKind {
    /// Cable or Wi-Fi: the preferred paths.
    Physical,
    /// Hyper-V, WSL, VPN and similar adapters.
    Virtual,
    Loopback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAddress {
    pub ip: IpAddr,
    pub kind: InterfaceKind,
    /// The adapter's friendly name (e.g. "Ethernet", "Wi-Fi").
    pub adapter: String,
}

/// Classifies an adapter address; `None` for addresses ICE cannot use, that
/// only reach the local link (IPv4 169.254/16, IPv6 non-global), or that
/// belong to Radmin VPN (by name or its 26.0.0.0/8 range).
pub fn classify(
    description: &str,
    friendly: &str,
    if_type: u32,
    ip: IpAddr,
) -> Option<InterfaceKind> {
    if if_type == IF_TYPE_SOFTWARE_LOOPBACK || ip.is_loopback() {
        return Some(InterfaceKind::Loopback);
    }
    match ip {
        IpAddr::V4(v4) if v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() => {
            return None;
        }
        IpAddr::V6(v6) if !is_global_unicast(v6) => return None,
        _ => {}
    }
    let names = format!("{description} {friendly}").to_lowercase();
    let radmin_range = matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 26);
    if names.contains("radmin") || radmin_range {
        return None;
    }
    // IPv6 over IPv4 tunnels (Teredo 2001:0::/32, 6to4 2002::/16) are not
    // real IPv6 connectivity: rank them with the virtual adapters.
    let tunnel = matches!(ip, IpAddr::V6(v6) if {
        let s = v6.segments();
        (s[0] == 0x2001 && s[1] == 0) || s[0] == 0x2002
    });
    if tunnel || names.contains("teredo") || names.contains("6to4") || names.contains("isatap") {
        return Some(InterfaceKind::Virtual);
    }
    const VIRTUAL: [&str; 12] = [
        "hyper-v",
        "vethernet",
        "virtual",
        "vmware",
        "virtualbox",
        "wsl",
        "tap-",
        "wireguard",
        "tailscale",
        "zerotier",
        "hamachi",
        "vpn",
    ];
    if VIRTUAL.iter().any(|v| names.contains(v)) {
        return Some(InterfaceKind::Virtual);
    }
    Some(InterfaceKind::Physical)
}

/// 2000::/3: the only IPv6 addresses reachable across the internet.
fn is_global_unicast(ip: Ipv6Addr) -> bool {
    ip.segments()[0] & 0xE000 == 0x2000
}

/// The usable addresses of every adapter that is up, best kinds first.
pub fn local_addresses() -> std::io::Result<Vec<LocalAddress>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size = 16 * 1024_u32;
    let mut buffer: Vec<u64>;
    loop {
        // u64 elements keep the buffer aligned for the structures in it.
        buffer = vec![0; (size as usize).div_ceil(8)];
        // SAFETY: `buffer` is writable for `size` bytes and suitably aligned;
        // on overflow the call updates `size` and we retry.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC.0),
                flags,
                None,
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
            )
        };
        match result {
            r if r == NO_ERROR.0 => break,
            r if r == ERROR_BUFFER_OVERFLOW.0 => continue,
            r => return Err(std::io::Error::from_raw_os_error(r as i32)),
        }
    }
    let mut found = Vec::new();
    let mut adapter = buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: the list and every string and address it points to live in
    // `buffer`, which outlives this loop; pointers are null-terminated lists.
    unsafe {
        while let Some(a) = adapter.as_ref() {
            if a.OperStatus == IfOperStatusUp {
                let description = a.Description.to_string().unwrap_or_default();
                let friendly = a.FriendlyName.to_string().unwrap_or_default();
                let mut unicast = a.FirstUnicastAddress;
                while let Some(u) = unicast.as_ref() {
                    if let Some(ip) = socket_ip(u.Address.lpSockaddr.cast_const().cast())
                        && let Some(kind) = classify(&description, &friendly, a.IfType, ip)
                    {
                        found.push(LocalAddress {
                            ip,
                            kind,
                            adapter: friendly.clone(),
                        });
                    }
                    unicast = u.Next;
                }
            }
            adapter = a.Next;
        }
    }
    found.sort_by_key(|a| (a.kind, a.ip.is_ipv4()));
    found.dedup_by(|a, b| a.ip == b.ip);
    Ok(found)
}

/// The IP of a SOCKADDR (IPv4 or IPv6).
///
/// # Safety
/// `sockaddr` is null or points to a SOCKADDR_IN or SOCKADDR_IN6 as its
/// family field says.
unsafe fn socket_ip(sockaddr: *const u16) -> Option<IpAddr> {
    // SAFETY: per the contract, the first field is the address family.
    let family = unsafe { sockaddr.as_ref() }.copied()?;
    if family == AF_INET.0 {
        // SAFETY: the family says this is a SOCKADDR_IN.
        let v4 = unsafe { &*sockaddr.cast::<SOCKADDR_IN>() };
        // SAFETY: reading the union as the 32-bit address it always holds.
        let raw = unsafe { v4.sin_addr.S_un.S_addr };
        Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(raw))))
    } else if family == AF_INET6.0 {
        // SAFETY: the family says this is a SOCKADDR_IN6.
        let v6 = unsafe { &*sockaddr.cast::<SOCKADDR_IN6>() };
        // SAFETY: the union always holds 16 address bytes.
        let bytes = unsafe { v6.sin6_addr.u.Byte };
        Some(IpAddr::V6(Ipv6Addr::from(bytes)))
    } else {
        None
    }
}

/// A socket address on `ip` with an OS-chosen port.
pub fn any_port(ip: IpAddr) -> SocketAddr {
    SocketAddr::new(ip, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn adapters_are_classified() {
        use InterfaceKind::*;
        let c = |d: &str, f: &str, t: u32, a: &str| classify(d, f, t, ip(a));
        assert_eq!(
            c("Realtek PCIe 2.5GbE", "Ethernet", 6, "192.168.0.10"),
            Some(Physical)
        );
        assert_eq!(
            c("Intel Wi-Fi 6E AX211", "Wi-Fi", 71, "10.0.0.5"),
            Some(Physical)
        );
        assert_eq!(
            c(
                "Famatech Radmin VPN Ethernet Adapter",
                "Radmin VPN",
                6,
                "26.1.2.3"
            ),
            None,
            "Radmin is not used"
        );
        assert_eq!(
            c("Some adapter", "Other", 6, "26.9.9.9"),
            None,
            "26/8 is Radmin"
        );
        assert_eq!(
            c(
                "Hyper-V Virtual Ethernet Adapter",
                "vEthernet (WSL)",
                6,
                "172.20.0.1"
            ),
            Some(Virtual)
        );
        assert_eq!(
            c("Software Loopback Interface 1", "Loopback", 24, "127.0.0.1"),
            Some(Loopback)
        );
        assert_eq!(
            c("Realtek", "Ethernet", 6, "2804:14c:1:2::10"),
            Some(Physical)
        );
        assert_eq!(
            c(
                "Teredo Tunneling Pseudo-Interface",
                "Teredo",
                131,
                "2001:0:14c9:cd04::1"
            ),
            Some(Virtual),
            "Teredo is a tunnel, not IPv6 connectivity"
        );
        assert_eq!(
            c("Realtek", "Ethernet", 6, "2002:c000:204::1"),
            Some(Virtual),
            "6to4"
        );
    }

    #[test]
    fn link_local_and_non_global_addresses_are_skipped() {
        assert_eq!(classify("Realtek", "Ethernet", 6, ip("169.254.10.1")), None);
        assert_eq!(classify("Realtek", "Ethernet", 6, ip("fe80::1")), None);
        assert_eq!(
            classify("Realtek", "Ethernet", 6, ip("fd00::1")),
            None,
            "ULA"
        );
    }

    #[test]
    fn this_pc_has_at_least_loopback() {
        let addresses = local_addresses().unwrap();
        assert!(addresses.iter().any(|a| a.kind == InterfaceKind::Loopback));
        // Best kinds first.
        assert!(addresses.windows(2).all(|w| w[0].kind <= w[1].kind));
    }
}
