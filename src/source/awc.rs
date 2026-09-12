//! Aviation Weather Center data API backend.
//!
//! Token-free, global METAR/SPECI coverage. Docs: <https://aviationweather.gov/data/api/>.
//! The service allows 100 requests per minute and asks for modest, identifiable clients.
//! Reports carry a `receiptTime`, which lets us measure each station's publish lag
//! straight from history instead of learning it by polling.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;
use tracing::debug;

use super::snippet;
use crate::model::{
    Batch, CloudLayer, FOOT_TO_M, INCH_TO_CM, INCH_TO_MM, KNOT_TO_MS, ObsKind, Observation,
    STATUTE_MILE_TO_KM, StationInfo, relative_humidity, round_to,
};

const ENDPOINT: &str = "https://aviationweather.gov/api/data/metar";

pub struct AwcSource {
    client: reqwest::Client,
}

impl AwcSource {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub async fn fetch(&self, station: &str, window: Duration) -> Result<Batch> {
        let hours = (window.as_secs_f64() / 3600.0).max(0.25);
        let url = format!(
            "{ENDPOINT}?ids={}&format=json&hours={hours:.2}",
            station.to_ascii_uppercase()
        );
        debug!(%url, "GET");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("requesting AWC METAR data")?;
        let status = resp.status();
        let body = resp.text().await.context("reading AWC response body")?;
        if !status.is_success() {
            bail!("AWC returned HTTP {status}: {}", snippet(&body));
        }
        parse_body(station, &body)
    }
}

pub fn parse_body(station: &str, body: &str) -> Result<Batch> {
    let value: Value = serde_json::from_str(body).context("AWC response is not JSON")?;
    let records: Vec<AwcMetar> = match value {
        Value::Array(_) => serde_json::from_value(value).context("decoding AWC METAR records")?,
        Value::Object(ref obj) if obj.contains_key("error") => {
            bail!("AWC error: {}", obj["error"])
        }
        other => bail!("unexpected AWC response: {}", snippet(&other.to_string())),
    };
    let wanted = station.to_ascii_uppercase();
    let mut batch = Batch::default();
    for rec in records
        .into_iter()
        .filter(|r| r.icao_id.eq_ignore_ascii_case(&wanted))
    {
        if batch.info.is_none() {
            batch.info = Some(rec.station_info());
        }
        if let Some(obs) = rec.into_observation() {
            batch.rows.push(obs);
        }
    }
    batch.rows.sort_by_key(|o| o.time);
    Ok(batch)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AwcMetar {
    icao_id: String,
    receipt_time: Option<String>,
    obs_time: Option<i64>,
    report_time: Option<String>,
    temp: Option<f64>,
    dewp: Option<f64>,
    /// Degrees, or the string "VRB".
    wdir: Option<Value>,
    wspd: Option<f64>,
    wgst: Option<f64>,
    /// Statute miles, or a string like "10+".
    visib: Option<Value>,
    altim: Option<f64>,
    slp: Option<f64>,
    wx_string: Option<String>,
    precip: Option<f64>,
    pcp3hr: Option<f64>,
    pcp6hr: Option<f64>,
    pcp24hr: Option<f64>,
    snow: Option<f64>,
    metar_type: Option<String>,
    raw_ob: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    elev: Option<f64>,
    name: Option<String>,
    clouds: Option<Vec<AwcCloud>>,
}

#[derive(Debug, Deserialize)]
struct AwcCloud {
    cover: Option<String>,
    base: Option<f64>,
}

impl AwcMetar {
    fn station_info(&self) -> StationInfo {
        StationInfo {
            id: self.icao_id.to_ascii_uppercase(),
            name: self.name.clone(),
            lat: self.lat,
            lon: self.lon,
            elevation_m: self.elev,
        }
    }

    fn into_observation(self) -> Option<Observation> {
        let time = match self.obs_time {
            Some(secs) => Utc.timestamp_opt(secs, 0).single()?,
            None => parse_timestamp(self.report_time.as_deref()?)?,
        };
        let mut obs = Observation::new(self.icao_id.to_ascii_uppercase(), time);
        obs.received = self.receipt_time.as_deref().and_then(parse_timestamp);
        obs.kind = match self.metar_type.as_deref() {
            Some("METAR") => ObsKind::Routine,
            Some("SPECI") => ObsKind::Special,
            _ => ObsKind::Unknown,
        };
        obs.air_temp_c = self.temp;
        obs.dew_point_c = self.dewp;
        obs.relative_humidity_pct = match (self.temp, self.dewp) {
            (Some(t), Some(td)) => Some(round_to(relative_humidity(t, td), 0)),
            _ => None,
        };
        match self.wdir {
            Some(Value::Number(n)) => {
                obs.wind_direction_deg = n
                    .as_f64()
                    .map(|d| (d.round() as i64).rem_euclid(360) as u16);
            }
            Some(Value::String(s)) if s.eq_ignore_ascii_case("VRB") => obs.wind_variable = true,
            _ => {}
        }
        obs.wind_speed_ms = self.wspd.map(|kt| round_to(kt * KNOT_TO_MS, 1));
        obs.wind_gust_ms = self.wgst.map(|kt| round_to(kt * KNOT_TO_MS, 1));
        obs.visibility_km = self
            .visib
            .as_ref()
            .and_then(number_or_plus_string)
            .map(|mi| round_to(mi * STATUTE_MILE_TO_KM, 1));
        obs.altimeter_hpa = self.altim.map(|p| round_to(p, 1));
        obs.sea_level_pressure_hpa = self.slp.map(|p| round_to(p, 1));
        obs.weather = self.wx_string.filter(|s| !s.trim().is_empty());
        obs.clouds = self
            .clouds
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| {
                Some(CloudLayer {
                    cover: c.cover?,
                    base_m: c.base.map(|ft| round_to(ft * FOOT_TO_M, 0)),
                })
            })
            .collect();
        obs.precip_1h_mm = self.precip.map(|i| round_to(i * INCH_TO_MM, 2));
        obs.precip_3h_mm = self.pcp3hr.map(|i| round_to(i * INCH_TO_MM, 2));
        obs.precip_6h_mm = self.pcp6hr.map(|i| round_to(i * INCH_TO_MM, 2));
        obs.precip_24h_mm = self.pcp24hr.map(|i| round_to(i * INCH_TO_MM, 2));
        obs.snow_depth_cm = self.snow.map(|i| round_to(i * INCH_TO_CM, 1));
        obs.raw = self.raw_ob;
        Some(obs)
    }
}

