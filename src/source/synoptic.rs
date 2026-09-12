//! Synoptic Data API backend, the provider behind the weather.gov WRH timeseries page.
//!
//! Needs an API token (`--token` or `SYNOPTIC_TOKEN`). The token embedded in the
//! weather.gov page is locked to that site, and Synoptic's free tier is limited to
//! US academic accounts, so this backend is opt-in. It has not been exercised against
//! the live API in this prototype; the parser is covered by a fixture in the shape
//! the API documentation describes.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::snippet;
use crate::model::{
    Batch, FOOT_TO_M, INCH_TO_CM, INCH_TO_MM, KNOT_TO_MS, ObsKind, Observation, STATUTE_MILE_TO_KM,
    StationInfo, relative_humidity, round_to,
};

const ENDPOINT: &str = "https://api.synopticdata.com/v2/stations/timeseries";
/// Metric preset with each variable group pinned, so an upstream preset change
/// cannot silently alter what we receive. Altimeter only offers Pa or inHg.
const UNITS: &str = "metric,temp|C,speed|mps,pres|mb,height|m,precip|mm";

pub struct SynopticSource {
    client: reqwest::Client,
    token: String,
}

impl SynopticSource {
    pub fn new(client: reqwest::Client, token: String) -> Self {
        Self { client, token }
    }

    pub async fn fetch(&self, station: &str, window: Duration) -> Result<Batch> {
        let minutes = (window.as_secs() / 60).max(1);
        let url = format!(
            "{ENDPOINT}?stid={}&recent={minutes}&units={UNITS}&obtimezone=UTC&token={}",
            station.to_ascii_uppercase(),
            self.token
        );
        debug!(url = %url.replace(&self.token, "<token>"), "GET");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("requesting Synoptic timeseries")?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("reading Synoptic response body")?;
        if !status.is_success() {
            // Synoptic explains token and argument problems in the JSON SUMMARY block.
            return parse_body(station, &body)
                .with_context(|| format!("Synoptic returned HTTP {status}: {}", snippet(&body)));
        }
        parse_body(station, &body)
    }
}

#[derive(Debug, Deserialize)]
struct SynResponse {
    #[serde(rename = "SUMMARY")]
    summary: SynSummary,
    #[serde(rename = "UNITS")]
    units: Option<HashMap<String, String>>,
    #[serde(rename = "STATION")]
    stations: Option<Vec<SynStation>>,
}

#[derive(Debug, Deserialize)]
struct SynSummary {
    #[serde(rename = "RESPONSE_CODE")]
    code: i64,
    #[serde(rename = "RESPONSE_MESSAGE", default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct SynStation {
    #[serde(rename = "STID")]
    stid: String,
    #[serde(rename = "NAME")]
    name: Option<String>,
    #[serde(rename = "LATITUDE")]
    lat: Option<Value>,
    #[serde(rename = "LONGITUDE")]
    lon: Option<Value>,
    #[serde(rename = "ELEVATION")]
    elevation: Option<Value>,
    #[serde(rename = "OBSERVATIONS")]
    observations: Option<HashMap<String, Vec<Value>>>,
}

