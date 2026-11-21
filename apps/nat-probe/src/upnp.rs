//! A read-only UPnP question to the router: "which external address do you
//! have?" (`GetExternalIPAddress`). Comparing it with the address the STUN
//! servers see tells whether another NAT sits after the router (CGNAT or a
//! second router). Nothing on the router is changed: no port mappings.
//!
//! Steps: an SSDP search on the local network finds the gateway's
//! description URL; its XML names the WAN connection service and control
//! URL; a SOAP request to that URL returns the address. Only HTTP to
//! private IPv4 addresses is followed, so a stray SSDP answer cannot send
//! the probe anywhere on the internet.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crate::verdict::Router;

const SSDP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
const SEARCH_TIME: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest HTTP response read (descriptions are a few KB).
const MAX_RESPONSE: u64 = 256 * 1024;
const SEARCH_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];

/// Asks the gateways found from `local` (the PC's LAN address).
pub fn query(local: Ipv4Addr) -> Router {
    let locations = match discover(local) {
        Ok(locations) => locations,
        Err(e) => return Router::NoAddress(format!("SSDP: {e}")),
    };
    if locations.is_empty() {
        return Router::NotFound;
    }
    let mut last_error = String::new();
    for location in &locations {
        match external_address(location) {
            Ok(address) => return Router::External(address),
            Err(e) => last_error = e,
        }
    }
    Router::NoAddress(last_error)
}

/// The description URLs of the gateways answering an SSDP search.
fn discover(local: Ipv4Addr) -> io::Result<Vec<Url>> {
    let socket = UdpSocket::bind(SocketAddrV4::new(local, 0))?;
    socket.set_multicast_ttl_v4(2)?;
    for target in SEARCH_TARGETS {
        let request = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {target}\r\n\r\n"
        );
        socket.send_to(request.as_bytes(), SSDP)?;
    }
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let started = Instant::now();
    let mut found: Vec<Url> = Vec::new();
    let mut buf = [0_u8; 2048];
    while started.elapsed() < SEARCH_TIME {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(url) = ssdp_location(&text).and_then(|l| Url::parse(&l))
                    && !found.contains(&url)
                {
                    found.push(url);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

/// The LOCATION header of an SSDP answer.
fn ssdp_location(response: &str) -> Option<String> {
    let mut lines = response.lines();
    if !lines.next()?.starts_with("HTTP/1.") {
        return None;
    }
    lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("location")
            .then(|| value.trim().to_owned())
    })
}

/// The router's external address, from its description URL.
fn external_address(location: &Url) -> Result<Ipv4Addr, String> {
    let description = http(location, "GET", &[], "").map_err(|e| format!("description: {e}"))?;
    let (service, control) =
        wan_service(&description).ok_or("no WAN connection service in the description")?;
    let base = xml_text(&description, "URLBase")
        .and_then(|b| Url::parse(&b))
        .unwrap_or_else(|| location.clone());
    let control = base
        .join(&control)
        .ok_or("control URL not on the local network")?;
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:GetExternalIPAddress xmlns:u=\"{service}\"></u:GetExternalIPAddress></s:Body></s:Envelope>\r\n"
    );
    let action = format!("\"{service}#GetExternalIPAddress\"");
    let headers = [
        ("Content-Type", "text/xml; charset=\"utf-8\""),
        ("SOAPAction", action.as_str()),
    ];
    let response = http(&control, "POST", &headers, &body).map_err(|e| format!("SOAP: {e}"))?;
    let text = xml_text(&response, "NewExternalIPAddress").ok_or("no address in the answer")?;
    if text.is_empty() {
        return Ok(Ipv4Addr::UNSPECIFIED);
    }
    text.parse()
        .map_err(|_| "the answer is not an IPv4 address".to_owned())
}

/// The first WAN connection service: (service type, control URL).
fn wan_service(description: &str) -> Option<(String, String)> {
    let mut rest = description;
    while let Some(start) = find_tag(rest, "service") {
        let block = &rest[start..];
        let end = block
            .find("</service>")
            .or_else(|| block.find(":service>"))?;
        let service = &block[..end];
        if let (Some(kind), Some(control)) = (
            xml_text(service, "serviceType"),
            xml_text(service, "controlURL"),
        ) && (kind.contains(":WANIPConnection:") || kind.contains(":WANPPPConnection:"))
        {
            return Some((kind, control));
        }
        rest = &block[end + 1..];
    }
    None
}

