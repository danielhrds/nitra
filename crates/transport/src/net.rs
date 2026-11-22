//! A UDP socket with a precise wait: socket read event + high-resolution
//! waitable timer + a wake event other threads can signal.
//!
//! Precision matters: a socket read timeout on Windows wakes up to ~15.6 ms
//! late, and str0m's congestion control reads the late wake-ups as network
//! delay and collapses the estimate (measured). The
//! waitable timer wakes within about a millisecond without spinning.

use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Networking::WinSock::{
    FD_READ, SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET, WSACreateEvent, WSAEventSelect, setsockopt,
};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW, INFINITE,
    ResetEvent, SetEvent, SetWaitableTimer, WaitForMultipleObjects,
};
use windows::core::PCWSTR;

use crate::impair::{Impairment, ImpairmentConfig, ImpairmentStats};

/// Socket buffer size each way. The Windows default receive buffer (64 KiB)
/// overflows on a keyframe burst (~250 KB) and drops packets silently.
const SOCKET_BUFFER_BYTES: i32 = 4 << 20;
const TIMER_ALL_ACCESS: u32 = 0x1F_0003;
/// WaitForMultipleObjects takes at most this many handles (winnt.h).
const MAXIMUM_WAIT_OBJECTS: u32 = 64;

/// An owned kernel handle, closed on drop.
#[derive(Debug)]
struct OwnedHandle(HANDLE);

// SAFETY: kernel handles may be used and closed from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above; the handles used here (events) are thread-safe objects.
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned here and closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Wakes a [`NetSocket`] wait from another thread (e.g. a new access unit).
#[derive(Clone, Debug)]
pub struct Waker(Arc<OwnedHandle>);

impl Waker {
    pub fn wake(&self) {
        // SAFETY: valid auto-reset event owned by the Arc.
        let _ = unsafe { SetEvent(self.0.0) };
    }
}

/// Why a wait returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// `n` bytes from `from`, received on the local address `to`.
    Packet {
        n: usize,
        from: SocketAddr,
        to: SocketAddr,
    },
    Deadline,
    Waker,
}

/// One bound UDP socket and its read event.
struct Bound {
    socket: UdpSocket,
    local: SocketAddr,
    readable: OwnedHandle,
}

/// UDP sockets, one per local address (each ICE host candidate needs its
/// own: str0m pairs a datagram with the local address it arrived on, which
/// a wildcard socket cannot tell), waited on together.
pub struct NetSocket {
    sockets: Vec<Bound>,
    /// Where the next wait starts reading, so no socket starves the others.
    next: usize,
    timer: OwnedHandle,
    waker: Waker,
    /// Times a wait blocked and woke up (to tell a busy loop from an idle one).
    wakeups: u64,
    /// Testing: impairs what these sockets receive.
    impairment: Option<Impairment>,
}

