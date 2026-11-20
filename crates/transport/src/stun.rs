//! A minimal STUN client (RFC 8489): Binding requests, and the mapped
//! address in the response. Enough to learn the public address a NAT gives
//! a socket (a server-reflexive ICE candidate); str0m does the connectivity
//! checks itself. No authentication: public STUN servers do not use it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const HEADER: usize = 20;
const MAPPED_ADDRESS: u16 = 0x0001;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

pub type TransactionId = [u8; 12];

/// A Binding request with this transaction id (no attributes).
pub fn binding_request(transaction: &TransactionId) -> [u8; HEADER] {
    let mut message = [0_u8; HEADER];
    message[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // Length 0: no attributes.
    message[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    message[8..20].copy_from_slice(transaction);
    message
}

/// Whether `datagram` looks like a STUN message (first two bits zero and
/// the magic cookie), as opposed to DTLS or RTP on the same socket.
pub fn is_stun(datagram: &[u8]) -> bool {
    datagram.len() >= HEADER
        && datagram[0] & 0xC0 == 0
        && datagram[4..8] == MAGIC_COOKIE.to_be_bytes()
}

/// The transaction id of a STUN message.
pub fn transaction(datagram: &[u8]) -> Option<TransactionId> {
    is_stun(datagram).then(|| datagram[8..20].try_into().expect("12 bytes"))
}

/// The mapped address in a Binding success response (XOR-MAPPED-ADDRESS,
/// else MAPPED-ADDRESS), or `None` for anything else or a malformed message.
pub fn mapped_address(datagram: &[u8]) -> Option<SocketAddr> {
    if !is_stun(datagram) || u16::from_be_bytes([datagram[0], datagram[1]]) != BINDING_SUCCESS {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([datagram[2], datagram[3]]));
    let body = datagram.get(HEADER..HEADER + length)?;
    let transaction = &datagram[8..20];
    let mut plain = None;
    let mut offset = 0;
    while offset + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[offset], body[offset + 1]]);
        let size = usize::from(u16::from_be_bytes([body[offset + 2], body[offset + 3]]));
        let value = body.get(offset + 4..offset + 4 + size)?;
        match kind {
            XOR_MAPPED_ADDRESS => return parse_address(value, Some(transaction)),
            MAPPED_ADDRESS => plain = parse_address(value, None),
            _ => {}
        }
        // Attributes are padded to 4 bytes.
        offset += 4 + size.div_ceil(4) * 4;
    }
    plain
}

