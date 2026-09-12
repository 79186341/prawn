//! NWS API backend (api.weather.gov). Token-free, US stations only.
//!
//! For ASOS airports this carries the 5-minute observations the weather.gov
//! timeseries page shows, not only the hourly METARs. Responses are cached
//! upstream for about two minutes, so the `start` parameter is truncated to
//! the minute: polls within one minute share a URL (and a cached answer), and
//! each new minute is a fresh query.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, DurationRound, TimeDelta, Utc};
use serde::Deserialize;
use tracing::debug;

use super::snippet;
use crate::model::{
    Batch, CloudLayer, FOOT_TO_M, INCH_TO_MM, KNOT_TO_MS, ObsKind, Observation, STATUTE_MILE_TO_KM,
    StationInfo, relative_humidity, round_to,
};

const ENDPOINT: &str = "https://api.weather.gov";

/// The NWS API has no such station; callers may fall back to another backend.
#[derive(Debug)]
pub struct StationNotFound(pub String);

impl fmt::Display for StationNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the NWS API has no station {}", self.0)
    }
}

impl std::error::Error for StationNotFound {}

pub struct NwsSource {
    client: reqwest::Client,
    stations: Mutex<HashMap<String, StationInfo>>,
}

impl NwsSource {
    /// Upstream caches observation lists for about two minutes, so polling
    /// more often than once a minute cannot return anything new.
    pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(60);

    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            stations: Mutex::new(HashMap::new()),
        }
    }

    pub async fn fetch(&self, station: &str, window: Duration) -> Result<Batch> {
        let id = station.to_ascii_uppercase();
        let info = self.station_info(&id).await?;
        let span = TimeDelta::from_std(window).unwrap_or_else(|_| TimeDelta::zero());
        let start = (Utc::now() - span)
            .duration_trunc(TimeDelta::minutes(1))
            .context("truncating start time")?;
        let url = format!(
            "{ENDPOINT}/stations/{id}/observations?start={}&limit=500",
            start.format("%Y-%m-%dT%H:%M:%SZ")
        );
        let body = self.get(&id, &url).await?;
        let mut batch = parse_observations(&id, &body)?;
        batch.info = Some(info);
        Ok(batch)
    }

    async fn get(&self, station: &str, url: &str) -> Result<String> {
        debug!(%url, "GET");
        let resp = self
            .client
            .get(url)
            .header("Accept", "application/geo+json")
            .send()
            .await
            .context("requesting NWS API")?;
        let status = resp.status();
        let body = resp.text().await.context("reading NWS response body")?;
        if status == reqwest::StatusCode::NOT_FOUND {
            bail!(StationNotFound(station.to_owned()));
        }
        if !status.is_success() {
            bail!("NWS API returned HTTP {status}: {}", snippet(&body));
        }
        Ok(body)
    }

    async fn station_info(&self, id: &str) -> Result<StationInfo> {
        let cached = self
            .stations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned();
        if let Some(info) = cached {
            return Ok(info);
        }
        let body = self.get(id, &format!("{ENDPOINT}/stations/{id}")).await?;
        let info = parse_station(id, &body)?;
        self.stations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), info.clone());
        Ok(info)
    }
}

#[derive(Debug, Deserialize)]
struct StationDoc {
    properties: StationProps,
    geometry: Option<Geometry>,
}

#[derive(Debug, Deserialize)]
struct StationProps {
    #[serde(rename = "stationIdentifier")]
    id: Option<String>,
    name: Option<String>,
    elevation: Option<Quantity>,
}

#[derive(Debug, Deserialize)]
struct Geometry {
    coordinates: Option<Vec<f64>>,
}

pub fn parse_station(id: &str, body: &str) -> Result<StationInfo> {
    let doc: StationDoc = serde_json::from_str(body).context("decoding NWS station")?;
    let coords = doc.geometry.and_then(|g| g.coordinates).unwrap_or_default();
    Ok(StationInfo {
        id: doc.properties.id.unwrap_or_else(|| id.to_owned()),
        name: doc.properties.name,
        lat: coords.get(1).copied(),
        lon: coords.first().copied(),
        elevation_m: doc
            .properties
            .elevation
            .as_ref()
            .and_then(Quantity::value)
            .map(|(v, unit)| round_to(length_m(v, unit), 1)),
    })
}

