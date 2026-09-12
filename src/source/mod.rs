//! Upstream data providers. Each one turns its native payload into metric
//! [`Observation`](crate::model::Observation) rows.

pub mod awc;
pub mod nws;
pub mod synoptic;

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::model::Batch;

const AWC_NAME: &str = "aviationweather.gov";
const NWS_NAME: &str = "api.weather.gov";

pub enum AnySource {
    Awc(awc::AwcSource),
    Nws(nws::NwsSource),
    Synoptic(synoptic::SynopticSource),
    /// NWS when it knows the station (US stations, with 5-minute ASOS rows),
    /// AWC otherwise. The choice is made on the first successful fetch.
    Auto {
        nws: nws::NwsSource,
        awc: awc::AwcSource,
        picks: Mutex<HashMap<String, Pick>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Nws,
    Awc,
}

impl AnySource {
    pub fn auto(client: reqwest::Client) -> Self {
        Self::Auto {
            nws: nws::NwsSource::new(client.clone()),
            awc: awc::AwcSource::new(client),
            picks: Mutex::new(HashMap::new()),
        }
    }

    fn pick_for(&self, station: &str) -> Option<Pick> {
        match self {
            Self::Awc(_) => Some(Pick::Awc),
            Self::Nws(_) => Some(Pick::Nws),
            Self::Synoptic(_) => None,
            Self::Auto { picks, .. } => picks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&station.to_ascii_uppercase())
                .copied(),
        }
    }

    /// Backend serving a station. For `Auto` this is settled by the first fetch.
    pub fn name_for(&self, station: &str) -> &'static str {
        match (self, self.pick_for(station)) {
            (Self::Synoptic(_), _) => "synopticdata.com",
            (_, Some(Pick::Nws)) => NWS_NAME,
            (_, Some(Pick::Awc)) => AWC_NAME,
            (_, None) => "auto (undecided)",
        }
    }

    /// Shortest poll interval that can still return something new.
    pub fn min_poll_interval(&self, station: &str) -> Duration {
        match self.pick_for(station) {
            Some(Pick::Nws) => nws::NwsSource::MIN_POLL_INTERVAL,
            _ => Duration::ZERO,
        }
    }

    /// Every observation for `station` within the trailing `window`.
    pub async fn fetch(&self, station: &str, window: Duration) -> Result<Batch> {
        match self {
            Self::Awc(s) => s.fetch(station, window).await,
            Self::Nws(s) => s.fetch(station, window).await,
            Self::Synoptic(s) => s.fetch(station, window).await,
            Self::Auto { nws, awc, picks } => {
                let key = station.to_ascii_uppercase();
                let record = |pick: Pick| {
                    picks
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(key.clone(), pick);
                };
                match self.pick_for(station) {
                    Some(Pick::Nws) => nws.fetch(station, window).await,
                    Some(Pick::Awc) => awc.fetch(station, window).await,
                    None => match nws.fetch(station, window).await {
                        Ok(batch) => {
                            record(Pick::Nws);
                            Ok(batch)
                        }
                        Err(err) if err.is::<nws::StationNotFound>() => {
                            record(Pick::Awc);
                            awc.fetch(station, window).await
                        }
                        Err(err) => Err(err),
                    },
                }
            }
        }
    }
}

pub fn http_client(user_agent: Option<&str>) -> Result<reqwest::Client> {
    let ua = user_agent.map(str::to_owned).unwrap_or_else(|| {
        format!(
            "prawn/{} (weather station feed prototype)",
            env!("CARGO_PKG_VERSION")
        )
    });
    reqwest::Client::builder()
        .user_agent(ua)
        .timeout(Duration::from_secs(20))
        .build()
        .context("building HTTP client")
}

pub(crate) fn snippet(s: &str) -> String {
    s.chars().take(200).collect()
}