pub fn parse_body(station: &str, body: &str) -> Result<Batch> {
    let resp: SynResponse = serde_json::from_str(body).context("decoding Synoptic response")?;
    if resp.summary.code != 1 {
        bail!(
            "Synoptic error (code {}): {}",
            resp.summary.code,
            resp.summary.message
        );
    }
    let units = resp.units.unwrap_or_default();
    let mut stations = resp.stations.unwrap_or_default();
    if stations.is_empty() {
        return Ok(Batch::default());
    }
    let idx = stations
        .iter()
        .position(|s| s.stid.eq_ignore_ascii_case(station))
        .unwrap_or(0);
    let st = stations.swap_remove(idx);
    let unit = |group: &str, fallback: &str| -> String {
        units
            .get(group)
            .cloned()
            .unwrap_or_else(|| fallback.to_owned())
    };
    // Synoptic reports elevation in feet unless told otherwise, even in metric mode.
    let elevation_unit = unit("elevation", "ft");
    let info = StationInfo {
        id: st.stid.to_ascii_uppercase(),
        name: st.name.clone(),
        lat: st.lat.as_ref().and_then(num),
        lon: st.lon.as_ref().and_then(num),
        elevation_m: st
            .elevation
            .as_ref()
            .and_then(num)
            .map(|e| round_to(length_to_m(e, &elevation_unit), 1)),
    };
    let obs = st.observations.unwrap_or_default();
    let times: Vec<Option<DateTime<Utc>>> = obs
        .get("date_time")
        .map(|col| {
            col.iter()
                .map(|t| t.as_str().and_then(parse_time))
                .collect()
        })
        .unwrap_or_default();

    let air_temp = pick_column(&obs, "air_temp");
    let dew_point = pick_column(&obs, "dew_point_temperature");
    let humidity = pick_column(&obs, "relative_humidity");
    let wind_dir = pick_column(&obs, "wind_direction");
    let wind_speed = pick_column(&obs, "wind_speed");
    let wind_gust = pick_column(&obs, "wind_gust");
    let visibility = pick_column(&obs, "visibility");
    let altimeter = pick_column(&obs, "altimeter");
    let slp = pick_column(&obs, "sea_level_pressure");
    let weather = pick_column(&obs, "weather_condition");
    let precip_1h = pick_column(&obs, "precip_accum_one_hour");
    let precip_3h = pick_column(&obs, "precip_accum_three_hour");
    let precip_6h = pick_column(&obs, "precip_accum_six_hour");
    let precip_24h = pick_column(&obs, "precip_accum_24_hour");
    let snow_depth = pick_column(&obs, "snow_depth");
    let metar = pick_column(&obs, "metar");

    let temp_unit = unit("air_temp", "Celsius");
    let dew_unit = unit("dew_point_temperature", "Celsius");
    let speed_unit = unit("wind_speed", "m/s");
    let gust_unit = unit("wind_gust", "m/s");
    let vis_unit = unit("visibility", "Statute miles");
    let alti_unit = unit("altimeter", "Pascals");
    let slp_unit = unit("sea_level_pressure", "Millibars");
    let precip_unit = unit("precip_accum_one_hour", "Millimeters");
    let snow_unit = unit("snow_depth", "Millimeters");

    let mut rows = Vec::with_capacity(times.len());
    for (i, time) in times.into_iter().enumerate() {
        let Some(time) = time else { continue };
        let mut o = Observation::new(info.id.clone(), time);
        o.raw = cell_str(metar, i);
        o.kind = match o.raw.as_deref().map(str::trim_start) {
            Some(raw) if raw.starts_with("SPECI") => ObsKind::Special,
            Some(raw) if raw.starts_with("METAR") => ObsKind::Routine,
            _ => ObsKind::Unknown,
        };
        o.set_air_temp_c(cell_num(air_temp, i).map(|v| round_to(temp_to_c(v, &temp_unit), 1)));
        o.set_dew_point_c(cell_num(dew_point, i).map(|v| round_to(temp_to_c(v, &dew_unit), 1)));
        o.relative_humidity_pct = cell_num(humidity, i)
            .map(|v| round_to(v, 0))
            .or_else(|| match (o.air_temp_c, o.dew_point_c) {
                (Some(t), Some(td)) => Some(round_to(relative_humidity(t, td), 0)),
                _ => None,
            });
        o.wind_direction_deg =
            cell_num(wind_dir, i).map(|d| (d.round() as i64).rem_euclid(360) as u16);
        o.wind_speed_ms = cell_num(wind_speed, i).map(|v| round_to(speed_to_ms(v, &speed_unit), 1));
        o.wind_gust_ms = cell_num(wind_gust, i).map(|v| round_to(speed_to_ms(v, &gust_unit), 1));
        o.visibility_km =
            cell_num(visibility, i).map(|v| round_to(distance_to_km(v, &vis_unit), 1));
        o.altimeter_hpa =
            cell_num(altimeter, i).map(|v| round_to(pressure_to_hpa(v, &alti_unit), 1));
        o.sea_level_pressure_hpa =
            cell_num(slp, i).map(|v| round_to(pressure_to_hpa(v, &slp_unit), 1));
        o.weather = cell_str(weather, i).filter(|s| !s.trim().is_empty());
        o.precip_1h_mm = cell_num(precip_1h, i).map(|v| round_to(precip_to_mm(v, &precip_unit), 2));
        o.precip_3h_mm = cell_num(precip_3h, i).map(|v| round_to(precip_to_mm(v, &precip_unit), 2));
        o.precip_6h_mm = cell_num(precip_6h, i).map(|v| round_to(precip_to_mm(v, &precip_unit), 2));
        o.precip_24h_mm =
            cell_num(precip_24h, i).map(|v| round_to(precip_to_mm(v, &precip_unit), 2));
        o.snow_depth_cm = cell_num(snow_depth, i).map(|v| round_to(depth_to_cm(v, &snow_unit), 1));
        rows.push(o);
    }
    rows.sort_by_key(|o| o.time);
    Ok(Batch {
        info: Some(info),
        rows,
    })
}

