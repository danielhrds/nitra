//! A WebSocket client on WinHTTP (the Windows HTTP stack: its TLS, proxy
//! settings and certificate store), so the app needs no WebSocket or TLS
//! library. Synchronous calls on the caller's threads: WinHTTP allows one
//! send and one receive in progress at a time on a WebSocket, so one thread
//! receives while others send (sends are serialized by a lock).

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::GetLastError;
use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
    WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_FLAG_NUMBER64, WINHTTP_QUERY_STATUS_CODE,
    WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE, WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_BUFFER_TYPE, WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE,
    WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE, WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
    WinHttpWebSocketCompleteUpgrade, WinHttpWebSocketQueryCloseStatus, WinHttpWebSocketReceive,
    WinHttpWebSocketSend, WinHttpWebSocketShutdown,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Longest message accepted (the room's own limit is 64 KiB).
const MAX_MESSAGE: usize = 256 * 1024;

/// A `ws://` or `wss://` URL, split for WinHTTP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsUrl {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub path: String,
}

impl WsUrl {
    pub fn parse(text: &str) -> Option<Self> {
        let (secure, rest) = if let Some(rest) = text.strip_prefix("wss://") {
            (true, rest)
        } else {
            (false, text.strip_prefix("ws://")?)
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let default_port = if secure { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            // "[::1]" is an IPv6 address without a port.
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

/// What one receive returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Received {
    Text(String),
    /// The other side closed (or answered our close).
    Closed {
        code: u16,
        reason: String,
    },
}

/// An owned WinHTTP handle, closed once on drop.
pub(crate) struct Handle(pub(crate) *mut core::ffi::c_void);

impl Handle {
    pub(crate) fn new(raw: *mut core::ffi::c_void, what: &str) -> io::Result<Self> {
        if raw.is_null() {
            Err(last_error(what))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: an open handle owned by this value, closed only here.
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

pub struct WebSocket {
    /// The WebSocket handle; null once aborted. Closed in `abort` (or on
    /// drop) before the connection and session handles below.
    socket: AtomicPtr<core::ffi::c_void>,
    _connection: Handle,
    _session: Handle,
    send_lock: Mutex<()>,
}

// SAFETY: WinHTTP handles may be used from any thread; on a WebSocket one
// send and one receive may run at the same time, and sends are serialized
// by `send_lock`. `abort` does not take it (it must not wait behind a send
// stuck on a dead connection); a send that loaded the handle just before it
// closed gets WinHTTP's invalid-handle error. Receives are only made by the one thread that owns the
// reading side (the room client's reader), as documented on `receive`.
// `abort` may close the handle while a receive is pending on another thread:
// WinHTTP cancels that operation (it fails with "operation cancelled") and
// keeps its own reference until it returns.
unsafe impl Send for WebSocket {}
// SAFETY: as above.
unsafe impl Sync for WebSocket {}

impl WebSocket {
    /// Connects and upgrades. `timeout` bounds each step of connecting
    /// (resolve, connect, send, response). It does not bound `receive`:
    /// measured, WinHTTP's receive timeout does not apply to WebSocket
    /// receives, so the room client watches for silence itself and calls
    /// `abort`.
    pub fn connect(url: &WsUrl, timeout: Duration) -> io::Result<Self> {
        let host = HSTRING::from(url.host.as_str());
        let path = HSTRING::from(url.path.as_str());
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: each handle is checked, owned by a `Handle` and closed on
        // drop (children before parents, by field order); the strings live
        // across the calls that read them.
        unsafe {
            let session = Handle::new(
                WinHttpOpen(
                    w!("nitra"),
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
                    w!("GET"),
                    &path,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    flags,
                ),
                "open request",
            )?;
            WinHttpSetOption(Some(request.0), WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, None)
                .map_err(|e| winhttp_error("ask for a WebSocket", &e))?;
            WinHttpSendRequest(request.0, None, None, 0, 0, 0)
                .map_err(|e| winhttp_error("send request", &e))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .map_err(|e| winhttp_error("receive response", &e))?;
            let status = status_code(&request)?;
            if status != 101 {
                return Err(io::Error::other(format!(
                    "the server answered HTTP {status}, not a WebSocket"
                )));
            }
            let socket = Handle::new(
                WinHttpWebSocketCompleteUpgrade(request.0, None),
                "complete upgrade",
            )?;
            drop(request);
            let raw = socket.0;
            std::mem::forget(socket);
            Ok(Self {
                socket: AtomicPtr::new(raw),
                _connection: connection,
                _session: session,
                send_lock: Mutex::new(()),
            })
        }
    }

    pub fn send_text(&self, text: &str) -> io::Result<()> {
        let _guard = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: valid WebSocket handle; the buffer outlives the call.
        let result = unsafe {
            WinHttpWebSocketSend(
                self.handle()?,
                WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE,
                Some(text.as_bytes()),
            )
        };
        check(result, "send")
    }

    /// The next whole message. Call from one thread only (see the `Sync`
    /// note above).
    pub fn receive(&self) -> io::Result<Received> {
        let mut message = Vec::new();
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            let (mut read, mut kind) = (0_u32, WINHTTP_WEB_SOCKET_BUFFER_TYPE(0));
            // SAFETY: valid handle; the buffer is writable for its length;
            // the out-parameters are live locals.
            let result = unsafe {
                WinHttpWebSocketReceive(
                    self.handle()?,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                    &mut kind,
                )
            };
            check(result, "receive")?;
            // Never past the buffer, whatever the call reports.
            let data = &buffer[..(read as usize).min(buffer.len())];
            match kind {
                WINHTTP_WEB_SOCKET_UTF8_FRAGMENT_BUFFER_TYPE => message.extend_from_slice(data),
                WINHTTP_WEB_SOCKET_UTF8_MESSAGE_BUFFER_TYPE => {
                    message.extend_from_slice(data);
                    return String::from_utf8(message)
                        .map(Received::Text)
                        .map_err(io::Error::other);
                }
                WINHTTP_WEB_SOCKET_CLOSE_BUFFER_TYPE => return Ok(self.close_status()),
                WINHTTP_WEB_SOCKET_BINARY_FRAGMENT_BUFFER_TYPE
                | WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE => {
                    // The room sends text only: skip binary messages whole.
                    if kind == WINHTTP_WEB_SOCKET_BINARY_MESSAGE_BUFFER_TYPE {
                        message.clear();
                    }
                }
                _ => {}
            }
            if message.len() > MAX_MESSAGE {
                return Err(io::Error::other("message too big"));
            }
        }
    }

    fn close_status(&self) -> Received {
        let unknown = Received::Closed {
            code: 1005,
            reason: String::new(),
        };
        let Ok(handle) = self.handle() else {
            return unknown;
        };
        let mut code = 0_u16;
        let mut reason = [0_u8; 123];
        let mut length = 0_u32;
        // SAFETY: valid handle; the reason buffer holds the 123 bytes a close
        // reason may have.
        let result = unsafe {
            WinHttpWebSocketQueryCloseStatus(
                handle,
                &mut code,
                Some(reason.as_mut_ptr().cast()),
                reason.len() as u32,
                &mut length,
            )
        };
        if result != 0 {
            return unknown;
        }
        Received::Closed {
            code,
            reason: String::from_utf8_lossy(&reason[..(length as usize).min(reason.len())])
                .into_owned(),
        }
    }

    /// Sends a close frame without waiting for the answer (the reader gets
    /// it, if the other side is still there).
    pub fn close(&self, code: u16, reason: &str) {
        let Ok(handle) = self.handle() else {
            return;
        };
        let reason = &reason.as_bytes()[..reason.len().min(123)];
        let _guard = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: valid handle; the reason outlives the call.
        let _ = unsafe {
            WinHttpWebSocketShutdown(
                handle,
                code,
                Some(reason.as_ptr().cast()),
                reason.len() as u32,
            )
        };
    }

    /// Closes the connection at once; a receive waiting on another thread
    /// returns an error. Later calls fail.
    pub fn abort(&self) {
        let handle = self.socket.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !handle.is_null() {
            // SAFETY: the handle was open and is closed once (the swap took
            // it); a pending receive is cancelled (see the `Sync` note).
            let _ = unsafe { WinHttpCloseHandle(handle) };
        }
    }

    fn handle(&self) -> io::Result<*mut core::ffi::c_void> {
        let handle = self.socket.load(Ordering::Acquire);
        if handle.is_null() {
            Err(io::Error::other("the connection was closed"))
        } else {
            Ok(handle)
        }
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        // Before the connection and session handles (dropped after this).
        self.abort();
    }
}

/// The HTTP status of a request that got its response.
///
/// # Safety
/// `request` is an open request handle whose response was received.
pub(crate) unsafe fn status_code(request: &Handle) -> io::Result<u32> {
    let mut status = 0_u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: per the contract; `status` is a writable u32 of `size` bytes.
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .map_err(|e| winhttp_error("read the status", &e))?;
    Ok(status)
}

/// The Content-Length of a received response, if the server sent one.
///
/// # Safety
/// `request` is an open request handle whose response was received.
pub(crate) unsafe fn content_length(request: &Handle) -> Option<usize> {
    let mut length = 0_u64;
    let mut size = size_of::<u64>() as u32;
    // SAFETY: per the contract; `length` is a writable u64 of `size` bytes
    // (WINHTTP_QUERY_FLAG_NUMBER64).
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
            PCWSTR::null(),
            Some((&raw mut length).cast()),
            &mut size,
            std::ptr::null_mut(),
        )
    }
    .ok()?;
    usize::try_from(length).ok()
}

fn check(result: u32, what: &str) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!("{what}: {}", describe(result))))
    }
}

