//! A plain HTTP GET on WinHTTP (the same stack as the WebSocket: Windows'
//! TLS, proxy settings and certificate store), for the update check and
//! download. Follows redirects (a GitHub release asset answers with one to
//! its storage host); refuses an answer larger than the caller allows.

use std::io;
use std::time::Duration;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
    WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpReadData, WinHttpReceiveResponse,
    WinHttpSendRequest, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR};

use crate::ws::{Handle, content_length, status_code, winhttp_error};

/// An `http://` or `https://` URL, split for WinHTTP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUrl {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl HttpUrl {
    pub fn parse(text: &str) -> Option<Self> {
        let (secure, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (true, rest)
        } else {
            (false, text.strip_prefix("http://")?)
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let default_port = if secure { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some(_) if authority.ends_with(']') => (authority, default_port),
            Some((host, port)) => (host, port.parse().ok()?),
            None => (authority, default_port),
        };
        if host.is_empty() || host.contains(['@', ' ']) {
            return None;
        }
        Some(Self {
            secure,
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }
}

/// GETs `url` with `headers` (CRLF-separated "Name: value" lines) and
/// returns the body of a 200 answer, at most `max` bytes. `progress` gets
/// (bytes so far, total if the server said it) as the body arrives.
pub fn get(
    url: &HttpUrl,
    headers: &str,
    timeout: Duration,
    max: usize,
    mut progress: impl FnMut(usize, Option<usize>),
) -> io::Result<Vec<u8>> {
    let host = HSTRING::from(url.host.as_str());
    let path = HSTRING::from(url.path.as_str());
    let headers: Vec<u16> = headers.encode_utf16().collect();
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: each handle is checked, owned by a `Handle` and closed on drop
    // (request, connection, session: reverse order of declaration); the
    // strings and buffers live across the calls that read or write them.
    unsafe {
        let session = Handle::new(
            WinHttpOpen(
                &HSTRING::from(format!("nitra/{}", env!("CARGO_PKG_VERSION"))),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "open session",
        )?;
        WinHttpSetTimeouts(session.0, timeout_ms, timeout_ms, timeout_ms, timeout_ms)
            .map_err(|e| winhttp_error("set timeouts", &e))?;
        let connection = Handle::new(WinHttpConnect(session.0, &host, url.port, 0), "connect")?;
        let flags = if url.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                windows::core::w!("GET"),
                &path,
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            ),
            "open request",
        )?;
        let extra = (!headers.is_empty()).then_some(headers.as_slice());
        WinHttpSendRequest(request.0, extra, None, 0, 0, 0)
            .map_err(|e| winhttp_error("send request", &e))?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| winhttp_error("receive response", &e))?;
        let status = status_code(&request)?;
        if status != 200 {
            return Err(io::Error::other(format!(
                "the server answered HTTP {status}"
            )));
        }
        let total = content_length(&request);
        if total.is_some_and(|t| t > max) {
            return Err(io::Error::other(format!(
                "the answer is larger than {max} bytes"
            )));
        }
        let mut body = Vec::with_capacity(total.unwrap_or(0).min(max));
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0_u32;
            WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            )
            .map_err(|e| winhttp_error("read the answer", &e))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > max {
                return Err(io::Error::other(format!(
                    "the answer is larger than {max} bytes"
                )));
            }
            progress(body.len(), total);
        }
        if total.is_some_and(|t| t != body.len()) {
            return Err(io::Error::other("the answer ended early"));
        }
        drop(request);
        drop(connection);
        drop(session);
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_for_winhttp() {
        let url = HttpUrl::parse("https://api.github.com/repos/a/b/releases/latest").unwrap();
        assert!(url.secure);
        assert_eq!((url.host.as_str(), url.port), ("api.github.com", 443));
        assert_eq!(url.path, "/repos/a/b/releases/latest");
        let local = HttpUrl::parse("http://127.0.0.1:8090/x?y=1").unwrap();
        assert_eq!(
            (local.secure, local.port, local.path.as_str()),
            (false, 8090, "/x?y=1")
        );
        assert!(HttpUrl::parse("ftp://host/").is_none());
        assert!(HttpUrl::parse("https://user@host/").is_none());
    }
}
