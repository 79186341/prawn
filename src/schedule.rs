//! Cadence inference and publish-lag tracking.
//!
//! A station's *cadence* is the period between its scheduled reports plus a
//! phase (where in that period the report lands, e.g. hourly at :54). The
//! *lag* is how long after the observation time a report becomes visible
//! upstream; polling starts a little before the shortest lag seen so far.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cadence {
    pub period: Duration,
    pub phase: Duration,
    /// Fraction of the observed intervals that matched `period`.
    pub agreement: f64,
    pub samples: usize,
}

impl Cadence {
    pub const MIN_AGREEMENT: f64 = 0.5;

    /// Infers the cadence from observation times. Returns `None` when there
    /// are too few rows or no interval clearly dominates.
    pub fn infer(times: &[DateTime<Utc>]) -> Option<Self> {
        let mut secs: Vec<i64> = times.iter().map(DateTime::timestamp).collect();
        secs.sort_unstable();
        secs.dedup();
        if secs.len() < 3 {
            return None;
        }
        let deltas: Vec<i64> = secs
            .windows(2)
            .map(|w| round_to_minute(w[1] - w[0]))
            .filter(|d| *d > 0)
            .collect();
        let (period, count) = mode(&deltas)?;
        let agreement = count as f64 / deltas.len() as f64;
        if count < 2 || agreement < Self::MIN_AGREEMENT {
            return None;
        }
        let phases: Vec<i64> = secs
            .iter()
            .map(|s| round_to_minute(s.rem_euclid(period)) % period)
            .collect();
        let (phase, _) = mode(&phases)?;
        Some(Self {
            period: Duration::from_secs(period as u64),
            phase: Duration::from_secs(phase as u64),
            agreement,
            samples: deltas.len(),
        })
    }

    /// First scheduled observation time strictly after `t`.
    pub fn next_after(&self, t: DateTime<Utc>) -> DateTime<Utc> {
        let period = self.period.as_secs() as i64;
        let phase = self.phase.as_secs() as i64;
        let s = t.timestamp();
        let mut next = s - s.rem_euclid(period) + phase;
        if next <= s {
            next += period;
        }
        Utc.timestamp_opt(next, 0).single().unwrap_or(t)
    }

    pub fn describe(&self) -> String {
        let period_min = self.period.as_secs() / 60;
        let phase_min = (self.phase.as_secs() / 60) % 60;
        format!("every {period_min} min, aligned to :{phase_min:02}")
    }
}

fn round_to_minute(secs: i64) -> i64 {
    ((secs as f64 / 60.0).round() as i64) * 60
}

/// Most frequent value and its count. Ties go to the smaller value, so an
/// ambiguous cadence errs toward polling more often rather than less.
fn mode(values: &[i64]) -> Option<(i64, usize)> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for v in values {
        *counts.entry(*v).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
}

/// Rolling record of how long after its observation time a report appeared.
#[derive(Debug, Clone, Default)]
pub struct LagModel {
    samples: VecDeque<f64>,
}

impl LagModel {
    const CAPACITY: usize = 64;

