//! Network impairment on received datagrams, for tests and measurements:
//! random and burst loss, delay with jitter, reordering, a bottleneck link
//! with a bounded queue, and an outage window.
//!
//! Loss and jitter decisions come from a seeded generator, so a run makes the
//! same decisions for the same packet sequence; timing is still real time,
//! so runs are not bit-for-bit identical. Not a WAN model: no shared queues
//! with other traffic, no route changes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Datagrams waiting in the impairment queue at most; more are dropped.
const MAX_QUEUED: usize = 4096;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImpairmentConfig {
    /// Probability of dropping each datagram (0..1).
    pub loss: f64,
    /// Loss bursts: probability of a burst starting at each datagram, and
    /// how many datagrams a burst drops.
    pub burst: Option<(f64, u32)>,
    /// Added one-way delay.
    pub delay: Duration,
    /// Each datagram's delay varies by up to ± this (order kept unless reordered).
    pub jitter: Duration,
    /// Probability that a datagram is held back and overtaken by later ones.
    pub reorder: f64,
    /// Bottleneck rate: datagrams queue behind each other at this rate and
    /// are dropped when they would wait more than `queue` (tail drop).
    pub rate_bps: Option<u64>,
    pub queue: Duration,
    /// Everything is dropped from `outage.0` after the start, for `outage.1`.
    pub outage: Option<(Duration, Duration)>,
    pub seed: u64,
}

impl ImpairmentConfig {
    /// A bottleneck queue as deep as typical home routers' (~100 ms).
    pub const DEFAULT_QUEUE: Duration = Duration::from_millis(100);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImpairmentStats {
    pub received: u64,
    pub delivered: u64,
    pub dropped_loss: u64,
    pub dropped_burst: u64,
    pub dropped_queue: u64,
    pub dropped_outage: u64,
    pub reordered: u64,
}

impl ImpairmentStats {
    pub fn dropped(&self) -> u64 {
        self.dropped_loss + self.dropped_burst + self.dropped_queue + self.dropped_outage
    }
}

pub struct Impairment {
    config: ImpairmentConfig,
    rng: SplitMix64,
    started: Instant,
    burst_left: u32,
    /// Release time of the last in-order datagram (jitter keeps order).
    last_release: Instant,
    /// When the bottleneck link is free again.
    link_free_at: Instant,
    queue: BinaryHeap<Reverse<Queued>>,
    sequence: u64,
    stats: ImpairmentStats,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Queued {
    release: Instant,
    sequence: u64,
    from: SocketAddr,
    data: Vec<u8>,
}

impl Impairment {
    pub fn new(config: ImpairmentConfig, now: Instant) -> Self {
        Self {
            rng: SplitMix64(config.seed),
            config,
            started: now,
            burst_left: 0,
            last_release: now,
            link_free_at: now,
            queue: BinaryHeap::new(),
            sequence: 0,
            stats: ImpairmentStats::default(),
        }
    }

    pub fn stats(&self) -> ImpairmentStats {
        self.stats
    }

    /// Takes a datagram that just arrived: drops it or queues it for later.
    pub fn admit(&mut self, data: &[u8], from: SocketAddr, now: Instant) {
        self.stats.received += 1;
        if let Some((at, length)) = self.config.outage {
            let elapsed = now.saturating_duration_since(self.started);
            if elapsed >= at && elapsed < at + length {
                self.stats.dropped_outage += 1;
                return;
            }
        }
        if self.burst_left > 0 {
            self.burst_left -= 1;
            self.stats.dropped_burst += 1;
            return;
        }
        if let Some((chance, length)) = self.config.burst
            && self.rng.chance(chance)
        {
            self.burst_left = length.saturating_sub(1);
            self.stats.dropped_burst += 1;
            return;
        }
        if self.rng.chance(self.config.loss) {
            self.stats.dropped_loss += 1;
            return;
        }
        // Bottleneck: serialize behind the datagrams before it.
        let mut at = now;
        if let Some(rate) = self.config.rate_bps.filter(|&r| r > 0) {
            let start = self.link_free_at.max(now);
            if start.saturating_duration_since(now) > self.config.queue {
                self.stats.dropped_queue += 1;
                return;
            }
            let transmit = Duration::from_secs_f64(data.len() as f64 * 8.0 / rate as f64);
            self.link_free_at = start + transmit;
            at = self.link_free_at;
        }
        if self.queue.len() >= MAX_QUEUED {
            self.stats.dropped_queue += 1;
            return;
        }
        let jitter = self.config.jitter.as_secs_f64();
        let offset = if jitter > 0.0 {
            (self.rng.unit() * 2.0 - 1.0) * jitter
        } else {
            0.0
        };
        let delay = (self.config.delay.as_secs_f64() + offset).max(0.0);
        let mut release = at + Duration::from_secs_f64(delay);
        if self.rng.chance(self.config.reorder) {
            // Held back past the next few datagrams; does not move the order line.
            release += self.config.jitter.max(Duration::from_millis(5));
            self.stats.reordered += 1;
        } else {
            release = release.max(self.last_release);
            self.last_release = release;
        }
        self.sequence += 1;
        self.queue.push(Reverse(Queued {
            release,
            sequence: self.sequence,
            from,
            data: data.to_vec(),
        }));
    }

    /// When the next queued datagram is due.
    pub fn next_release(&self) -> Option<Instant> {
        self.queue.peek().map(|Reverse(q)| q.release)
    }

