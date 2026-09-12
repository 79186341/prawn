# prawn

Live weather-station feed in metric units, polled around each station's own
reporting cadence. Prototype.

```
cargo run -- RJTT            # Tokyo Haneda, reports every 30 min
cargo run -- RJTT KSLC KJFK  # several stations, one task each
cargo run -- --json RJTT     # one JSON object per line, pipe-friendly
cargo run -- --once RJTT     # print the last 24 h and exit
```

## How the weather.gov page works, and why this doesn't scrape it

`https://www.weather.gov/wrh/timeseries?site=rjtt` is a static HTML page whose
`obs.js` calls the Synoptic Data API (`api.synopticdata.com/v2/stations/timeseries`)
from the browser with a token published in `apiKey.js`. The "Metric" toggle simply
omits the `units=` parameter, because metric is Synoptic's default. The page
re-fetches the whole series every 5 minutes with no awareness of when the next
report is due.

That token is locked to weather.gov (`Invalid request per token rules` from
anywhere else), and Synoptic's free "Open Access" tier is now limited to US academic
accounts. So the default backend here is the Aviation Weather Center data API
(`aviationweather.gov/api/data/metar`), which is token-free, covers airport
stations worldwide, and returns the same METAR reports the weather.gov page shows
for RJTT. A Synoptic backend is included for anyone with their own token; it also
covers non-airport networks (RAWS, mesonets) that report every 5 or 10 minutes.

| Backend | Flag | Token | Coverage | Verified live |
|---|---|---|---|---|
| Aviation Weather Center | default | none | ICAO airport stations, global | yes |
| Synoptic Data | `--source synoptic` | `SYNOPTIC_TOKEN` | every network on the weather.gov page | parser only (fixture) |

## Polling strategy

1. **Prime.** Load 24 h of history. Infer the cadence as the most common
   interval between routine reports, rounded to the minute, plus the phase
   within that interval (RJTT: every 30 min at :00; KSLC: every 60 min at :54).
   SPECIs are excluded from the inference.
2. **Learn the publish lag.** AWC stamps each report with a `receiptTime`, so
   the history alone says how long after the observation time reports appear.
   Measured on 2026-09-12: RJTT 329 s to 635 s (median 477 s), KSLC 242 s to
   957 s (median 253 s). Without receipt times (Synoptic) the lag is learned from
   live detections instead.
3. **Wait, then poll fast.** For each expected time, sleep until
   `expected + min_lag - 60 s`, then poll every 20 s until a row at or after the
   expected time appears. After the longest lag seen (or 10 min) the poll
   interval drops to 60 s. If the next slot arrives first, it is reported as
   missed and the loop moves on; any late row is still emitted the moment a
   later poll sees it.
4. **Dedupe by observation time.** Every row not seen before is emitted, so
   SPECIs and other unscheduled reports come through as well. A re-issued report
   for an already-seen time is emitted as a correction.

Request budget: roughly 6 to 20 requests per station per report, well inside
AWC's 100 requests per minute. A cache in front of AWC reports `max-age=60` but
does not actually cache these responses, so every poll is fresh.

## Output

Human-readable log by default, with times shown in Eastern (America/New_York)
unless `--tz` says otherwise; `--tz UTC` or `--tz Asia/Tokyo` also work, and the
zone abbreviation (EDT, EST, UTC, JST) is printed next to every time. The
minute alignment in the cadence line is zone-independent. `--json` keeps all
timestamps in UTC and emits one event per line:
`station`, `history`, `cadence`, `waiting`, `observation`, `corrected`,
`missed`, `error`. Observations are metric throughout: degrees Celsius, metres per
second, kilometres, hectopascals, millimetres, metres. Air temperature and dew
point are also given in Fahrenheit (`T 20.0°C/68.0°F` in the log, `air_temp_f`
and `dew_point_f` in JSON).

```
15:37:34Z RJTT   cadence every 30 min, aligned to :00 (46 intervals, 100% agree); reports appear no earlier than 329s after the obs time (median 477s)
15:37:34Z RJTT   next row expected 15:30Z, polling from 15:34:29Z
15:38:57Z RJTT   NEW   09-12 15:30Z  T 20.0°C  Td 20.0°C  RH 100%  wind 030° 3.1 m/s  vis 7.0 km  QNH 1023.0 hPa  -RA  FEW122m BKN427m BKN610m  [seen 537s after obs time]
15:38:57Z RJTT   next row expected 16:00Z, polling from 16:04:29Z
```

Those are real lines from the first live run, captured with `--tz UTC` and before
the Fahrenheit column was added. AWC stamped that 15:30Z report as
received at 15:38:07Z; polls at 15:38:14Z and 15:38:34Z still did not return it,
and the one at 15:38:54Z did. So the API seems to refresh about once a minute
after receipt, and the feed adds at most one poll interval on top of that.

## Options

```
--history 24h         history loaded at startup
--replay 3            historical rows printed at startup
--poll-interval 20s   fast poll interval
--slow-interval 60s   poll interval once a row is overdue
--fast-window 10m     minimum fast-polling window
--lag-margin 60s      start polling this much before the shortest lag seen
--idle-poll <dur>     optional background poll between slots (catches SPECIs sooner)
--user-agent <ua>     identify yourself to the upstream API
--tz America/New_York time zone for displayed times (IANA name); JSON stays UTC
RUST_LOG=debug        show every request on stderr
```

## Layout

- `src/model.rs` metric observation model and unit constants
- `src/schedule.rs` cadence inference, next-slot prediction, lag model
- `src/source/awc.rs` Aviation Weather Center backend
- `src/source/synoptic.rs` Synoptic Data backend
- `src/feed.rs` per-station polling loop and event stream
- `src/main.rs` CLI and output formatting

## Not done yet

- Synoptic backend untested against the live API (needs a token).
- ETag conditional requests: AWC returns 304 for unchanged data, which would
  make each poll nearly free.
- Persisting learned cadence and lag across restarts.
