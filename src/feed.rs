//! Per-station polling loop.
//!
//! 1. Prime: load history, learn the cadence (period + phase) and how long
//!    after the observation time reports usually appear.
//! 2. For each expected observation time: sleep until just before the report
//!    is likely to be out, then poll quickly until it shows up. If it never
//!    does, slow down and move on to the next slot; a late row is still
//!    reported the moment any later poll sees it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::debug;

use crate::model::{Batch, ObsKind, Observation, StationInfo};
use crate::schedule::{Cadence, LagModel, add, sub, until};
use crate::source::AnySource;

#[derive(Debug, Clone)]
pub struct FeedConfig {
    /// History window loaded at startup.
    pub history: Duration,
    /// Historical rows to replay at startup.
    pub replay: usize,
    /// Poll interval while a report is expected any moment.
    pub poll_interval: Duration,
    /// Poll interval once a report is overdue.
    pub slow_interval: Duration,
    /// Minimum length of the fast-polling window after the expected time.
    pub fast_window: Duration,
    /// Start polling this much before the shortest publish lag seen so far.
    pub lag_margin: Duration,
    /// Optional background poll interval while idle between expected times.
    pub idle_poll: Option<Duration>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Station {
        station: String,
        source: &'static str,
        info: StationInfo,
    },
    History {
        station: String,
        obs: Observation,
    },
    #[serde(rename = "cadence")]
    CadenceLearned {
        station: String,
        description: String,
        period_s: u64,
        phase_s: u64,
        agreement: f64,
        samples: usize,
        lag_min_s: Option<f64>,
        lag_median_s: Option<f64>,
        lag_max_s: Option<f64>,
    },
    NoCadence {
        station: String,
        rows: usize,
    },
    Waiting {
        station: String,
        expected: DateTime<Utc>,
        poll_from: DateTime<Utc>,
        /// Poll interval used once `poll_from` is reached.
        poll_every_s: u64,
    },
    #[serde(rename = "observation")]
    New {
        station: String,
        obs: Observation,
        first_seen: DateTime<Utc>,
        /// Seconds between the observation time and when this feed first saw it.
        latency_s: i64,
        expected: Option<DateTime<Utc>>,
        on_schedule: bool,
    },
    Corrected {
        station: String,
        obs: Observation,
        first_seen: DateTime<Utc>,
    },
    Missed {
        station: String,
        expected: DateTime<Utc>,
    },
    Error {
        station: String,
        message: String,
    },
}