pub(crate) fn last_error(what: &str) -> io::Error {
    // SAFETY: reads this thread's last error code, set by the failed call.
    let code = unsafe { GetLastError() };
    io::Error::other(format!("{what}: {}", describe(code.0)))
}

/// A failed WinHTTP call reported as an HRESULT (0x8007xxxx): named like
/// the plain error codes.
pub(crate) fn winhttp_error(what: &str, error: &windows::core::Error) -> io::Error {
    io::Error::other(format!(
        "{what}: {}",
        describe(error.code().0 as u32 & 0xFFFF)
    ))
}

/// WinHTTP's own errors (12000-12999) have no system message text; name
/// the ones a room connection meets.
fn describe(code: u32) -> String {
    let name = match code {
        12002 => "timed out",
        12007 => "the server name was not found",
        12017 => "cancelled",
        12029 => "could not connect to the server",
        12030 => "the connection was aborted",
        12031 => "the connection was reset",
        12152 => "invalid answer from the server",
        12175 => "secure connection (TLS) failed",
        _ => return io::Error::from_raw_os_error(code as i32).to_string(),
    };
    format!("{name} (WinHTTP {code})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_for_winhttp() {
        assert_eq!(
            WsUrl::parse("ws://127.0.0.1:8787/rooms/x/ws"),
            Some(WsUrl {
                secure: false,
                host: "127.0.0.1".into(),
                port: 8787,
                path: "/rooms/x/ws".into()
            })
        );
        let secure = WsUrl::parse("wss://nitra.example.workers.dev").unwrap();
        assert_eq!(
            (secure.secure, secure.port, secure.path.as_str()),
            (true, 443, "/")
        );
        assert_eq!(WsUrl::parse("ws://host/").unwrap().port, 80);
        assert_eq!(WsUrl::parse("http://host/"), None);
        assert_eq!(WsUrl::parse("ws://:80/"), None);
        assert_eq!(WsUrl::parse("ws://host:nope/"), None);
        assert_eq!(WsUrl::parse("ws://user@host/"), None);
        assert_eq!(WsUrl::parse("ws://[::1]:8787/").unwrap().port, 8787);
        assert_eq!(WsUrl::parse("ws://[::1]/").unwrap().host, "[::1]");
    }

    #[test]
    fn winhttp_errors_get_names() {
        assert_eq!(
            describe(12029),
            "could not connect to the server (WinHTTP 12029)"
        );
        assert!(!describe(5).contains("WinHTTP"));
    }

    #[test]
    fn a_closed_port_fails_to_connect() {
        // Port 9 (discard) is not listening on a normal PC.
        let url = WsUrl::parse("ws://127.0.0.1:9/").unwrap();
        assert!(WebSocket::connect(&url, Duration::from_secs(2)).is_err());
    }
}