/// Finds the `<group>_set_N` (or `_set_Nd`, derived) column for a variable group,
/// preferring the lowest-numbered primary sensor.
fn pick_column<'a>(obs: &'a HashMap<String, Vec<Value>>, group: &str) -> Option<&'a Vec<Value>> {
    let prefix = format!("{group}_set_");
    obs.iter()
        .filter(|(key, _)| {
            key.strip_prefix(&prefix).is_some_and(|rest| {
                let digits = rest.strip_suffix('d').unwrap_or(rest);
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .min_by(|a, b| a.0.cmp(b.0))
        .map(|(_, col)| col)
}

fn cell_num(col: Option<&Vec<Value>>, i: usize) -> Option<f64> {
    col?.get(i).and_then(num)
}

fn cell_str(col: Option<&Vec<Value>>, i: usize) -> Option<String> {
    col?.get(i)?.as_str().map(str::to_owned)
}

/// Synoptic sends numbers as numbers in observations but as strings in metadata.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn temp_to_c(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "fahrenheit" | "f" => (v - 32.0) * 5.0 / 9.0,
        "kelvin" | "k" => v - 273.15,
        _ => v,
    }
}

fn speed_to_ms(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "knots" | "kts" | "kt" => v * KNOT_TO_MS,
        "mph" | "miles/hour" | "miles per hour" => v * 0.447_04,
        "km/h" | "kph" | "kilometers/hour" => v / 3.6,
        _ => v,
    }
}

fn pressure_to_hpa(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "pascals" | "pa" => v / 100.0,
        "inches hg" | "inhg" | "inches mercury" | "inches of mercury" => v * 33.863_9,
        _ => v,
    }
}

fn length_to_m(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "ft" | "feet" => v * FOOT_TO_M,
        _ => v,
    }
}

fn distance_to_km(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "statute miles" | "miles" | "mi" => v * STATUTE_MILE_TO_KM,
        "meters" | "m" => v / 1000.0,
        _ => v,
    }
}

fn precip_to_mm(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "inches" | "in" => v * INCH_TO_MM,
        "centimeters" | "cm" => v * 10.0,
        _ => v,
    }
}

