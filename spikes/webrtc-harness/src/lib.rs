//! Shared parts of the WebRTC spikes, so both libraries do the
//! exact same job: the H.264 source, the scenarios, an impaired UDP link,
//! the frame timeline and the report. Disposable, like the spikes.

use std::collections::{BinaryHeap, HashMap};
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

pub const FPS: u32 = 60;
pub const RTP_VIDEO_CLOCK: u64 = 90_000;
pub const FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / FPS as u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scenario {
    /// One viewer, clean link.
    Basic,
    /// One bitstream to three viewers, clean link.
    Fanout,
    /// A second viewer joins late and needs a keyframe (PLI).
    Late,
    /// 2% loss host -> viewer; NACK/retransmission.
    Loss,
    /// 6 Mbit/s bottleneck host -> viewer; bandwidth estimation.
    Bwe,
}

#[derive(Clone, Debug, clap::Parser)]
pub struct Args {
    #[arg(long, value_enum)]
    pub scenario: Scenario,
    /// Annex B H.264 file (looped).
    #[arg(long, default_value = r"D:\Projects\live\lab\viewer\streams\1080p60.h264")]
    pub file: PathBuf,
    #[arg(long, default_value_t = 30)]
    pub seconds: u64,
    /// Overrides the scenario's number of viewers.
    #[arg(long)]
    pub viewers: Option<usize>,
    /// JSON report path.
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
}

/// What a run does, derived from the scenario.
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub scenario: Scenario,
    pub seconds: u64,
    /// When each viewer connects, from the start.
    pub joins: Vec<Duration>,
    pub impairment: Impairment,
}

impl Plan {
    pub fn from_args(args: &Args) -> Self {
        let (viewers, impairment, late) = match args.scenario {
            Scenario::Basic => (1, Impairment::default(), false),
            Scenario::Fanout => (3, Impairment::default(), false),
            Scenario::Late => (2, Impairment::default(), true),
            Scenario::Loss => (1, Impairment { loss: 0.02, ..Default::default() }, false),
            Scenario::Bwe => (
                1,
                Impairment { bandwidth_bps: Some(6_000_000), ..Default::default() },
                false,
            ),
        };
        let viewers = args.viewers.unwrap_or(viewers);
        let joins = (0..viewers)
            .map(|i| if late && i == viewers - 1 { Duration::from_secs(7) } else { Duration::ZERO })
            .collect();
        Self {
            scenario: args.scenario,
            seconds: args.seconds,
            joins,
            impairment: Impairment { seed: args.seed, ..impairment },
        }
    }
}

// ---------------------------------------------------------------- source

/// Access units of the file, looped at 60 fps. `request_keyframe` makes the
/// next frame the start of the next GOP, as an encoder forcing an IDR would.
pub struct Source {
    units: Arc<Vec<Vec<u8>>>,
    keyframes: Vec<usize>,
    next_index: usize,
    next_id: u64,
    keyframe_requested: bool,
}

pub struct SourceFrame<'a> {
    pub id: u64,
    pub data: &'a [u8],
    pub keyframe: bool,
    /// RTP media time (90 kHz) of the frame.
    pub rtp_time: u64,
}

impl Source {
    pub fn open(path: &std::path::Path) -> Self {
        let stream = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let units: Vec<Vec<u8>> =
            encode::annexb::access_units(&stream).into_iter().map(<[u8]>::to_vec).collect();
        let keyframes =
            (0..units.len()).filter(|&i| encode::annexb::is_keyframe(&units[i])).collect();
        Self { units: Arc::new(units), keyframes, next_index: 0, next_id: 0, keyframe_requested: false }
    }

    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// The access units, shared with the viewers to check what they get.
    pub fn units(&self) -> Arc<Vec<Vec<u8>>> {
        Arc::clone(&self.units)
    }

    pub fn unit(&self, index: usize) -> &[u8] {
        &self.units[index]
    }

    pub fn next(&mut self) -> (usize, SourceFrame<'_>) {
        if std::mem::take(&mut self.keyframe_requested) {
            let current = self.next_index;
            self.next_index = self
                .keyframes
                .iter()
                .copied()
                .find(|&k| k >= current)
                .unwrap_or(self.keyframes[0]);
        }
        let index = self.next_index;
        self.next_index = (index + 1) % self.units.len();
        let id = self.next_id;
        self.next_id += 1;
        let frame = SourceFrame {
            id,
            data: &self.units[index],
            keyframe: self.keyframes.binary_search(&index).is_ok(),
            rtp_time: id * (RTP_VIDEO_CLOCK / FPS as u64),
        };
        (index, frame)
    }
}

/// Frame id from an RTP media time (90 kHz).
pub fn frame_id(rtp_time: u64) -> u64 {
    (rtp_time + RTP_VIDEO_CLOCK / FPS as u64 / 2) / (RTP_VIDEO_CLOCK / FPS as u64)
}

