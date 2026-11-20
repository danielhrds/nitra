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

/// Socket buffer size each way. The Windows default receive buffer (64 KiB)
/// overflows on a keyframe burst (~250 KB) and drops packets silently.
const SOCKET_BUFFER_BYTES: i32 = 4 << 20;
const TIMER_ALL_ACCESS: u32 = 0x1F_0003;

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
    Packet(usize, SocketAddr),
    Deadline,
    Waker,
}

#[derive(Debug)]
pub struct NetSocket {
    socket: UdpSocket,
    local: SocketAddr,
    readable: OwnedHandle,
    timer: OwnedHandle,
    waker: Waker,
}

impl NetSocket {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        let local = socket.local_addr()?;
        let raw = SOCKET(socket.as_raw_socket() as usize);
        let size = SOCKET_BUFFER_BYTES.to_ne_bytes();
        // SAFETY: valid socket owned by `socket`; the option values are live.
        // The event, timer and wake event are new handles owned by the result.
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
            let timer = OwnedHandle(
                CreateWaitableTimerExW(
                    None,
                    PCWSTR::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS,
                )
                .map_err(io::Error::from)?,
            );
            let wake =
                OwnedHandle(CreateEventW(None, false, false, None).map_err(io::Error::from)?);
            Ok(Self {
                socket,
                local,
                readable,
                timer,
                waker: Waker(Arc::new(wake)),
            })
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn waker(&self) -> Waker {
        self.waker.clone()
    }

    pub fn send_to(&self, data: &[u8], to: SocketAddr) {
        // A full send buffer drops the datagram, as the network would.
        let _ = self.socket.send_to(data, to);
    }

    /// Waits for a packet (into `buf`), the deadline or the waker, whichever
    /// comes first.
    pub fn wait(&self, buf: &mut [u8], deadline: Instant) -> io::Result<Woke> {
        loop {
            // SAFETY: valid manual-reset event; reading re-arms FD_READ.
            let _ = unsafe { ResetEvent(self.readable.0) };
            match self.socket.recv_from(buf) {
                Ok((n, from)) => return Ok(Woke::Packet(n, from)),
                // ICMP port unreachable from a peer that left shows up as a reset.
                Err(e)
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::ConnectionReset) => {}
                Err(e) => return Err(e),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(Woke::Deadline);
            }
            // Relative due time in 100 ns units (negative = relative).
            let due = -((remaining.as_nanos() / 100).clamp(1, i64::MAX as u128) as i64);
            // SAFETY: valid handles owned by this socket; the array is a live local.
            let woke = unsafe {
                SetWaitableTimer(self.timer.0, &due, 0, None, None, false)
                    .map_err(io::Error::from)?;
                WaitForMultipleObjects(
                    &[self.readable.0, self.timer.0, self.waker.0.0],
                    false,
                    INFINITE,
                )
            };
            match woke.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => continue,
                1 => return Ok(Woke::Deadline),
                2 => return Ok(Woke::Waker),
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
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
        let socket = NetSocket::bind(localhost()).unwrap();
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
        let a = NetSocket::bind(localhost()).unwrap();
        let b = NetSocket::bind(localhost()).unwrap();
        let mut buf = [0_u8; 64];
        b.send_to(b"hello", a.local_addr());
        let woke = a
            .wait(&mut buf, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(woke, Woke::Packet(5, b.local_addr()));
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
}