/// An address attribute; XORed with the cookie (and transaction id for
/// IPv6) when `xor_with` is given.
fn parse_address(value: &[u8], xor_with: Option<&[u8]>) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let mut port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    if xor_with.is_some() {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip = match family {
        0x01 => {
            let mut octets: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            if xor_with.is_some() {
                for (o, c) in octets.iter_mut().zip(cookie) {
                    *o ^= c;
                }
            }
            IpAddr::V4(Ipv4Addr::from(octets))
        }
        0x02 => {
            let mut octets: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            if let Some(transaction) = xor_with {
                let key = cookie.iter().chain(transaction);
                for (o, k) in octets.iter_mut().zip(key) {
                    *o ^= k;
                }
            }
            IpAddr::V6(Ipv6Addr::from(octets))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// A Binding success response for `transaction` carrying `mapped` (for
/// tests and a local fake server).
pub fn binding_response(transaction: &TransactionId, mapped: SocketAddr) -> Vec<u8> {
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let port = mapped.port() ^ (MAGIC_COOKIE >> 16) as u16;
    let mut value = vec![0, 0];
    value.extend_from_slice(&port.to_be_bytes());
    match mapped.ip() {
        IpAddr::V4(ip) => {
            value[1] = 0x01;
            value.extend(ip.octets().iter().zip(cookie).map(|(o, c)| o ^ c));
        }
        IpAddr::V6(ip) => {
            value[1] = 0x02;
            let key = cookie.iter().chain(transaction.iter());
            value.extend(ip.octets().iter().zip(key).map(|(o, k)| o ^ k));
        }
    }
    let mut message = Vec::with_capacity(HEADER + 4 + value.len());
    message.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
    message.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    message.extend_from_slice(&cookie);
    message.extend_from_slice(transaction);
    message.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
    message.extend_from_slice(&(value.len() as u16).to_be_bytes());
    message.extend_from_slice(&value);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5769 section 2.2: sample IPv4 response.
    const IPV4_RESPONSE: [u8; 80] = [
        0x01, 0x01, 0x00, 0x3c, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76,
        0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x08, 0x00, 0x01, 0xa1, 0x47, 0xe1,
        0x12, 0xa6, 0x43, 0x00, 0x08, 0x00, 0x14, 0x2b, 0x91, 0xf5, 0x99, 0xfd, 0x9e, 0x90, 0xc3,
        0x8c, 0x74, 0x89, 0xf9, 0x2a, 0xf9, 0xba, 0x53, 0xf0, 0x6b, 0xe7, 0xd7, 0x80, 0x28, 0x00,
        0x04, 0xc0, 0x7d, 0x4c, 0x96,
    ];

    /// RFC 5769 section 2.3: sample IPv6 response.
    const IPV6_RESPONSE: [u8; 92] = [
        0x01, 0x01, 0x00, 0x48, 0x21, 0x12, 0xa4, 0x42, 0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6,
        0x86, 0xfa, 0x87, 0xdf, 0xae, 0x80, 0x22, 0x00, 0x0b, 0x74, 0x65, 0x73, 0x74, 0x20, 0x76,
        0x65, 0x63, 0x74, 0x6f, 0x72, 0x20, 0x00, 0x20, 0x00, 0x14, 0x00, 0x02, 0xa1, 0x47, 0x01,
        0x13, 0xa9, 0xfa, 0xa5, 0xd3, 0xf1, 0x79, 0xbc, 0x25, 0xf4, 0xb5, 0xbe, 0xd2, 0xb9, 0xd9,
        0x00, 0x08, 0x00, 0x14, 0xa3, 0x82, 0x95, 0x4e, 0x4b, 0xe6, 0x7b, 0xf1, 0x17, 0x84, 0xc9,
        0x7c, 0x82, 0x92, 0xc2, 0x75, 0xbf, 0xe3, 0xed, 0x41, 0x80, 0x28, 0x00, 0x04, 0xc8, 0xfb,
        0x0b, 0x4c,
    ];

    #[test]
    fn rfc_5769_ipv4_response() {
        assert!(is_stun(&IPV4_RESPONSE));
        assert_eq!(
            mapped_address(&IPV4_RESPONSE),
            Some("192.0.2.1:32853".parse().unwrap())
        );
        assert_eq!(
            transaction(&IPV4_RESPONSE),
            Some([
                0xb7, 0xe7, 0xa7, 0x01, 0xbc, 0x34, 0xd6, 0x86, 0xfa, 0x87, 0xdf, 0xae
            ])
        );
    }

    #[test]
    fn rfc_5769_ipv6_response() {
        assert_eq!(
            mapped_address(&IPV6_RESPONSE),
            Some(
                "[2001:db8:1234:5678:11:2233:4455:6677]:32853"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn our_responses_round_trip() {
        let id = [7_u8; 12];
        for mapped in ["203.0.113.9:40000", "[2001:db8::42]:51234"] {
            let mapped: SocketAddr = mapped.parse().unwrap();
            let response = binding_response(&id, mapped);
            assert_eq!(transaction(&response), Some(id));
            assert_eq!(mapped_address(&response), Some(mapped));
        }
    }

    #[test]
    fn requests_have_the_cookie_and_the_id() {
        let id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let request = binding_request(&id);
        assert!(is_stun(&request));
        assert_eq!(transaction(&request), Some(id));
        assert_eq!(mapped_address(&request), None, "a request has no mapping");
    }

    #[test]
    fn malformed_or_foreign_datagrams_are_rejected() {
        // DTLS, RTP, too short, truncated attribute, bad family.
        assert!(!is_stun(&[
            0x16, 0xfe, 0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]));
        assert!(!is_stun(&[0x80; 20]));
        assert!(!is_stun(&IPV4_RESPONSE[..10]));
        let mut truncated = IPV4_RESPONSE;
        truncated[3] = 0xff; // length beyond the datagram
        assert_eq!(mapped_address(&truncated), None);
        let mut bad_family = binding_response(&[0; 12], "1.2.3.4:5".parse().unwrap());
        bad_family[25] = 0x07;
        assert_eq!(mapped_address(&bad_family), None);
        for len in 0..IPV4_RESPONSE.len() {
            let _ = mapped_address(&IPV4_RESPONSE[..len]); // never panics
        }
    }
}
