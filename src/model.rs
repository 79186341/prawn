//! Normalized observation model.
//!
//! Every backend converts its native payload into [`Observation`], which is
//! always metric: degrees Celsius, metres per second, kilometres, hectopascals,
//! millimetres, centimetres and metres.

use chrono::{DateTime, Utc};
use serde::Serialize;

pub const KNOT_TO_MS: f64 = 0.514_444;
pub const STATUTE_MILE_TO_KM: f64 = 1.609_344;
pub const INCH_TO_MM: f64 = 25.4;
pub const INCH_TO_CM: f64 = 2.54;
pub const FOOT_TO_M: f64 = 0.3048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObsKind {
    /// Scheduled report (an hourly or half-hourly METAR, a mesonet interval).
    Routine,
    /// Unscheduled report issued because conditions changed (a SPECI).
    Special,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CloudLayer {
    pub cover: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StationInfo {
    pub id: String,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub elevation_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Observation {
    pub station: String,
    /// Observation (valid) time.
    pub time: DateTime<Utc>,
    /// When the upstream provider received the report, if it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub received: Option<DateTime<Utc>>,
    pub kind: ObsKind,
    pub air_temp_c: Option<f64>,
    pub dew_point_c: Option<f64>,
    pub relative_humidity_pct: Option<f64>,
    pub wind_direction_deg: Option<u16>,
    pub wind_variable: bool,
    pub wind_speed_ms: Option<f64>,
    pub wind_gust_ms: Option<f64>,
    pub visibility_km: Option<f64>,
    pub altimeter_hpa: Option<f64>,
    pub sea_level_pressure_hpa: Option<f64>,
    pub weather: Option<String>,
    pub clouds: Vec<CloudLayer>,
    pub precip_1h_mm: Option<f64>,
    pub precip_3h_mm: Option<f64>,
    pub precip_6h_mm: Option<f64>,
    pub precip_24h_mm: Option<f64>,
    pub snow_depth_cm: Option<f64>,
    /// Raw report text when the source provides one (the METAR string).
    pub raw: Option<String>,
}

impl Observation {
    pub fn new(station: impl Into<String>, time: DateTime<Utc>) -> Self {
        Self {
            station: station.into(),
            time,
            received: None,
            kind: ObsKind::Unknown,
            air_temp_c: None,
            dew_point_c: None,
            relative_humidity_pct: None,
            wind_direction_deg: None,
            wind_variable: false,
            wind_speed_ms: None,
            wind_gust_ms: None,
            visibility_km: None,
            altimeter_hpa: None,
            sea_level_pressure_hpa: None,
            weather: None,
            clouds: Vec::new(),
            precip_1h_mm: None,
            precip_3h_mm: None,
            precip_6h_mm: None,
            precip_24h_mm: None,
            snow_depth_cm: None,
            raw: None,
        }
    }

    /// Stable identity of the row's content, used to notice a correction
    /// re-issued for an observation time we already reported.
    pub fn fingerprint(&self) -> String {
        match &self.raw {
            Some(raw) => raw.clone(),
            None => serde_json::to_string(self).unwrap_or_default(),
        }
    }
}

/// Everything one fetch returned for a station.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub info: Option<StationInfo>,
    pub rows: Vec<Observation>,
}

/// Relative humidity in percent from air and dew-point temperature (Magnus formula).
pub fn relative_humidity(t_c: f64, td_c: f64) -> f64 {
    let e = |t: f64| (17.625 * t / (243.04 + t)).exp();
    (100.0 * e(td_c) / e(t_c)).clamp(0.0, 100.0)
}

pub fn round_to(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humidity_saturates_at_dew_point() {
        assert_eq!(round_to(relative_humidity(20.0, 20.0), 0), 100.0);
    }

    #[test]
    fn humidity_matches_reference_value() {
        // 21.1 C with a 6.1 C dew point is about 37-38 % RH.
        let rh = relative_humidity(21.1, 6.1);
        assert!((37.0..39.0).contains(&rh), "got {rh}");
    }
}