#[derive(Debug, Deserialize)]
struct Collection {
    #[serde(default)]
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    properties: Props,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Props {
    timestamp: Option<String>,
    raw_message: Option<String>,
    temperature: Option<Quantity>,
    dewpoint: Option<Quantity>,
    wind_direction: Option<Quantity>,
    wind_speed: Option<Quantity>,
    wind_gust: Option<Quantity>,
    barometric_pressure: Option<Quantity>,
    sea_level_pressure: Option<Quantity>,
    visibility: Option<Quantity>,
    relative_humidity: Option<Quantity>,
    precipitation_last_hour: Option<Quantity>,
    precipitation_last3_hours: Option<Quantity>,
    precipitation_last6_hours: Option<Quantity>,
    cloud_layers: Option<Vec<Layer>>,
    present_weather: Option<Vec<Weather>>,
}

/// NWS measurement: a value, a WMO unit code and a quality-control flag.
#[derive(Debug, Deserialize)]
struct Quantity {
    #[serde(rename = "unitCode")]
    unit_code: Option<String>,
    value: Option<f64>,
    #[serde(rename = "qualityControl")]
    quality_control: Option<String>,
}

impl Quantity {
    /// The value with its unit, unless quality control rejected it ("X").
    fn value(&self) -> Option<(f64, &str)> {
        if self.quality_control.as_deref() == Some("X") {
            return None;
        }
        Some((self.value?, self.unit_code.as_deref().unwrap_or("")))
    }
}

#[derive(Debug, Deserialize)]
struct Layer {
    base: Option<Quantity>,
    amount: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Weather {
    #[serde(rename = "rawString")]
    raw_string: Option<String>,
}

pub fn parse_observations(station: &str, body: &str) -> Result<Batch> {
    let collection: Collection = serde_json::from_str(body).context("decoding NWS observations")?;
    let mut rows = Vec::with_capacity(collection.features.len());
    for feature in collection.features {
        let p = feature.properties;
        let Some(time) = p.timestamp.as_deref().and_then(parse_time) else {
            continue;
        };
        let mut o = Observation::new(station, time);
        // NWS drops the METAR/SPECI type word, so scheduled and special reports
        // cannot be told apart here; the cadence inference tolerates a few extras.
        o.kind = ObsKind::Unknown;
        o.set_air_temp_c(measure(&p.temperature, temp_c).map(|v| round_to(v, 1)));
        o.set_dew_point_c(measure(&p.dewpoint, temp_c).map(|v| round_to(v, 1)));
        o.relative_humidity_pct = measure(&p.relative_humidity, |v, _| v)
            .map(|v| round_to(v, 0))
            .or_else(|| match (o.air_temp_c, o.dew_point_c) {
                (Some(t), Some(td)) => Some(round_to(relative_humidity(t, td), 0)),
                _ => None,
            });
        o.wind_direction_deg =
            measure(&p.wind_direction, |v, _| v).map(|d| (d.round() as i64).rem_euclid(360) as u16);
        o.wind_speed_ms = measure(&p.wind_speed, speed_ms).map(|v| round_to(v, 1));
        o.wind_gust_ms = measure(&p.wind_gust, speed_ms).map(|v| round_to(v, 1));
        o.visibility_km = measure(&p.visibility, length_km).map(|v| round_to(v, 1));
        o.altimeter_hpa = measure(&p.barometric_pressure, pressure_hpa).map(|v| round_to(v, 1));
        o.sea_level_pressure_hpa =
            measure(&p.sea_level_pressure, pressure_hpa).map(|v| round_to(v, 1));
        let weather: Vec<String> = p
            .present_weather
            .unwrap_or_default()
            .into_iter()
            .filter_map(|w| w.raw_string)
            .filter(|s| !s.trim().is_empty())
            .collect();
        o.weather = (!weather.is_empty()).then(|| weather.join(" "));
        o.clouds = p
            .cloud_layers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|layer| {
                Some(CloudLayer {
                    cover: layer.amount?,
                    base_m: layer
                        .base
                        .as_ref()
                        .and_then(Quantity::value)
                        .map(|(v, unit)| round_to(length_m(v, unit), 0)),
                })
            })
            .collect();
        o.precip_1h_mm = measure(&p.precipitation_last_hour, depth_mm).map(|v| round_to(v, 2));
        o.precip_3h_mm = measure(&p.precipitation_last3_hours, depth_mm).map(|v| round_to(v, 2));
        o.precip_6h_mm = measure(&p.precipitation_last6_hours, depth_mm).map(|v| round_to(v, 2));
        o.raw = p.raw_message.filter(|s| !s.trim().is_empty());
        rows.push(o);
    }
    rows.sort_by_key(|o| o.time);
    Ok(Batch { info: None, rows })
}

fn measure(q: &Option<Quantity>, convert: impl Fn(f64, &str) -> f64) -> Option<f64> {
    q.as_ref()
        .and_then(Quantity::value)
        .map(|(v, unit)| convert(v, unit))
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn temp_c(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:degF" => (v - 32.0) * 5.0 / 9.0,
        "wmoUnit:K" => v - 273.15,
        _ => v,
    }
}

fn speed_ms(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:km_h-1" => v / 3.6,
        "wmoUnit:kn" | "wmoUnit:kt" => v * KNOT_TO_MS,
        "wmoUnit:mi_h-1" => v * 0.447_04,
        _ => v,
    }
}

fn pressure_hpa(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:Pa" => v / 100.0,
        _ => v,
    }
}

fn length_km(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:m" => v / 1000.0,
        "wmoUnit:mi" => v * STATUTE_MILE_TO_KM,
        _ => v,
    }
}

fn length_m(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:ft" => v * FOOT_TO_M,
        "wmoUnit:km" => v * 1000.0,
        _ => v,
    }
}