fn depth_to_cm(v: f64, unit: &str) -> f64 {
    match unit.to_ascii_lowercase().as_str() {
        "inches" | "in" => v * INCH_TO_CM,
        "millimeters" | "mm" => v / 10.0,
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const FIXTURE: &str = r#"{"SUMMARY":{"RESPONSE_CODE":1,"RESPONSE_MESSAGE":"OK"},
"UNITS":{"position":"m","elevation":"ft","air_temp":"Celsius","dew_point_temperature":"Celsius","relative_humidity":"%","wind_speed":"m/s","wind_direction":"Degrees","altimeter":"Pascals","sea_level_pressure":"Millibars","visibility":"Statute miles","precip_accum_one_hour":"Millimeters"},
"STATION":[{"STID":"RJTT","NAME":"Tokyo Haneda","LATITUDE":"35.553","LONGITUDE":"139.781","ELEVATION":"16",
"OBSERVATIONS":{"date_time":["2026-09-12T15:00:00Z","2026-09-12T14:30:00Z"],
"air_temp_set_1":[20.0,20.0],"dew_point_temperature_set_1d":[19.5,20.0],"relative_humidity_set_1":[97.0,100.0],
"wind_speed_set_1":[null,2.57],"wind_direction_set_1":[40.0,30.0],"altimeter_set_1":[102310.0,102300.0],
"sea_level_pressure_set_1d":[null,1023.4],"visibility_set_1":[6.21,3.11],"precip_accum_one_hour_set_1":[null,0.5],
"metar_set_1":["SPECI RJTT 121500Z 04000KT 9999 20/19 Q1023","METAR RJTT 121430Z 03005KT 5000 -RA BR 20/20 Q1023"]}}]}"#;

    #[test]
    fn parses_documented_shape_and_normalizes_units() {
        let batch = parse_body("rjtt", FIXTURE).unwrap();
        let info = batch.info.unwrap();
        assert_eq!(info.id, "RJTT");
        assert_eq!(info.lat, Some(35.553));
        assert_eq!(info.elevation_m, Some(4.9));
        assert_eq!(batch.rows.len(), 2);
        let first = &batch.rows[0];
        assert_eq!(
            first.time,
            Utc.with_ymd_and_hms(2026, 9, 12, 14, 30, 0).unwrap()
        );
        assert_eq!(first.kind, ObsKind::Routine);
        assert_eq!(first.air_temp_c, Some(20.0));
        assert_eq!(first.air_temp_f, Some(68.0));
        assert_eq!(first.relative_humidity_pct, Some(100.0));
        assert_eq!(first.wind_speed_ms, Some(2.6));
        assert_eq!(first.wind_direction_deg, Some(30));
        assert_eq!(first.altimeter_hpa, Some(1023.0));
        assert_eq!(first.sea_level_pressure_hpa, Some(1023.4));
        assert_eq!(first.visibility_km, Some(5.0));
        assert_eq!(first.precip_1h_mm, Some(0.5));
        let second = &batch.rows[1];
        assert_eq!(second.kind, ObsKind::Special);
        assert_eq!(second.wind_speed_ms, None);
        assert_eq!(second.sea_level_pressure_hpa, None);
        assert_eq!(second.visibility_km, Some(10.0));
    }

    #[test]
    fn token_errors_are_reported() {
        let body = r#"{"SUMMARY":{"RESPONSE_CODE":2,"RESPONSE_MESSAGE":"Invalid request per token rules","VERSION":null,"HTTP_STATUS_CODE":403}}"#;
        let err = parse_body("RJTT", body).unwrap_err();
        assert!(err.to_string().contains("token rules"), "{err}");
    }

    #[test]
    fn no_station_block_is_an_empty_batch() {
        let body = r#"{"SUMMARY":{"RESPONSE_CODE":1,"RESPONSE_MESSAGE":"OK"},"STATION":[]}"#;
        assert!(parse_body("RJTT", body).unwrap().rows.is_empty());
    }

    #[test]
    fn column_picking_prefers_primary_sensor() {
        let mut obs = HashMap::new();
        obs.insert("air_temp_set_2".to_owned(), vec![Value::from(1.0)]);
        obs.insert("air_temp_set_1d".to_owned(), vec![Value::from(2.0)]);
        obs.insert("air_temp_set_1".to_owned(), vec![Value::from(3.0)]);
        obs.insert(
            "air_temp_high_6_hour_set_1".to_owned(),
            vec![Value::from(4.0)],
        );
        assert_eq!(cell_num(pick_column(&obs, "air_temp"), 0), Some(3.0));
        assert!(pick_column(&obs, "dew_point_temperature").is_none());
    }
}