    /// Copies the next due datagram into `buf`.
    pub fn pop_due(&mut self, buf: &mut [u8], now: Instant) -> Option<(usize, SocketAddr)> {
        if self.next_release()? > now {
            return None;
        }
        let Reverse(queued) = self.queue.pop()?;
        let n = queued.data.len().min(buf.len());
        buf[..n].copy_from_slice(&queued.data[..n]);
        self.stats.delivered += 1;
        Some((n, queued.from))
    }
}

/// Small seeded generator (SplitMix64): enough for loss decisions, no
/// dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn chance(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.unit() < probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn from() -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }

    /// Admits `count` datagrams of `size` bytes, one every `spacing`, and
    /// returns the payload index of each delivered datagram in order, with
    /// its delivery time.
    fn run(
        config: ImpairmentConfig,
        count: u32,
        size: usize,
        spacing: Duration,
    ) -> (Vec<(u32, Duration)>, ImpairmentStats) {
        let t = Instant::now();
        let mut imp = Impairment::new(config, t);
        let mut out = Vec::new();
        let mut buf = vec![0_u8; 2000];
        let mut collect = |imp: &mut Impairment, now: Instant| {
            while let Some((n, _)) = imp.pop_due(&mut buf, now) {
                let index = u32::from_le_bytes(buf[..4].try_into().unwrap());
                assert_eq!(n, size);
                out.push((index, now - t));
            }
        };
        for i in 0..count {
            let now = t + spacing * i;
            collect(&mut imp, now);
            let mut data = vec![0_u8; size];
            data[..4].copy_from_slice(&i.to_le_bytes());
            imp.admit(&data, from(), now);
        }
        collect(&mut imp, t + spacing * count + Duration::from_secs(5));
        (out, imp.stats())
    }

    #[test]
    fn no_impairment_delivers_everything_in_order_at_once() {
        let (out, stats) = run(ImpairmentConfig::default(), 100, 100, MS);
        assert_eq!(out.len(), 100);
        assert!(out.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(stats.dropped(), 0);
    }

    #[test]
    fn random_loss_matches_the_rate_and_the_seed() {
        let config = ImpairmentConfig {
            loss: 0.05,
            seed: 42,
            ..Default::default()
        };
        let (a, stats) = run(config.clone(), 20_000, 100, MS);
        let rate = stats.dropped_loss as f64 / 20_000.0;
        assert!((0.04..0.06).contains(&rate), "loss {rate}");
        let (b, _) = run(config, 20_000, 100, MS);
        assert_eq!(a, b, "same seed, same decisions");
    }

    #[test]
    fn bursts_drop_consecutive_datagrams() {
        let config = ImpairmentConfig {
            burst: Some((0.001, 50)),
            seed: 7,
            ..Default::default()
        };
        let (out, stats) = run(config, 20_000, 100, MS);
        assert!(stats.dropped_burst >= 500, "{stats:?}");
        // Every gap in the delivered sequence is a whole burst (or bursts back to back).
        for w in out.windows(2) {
            let gap = w[1].0 - w[0].0 - 1;
            assert!(gap == 0 || gap >= 50, "gap of {gap}");
        }
    }

    #[test]
    fn delay_and_jitter_keep_the_order() {
        let config = ImpairmentConfig {
            delay: 50 * MS,
            jitter: 20 * MS,
            seed: 3,
            ..Default::default()
        };
        let (out, _) = run(config, 1000, 100, MS);
        assert_eq!(out.len(), 1000);
        assert!(out.windows(2).all(|w| w[0].0 < w[1].0), "reordered");
        // Delivered no earlier than delay - jitter after being sent.
        for (index, at) in &out {
            assert!(*at >= MS * *index + 30 * MS - MS);
        }
    }

    #[test]
    fn reordering_lets_later_datagrams_pass() {
        let config = ImpairmentConfig {
            reorder: 0.05,
            seed: 11,
            ..Default::default()
        };
        let (out, stats) = run(config, 2000, 100, MS);
        assert_eq!(out.len(), 2000);
        assert!(stats.reordered > 50);
        assert!(out.windows(2).any(|w| w[0].0 > w[1].0));
    }

    #[test]
    fn a_bottleneck_paces_and_drops_what_overflows_its_queue() {
        // 4 Mbps offered (1000 B / 2 ms) into 2 Mbps with a 100 ms queue.
        let config = ImpairmentConfig {
            rate_bps: Some(2_000_000),
            queue: ImpairmentConfig::DEFAULT_QUEUE,
            ..Default::default()
        };
        let (out, stats) = run(config, 2000, 1000, 2 * MS);
        // About half gets through, never faster than the link.
        let rate = out.len() as f64 / 2000.0;
        assert!((0.45..0.55).contains(&rate), "delivered {rate}");
        assert!(stats.dropped_queue > 800);
        let span = out.last().unwrap().1 - out.first().unwrap().1;
        let bps = (out.len() - 1) as f64 * 8000.0 / span.as_secs_f64();
        assert!(bps <= 2_050_000.0, "{bps} bps");
    }

    #[test]
    fn an_outage_drops_everything_inside_its_window() {
        let config = ImpairmentConfig {
            outage: Some((100 * MS, 200 * MS)),
            ..Default::default()
        };
        let (out, stats) = run(config, 500, 100, MS);
        assert_eq!(stats.dropped_outage, 200);
        assert!(out.iter().all(|(i, _)| *i < 100 || *i >= 300));
    }

    #[test]
    fn the_queue_is_bounded() {
        let config = ImpairmentConfig {
            delay: Duration::from_secs(10),
            ..Default::default()
        };
        let t = Instant::now();
        let mut imp = Impairment::new(config, t);
        for _ in 0..(MAX_QUEUED + 100) {
            imp.admit(&[0; 100], from(), t);
        }
        assert_eq!(imp.stats().dropped_queue, 100);
    }
}