fn depth_mm(v: f64, unit: &str) -> f64 {
    match unit {
        "wmoUnit:m" => v * 1000.0,
        "wmoUnit:in" => v * INCH_TO_MM,
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ROWS: &str = r#"{"type":"FeatureCollection","features":[
{"properties":{"timestamp":"2026-09-12T17:45:00+00:00","rawMessage":"","textDescription":"Mostly Cloudy",
"temperature":{"unitCode":"wmoUnit:degC","value":33,"qualityControl":"V"},
"dewpoint":{"unitCode":"wmoUnit:degC","value":24,"qualityControl":"V"},
"windDirection":{"unitCode":"wmoUnit:degree_(angle)","value":90,"qualityControl":"V"},
"windSpeed":{"unitCode":"wmoUnit:km_h-1","value":18.504,"qualityControl":"V"},
"windGust":{"unitCode":"wmoUnit:km_h-1","value":null,"qualityControl":"Z"},
"barometricPressure":{"unitCode":"wmoUnit:Pa","value":101659.39,"qualityControl":"V"},
"seaLevelPressure":{"unitCode":"wmoUnit:Pa","value":null,"qualityControl":"Z"},
"visibility":{"unitCode":"wmoUnit:m","value":16093.44,"qualityControl":"C"},
"relativeHumidity":{"unitCode":"wmoUnit:percent","value":59.274011248809,"qualityControl":"V"},
"precipitationLastHour":null,
"cloudLayers":[{"base":{"unitCode":"wmoUnit:m","value":762},"amount":"FEW"},{"base":{"unitCode":"wmoUnit:m","value":1158.24},"amount":"BKN"}]}},
{"properties":{"timestamp":"2026-09-12T17:37:00+00:00","rawMessage":"KMIA 121737Z COR 10008KT 10SM TS FEW025CB BKN038 BKN250 33/25 A3002",
"temperature":{"unitCode":"wmoUnit:degC","value":33.3,"qualityControl":"X"},
"presentWeather":[{"rawString":"TS"}],
"precipitationLastHour":{"unitCode":"wmoUnit:mm","value":0.25,"qualityControl":"V"}}}
]}"#;

    #[test]
    fn five_minute_row_is_decoded_to_metric() {
        let batch = parse_observations("KMIA", ROWS).unwrap();
        assert_eq!(batch.rows.len(), 2);
        let o = &batch.rows[1];
        assert_eq!(
            o.time,
            Utc.with_ymd_and_hms(2026, 9, 12, 17, 45, 0).unwrap()
        );
        assert_eq!(o.kind, ObsKind::Unknown);
        assert_eq!(o.air_temp_c, Some(33.0));
        assert_eq!(o.air_temp_f, Some(91.4));
        assert_eq!(o.dew_point_c, Some(24.0));
        assert_eq!(o.relative_humidity_pct, Some(59.0));
        assert_eq!(o.wind_direction_deg, Some(90));
        assert_eq!(o.wind_speed_ms, Some(5.1));
        assert_eq!(o.wind_gust_ms, None);
        assert_eq!(o.altimeter_hpa, Some(1016.6));
        assert_eq!(o.sea_level_pressure_hpa, None);
        assert_eq!(o.visibility_km, Some(16.1));
        assert_eq!(o.clouds.len(), 2);
        assert_eq!(o.clouds[1].base_m, Some(1158.0));
        assert_eq!(o.raw, None);
        assert_eq!(o.weather, None);
    }

    #[test]
    fn rejected_values_are_dropped_and_weather_kept() {
        let batch = parse_observations("KMIA", ROWS).unwrap();
        let o = &batch.rows[0];
        assert_eq!(o.time.timestamp() % 3600, 37 * 60);
        assert_eq!(o.air_temp_c, None, "quality flag X must drop the value");
        assert_eq!(o.weather.as_deref(), Some("TS"));
        assert_eq!(o.precip_1h_mm, Some(0.25));
        assert!(o.raw.as_deref().unwrap().starts_with("KMIA 121737Z"));
    }

    #[test]
    fn station_metadata() {
        let body = r#"{"geometry":{"type":"Point","coordinates":[-80.31639,25.79056]},"properties":{"stationIdentifier":"KMIA","name":"Miami, Miami International Airport","elevation":{"unitCode":"wmoUnit:m","value":3.048}}}"#;
        let info = parse_station("kmia", body).unwrap();
        assert_eq!(info.id, "KMIA");
        assert_eq!(info.lat, Some(25.79056));
        assert_eq!(info.lon, Some(-80.31639));
        assert_eq!(info.elevation_m, Some(3.0));
        assert_eq!(
            info.name.as_deref(),
            Some("Miami, Miami International Airport")
        );
    }

    #[test]
    fn not_found_error_is_identifiable() {
        let err = anyhow::Error::new(StationNotFound("RJTT".into()));
        assert!(err.is::<StationNotFound>());
        assert!(err.to_string().contains("RJTT"));
    }
}
