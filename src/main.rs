//! prawn: a live weather-station feed in metric units.
//!
//! Loads a station's recent history, learns how often and at what minute it
//! reports and how long reports take to appear upstream, then polls only in
//! the window where the next row is likely to land.

mod feed;
mod model;
mod schedule;
mod source;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use clap::{Parser, ValueEnum};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use feed::{Event, Feed, FeedConfig};
use model::{ObsKind, Observation, StationInfo};
use source::{AnySource, awc::AwcSource, synoptic::SynopticSource};

#[derive(Parser, Debug)]
#[command(
    name = "prawn",
    version,
    about = "Live weather-station feed in metric units, polled around each station's own reporting cadence"
)]
struct Cli {
    /// Station identifiers, e.g. RJTT KSLC (ICAO ids for the default source)
    #[arg(required = true)]
    stations: Vec<String>,

    /// Upstream data provider
    #[arg(long, value_enum, default_value_t = SourceKind::Awc)]
    source: SourceKind,

    /// Synoptic API token (only with --source synoptic)
    #[arg(long, env = "SYNOPTIC_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// History loaded at startup to learn the cadence and publish lag
    #[arg(long, default_value = "24h", value_parser = humantime::parse_duration)]
    history: Duration,

    /// Historical rows to print at startup (default 3, or all with --once)
    #[arg(long)]
    replay: Option<usize>,

    /// Poll interval while a new row is expected any moment
    #[arg(long, default_value = "20s", value_parser = humantime::parse_duration)]
    poll_interval: Duration,

    /// Poll interval once a row is overdue
    #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
    slow_interval: Duration,

    /// Minimum length of the fast-polling window after the expected time
    #[arg(long, default_value = "10m", value_parser = humantime::parse_duration)]
    fast_window: Duration,

    /// Start polling this much earlier than the shortest publish lag seen so far
    #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
    lag_margin: Duration,

    /// Also poll at this interval while idle between expected rows (catches SPECIs sooner)
    #[arg(long, value_parser = humantime::parse_duration)]
    idle_poll: Option<Duration>,

    /// Emit one JSON object per line instead of the human-readable log
    #[arg(long)]
    json: bool,

    /// Load and print history, then exit without polling
    #[arg(long)]
    once: bool,

    /// HTTP User-Agent; upstream APIs ask for an identifiable one
    #[arg(long, env = "PRAWN_USER_AGENT")]
    user_agent: Option<String>,

    /// Time zone for displayed times (IANA name, e.g. UTC, Asia/Tokyo); JSON output stays UTC
    #[arg(long, default_value = "America/New_York", value_parser = parse_tz)]
    tz: Tz,
}