impl NetSocket {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::bind_all(&[addr])
    }

    /// Binds one socket per address. Addresses that fail to bind (e.g. an
    /// adapter going away) are skipped; at least one must succeed.
    pub fn bind_all(addrs: &[SocketAddr]) -> io::Result<Self> {
        let mut sockets = Vec::new();
        let mut last_error = None;
        // The wait takes the sockets' events plus the timer and the waker.
        for addr in addrs.iter().take(MAXIMUM_WAIT_OBJECTS as usize - 2) {
            match bind_one(*addr) {
                Ok(bound) => sockets.push(bound),
                Err(error) => {
                    tracing::debug!(%addr, %error, "could not bind");
                    last_error = Some(error);
                }
            }
        }
        if sockets.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no address to bind")));
        }
        // SAFETY: new handles owned by the result.
        let (timer, wake) = unsafe {
            (
                OwnedHandle(
                    CreateWaitableTimerExW(
                        None,
                        PCWSTR::null(),
                        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                        TIMER_ALL_ACCESS,
                    )
                    .map_err(io::Error::from)?,
                ),
                OwnedHandle(CreateEventW(None, false, false, None).map_err(io::Error::from)?),
            )
        };
        Ok(Self {
            sockets,
            next: 0,
            timer,
            waker: Waker(Arc::new(wake)),
            wakeups: 0,
            impairment: None,
        })
    }

    /// The first bound address (the only one for a single-address socket).
    pub fn local_addr(&self) -> SocketAddr {
        self.sockets[0].local
    }

    /// Every bound address.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.sockets.iter().map(|s| s.local).collect()
    }

    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    /// Sends from the socket bound to `from` (or, if none is, from the first
    /// socket of the same IP family).
    pub fn send_to(&self, data: &[u8], from: SocketAddr, to: SocketAddr) {
        let socket = self.sockets.iter().find(|s| s.local == from).or_else(|| {
            self.sockets
                .iter()
                .find(|s| s.local.is_ipv4() == to.is_ipv4())
        });
        if let Some(s) = socket {
            // A full send buffer drops the datagram, as the network would.
            let _ = s.socket.send_to(data, to);
        }
    }

    /// Testing: impairs every datagram received from now on (`None` stops).
    pub fn set_impairment(&mut self, config: Option<ImpairmentConfig>) {
        self.impairment = config.map(|c| Impairment::new(c, Instant::now()));
    }

    pub fn impairment_stats(&self) -> Option<ImpairmentStats> {
        self.impairment.as_ref().map(Impairment::stats)
    }

    /// How many times a wait blocked and woke up so far.
    pub fn wakeups(&self) -> u64 {
        self.wakeups
    }

    /// Reads one datagram from any socket, starting after the last one read.
    fn recv_any(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr, SocketAddr)>> {
        let count = self.sockets.len();
        for i in 0..count {
            let index = (self.next + i) % count;
            let s = &self.sockets[index];
            match s.socket.recv_from(buf) {
                Ok((n, from)) => {
                    self.next = (index + 1) % count;
                    return Ok(Some((n, from, s.local)));
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Waits for a packet (into `buf`), the deadline or the waker, whichever
    /// comes first.
    pub fn wait(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<Woke> {
        loop {
            for s in &self.sockets {
                // SAFETY: valid manual-reset event; reading re-arms FD_READ.
                let _ = unsafe { ResetEvent(s.readable.0) };
            }
            let mut wait_until = deadline;
            if self.impairment.is_some() {
                // Everything that arrived goes through the impairment first.
                while let Some((n, from, to)) = self.recv_any(buf)? {
                    let impairment = self.impairment.as_mut().expect("checked above");
                    impairment.admit(&buf[..n], from, to, Instant::now());
                }
                let impairment = self.impairment.as_mut().expect("checked above");
                if let Some((n, from, to)) = impairment.pop_due(buf, Instant::now()) {
                    return Ok(Woke::Packet { n, from, to });
                }
                if let Some(release) = impairment.next_release() {
                    wait_until = wait_until.min(release);
                }
            } else if let Some((n, from, to)) = self.recv_any(buf)? {
                return Ok(Woke::Packet { n, from, to });
            }
            let remaining = wait_until.saturating_duration_since(Instant::now());
            if remaining.is_zero() && wait_until < deadline {
                continue; // a delayed datagram is due
            }
            if remaining.is_zero() {
                return Ok(Woke::Deadline);
            }
            // Relative due time in 100 ns units (negative = relative).
            let due = -((remaining.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
            let mut handles: Vec<HANDLE> = self.sockets.iter().map(|s| s.readable.0).collect();
            let sockets = handles.len() as u32;
            handles.push(self.timer.0);
            handles.push(self.waker.0.0);
            // SAFETY: valid handles owned by this value; the array is a live
            // local of at most MAXIMUM_WAIT_OBJECTS handles (see `bind_all`).
            let woke = unsafe {
                SetWaitableTimer(self.timer.0, &due, 0, None, None, false)
                    .map_err(io::Error::from)?;
                WaitForMultipleObjects(&handles, false, INFINITE)
            };
            self.wakeups += 1;
            match woke.0.wrapping_sub(WAIT_OBJECT_0.0) {
                i if i < sockets => continue,
                i if i == sockets && wait_until < deadline => continue,
                i if i == sockets => return Ok(Woke::Deadline),
                i if i == sockets + 1 => return Ok(Woke::Waker),
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

/// One socket with 4 MiB buffers, non-blocking, with a read event.
fn bind_one(addr: SocketAddr) -> io::Result<Bound> {
    let socket = UdpSocket::bind(addr)?;
    let local = socket.local_addr()?;
    let raw = SOCKET(socket.as_raw_socket() as usize);
    let size = SOCKET_BUFFER_BYTES.to_ne_bytes();
    // SAFETY: valid socket owned by `socket`; the option values are live;
    // the event is a new handle owned by the result.
    unsafe {
        for option in [SO_RCVBUF, SO_SNDBUF] {
            if setsockopt(raw, SOL_SOCKET, option, Some(&size)) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let event = WSACreateEvent().map_err(io::Error::from)?;
        let readable = OwnedHandle(HANDLE(event.0 as *mut _));
        // Also makes the socket non-blocking.
        if WSAEventSelect(raw, Some(event), FD_READ as i32) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Bound {
            socket,
            local,
            readable,
        })
    }
}

/// Windows: a datagram larger than the receive buffer (it is dropped).
const WSAEMSGSIZE: i32 = 10040;

/// Nothing to read, an ICMP port unreachable from a peer that left (shows up
/// as a reset), or a datagram too big for the buffer (dropped). Anyone can
/// send to the port, so none of these may end the session: one oversized
/// datagram used to (WSAEMSGSIZE went up as an error).
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::ConnectionReset
    ) || error.raw_os_error() == Some(WSAEMSGSIZE)
}

/// Upper bound on a wait when nothing else is due (keeps loops responsive
/// to stop requests even if every timeout is far away).
pub const MAX_WAIT: Duration = Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn localhost() -> SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[test]
    fn deadline_wakes_within_two_milliseconds() {
        let mut socket = NetSocket::bind(localhost()).unwrap();
        let mut buf = [0_u8; 64];
        let mut worst = Duration::ZERO;
        for _ in 0..20 {
            let deadline = Instant::now() + Duration::from_millis(3);
            assert_eq!(socket.wait(&mut buf, deadline).unwrap(), Woke::Deadline);
            worst = worst.max(Instant::now().saturating_duration_since(deadline));
        }
        assert!(worst < Duration::from_millis(2), "woke {worst:?} late");
    }

    #[test]
    fn packets_and_the_waker_end_the_wait() {
        let mut a = NetSocket::bind(localhost()).unwrap();
        let b = NetSocket::bind(localhost()).unwrap();
        let mut buf = [0_u8; 64];
        b.send_to(b"hello", b.local_addr(), a.local_addr());
        let woke = a
            .wait(&mut buf, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            woke,
            Woke::Packet {
                n: 5,
                from: b.local_addr(),
                to: a.local_addr()
            }
        );
        assert_eq!(&buf[..5], b"hello");

        let waker = a.waker();
        let wake = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            waker.wake();
        });
        let started = Instant::now();
        assert_eq!(
            a.wait(&mut buf, started + Duration::from_secs(1)).unwrap(),
            Woke::Waker
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        wake.join().unwrap();
    }

    #[test]
    fn a_datagram_bigger_than_the_buffer_is_dropped_not_an_error() {
        // Anyone can send to the port: one oversized datagram must not end
        // the session (Windows reports it as WSAEMSGSIZE).
        let mut a = NetSocket::bind(localhost()).unwrap();
        let b = NetSocket::bind(localhost()).unwrap();
        let mut buf = [0_u8; 64];
        b.send_to(&[7_u8; 3000], b.local_addr(), a.local_addr());
        b.send_to(b"after", b.local_addr(), a.local_addr());
        let woke = a
            .wait(&mut buf, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            woke,
            Woke::Packet {
                n: 5,
                from: b.local_addr(),
                to: a.local_addr()
            }
        );
        assert_eq!(&buf[..5], b"after");
    }
}
