//! Interface statistics engine.
//!
//! Tracks, per interface (ASIC ports and kernel netdevs alike):
//! - cumulative counters, sampled every 5s by the collector task
//! - load-interval rates (exponentially weighted moving average whose
//!   time constant is the load interval, EOS-style)
//! - link-state change counts and the time of the last change
//! - `clear counters` baselines: reported counters are cumulative minus
//!   the baseline captured at the last clear
//!
//! The engine is pure bookkeeping over injected samples and timestamps,
//! so all the math is unit-testable without hardware or clocks.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Bits of framing overhead per frame that EOS includes in utilization:
/// 8 bytes preamble + 12 bytes inter-frame gap.
const FRAMING_OVERHEAD_BITS_PER_FRAME: f64 = 20.0 * 8.0;

/// The full counter set the engine tracks. Field-for-field the shape of
/// `hemlock.v1.InterfaceCounters`; sources fill what they have.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawCounters {
    pub in_pkts: u64,
    pub in_octets: u64,
    pub in_ucast_pkts: u64,
    pub in_mcast_pkts: u64,
    pub in_bcast_pkts: u64,
    pub in_discards: u64,
    pub in_errors: u64,
    pub in_crc_errors: u64,
    pub in_alignment_errors: u64,
    pub in_symbol_errors: u64,
    pub in_runts: u64,
    pub in_giants: u64,
    pub in_pause: u64,
    pub out_pkts: u64,
    pub out_octets: u64,
    pub out_ucast_pkts: u64,
    pub out_mcast_pkts: u64,
    pub out_bcast_pkts: u64,
    pub out_discards: u64,
    pub out_errors: u64,
    pub out_pause: u64,
    pub collisions: u64,
    pub late_collisions: u64,
    pub deferred: u64,
    pub rx_bins: [u64; 7],
    pub tx_bins: [u64; 7],
}

macro_rules! sub_fields {
    ($a:expr, $b:expr; $($field:ident),+; $($bins:ident),+) => {
        RawCounters {
            $($field: $a.$field.saturating_sub($b.$field),)+
            $($bins: std::array::from_fn(|i| $a.$bins[i].saturating_sub($b.$bins[i])),)+
        }
    };
}

impl RawCounters {
    /// `self - baseline`, saturating per field (a counter that wrapped or
    /// reset since the clear reads 0, never garbage).
    pub fn since(&self, baseline: &RawCounters) -> RawCounters {
        sub_fields!(self, baseline;
            in_pkts, in_octets, in_ucast_pkts, in_mcast_pkts, in_bcast_pkts,
            in_discards, in_errors, in_crc_errors, in_alignment_errors,
            in_symbol_errors, in_runts, in_giants, in_pause,
            out_pkts, out_octets, out_ucast_pkts, out_mcast_pkts,
            out_bcast_pkts, out_discards, out_errors, out_pause,
            collisions, late_collisions, deferred;
            rx_bins, tx_bins)
    }
}

impl From<hemlock_sai::PortCounters> for RawCounters {
    fn from(c: hemlock_sai::PortCounters) -> Self {
        RawCounters {
            // The ASIC has no total-packets counter; the total is the sum
            // of the cast classes.
            in_pkts: c.in_ucast_pkts + c.in_mcast_pkts + c.in_bcast_pkts,
            in_octets: c.in_octets,
            in_ucast_pkts: c.in_ucast_pkts,
            in_mcast_pkts: c.in_mcast_pkts,
            in_bcast_pkts: c.in_bcast_pkts,
            in_discards: c.in_discards,
            in_errors: c.in_errors,
            in_crc_errors: c.in_crc_errors,
            in_alignment_errors: c.in_alignment_errors,
            in_symbol_errors: c.in_symbol_errors,
            in_runts: c.in_runts,
            in_giants: c.in_giants,
            in_pause: c.in_pause,
            out_pkts: c.out_ucast_pkts + c.out_mcast_pkts + c.out_bcast_pkts,
            out_octets: c.out_octets,
            out_ucast_pkts: c.out_ucast_pkts,
            out_mcast_pkts: c.out_mcast_pkts,
            out_bcast_pkts: c.out_bcast_pkts,
            out_discards: c.out_discards,
            out_errors: c.out_errors,
            out_pause: c.out_pause,
            collisions: c.collisions,
            late_collisions: c.late_collisions,
            deferred: c.deferred,
            rx_bins: c.rx_bins,
            tx_bins: c.tx_bins,
        }
    }
}