fn number_or_plus_string(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('+').parse().ok(),
        _ => None,
    }
}

/// Accepts `2026-09-12T15:07:37.896Z` and the space-separated form the docs describe.
fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    let candidate = match s.as_bytes().get(10) {
        Some(b' ') => s.replacen(' ', "T", 1),
        _ => s.to_owned(),
    };
    DateTime::parse_from_rfc3339(&candidate)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RJTT: &str = r#"[{"icaoId":"RJTT","receiptTime":"2026-09-12T15:07:37.896Z","obsTime":1789225200,"reportTime":"2026-09-12T15:00:00.000Z","temp":20,"dewp":20,"wdir":30,"wspd":5,"visib":3.11,"altim":1023,"qcField":0,"wxString":"-RA BR","metarType":"METAR","rawOb":"METAR RJTT 121500Z 03005KT 5000 -RA BR FEW003 BKN010 BKN020 20/20 Q1023","lat":35.553,"lon":139.781,"elev":5,"name":"Tokyo/Haneda Intl, 13, JP","cover":"BKN","clouds":[{"cover":"FEW","base":300},{"cover":"BKN","base":1000},{"cover":"BKN","base":2000}],"fltCat":"MVFR"}]"#;

    #[test]
    fn parses_and_converts_to_metric() {
        let batch = parse_body("rjtt", RJTT).unwrap();
        assert_eq!(batch.rows.len(), 1);
        let o = &batch.rows[0];
        assert_eq!(o.station, "RJTT");
        assert_eq!(o.time, Utc.with_ymd_and_hms(2026, 9, 12, 15, 0, 0).unwrap());
        assert_eq!(o.received.unwrap().timestamp(), 1_789_225_657);
        assert_eq!(o.kind, ObsKind::Routine);
        assert_eq!(o.air_temp_c, Some(20.0));
        assert_eq!(o.dew_point_c, Some(20.0));
        assert_eq!(o.relative_humidity_pct, Some(100.0));
        assert_eq!(o.wind_direction_deg, Some(30));
        assert!(!o.wind_variable);
        assert_eq!(o.wind_speed_ms, Some(2.6));
        assert_eq!(o.visibility_km, Some(5.0));
        assert_eq!(o.altimeter_hpa, Some(1023.0));
        assert_eq!(o.weather.as_deref(), Some("-RA BR"));
        assert_eq!(o.clouds.len(), 3);
        assert_eq!(o.clouds[0].cover, "FEW");
        assert_eq!(o.clouds[0].base_m, Some(91.0));
        assert!(o.raw.as_deref().unwrap().starts_with("METAR RJTT"));
        let info = batch.info.unwrap();
        assert_eq!(info.name.as_deref(), Some("Tokyo/Haneda Intl, 13, JP"));
        assert_eq!(info.elevation_m, Some(5.0));
    }

    #[test]
    fn variable_wind_unbounded_visibility_and_speci() {
        let body = r#"[{"icaoId":"KSLC","obsTime":1789225200,"wdir":"VRB","wspd":3,"wgst":15,"visib":"10+","metarType":"SPECI","precip":0.1,"snow":2}]"#;
        let o = &parse_body("KSLC", body).unwrap().rows[0];
        assert!(o.wind_variable);
        assert_eq!(o.wind_direction_deg, None);
        assert_eq!(o.wind_speed_ms, Some(1.5));
        assert_eq!(o.wind_gust_ms, Some(7.7));
        assert_eq!(o.visibility_km, Some(16.1));
        assert_eq!(o.kind, ObsKind::Special);
        assert_eq!(o.precip_1h_mm, Some(2.54));
        assert_eq!(o.snow_depth_cm, Some(5.1));
        assert_eq!(o.relative_humidity_pct, None);
    }

    #[test]
    fn falls_back_to_report_time_and_filters_other_stations() {
        let body = r#"[{"icaoId":"KJFK","reportTime":"2026-09-12 15:51:00.000Z"},{"icaoId":"KSLC","reportTime":"2026-09-12T15:54:00.000Z"}]"#;
        let batch = parse_body("kslc", body).unwrap();
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(
            batch.rows[0].time,
            Utc.with_ymd_and_hms(2026, 9, 12, 15, 54, 0).unwrap()
        );
        assert_eq!(
            parse_body("kjfk", body).unwrap().rows[0].time.timestamp() % 3600,
            51 * 60
        );
    }

    #[test]
    fn error_object_is_reported() {
        let err = parse_body(
            "x",
            r#"{"status":"error","error":"Unexpected query parameter provided"}"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Unexpected query parameter"),
            "{err}"
        );
    }

    #[test]
    fn empty_array_is_an_empty_batch() {
        let batch = parse_body("ZZZZ", "[]").unwrap();
        assert!(batch.rows.is_empty());
        assert!(batch.info.is_none());
    }
}