fn parse_tz(s: &str) -> Result<Tz, String> {
    s.trim().parse::<Tz>().map_err(|_| {
        format!("unknown time zone '{s}'; use an IANA name such as UTC or America/New_York")
    })
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SourceKind {
    /// aviationweather.gov METAR API: no token, global airport stations
    Awc,
    /// api.synopticdata.com timeseries: token required, all networks
    Synoptic,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    for (flag, value) in [
        ("--poll-interval", cli.poll_interval),
        ("--slow-interval", cli.slow_interval),
    ] {
        ensure!(
            value >= Duration::from_secs(1),
            "{flag} must be at least 1s"
        );
    }
    if let Some(idle) = cli.idle_poll {
        ensure!(
            idle >= Duration::from_secs(1),
            "--idle-poll must be at least 1s"
        );
    }

    let client = source::http_client(cli.user_agent.as_deref())?;
    let source = Arc::new(match cli.source {
        SourceKind::Awc => AnySource::Awc(AwcSource::new(client)),
        SourceKind::Synoptic => {
            let token = cli
                .token
                .clone()
                .context("--source synoptic needs --token or SYNOPTIC_TOKEN")?;
            AnySource::Synoptic(SynopticSource::new(client, token))
        }
    });

    let cfg = FeedConfig {
        history: cli.history,
        replay: cli.replay.unwrap_or(if cli.once { usize::MAX } else { 3 }),
        poll_interval: cli.poll_interval,
        slow_interval: cli.slow_interval,
        fast_window: cli.fast_window,
        lag_margin: cli.lag_margin,
        idle_poll: cli.idle_poll,
    };

    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let once = cli.once;
    let mut tasks = Vec::new();
    for station in &cli.stations {
        let mut feed = Feed::new(
            Arc::clone(&source),
            station.trim().to_ascii_uppercase(),
            cfg.clone(),
            tx.clone(),
        );
        tasks.push(tokio::spawn(async move {
            feed.prime().await;
            if !once {
                feed.run().await;
            }
        }));
    }
    drop(tx);

    let json = cli.json;
    let tz = cli.tz;
    let printer = async move {
        while let Some(event) = rx.recv().await {
            print_event(&event, json, &tz);
        }
    };
    tokio::select! {
        _ = printer => {}
        _ = tokio::signal::ctrl_c() => eprintln!("interrupted, shutting down"),
    }
    for task in tasks {
        task.abort();
    }
    Ok(())
}

fn print_event(event: &Event, json: bool, tz: &Tz) {
    if json {
        if let Ok(line) = serde_json::to_string(event) {
            println!("{line}");
        }
        return;
    }
    let stamp = fmt_time(Utc::now(), tz, "%H:%M:%S %Z");
    let station = event.station();
    let body = match event {
        Event::Station { source, info, .. } => {
            format!(
                "{} via {source}{}",
                info.name.as_deref().unwrap_or("(unnamed)"),
                fmt_location(info)
            )
        }
        Event::History { obs, .. } => format!("hist  {}", fmt_obs(obs, tz)),
        Event::CadenceLearned {
            description,
            samples,
            agreement,
            lag_min_s,
            lag_median_s,
            ..
        } => format!(
            "cadence {description} ({samples} intervals, {:.0}% agree); reports appear {} after the obs time (median {})",
            agreement * 100.0,
            fmt_lag(*lag_min_s, "no earlier than "),
            fmt_lag(*lag_median_s, ""),
        ),
        Event::NoCadence { rows, .. } => {
            format!("no regular cadence in {rows} rows; polling at the slow interval")
        }
        Event::Waiting {
            expected,
            poll_from,
            ..
        } => format!(
            "next row expected {}, polling from {}",
            fmt_time(*expected, tz, "%H:%M %Z"),
            fmt_time(*poll_from, tz, "%H:%M:%S %Z")
        ),
        Event::New {
            obs,
            latency_s,
            on_schedule,
            ..
        } => format!(
            "NEW   {}  [seen {latency_s}s after obs time{}]",
            fmt_obs(obs, tz),
            if *on_schedule { "" } else { ", unscheduled" }
        ),
        Event::Corrected { obs, .. } => format!("CORR  {}", fmt_obs(obs, tz)),
        Event::Missed { expected, .. } => format!(
            "no row for {} yet, moving on (it will still be reported if it turns up)",
            fmt_time(*expected, tz, "%H:%M %Z")
        ),
        Event::Error { message, .. } => format!("error {message}"),
    };
    println!("{stamp} {station:<6} {body}");
}

fn fmt_location(info: &StationInfo) -> String {
    let mut s = String::new();
    if let (Some(lat), Some(lon)) = (info.lat, info.lon) {
        s.push_str(&format!(" at {lat:.3}, {lon:.3}"));
    }
    if let Some(elev) = info.elevation_m {
        s.push_str(&format!(", {elev:.0} m"));
    }
    s
}

fn fmt_lag(secs: Option<f64>, prefix: &str) -> String {
    match secs {
        Some(s) => format!("{prefix}{}s", s.round() as i64),
        None => "unknown".to_owned(),
    }
}

/// Renders a UTC instant in the display zone; `%Z` gives the zone abbreviation (EDT, JST, UTC).
fn fmt_time(t: DateTime<Utc>, tz: &Tz, fmt: &str) -> String {
    t.with_timezone(tz).format(fmt).to_string()
}

fn fmt_obs(o: &Observation, tz: &Tz) -> String {
    let mut parts = vec![fmt_time(o.time, tz, "%m-%d %H:%M %Z")];
    if o.kind == ObsKind::Special {
        parts.push("SPECI".to_owned());
    }
    if let (Some(c), Some(f)) = (o.air_temp_c, o.air_temp_f) {
        parts.push(format!("T {c:.1}°C/{f:.1}°F"));
    }
    if let (Some(c), Some(f)) = (o.dew_point_c, o.dew_point_f) {
        parts.push(format!("Td {c:.1}°C/{f:.1}°F"));
    }
    if let Some(rh) = o.relative_humidity_pct {
        parts.push(format!("RH {rh:.0}%"));
    }
    match (o.wind_direction_deg, o.wind_speed_ms) {
        (Some(dir), Some(spd)) => parts.push(format!("wind {dir:03}° {spd:.1} m/s")),
        (None, Some(spd)) if o.wind_variable => parts.push(format!("wind VRB {spd:.1} m/s")),
        (None, Some(spd)) => parts.push(format!("wind {spd:.1} m/s")),
        _ => {}
    }
    if let Some(g) = o.wind_gust_ms {
        parts.push(format!("gust {g:.1} m/s"));
    }
    if let Some(v) = o.visibility_km {
        parts.push(format!("vis {v:.1} km"));
    }
    if let Some(p) = o.altimeter_hpa {
        parts.push(format!("QNH {p:.1} hPa"));
    }
    if let Some(p) = o.sea_level_pressure_hpa {
        parts.push(format!("SLP {p:.1} hPa"));
    }
    if let Some(w) = &o.weather {
        parts.push(w.clone());
    }
    if !o.clouds.is_empty() {
        let layers: Vec<String> = o
            .clouds
            .iter()
            .map(|c| match c.base_m {
                Some(base) => format!("{}{}m", c.cover, base as i64),
                None => c.cover.clone(),
            })
            .collect();
        parts.push(layers.join(" "));
    }
    if let Some(p) = o.precip_1h_mm {
        parts.push(format!("precip1h {p:.1} mm"));
    }
    parts.join("  ")
}