/// One egress queue's counters, labeled (`UC0`, `MC0`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueSample {
    pub label: String,
    pub pkts: u64,
    pub bytes: u64,
    pub dropped_pkts: u64,
    pub dropped_bytes: u64,
}

/// EWMA rate state. The smoothing constant follows the load interval:
/// alpha = 1 - e^(-dt/T), the standard EOS/IOS load-interval decay.
#[derive(Debug, Clone, Copy, Default)]
struct RateEwma {
    in_bps: f64,
    in_pps: f64,
    out_bps: f64,
    out_pps: f64,
    last: Option<(Instant, u64, u64, u64, u64)>, // t, in_oct, in_pkts, out_oct, out_pkts
}

impl RateEwma {
    fn sample(&mut self, now: Instant, c: &RawCounters, interval_secs: u32) {
        let Some((then, in_oct, in_pkts, out_oct, out_pkts)) = self.last else {
            self.last = Some((now, c.in_octets, c.in_pkts, c.out_octets, c.out_pkts));
            return;
        };
        let dt = now.duration_since(then).as_secs_f64();
        if dt <= 0.0 {
            return;
        }
        let inst = |now_v: u64, then_v: u64| now_v.saturating_sub(then_v) as f64 / dt;
        let alpha = 1.0 - (-dt / f64::from(interval_secs.max(1))).exp();
        let blend = |rate: &mut f64, inst: f64| *rate += alpha * (inst - *rate);
        blend(&mut self.in_bps, inst(c.in_octets, in_oct) * 8.0);
        blend(&mut self.in_pps, inst(c.in_pkts, in_pkts));
        blend(&mut self.out_bps, inst(c.out_octets, out_oct) * 8.0);
        blend(&mut self.out_pps, inst(c.out_pkts, out_pkts));
        self.last = Some((now, c.in_octets, c.in_pkts, c.out_octets, c.out_pkts));
    }
}

/// Utilization vs line speed including the per-frame preamble + IFG
/// overhead, in percent. 0 when the speed is unknown.
pub fn utilization_pct(bps: f64, pps: f64, speed_mbps: u64) -> f64 {
    if speed_mbps == 0 {
        return 0.0;
    }
    let line_bps = speed_mbps as f64 * 1e6;
    ((bps + pps * FRAMING_OVERHEAD_BITS_PER_FRAME) / line_bps * 100.0).max(0.0)
}

/// Everything the engine knows about one interface.
#[derive(Debug, Default)]
pub struct Tracked {
    counters: RawCounters,
    queues: Vec<QueueSample>,
    baseline: RawCounters,
    baseline_queues: Vec<QueueSample>,
    cleared_at: Option<Instant>,
    link_changes: u64,
    oper_up: Option<bool>,
    last_change: Option<Instant>,
    rate: RateEwma,
    pub load_interval_secs: u32,
}

/// The read-side snapshot handed to the gRPC service.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub counters: RawCounters,
    pub queues: Vec<QueueSample>,
    pub link_changes: u64,
    pub seconds_since_change: Option<u64>,
    pub seconds_since_clear: Option<u64>,
    pub load_interval_secs: u32,
    pub in_bps: f64,
    pub in_pps: f64,
    pub out_bps: f64,
    pub out_pps: f64,
}

#[derive(Debug, Default)]
pub struct Engine {
    table: RwLock<HashMap<String, Tracked>>,
    default_load_interval: u32,
}

pub type SharedEngine = Arc<Engine>;

impl Engine {
    pub fn new(default_load_interval: u32) -> SharedEngine {
        Arc::new(Engine {
            table: RwLock::new(HashMap::new()),
            default_load_interval,
        })
    }