impl Event {
    pub fn station(&self) -> &str {
        match self {
            Event::Station { station, .. }
            | Event::History { station, .. }
            | Event::CadenceLearned { station, .. }
            | Event::NoCadence { station, .. }
            | Event::Waiting { station, .. }
            | Event::New { station, .. }
            | Event::Corrected { station, .. }
            | Event::Missed { station, .. }
            | Event::Error { station, .. } => station,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Fast,
    Slow,
}

enum Seen {
    New,
    Same,
    Changed,
}

pub struct Feed {
    source: Arc<AnySource>,
    station: String,
    cfg: FeedConfig,
    tx: mpsc::Sender<Event>,
    /// Observation time -> content fingerprint of every row already reported.
    seen: HashMap<DateTime<Utc>, String>,
    lag: LagModel,
    cadence: Option<Cadence>,
    last_time: Option<DateTime<Utc>>,
    /// Window requested on each poll; must comfortably cover one period.
    poll_window: Duration,
}

impl Feed {
    pub fn new(
        source: Arc<AnySource>,
        station: String,
        cfg: FeedConfig,
        tx: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            source,
            station,
            cfg,
            tx,
            seen: HashMap::new(),
            lag: LagModel::default(),
            cadence: None,
            last_time: None,
            poll_window: Duration::from_secs(3 * 3600),
        }
    }

    fn st(&self) -> String {
        self.station.clone()
    }

    async fn emit(&self, event: Event) {
        // A closed receiver means the program is shutting down; nothing to do.
        let _ = self.tx.send(event).await;
    }

    /// Loads history, learns cadence and lag, and replays recent rows.
    /// Retries with backoff until the history fetch succeeds.
    pub async fn prime(&mut self) {
        let mut attempt: u32 = 0;
        let batch = loop {
            match self.source.fetch(&self.station, self.cfg.history).await {
                Ok(batch) => break batch,
                Err(err) => {
                    attempt += 1;
                    self.emit(Event::Error {
                        station: self.st(),
                        message: format!("history fetch failed (attempt {attempt}): {err:#}"),
                    })
                    .await;
                    tokio::time::sleep(backoff(Duration::from_secs(5), attempt)).await;
                }
            }
        };
        let Batch { info, mut rows } = batch;
        rows.sort_by_key(|o| o.time);
        if let Some(info) = info {
            self.emit(Event::Station {
                station: self.st(),
                source: self.source.name_for(&self.station),
                info,
            })
            .await;
        }
        for obs in &rows {
            self.seen.insert(obs.time, obs.fingerprint());
            if let Some(received) = obs.received
                && obs.kind != ObsKind::Special
            {
                self.lag.observe((received - obs.time).num_seconds() as f64);
            }
        }
        let skip = rows.len().saturating_sub(self.cfg.replay);
        for obs in rows.iter().skip(skip) {
            self.emit(Event::History {
                station: self.st(),
                obs: obs.clone(),
            })
            .await;
        }
        self.last_time = rows.last().map(|o| o.time);
        let routine: Vec<_> = rows
            .iter()
            .filter(|o| o.kind != ObsKind::Special)
            .map(|o| o.time)
            .collect();
        self.cadence = Cadence::infer(&routine);
        match self.cadence {
            Some(c) => {
                self.poll_window = self.poll_window.max(c.period * 3);
                self.emit(Event::CadenceLearned {
                    station: self.st(),
                    description: c.describe(),
                    period_s: c.period.as_secs(),
                    phase_s: c.phase.as_secs(),
                    agreement: c.agreement,
                    samples: c.samples,
                    lag_min_s: self.lag.min(),
                    lag_median_s: self.lag.median(),
                    lag_max_s: self.lag.max(),
                })
                .await;
            }
            None => {
                self.emit(Event::NoCadence {
                    station: self.st(),
                    rows: rows.len(),
                })
                .await;
            }
        }
    }

    pub async fn run(mut self) {
        match self.cadence {
            Some(cadence) => self.run_scheduled(cadence).await,
            None => self.run_unscheduled().await,
        }
    }

    /// Configured interval, raised to whatever the backend can usefully serve.
    fn poll_interval(&self, configured: Duration) -> Duration {
        configured.max(self.source.min_poll_interval(&self.station))
    }

    async fn run_scheduled(&mut self, cadence: Cadence) {
        let fast = self.poll_interval(self.cfg.poll_interval);
        let slow = self.poll_interval(self.cfg.slow_interval);
        loop {
            let now = Utc::now();
            let start_offset = self.lag.start_offset(self.cfg.lag_margin);
            // Never chase a slot older than one period plus the usual publish
            // lag (rows can lag by several periods on slow feeds); a later poll
            // still reports anything that turns up after that.
            let floor = sub(now, cadence.period + start_offset);
            let anchor = self.last_time.map_or(floor, |t| t.max(floor));
            let expected = cadence.next_after(anchor);
            let poll_from = add(expected, start_offset);
            let fast_until = add(
                expected,
                self.lag
                    .fast_window(self.cfg.lag_margin, self.cfg.fast_window),
            );
            let deadline = add(cadence.next_after(expected), start_offset);
            self.emit(Event::Waiting {
                station: self.st(),
                expected,
                poll_from,
                poll_every_s: fast.as_secs(),
            })
            .await;

            let mut errors: u32 = 0;
            loop {
                let now = Utc::now();
                let phase = if now < poll_from {
                    Phase::Idle
                } else if now < fast_until {
                    Phase::Fast
                } else {
                    Phase::Slow
                };
                if phase != Phase::Idle || self.cfg.idle_poll.is_some() {
                    match self.source.fetch(&self.station, self.poll_window).await {
                        Ok(batch) => {
                            errors = 0;
                            if self.ingest(batch.rows, Some(expected)).await {
                                break;
                            }
                        }
                        Err(err) => {
                            errors += 1;
                            self.emit(Event::Error {
                                station: self.st(),
                                message: format!("poll failed ({errors} in a row): {err:#}"),
                            })
                            .await;
                        }
                    }
                }
                let now = Utc::now();
                if now >= deadline {
                    self.emit(Event::Missed {
                        station: self.st(),
                        expected,
                    })
                    .await;
                    break;
                }
                let base = match phase {
                    Phase::Idle => {
                        let remaining = until(poll_from, now);
                        self.cfg.idle_poll.map_or(remaining, |d| d.min(remaining))
                    }
                    Phase::Fast => fast,
                    Phase::Slow => slow,
                };
                let wait = backoff(base, errors).min(until(deadline, now));
                debug!(station = %self.station, ?phase, ?wait, "sleeping");
                tokio::time::sleep(wait).await;
            }
        }
    }

    async fn run_unscheduled(&mut self) {
        let interval = self.poll_interval(self.cfg.slow_interval);
        let mut errors: u32 = 0;
        loop {
            match self.source.fetch(&self.station, self.poll_window).await {
                Ok(batch) => {
                    errors = 0;
                    self.ingest(batch.rows, None).await;
                }
                Err(err) => {
                    errors += 1;
                    self.emit(Event::Error {
                        station: self.st(),
                        message: format!("poll failed ({errors} in a row): {err:#}"),
                    })
                    .await;
                }
            }
            tokio::time::sleep(backoff(interval, errors)).await;
        }
    }

    /// Reports every row not seen before (and corrections of seen rows).
    /// Returns true once a row at or after `expected` has appeared.
    async fn ingest(
        &mut self,
        mut rows: Vec<Observation>,
        expected: Option<DateTime<Utc>>,
    ) -> bool {
        rows.sort_by_key(|o| o.time);
        let now = Utc::now();
        let mut found = false;
        for obs in rows {
            let fingerprint = obs.fingerprint();
            let status = match self.seen.get(&obs.time) {
                None => Seen::New,
                Some(prev) if *prev == fingerprint => Seen::Same,
                Some(_) => Seen::Changed,
            };
            match status {
                Seen::Same => continue,
                Seen::Changed => {
                    self.seen.insert(obs.time, fingerprint);
                    self.emit(Event::Corrected {
                        station: self.st(),
                        obs,
                        first_seen: now,
                    })
                    .await;
                }
                Seen::New => {
                    self.seen.insert(obs.time, fingerprint);
                    let latency_s = (now - obs.time).num_seconds();
                    if obs.kind != ObsKind::Special {
                        let sample = obs
                            .received
                            .map_or(latency_s as f64, |r| (r - obs.time).num_seconds() as f64);
                        self.lag.observe(sample);
                    }
                    let on_schedule = expected.is_some_and(|e| obs.time >= e);
                    found |= on_schedule;
                    self.last_time = Some(self.last_time.map_or(obs.time, |t| t.max(obs.time)));
                    self.emit(Event::New {
                        station: self.st(),
                        obs,
                        first_seen: now,
                        latency_s,
                        expected,
                        on_schedule,
                    })
                    .await;
                }
            }
        }
        let horizon = sub(now, self.cfg.history * 2);
        self.seen.retain(|t, _| *t >= horizon);
        found
    }
}

/// Doubles `base` per consecutive error, capped at five minutes.
fn backoff(base: Duration, errors: u32) -> Duration {
    if errors == 0 {
        return base;
    }
    let factor = 2u32.saturating_pow(errors.min(5));
    (base.max(Duration::from_secs(1)) * factor).min(Duration::from_secs(300))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_secs(20);
        assert_eq!(backoff(base, 0), base);
        assert_eq!(backoff(base, 1), Duration::from_secs(40));
        assert_eq!(backoff(base, 3), Duration::from_secs(160));
        assert_eq!(backoff(base, 10), Duration::from_secs(300));
        assert_eq!(backoff(Duration::ZERO, 1), Duration::from_secs(2));
    }
}