    pub fn observe(&mut self, secs: f64) {
        if !secs.is_finite() || !(0.0..=86_400.0).contains(&secs) {
            return;
        }
        if self.samples.len() == Self::CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(secs);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn min(&self) -> Option<f64> {
        self.samples.iter().copied().reduce(f64::min)
    }

    pub fn max(&self) -> Option<f64> {
        self.samples.iter().copied().reduce(f64::max)
    }

    pub fn median(&self) -> Option<f64> {
        let mut v: Vec<f64> = self.samples.iter().copied().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        Some(v[v.len() / 2])
    }

    /// How long after the expected observation time to start polling:
    /// the shortest lag seen so far minus a safety margin, never negative.
    /// With no samples yet, poll from the observation time itself.
    pub fn start_offset(&self, margin: Duration) -> Duration {
        match self.min() {
            Some(min) => Duration::from_secs_f64((min - margin.as_secs_f64()).max(0.0)),
            None => Duration::ZERO,
        }
    }

    /// How long after the expected time to keep polling at the fast rate:
    /// at least `floor`, and at least the longest lag seen plus the margin.
    pub fn fast_window(&self, margin: Duration, floor: Duration) -> Duration {
        match self.max() {
            Some(max) => floor.max(Duration::from_secs_f64(max) + margin),
            None => floor,
        }
    }
}

pub fn add(t: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    t + TimeDelta::from_std(d).unwrap_or_else(|_| TimeDelta::zero())
}

pub fn sub(t: DateTime<Utc>, d: Duration) -> DateTime<Utc> {
    t - TimeDelta::from_std(d).unwrap_or_else(|_| TimeDelta::zero())
}

/// Time from `now` until `t`, or zero if `t` has passed.
pub fn until(t: DateTime<Utc>, now: DateTime<Utc>) -> Duration {
    (t - now).to_std().unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, h, m, 0).unwrap()
    }

    #[test]
    fn half_hourly_metar() {
        let times: Vec<_> = (0..48)
            .map(|i| at(0, 0) + TimeDelta::minutes(30 * i))
            .collect();
        let c = Cadence::infer(&times).unwrap();
        assert_eq!(c.period, Duration::from_secs(1800));
        assert_eq!(c.phase, Duration::ZERO);
        assert_eq!(c.agreement, 1.0);
        assert_eq!(c.next_after(at(15, 7)), at(15, 30));
        assert_eq!(c.next_after(at(15, 30)), at(16, 0));
        assert_eq!(c.describe(), "every 30 min, aligned to :00");
    }

    #[test]
    fn hourly_at_fifty_four() {
        let times: Vec<_> = (0..24).map(|i| at(0, 54) + TimeDelta::hours(i)).collect();
        let c = Cadence::infer(&times).unwrap();
        assert_eq!(c.period, Duration::from_secs(3600));
        assert_eq!(c.phase, Duration::from_secs(54 * 60));
        assert_eq!(c.next_after(at(15, 10)), at(15, 54));
        assert_eq!(c.next_after(at(15, 56)), at(16, 54));
        assert_eq!(c.describe(), "every 60 min, aligned to :54");
    }

    #[test]
    fn five_minute_mesonet() {
        let times: Vec<_> = (0..100)
            .map(|i| at(0, 0) + TimeDelta::minutes(5 * i))
            .collect();
        let c = Cadence::infer(&times).unwrap();
        assert_eq!(c.period, Duration::from_secs(300));
        assert_eq!(c.next_after(at(8, 12)), at(8, 15));
    }

    #[test]
    fn specials_do_not_hide_the_hourly_schedule() {
        let mut times: Vec<_> = (0..24).map(|i| at(0, 54) + TimeDelta::hours(i)).collect();
        times.extend([at(3, 17), at(9, 33), at(9, 41)]);
        let c = Cadence::infer(&times).unwrap();
        assert_eq!(c.period, Duration::from_secs(3600));
        assert_eq!(c.phase, Duration::from_secs(54 * 60));
        assert!(c.agreement < 1.0);
    }

    #[test]
    fn too_little_history() {
        assert!(Cadence::infer(&[at(1, 0), at(2, 0)]).is_none());
        assert!(Cadence::infer(&[]).is_none());
    }

    #[test]
    fn irregular_times_have_no_cadence() {
        let times = [at(0, 0), at(0, 7), at(0, 31), at(1, 2), at(1, 50), at(2, 3)];
        assert!(Cadence::infer(&times).is_none());
    }

    #[test]
    fn lag_model_offsets() {
        let margin = Duration::from_secs(60);
        let mut lag = LagModel::default();
        assert_eq!(lag.start_offset(margin), Duration::ZERO);
        assert_eq!(
            lag.fast_window(margin, Duration::from_secs(600)),
            Duration::from_secs(600)
        );
        for s in [329.0, 483.0, 635.0] {
            lag.observe(s);
        }
        assert_eq!(lag.min(), Some(329.0));
        assert_eq!(lag.median(), Some(483.0));
        assert_eq!(lag.start_offset(margin), Duration::from_secs(269));
        assert_eq!(
            lag.fast_window(margin, Duration::from_secs(600)),
            Duration::from_secs(695)
        );
        lag.observe(-5.0);
        lag.observe(f64::NAN);
        assert_eq!(lag.len(), 3);
        let mut tiny = LagModel::default();
        tiny.observe(10.0);
        assert_eq!(tiny.start_offset(margin), Duration::ZERO);
    }

    #[test]
    fn until_never_goes_negative() {
        assert_eq!(until(at(1, 0), at(2, 0)), Duration::ZERO);
        assert_eq!(until(at(2, 0), at(1, 0)), Duration::from_secs(3600));
    }
}