/// The trimmed text of the first `<tag>` element (namespace prefixes
/// allowed, attributes allowed).
fn xml_text(xml: &str, tag: &str) -> Option<String> {
    let start = find_tag(xml, tag)?;
    let after = &xml[start..];
    let content_start = after.find('>')? + 1;
    let content = &after[content_start..];
    let end = content.find("</")?;
    Some(unescape(content[..end].trim()))
}

/// The byte offset of the first opening `<tag` or `<prefix:tag`.
fn find_tag(xml: &str, tag: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(i) = xml[offset..].find('<') {
        let at = offset + i;
        let name_start = at + 1;
        let name_end = xml[name_start..]
            .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
            .map_or(xml.len(), |e| name_start + e);
        let name = &xml[name_start..name_end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local == tag && !name.starts_with('/') {
            return Some(at);
        }
        offset = name_start;
    }
    None
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// An `http://` URL whose host is a private IPv4 address.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    host: Ipv4Addr,
    port: u16,
    path: String,
}

impl Url {
    fn parse(text: &str) -> Option<Self> {
        let rest = text.trim().strip_prefix("http://")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.split_once(':') {
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, 80),
        };
        let host: Ipv4Addr = host.parse().ok()?;
        if !(host.is_private() || host.is_link_local()) {
            return None;
        }
        Some(Self {
            host,
            port,
            path: path.to_owned(),
        })
    }

    /// `reference` relative to this URL (absolute URLs must stay local).
    fn join(&self, reference: &str) -> Option<Self> {
        let reference = reference.trim();
        if reference.starts_with("http://") {
            return Self::parse(reference);
        }
        let path = if reference.starts_with('/') {
            reference.to_owned()
        } else {
            let dir = &self.path[..self.path.rfind('/').map_or(0, |i| i + 1)];
            format!("{}{reference}", if dir.is_empty() { "/" } else { dir })
        };
        Some(Self {
            path,
            ..self.clone()
        })
    }
}

/// One HTTP/1.0 request (1.0 so the router answers without chunks, closing
/// the connection at the end); returns the body of a 200 answer.
fn http(url: &Url, method: &str, headers: &[(&str, &str)], body: &str) -> io::Result<String> {
    let address = SocketAddr::V4(SocketAddrV4::new(url.host, url.port));
    let mut stream = TcpStream::connect_timeout(&address, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let mut request = format!(
        "{method} {} HTTP/1.0\r\nHost: {}:{}\r\nConnection: close\r\n",
        url.path, url.host, url.port
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes())?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response)?;
    parse_response(&String::from_utf8_lossy(&response))
}

/// The body of a 200 answer (de-chunked if the router chunked it anyway).
fn parse_response(response: &str) -> io::Result<String> {
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::other("incomplete HTTP answer"))?;
    let status = head.lines().next().unwrap_or_default();
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(io::Error::other(format!("HTTP answer \"{status}\"")));
    }
    let chunked = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(n, v)| {
            n.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    Ok(if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    })
}

fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size_line, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        if size == 0 || rest.len() < size {
            break;
        }
        out.push_str(&rest[..size]);
        body = rest[size..].strip_prefix("\r\n").unwrap_or(&rest[size..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description shaped like a common router's (trimmed).
    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
 <specVersion><major>1</major><minor>0</minor></specVersion>
 <device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <serviceList>
   <service>
    <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
    <controlURL>/upnp/control/L3F</controlURL>
   </service>
  </serviceList>
  <deviceList><device><deviceList><device>
   <serviceList>
    <service>
     <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
     <serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId>
     <controlURL>/upnp/control/WANIPConn1</controlURL>
    </service>
   </serviceList>
  </device></deviceList></device></deviceList>
 </device>
</root>"#;

    const SOAP_ANSWER: &str = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:GetExternalIPAddressResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1">
<NewExternalIPAddress>100.70.12.34</NewExternalIPAddress>
</u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;

    #[test]
    fn ssdp_answers() {
        let answer = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nlocation: http://192.168.0.1:1900/igd.xml\r\nSERVER: Linux UPnP/1.0\r\n\r\n";
        assert_eq!(
            ssdp_location(answer).as_deref(),
            Some("http://192.168.0.1:1900/igd.xml")
        );
        assert_eq!(
            ssdp_location("NOTIFY * HTTP/1.1\r\nLOCATION: http://x/\r\n\r\n"),
            None
        );
        assert_eq!(ssdp_location("HTTP/1.1 200 OK\r\n\r\n"), None);
        assert_eq!(ssdp_location(""), None);
    }

    #[test]
    fn only_local_http_urls_are_followed() {
        let url = Url::parse("http://192.168.0.1:1900/igd.xml").unwrap();
        assert_eq!(
            (url.host, url.port, url.path.as_str()),
            (Ipv4Addr::new(192, 168, 0, 1), 1900, "/igd.xml")
        );
        assert_eq!(Url::parse("http://10.0.0.1").unwrap().path, "/");
        assert_eq!(Url::parse("http://8.8.8.8/x.xml"), None, "public host");
        assert_eq!(Url::parse("https://192.168.0.1/x.xml"), None);
        assert_eq!(
            Url::parse("http://router.local/x.xml"),
            None,
            "names are not resolved"
        );
        assert_eq!(Url::parse("http://192.168.0.1:notaport/"), None);
    }

    #[test]
    fn relative_urls_join() {
        let base = Url::parse("http://192.168.0.1:5000/rootDesc.xml").unwrap();
        assert_eq!(base.join("/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(base.join("ctl/IPConn").unwrap().path, "/ctl/IPConn");
        let nested = Url::parse("http://192.168.0.1:5000/a/b/desc.xml").unwrap();
        assert_eq!(nested.join("ctl").unwrap().path, "/a/b/ctl");
        assert_eq!(
            base.join("http://192.168.0.1:49152/ctl").unwrap().port,
            49152
        );
        assert_eq!(base.join("http://1.1.1.1/ctl"), None);
    }

    #[test]
    fn the_wan_service_is_found_past_others() {
        assert_eq!(
            wan_service(DESCRIPTION),
            Some((
                "urn:schemas-upnp-org:service:WANIPConnection:1".to_owned(),
                "/upnp/control/WANIPConn1".to_owned()
            ))
        );
        let ppp = DESCRIPTION.replace("WANIPConnection", "WANPPPConnection");
        assert!(wan_service(&ppp).unwrap().0.contains("WANPPPConnection"));
        assert_eq!(
            wan_service("<root><service><serviceType>x</serviceType></service></root>"),
            None
        );
        assert_eq!(wan_service("garbage <service"), None);
        assert_eq!(wan_service(""), None);
    }

    #[test]
    fn the_soap_answer_gives_the_address() {
        assert_eq!(
            xml_text(SOAP_ANSWER, "NewExternalIPAddress").as_deref(),
            Some("100.70.12.34")
        );
        assert_eq!(
            xml_text(
                "<a><NewExternalIPAddress></NewExternalIPAddress></a>",
                "NewExternalIPAddress"
            )
            .as_deref(),
            Some("")
        );
        assert_eq!(xml_text("<a>", "NewExternalIPAddress"), None);
        assert_eq!(
            xml_text("<URLBase>http://a&amp;b</URLBase>", "URLBase").as_deref(),
            Some("http://a&b")
        );
    }

    #[test]
    fn http_answers() {
        assert_eq!(
            parse_response("HTTP/1.0 200 OK\r\nContent-Type: text/xml\r\n\r\n<x/>").unwrap(),
            "<x/>"
        );
        assert_eq!(
            parse_response("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n<x/>\r\n3\r\nabc\r\n0\r\n\r\n").unwrap(),
            "<x/>abc"
        );
        let error =
            parse_response("HTTP/1.1 500 Internal Server Error\r\n\r\n<fault/>").unwrap_err();
        assert!(error.to_string().contains("500"));
        assert!(parse_response("HTTP/1.1 200 OK\r\n").is_err());
    }

    /// A fake gateway on loopback answers the description and the SOAP
    /// request: the whole path from description URL to address.
    #[test]
    fn a_fake_gateway_gives_its_address() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            for body in [DESCRIPTION, SOAP_ANSWER] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request).unwrap();
                let answer = format!("HTTP/1.0 200 OK\r\nContent-Type: text/xml\r\n\r\n{body}");
                stream.write_all(answer.as_bytes()).unwrap();
            }
        });
        // Loopback is not a private range, so build the URL directly.
        let location = Url {
            host: Ipv4Addr::LOCALHOST,
            port,
            path: "/igd.xml".into(),
        };
        let address = external_address(&location).unwrap();
        server.join().unwrap();
        assert_eq!(address, Ipv4Addr::new(100, 70, 12, 34));
    }
}
