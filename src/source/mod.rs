//! Upstream data providers. Each one turns its native payload into metric
//! [`Observation`](crate::model::Observation) rows.

pub mod awc;
pub mod synoptic;

use std::time::Duration;

use anyhow::{Context, Result};

use crate::model::Batch;

pub enum AnySource {
    Awc(awc::AwcSource),
    Synoptic(synoptic::SynopticSource),
}

impl AnySource {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Awc(_) => "aviationweather.gov",
            Self::Synoptic(_) => "synopticdata.com",
        }
    }

    /// Every observation for `station` within the trailing `window`.
    pub async fn fetch(&self, station: &str, window: Duration) -> Result<Batch> {
        match self {
            Self::Awc(s) => s.fetch(station, window).await,
            Self::Synoptic(s) => s.fetch(station, window).await,
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
