//! Embedding throughput measured where the work happens.
//!
//! `status` used to derive chunks/sec from the moments a client happened to
//! poll, while the queue only advances in whole committed batches (128
//! items). Two polls 40 s apart showed 0 or 256, so the rate swung between 1
//! and 20 chunks/s and the ETA between 5 and 374 minutes. Every committed
//! batch is now recorded here with its wall-clock span, so the rate is the
//! real throughput over a fixed window — idle gaps included — whoever polls,
//! and however often.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Window the reported rate and batch latency are averaged over.
pub const THROUGHPUT_WINDOW: Duration = Duration::from_secs(300);

/// Hard bound on retained samples (a batch every 100 ms for the full window).
const MAX_SAMPLES: usize = 3_000;

#[derive(Debug, Clone, Copy)]
struct Sample {
    start: Instant,
    end: Instant,
    items: u64,
}

static SAMPLES: Mutex<VecDeque<Sample>> = Mutex::new(VecDeque::new());

/// Measured throughput over [`THROUGHPUT_WINDOW`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Throughput {
    /// Committed chunks per second of wall-clock time (gaps included).
    pub chunks_per_sec: f64,
    /// Mean measured wall time of one committed batch (claim to commit).
    pub batch_latency_ms: f64,
    /// Seconds since the last committed batch, if any batch committed yet.
    pub secs_since_last_commit: Option<u64>,
}

/// Record one committed batch of `items` that took `busy` from claim to
/// commit and finished now.
pub fn record_commit(items: u64, busy: Duration) {
    if items == 0 {
        return;
    }
    let end = Instant::now();
    let start = end.checked_sub(busy).unwrap_or(end);
    let mut s = SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    s.push_back(Sample { start, end, items });
    while s.len() > MAX_SAMPLES {
        s.pop_front();
    }
}

/// Current measured throughput.
pub fn snapshot() -> Throughput {
    let mut s = SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    while s
        .front()
        .is_some_and(|x| now.duration_since(x.end) > THROUGHPUT_WINDOW)
        && s.len() > 1
    {
        s.pop_front();
    }
    compute(s.iter().copied(), now, THROUGHPUT_WINDOW)
}

fn compute(samples: impl Iterator<Item = Sample>, now: Instant, window: Duration) -> Throughput {
    let mut items = 0u64;
    let mut batches = 0u64;
    let mut busy = Duration::ZERO;
    let mut first_start: Option<Instant> = None;
    let mut last_end: Option<Instant> = None;
    for x in samples {
        last_end = Some(last_end.map_or(x.end, |l| l.max(x.end)));
        if now.duration_since(x.end) > window {
            continue;
        }
        items += x.items;
        batches += 1;
        busy += x.end.duration_since(x.start);
        first_start = Some(first_start.map_or(x.start, |f| f.min(x.start)));
    }
    let span = first_start
        .map(|f| now.duration_since(f).min(window).as_secs_f64())
        .unwrap_or(0.0);
    Throughput {
        chunks_per_sec: if span > 0.0 { items as f64 / span } else { 0.0 },
        batch_latency_ms: if batches > 0 {
            busy.as_secs_f64() * 1000.0 / batches as f64
        } else {
            0.0
        },
        secs_since_last_commit: last_end.map(|l| now.duration_since(l).as_secs()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(now: Instant, ago_end: u64, busy: u64, items: u64) -> Sample {
        let end = now - Duration::from_secs(ago_end);
        Sample {
            start: end - Duration::from_secs(busy),
            end,
            items,
        }
    }

    #[test]
    fn rate_is_items_over_wall_time_including_idle_gaps() {
        let now = Instant::now() + Duration::from_secs(1_000);
        // Eight 128-item batches of 8 s each, one every 20 s (12 s idle in
        // between): real throughput is 128 / 20 = 6.4 chunks/s, not the
        // 16 chunks/s a single busy batch suggests.
        let samples: Vec<Sample> = (0..8).map(|i| sample(now, i * 20, 8, 128)).collect();
        let t = compute(samples.into_iter(), now, Duration::from_secs(300));
        let expected = (8.0 * 128.0) / (7.0 * 20.0 + 8.0);
        assert!((t.chunks_per_sec - expected).abs() < 0.01, "{t:?}");
        assert!((t.batch_latency_ms - 8_000.0).abs() < 1.0, "{t:?}");
        assert_eq!(t.secs_since_last_commit, Some(0));
    }

    #[test]
    fn mid_batch_poll_does_not_report_zero() {
        let now = Instant::now() + Duration::from_secs(1_000);
        // Last commit 40 s ago, a batch is in flight: the rate decays
        // smoothly instead of dropping to 0.
        let samples = vec![sample(now, 60, 8, 128), sample(now, 40, 8, 128)];
        let t = compute(samples.into_iter(), now, Duration::from_secs(300));
        assert!(t.chunks_per_sec > 0.0);
        assert_eq!(t.secs_since_last_commit, Some(40));
    }

    #[test]
    fn samples_outside_the_window_do_not_count() {
        let now = Instant::now() + Duration::from_secs(10_000);
        let samples = vec![sample(now, 4_000, 8, 128)];
        let t = compute(samples.into_iter(), now, Duration::from_secs(300));
        assert_eq!(t.chunks_per_sec, 0.0);
        assert_eq!(t.secs_since_last_commit, Some(4_000));
    }
}