/// Same access unit, ignoring start-code length and access unit delimiters.
pub fn same_access_unit(a: &[u8], b: &[u8]) -> bool {
    let nals = |s| {
        encode::annexb::nal_units(s)
            .into_iter()
            .filter(|nal| encode::annexb::nal_unit_type(nal) != 9)
            .collect::<Vec<_>>()
    };
    nals(a) == nals(b)
}

// ---------------------------------------------------------------- timeline

#[derive(Clone, Copy)]
struct Sent {
    unit: usize,
    at: Instant,
    keyframe: bool,
}

/// What the host sent, for the viewers to check what they got.
#[derive(Default)]
pub struct Timeline {
    sent: Mutex<HashMap<u64, Sent>>,
}

impl Timeline {
    pub fn sent(&self, id: u64, unit: usize, keyframe: bool, at: Instant) {
        self.sent.lock().unwrap().insert(id, Sent { unit, at, keyframe });
    }

    pub fn sent_since(&self, since: Instant) -> u64 {
        self.sent.lock().unwrap().values().filter(|s| s.at >= since).count() as u64
    }

    fn lookup(&self, id: u64) -> Option<Sent> {
        self.sent.lock().unwrap().get(&id).copied()
    }
}

// ---------------------------------------------------------------- viewer stats

#[derive(Debug, Default, Serialize)]
pub struct ViewerReport {
    pub index: usize,
    pub joined_after_ms: u64,
    pub connected_after_ms: Option<f64>,
    /// From joining to the first complete keyframe (decodable picture).
    pub first_keyframe_after_ms: Option<f64>,
    pub frames_sent_since_join: u64,
    pub frames_ok: u64,
    pub frames_corrupt: u64,
    pub frames_unknown: u64,
    /// Frames sent after the first keyframe that never arrived complete.
    pub frames_missing_after_keyframe: u64,
    pub latency_ms: Option<Summary>,
    pub keyframe_requests_sent: u64,
    pub cpu_ms: f64,
    pub library_stats: serde_json::Value,
}

/// Collects what one viewer received.
pub struct ViewerStats {
    pub report: ViewerReport,
    joined: Instant,
    first_keyframe_id: Option<u64>,
    last_id: Option<u64>,
    latencies: Vec<Duration>,
}

impl ViewerStats {
    pub fn new(index: usize, start: Instant, joined: Instant) -> Self {
        Self {
            report: ViewerReport {
                index,
                joined_after_ms: (joined - start).as_millis() as u64,
                ..Default::default()
            },
            joined,
            first_keyframe_id: None,
            last_id: None,
            latencies: Vec::new(),
        }
    }

    pub fn connected(&mut self, now: Instant) {
        self.report.connected_after_ms.get_or_insert(ms(now - self.joined));
    }

    /// A complete frame arrived (Annex B or raw NAL payloads as the library gives them).
    pub fn frame(&mut self, now: Instant, rtp_time: u64, data: &[u8], units: &[Vec<u8>], timeline: &Timeline) {
        let id = frame_id(rtp_time);
        let Some(sent) = timeline.lookup(id) else {
            self.report.frames_unknown += 1;
            return;
        };
        if !same_access_unit(&units[sent.unit], data) {
            self.report.frames_corrupt += 1;
            return;
        }
        self.report.frames_ok += 1;
        self.latencies.push(now - sent.at);
        if sent.keyframe && self.first_keyframe_id.is_none() {
            self.first_keyframe_id = Some(id);
            self.report.first_keyframe_after_ms = Some(ms(now - self.joined));
        }
        self.last_id = Some(self.last_id.map_or(id, |last| last.max(id)));
    }

    pub fn finish(mut self, timeline: &Timeline, cpu_ms: f64, library_stats: serde_json::Value) -> ViewerReport {
        self.report.frames_sent_since_join = timeline.sent_since(self.joined);
        if let (Some(first), Some(last)) = (self.first_keyframe_id, self.last_id) {
            let expected = last - first + 1;
            let ok_after: u64 = self.report.frames_ok; // frames before the keyframe are rare
            self.report.frames_missing_after_keyframe = expected.saturating_sub(ok_after);
        }
        self.report.latency_ms = Summary::of(&self.latencies);
        self.report.cpu_ms = cpu_ms;
        self.report.library_stats = library_stats;
        self.report
    }
}

// ---------------------------------------------------------------- report

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Summary {
    pub fn of(values: &[Duration]) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = values.iter().map(|d| ms(*d)).collect();
        v.sort_by(f64::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        Some(Self {
            count: v.len(),
            mean: v.iter().sum::<f64>() / v.len() as f64,
            p50: at(0.5),
            p95: at(0.95),
            p99: at(0.99),
            max: v[v.len() - 1],
        })
    }
}