    /// Feed one counter sample. Creates the interface on first sight.
    pub fn ingest(
        &self,
        name: &str,
        counters: RawCounters,
        queues: Vec<QueueSample>,
        now: Instant,
    ) {
        let Ok(mut table) = self.table.write() else {
            return;
        };
        let default_interval = self.default_load_interval;
        let entry = table.entry(name.to_string()).or_insert_with(|| Tracked {
            load_interval_secs: default_interval,
            ..Tracked::default()
        });
        entry.counters = counters;
        entry.queues = queues;
        entry.rate.sample(now, &counters, entry.load_interval_secs);
    }

    /// Record the interface's oper state; counts a change when it
    /// differs from the last recorded state. Safe to call from both the
    /// event pump and the sampler (state comparison dedups).
    pub fn note_link(&self, name: &str, up: bool, now: Instant) {
        let Ok(mut table) = self.table.write() else {
            return;
        };
        let default_interval = self.default_load_interval;
        let entry = table.entry(name.to_string()).or_insert_with(|| Tracked {
            load_interval_secs: default_interval,
            ..Tracked::default()
        });
        match entry.oper_up {
            Some(previous) if previous == up => {}
            Some(_) => {
                entry.link_changes += 1;
                entry.last_change = Some(now);
                entry.oper_up = Some(up);
            }
            // First observation is the initial state, not a change.
            None => {
                entry.oper_up = Some(up);
                if entry.last_change.is_none() {
                    entry.last_change = Some(now);
                }
            }
        }
    }

    /// Capture `clear counters` baselines. Empty `names` = all tracked.
    /// Returns how many interfaces were cleared.
    pub fn clear(&self, names: &[String], now: Instant) -> u32 {
        let Ok(mut table) = self.table.write() else {
            return 0;
        };
        let mut cleared = 0;
        for (name, entry) in table.iter_mut() {
            if !names.is_empty() && !names.iter().any(|n| n == name) {
                continue;
            }
            entry.baseline = entry.counters;
            entry.baseline_queues = entry.queues.clone();
            entry.cleared_at = Some(now);
            entry.link_changes = 0;
            cleared += 1;
        }
        cleared
    }

    /// Read-side snapshot for one interface.
    pub fn snapshot(&self, name: &str, now: Instant) -> Option<Snapshot> {
        let table = self.table.read().ok()?;
        let entry = table.get(name)?;
        let queues = entry
            .queues
            .iter()
            .map(|q| {
                let base = entry
                    .baseline_queues
                    .iter()
                    .find(|b| b.label == q.label)
                    .cloned()
                    .unwrap_or_default();
                QueueSample {
                    label: q.label.clone(),
                    pkts: q.pkts.saturating_sub(base.pkts),
                    bytes: q.bytes.saturating_sub(base.bytes),
                    dropped_pkts: q.dropped_pkts.saturating_sub(base.dropped_pkts),
                    dropped_bytes: q.dropped_bytes.saturating_sub(base.dropped_bytes),
                }
            })
            .collect();
        Some(Snapshot {
            counters: entry.counters.since(&entry.baseline),
            queues,
            link_changes: entry.link_changes,
            seconds_since_change: entry.last_change.map(|t| now.duration_since(t).as_secs()),
            seconds_since_clear: entry.cleared_at.map(|t| now.duration_since(t).as_secs()),
            load_interval_secs: entry.load_interval_secs,
            in_bps: entry.rate.in_bps,
            in_pps: entry.rate.in_pps,
            out_bps: entry.rate.out_bps,
            out_pps: entry.rate.out_pps,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn octets(in_octets: u64, in_pkts: u64) -> RawCounters {
        RawCounters {
            in_octets,
            in_pkts,
            ..RawCounters::default()
        }
    }

    #[test]
    fn ewma_converges_to_a_steady_rate() {
        let engine = Engine::new(300);
        let t0 = Instant::now();
        // 125_000 bytes / 5s = 25 kB/s = 200 kbit/s steady.
        for i in 0..2000u64 {
            engine.ingest(
                "Ethernet1",
                octets(i * 125_000, i * 100),
                vec![],
                t0 + Duration::from_secs(i * 5),
            );
        }
        let snap = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(2000 * 5))
            .unwrap();
        assert!(
            (snap.in_bps - 200_000.0).abs() < 1_000.0,
            "in_bps = {}",
            snap.in_bps
        );
        assert!((snap.in_pps - 20.0).abs() < 0.5, "in_pps = {}", snap.in_pps);
    }

    #[test]
    fn ewma_decays_toward_zero_when_traffic_stops() {
        let engine = Engine::new(300);
        let t0 = Instant::now();
        for i in 0..200u64 {
            engine.ingest(
                "Ethernet1",
                octets(i * 1_000_000, i * 1000),
                vec![],
                t0 + Duration::from_secs(i * 5),
            );
        }
        let busy = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(1000))
            .unwrap()
            .in_bps;
        // Silence for 4 load intervals: the EWMA must collapse.
        for i in 200..440u64 {
            engine.ingest(
                "Ethernet1",
                octets(199 * 1_000_000, 199 * 1000),
                vec![],
                t0 + Duration::from_secs(i * 5),
            );
        }
        let idle = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(440 * 5))
            .unwrap()
            .in_bps;
        assert!(idle < busy / 20.0, "busy {busy} -> idle {idle}");
    }

    #[test]
    fn utilization_includes_framing_overhead() {
        // 24.7 Mbps + 4123 pps * 160 bits = 25.36 Mbps of 1G = 2.54%.
        let pct = utilization_pct(24_700_000.0, 4123.0, 1000);
        assert!((pct - 2.536).abs() < 0.01, "pct = {pct}");
        // Zero-speed interfaces report 0, never NaN/inf.
        assert_eq!(utilization_pct(1e9, 1e6, 0), 0.0);
        // Overhead matters: a minimum-size-frame flood at line rate
        // exceeds the payload-only percentage.
        let no_overhead = 500_000_000.0 / 1e9 * 100.0;
        assert!(utilization_pct(500_000_000.0, 744_047.0, 1000) > no_overhead);
    }

    #[test]
    fn clear_baselines_counters_and_link_changes() {
        let engine = Engine::new(300);
        let t0 = Instant::now();
        engine.note_link("Ethernet1", true, t0);
        engine.ingest("Ethernet1", octets(1000, 10), vec![], t0);
        engine.note_link("Ethernet1", false, t0 + Duration::from_secs(1));
        engine.note_link("Ethernet1", true, t0 + Duration::from_secs(2));

        let before = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(3))
            .unwrap();
        assert_eq!(before.counters.in_octets, 1000);
        assert_eq!(before.link_changes, 2);
        assert_eq!(before.seconds_since_clear, None, "never cleared");

        let cleared = engine.clear(&[], t0 + Duration::from_secs(3));
        assert_eq!(cleared, 1);
        engine.ingest(
            "Ethernet1",
            octets(1500, 15),
            vec![],
            t0 + Duration::from_secs(5),
        );
        let after = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(6))
            .unwrap();
        assert_eq!(after.counters.in_octets, 500, "baselined");
        assert_eq!(after.link_changes, 0, "changes reset by clear");
        assert_eq!(after.seconds_since_clear, Some(3));
    }

    #[test]
    fn named_clear_leaves_other_interfaces_alone() {
        let engine = Engine::new(300);
        let t0 = Instant::now();
        engine.ingest("Ethernet1", octets(1000, 1), vec![], t0);
        engine.ingest("Ethernet2", octets(2000, 2), vec![], t0);
        engine.clear(&["Ethernet1".into()], t0);
        assert_eq!(
            engine.snapshot("Ethernet1", t0).unwrap().counters.in_octets,
            0
        );
        assert_eq!(
            engine.snapshot("Ethernet2", t0).unwrap().counters.in_octets,
            2000
        );
    }

    #[test]
    fn first_link_observation_is_not_a_change() {
        let engine = Engine::new(300);
        let t0 = Instant::now();
        engine.note_link("Ethernet1", true, t0);
        engine.note_link("Ethernet1", true, t0 + Duration::from_secs(5));
        let snap = engine
            .snapshot("Ethernet1", t0 + Duration::from_secs(10))
            .unwrap();
        assert_eq!(snap.link_changes, 0);
        assert_eq!(snap.seconds_since_change, Some(10));
    }

    #[test]
    fn baseline_subtraction_saturates_after_counter_reset() {
        let a = octets(100, 1);
        let b = octets(500, 5);
        let diff = a.since(&b);
        assert_eq!(diff.in_octets, 0);
        assert_eq!(diff.in_pkts, 0);
    }
}