#[derive(Debug, Serialize)]
pub struct HostReport {
    pub frames_sent: u64,
    pub keyframe_requests_received: u64,
    pub cpu_ms: f64,
    pub link: LinkCounters,
    /// (seconds since start, estimate in bit/s).
    pub bandwidth_estimates: Vec<(f64, u64)>,
    pub library_stats: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub library: &'static str,
    pub plan: Plan,
    pub host: HostReport,
    pub viewers: Vec<ViewerReport>,
    pub process_cpu_ms: f64,
}

impl Report {
    pub fn print_and_save(&self, out: Option<&std::path::Path>) {
        let json = serde_json::to_string_pretty(self).expect("serializable");
        println!("{json}");
        if let Some(path) = out {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::write(path, json).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// CPU time (user + kernel) of the calling thread, in ms.
pub fn thread_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let (mut a, mut b, mut kernel, mut user) = Default::default();
    // SAFETY: pseudo-handle of the current thread; live out-pointers.
    let _ = unsafe { GetThreadTimes(GetCurrentThread(), &mut a, &mut b, &mut kernel, &mut user) };
    let to_ms = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.0;
    to_ms(kernel) + to_ms(user)
}

/// CPU time of the whole process, in ms.
pub fn process_cpu_ms() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut a, mut b, mut kernel, mut user) = Default::default();
    // SAFETY: pseudo-handle of the current process; live out-pointers.
    let _ = unsafe { GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut kernel, &mut user) };
    let to_ms = |t: FILETIME| ((u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)) as f64 / 10_000.0;
    to_ms(kernel) + to_ms(user)
}

// ---------------------------------------------------------------- link

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Impairment {
    /// Random loss probability per packet.
    pub loss: f64,
    /// Bottleneck rate; packets queue behind it (max 200 ms) and overflow drops.
    pub bandwidth_bps: Option<u64>,
    pub seed: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct LinkCounters {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub dropped_loss: u64,
    pub dropped_queue: u64,
}

struct Delayed {
    due: Instant,
    to: SocketAddr,
    data: Vec<u8>,
}

impl PartialEq for Delayed {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due
    }
}
impl Eq for Delayed {}
impl PartialOrd for Delayed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Delayed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.due.cmp(&self.due) // min-heap
    }
}

/// A UDP socket on 127.0.0.1 whose outgoing packets go through a
/// deterministic impairment (loss, bottleneck). Blocking receive with a timeout.
pub struct Link {
    pub socket: UdpSocket,
    pub local: SocketAddr,
    impairment: Impairment,
    rng: u64,
    free_at: Option<Instant>,
    queue: BinaryHeap<Delayed>,
    pub counters: LinkCounters,
    event: windows::Win32::Foundation::HANDLE,
    timer: windows::Win32::Foundation::HANDLE,
}

impl Link {
    pub fn bind(impairment: Impairment) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let local = socket.local_addr().expect("local addr");
        if std::env::var_os("ZIZ_SMALL_BUFFERS").is_none() {
            set_buffers(&socket, 4 << 20);
        }
        let (event, timer) = read_event_and_timer(&socket);
        Self {
            socket,
            local,
            impairment,
            rng: impairment.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
            free_at: None,
            queue: BinaryHeap::new(),
            counters: LinkCounters::default(),
            event,
            timer,
        }
    }

    fn random(&mut self) -> f64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn send(&mut self, now: Instant, to: SocketAddr, data: &[u8]) {
        if self.impairment.loss > 0.0 && self.random() < self.impairment.loss {
            self.counters.dropped_loss += 1;
            return;
        }
        let Some(rate) = self.impairment.bandwidth_bps else {
            self.transmit(to, data);
            return;
        };
        let start = self.free_at.map_or(now, |free| free.max(now));
        if start - now > Duration::from_millis(200) {
            self.counters.dropped_queue += 1;
            return;
        }
        let done = start + Duration::from_secs_f64(data.len() as f64 * 8.0 / rate as f64);
        self.free_at = Some(done);
        self.queue.push(Delayed { due: done, to, data: data.to_vec() });
    }

    fn transmit(&mut self, to: SocketAddr, data: &[u8]) {
        self.counters.packets_sent += 1;
        self.counters.bytes_sent += data.len() as u64;
        let _ = self.socket.send_to(data, to);
    }

    /// Sends the queued packets that are due.
    pub fn flush(&mut self, now: Instant) {
        while self.queue.peek().is_some_and(|d| d.due <= now) {
            let d = self.queue.pop().expect("peeked");
            self.transmit(d.to, &d.data);
        }
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.queue.peek().map(|d| d.due)
    }

    /// Waits for a packet or `deadline`, whichever comes first, without
    /// spinning: socket read event + high-resolution waitable timer, so the
    /// wake-up is sub-millisecond (socket read timeouts are 1-16 ms).
    fn wait_precisely(&mut self, buf: &mut [u8], deadline: Instant) -> Option<(usize, SocketAddr)> {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::{INFINITE, ResetEvent, SetWaitableTimer, WaitForMultipleObjects};
        loop {
            // SAFETY: valid manual-reset event; a read re-arms FD_READ.
            let _ = unsafe { ResetEvent(self.event) };
            match self.socket.recv_from(buf) {
                Ok(got) => return Some(got),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::ConnectionReset) => {}
                Err(e) => panic!("recv: {e}"),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // Relative due time in 100 ns units (negative = relative).
            let due = -((remaining.as_nanos() / 100).max(1) as i64);
            // SAFETY: valid timer and event handles owned by this link.
            unsafe {
                SetWaitableTimer(self.timer, &due, 0, None, None, false).expect("timer");
                let r = WaitForMultipleObjects(&[self.event, self.timer], false, INFINITE);
                if r.0 == WAIT_OBJECT_0.0 + 1 {
                    return None;
                }
            }
        }
    }

    /// Waits until `deadline` (or a packet). `Some((n, from))` for a packet.
    pub fn recv(&mut self, buf: &mut [u8], deadline: Instant) -> Option<(usize, SocketAddr)> {
        let deadline = self.next_due().map_or(deadline, |due| due.min(deadline));
        let result = if std::env::var_os("ZIZ_SPIN").is_some() {
            // Diagnostic: poll until the deadline (exact timing, full core).

            loop {
                match self.socket.recv_from(buf) {
                    Ok(got) => break Some(got),
                    Err(_) if Instant::now() < deadline => std::hint::spin_loop(),
                    Err(_) => break None,
                }
            }
        } else {
            self.wait_precisely(buf, deadline)
        };
        self.flush(Instant::now());
        result
    }
}


/// 1 ms timer resolution for this process (documented `timeBeginPeriod`;
/// per process on Windows 11, undone at exit), so socket read timeouts and
/// sleeps wake within about 1 ms instead of the default ~15.6 ms.
pub fn fine_timers() {
    // SAFETY: plain call with a constant argument.
    unsafe {
        windows::Win32::Media::timeBeginPeriod(1);
    }
}

/// How late a 1 ms socket read timeout really wakes (diagnostic).
pub fn timeout_granularity_ms() -> f64 {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
    socket.set_read_timeout(Some(Duration::from_millis(1))).expect("timeout");
    let mut buf = [0_u8; 16];
    let started = Instant::now();
    for _ in 0..20 {
        let _ = socket.recv_from(&mut buf);
    }
    ms(started.elapsed()) / 20.0
}

/// Socket buffers of `bytes` each way (the Windows default receive buffer,
/// 64 KiB, overflows on a keyframe burst and drops packets silently).
fn set_buffers(socket: &UdpSocket, bytes: i32) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{SO_RCVBUF, SO_SNDBUF, SOCKET, SOL_SOCKET, setsockopt};
    let raw = SOCKET(socket.as_raw_socket() as usize);
    let value = bytes.to_ne_bytes();
    for option in [SO_RCVBUF, SO_SNDBUF] {
        // SAFETY: valid socket owned by `socket`; the value is a live i32.
        let _ = unsafe { setsockopt(raw, SOL_SOCKET, option, Some(&value)) };
    }
}

/// A manual-reset event signaled when `socket` has data (WSAEventSelect;
/// makes the socket non-blocking) and a high-resolution waitable timer.
fn read_event_and_timer(socket: &UdpSocket) -> (windows::Win32::Foundation::HANDLE, windows::Win32::Foundation::HANDLE) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Networking::WinSock::{FD_READ, SOCKET, WSACreateEvent, WSAEventSelect};
    use windows::Win32::System::Threading::{CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW};
    const TIMER_ALL_ACCESS: u32 = 0x1F_0003;
    // SAFETY: valid socket owned by `socket`; the event and timer are new
    // handles owned by the link and closed on drop.
    unsafe {
        let event = WSACreateEvent().expect("WSACreateEvent");
        let raw = SOCKET(socket.as_raw_socket() as usize);
        assert_eq!(WSAEventSelect(raw, Some(event), FD_READ as i32), 0, "WSAEventSelect");
        let timer = CreateWaitableTimerExW(None, windows::core::PCWSTR::null(), CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
            .expect("high-resolution timer");
        (HANDLE(event.0 as *mut _), timer)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        // SAFETY: both handles are owned by this link and closed once.
        unsafe {
            let _ = CloseHandle(self.timer);
            let _ = CloseHandle(self.event);
        }
    }
}
