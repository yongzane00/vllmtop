//! Per-endpoint polling tasks.
//!
//! Each configured endpoint gets independent tokio tasks that:
//! - start non-overlapping `/metrics` requests on an anchored cadence;
//! - skip cadence slots missed by a slow request instead of catching up;
//! - use fixed quarter-second fleet phase offsets instead of random jitter;
//! - probe `/health`, `/version`, and `/v1/models` independently;
//! - parses the exposition text off the UI path;
//! - reports a [`ScrapeOutcome`] over one shared bounded channel;
//! - never blocks or crashes any other endpoint's collection.
//!
//! Auth material resolved from the environment lives only inside the
//! reqwest client's default headers; it is never sent over the state
//! channel, stored, or formatted into errors.

use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{mpsc, watch};

use crate::config::{Config, EndpointConfig};
use crate::event::AppEvent;
use crate::metrics::parse::parse_text;
use crate::state::{ScrapeOutcome, ScrapePayload};

/// Cap on a `/metrics` response body; beyond this the scrape is an error
/// (runaway label cardinality would otherwise eat the monitor's memory).
pub const MAX_METRICS_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Cap on `/version` and `/v1/models` bodies. These are tiny JSON documents;
/// anything bigger means the URL points at the wrong service.
pub const MAX_METADATA_BODY_BYTES: usize = 256 * 1024;

/// Fetch `/version` and `/v1/models` every N cycles.
const METADATA_EVERY_CYCLES: u64 = 30;

/// Fleet-wide deterministic staggering. Endpoints after the fourth reuse the
/// four quarter-second slots rather than introducing per-cycle randomness.
const PHASE_STEP: Duration = Duration::from_millis(250);
const PHASE_SLOTS: usize = 4;

fn phase_offset(endpoint_index: usize) -> Duration {
    PHASE_STEP * (endpoint_index % PHASE_SLOTS) as u32
}

/// Return the first cadence slot strictly after `now`. Advancing from the
/// original anchor keeps starts phase-aligned; looping skips every slot that a
/// slow request occupied instead of queueing catch-up work.
fn next_slot_strictly_after(anchor: Instant, cadence: Duration, now: Instant) -> Instant {
    let mut next = anchor;
    while next <= now {
        next += cadence;
    }
    next
}

/// Handles for steering running collectors.
pub struct CollectorControl {
    /// Broadcast the (runtime-adjustable) refresh interval.
    pub interval_tx: watch::Sender<Duration>,
    /// Bump to force an immediate refresh of all endpoints.
    pub force_tx: watch::Sender<u64>,
}

impl CollectorControl {
    pub fn force_refresh(&self) {
        self.force_tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn set_interval(&self, interval: Duration) {
        // send_if_modified: an unchanged value (e.g. key auto-repeat at the
        // clamp floor) must not wake every collector.
        self.interval_tx.send_if_modified(|current| {
            if *current == interval {
                false
            } else {
                *current = interval;
                true
            }
        });
    }
}

/// Spawn one collector task per endpoint. Returns control handles.
/// `events` is the single bounded channel into the main loop.
pub fn spawn_all(config: &Config, events: mpsc::Sender<AppEvent>) -> CollectorControl {
    let (interval_tx, _) = watch::channel(config.refresh_interval);
    let (force_tx, _) = watch::channel(0u64);
    let fleet_anchor = Instant::now();

    for (idx, endpoint) in config.endpoints.iter().enumerate() {
        let task = CollectorTask::new(
            idx,
            endpoint.clone(),
            events.clone(),
            interval_tx.subscribe(),
            force_tx.subscribe(),
            fleet_anchor + phase_offset(idx),
        );
        let optional = OptionalTask {
            idx,
            endpoint: endpoint.clone(),
            events: events.clone(),
            client: task.client.clone(),
            cadence: config.refresh_interval,
            first_start: fleet_anchor + phase_offset(idx) + Duration::from_millis(125),
            cycle: 0,
        };
        tokio::spawn(task.run());
        tokio::spawn(optional.run());
    }

    CollectorControl {
        interval_tx,
        force_tx,
    }
}

struct CollectorTask {
    idx: usize,
    endpoint: EndpointConfig,
    events: mpsc::Sender<AppEvent>,
    interval_rx: watch::Receiver<Duration>,
    force_rx: watch::Receiver<u64>,
    /// Client is None when auth env resolution failed; `auth_error` says why.
    client: Option<reqwest::Client>,
    auth_error: Option<String>,
    first_start: Instant,
}

impl CollectorTask {
    fn new(
        idx: usize,
        endpoint: EndpointConfig,
        events: mpsc::Sender<AppEvent>,
        interval_rx: watch::Receiver<Duration>,
        force_rx: watch::Receiver<u64>,
        first_start: Instant,
    ) -> Self {
        let (client, auth_error) = match build_client(&endpoint) {
            Ok(client) => (Some(client), None),
            Err(e) => (None, Some(e)),
        };
        CollectorTask {
            idx,
            endpoint,
            events,
            interval_rx,
            force_rx,
            client,
            auth_error,
            first_start,
        }
    }

    async fn run(mut self) {
        let mut next_start = self.first_start;
        loop {
            if !self.wait_for_start(&mut next_start).await {
                return;
            }
            let started = Instant::now();
            if self
                .events
                .send(AppEvent::MetricsStarted {
                    endpoint: self.idx,
                    at: started,
                })
                .await
                .is_err()
            {
                return;
            }
            let outcome = self.poll_once().await;
            if self
                .events
                .send(AppEvent::Scrape {
                    endpoint: self.idx,
                    outcome,
                })
                .await
                .is_err()
            {
                return; // main loop is gone; shut down quietly
            }
            let cadence = *self.interval_rx.borrow_and_update();
            next_start = next_slot_strictly_after(next_start, cadence, Instant::now());
        }
    }

    async fn poll_once(&mut self) -> ScrapeOutcome {
        let Some(client) = self.client.clone() else {
            // Auth env vars missing: report a static, clear error. No HTTP.
            return ScrapeOutcome {
                at: Instant::now(),
                wall: SystemTime::now(),
                duration: Duration::ZERO,
                result: Err(self
                    .auth_error
                    .clone()
                    .unwrap_or_else(|| "endpoint disabled".into())),
            };
        };

        let started = Instant::now();
        let metrics = fetch_metrics(&client, &self.endpoint).await;
        let duration = started.elapsed();

        let mut payload = ScrapePayload {
            metrics: None,
            healthy: None,
            version: None,
            models: None,
        };

        let result = match metrics {
            Ok(text) => {
                payload.metrics = Some(parse_text(&text));
                Ok(payload)
            }
            Err(e) => Err(e),
        };

        ScrapeOutcome {
            at: Instant::now(),
            wall: SystemTime::now(),
            duration,
            result,
        }
    }

    /// Wait for the next anchored slot. Force-refresh may start early while
    /// idle; interval changes reset the fleet's fixed phase from change time.
    async fn wait_for_start(&mut self, next_start: &mut Instant) -> bool {
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until((*next_start).into()) => return true,
                res = self.force_rx.changed() => {
                    if res.is_ok() {
                        self.force_rx.borrow_and_update();
                        return true; // user asked for an immediate refresh
                    }
                    tokio::time::sleep_until((*next_start).into()).await;
                    return true;
                }
                res = self.interval_rx.changed() => {
                    if res.is_err() {
                        tokio::time::sleep_until((*next_start).into()).await;
                        return true;
                    }
                    self.interval_rx.borrow_and_update();
                    *next_start = Instant::now() + phase_offset(self.idx);
                }
            }
        }
    }
}

/// Optional probes have their own sequential schedule and client handle, so
/// none of them can hold up `/metrics` publication. They may overlap metrics,
/// but optional cycles cannot overlap one another or queue missed ticks.
struct OptionalTask {
    idx: usize,
    endpoint: EndpointConfig,
    events: mpsc::Sender<AppEvent>,
    client: Option<reqwest::Client>,
    cadence: Duration,
    first_start: Instant,
    cycle: u64,
}

impl OptionalTask {
    async fn run(mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let mut next_start = self.first_start;
        loop {
            tokio::time::sleep_until(next_start.into()).await;
            let want_metadata = self.cycle.is_multiple_of(METADATA_EVERY_CYCLES);
            let (healthy, version, models) = if want_metadata {
                tokio::join!(
                    fetch_health(&client, &self.endpoint),
                    fetch_version(&client, &self.endpoint),
                    fetch_models(&client, &self.endpoint),
                )
            } else {
                (fetch_health(&client, &self.endpoint).await, None, None)
            };
            if self
                .events
                .send(AppEvent::OptionalUpdate {
                    endpoint: self.idx,
                    healthy,
                    version,
                    models,
                })
                .await
                .is_err()
            {
                return;
            }
            self.cycle = self.cycle.wrapping_add(1);
            next_start = next_slot_strictly_after(next_start, self.cadence, Instant::now());
        }
    }
}

/// Build a reqwest client with resolved auth headers. The only place secret
/// values exist outside the environment.
fn build_client(endpoint: &EndpointConfig) -> Result<reqwest::Client, String> {
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

    let auth = endpoint.resolve_auth(|k| std::env::var(k).ok())?;

    let mut headers = HeaderMap::new();
    if let Some(token) = auth.bearer {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "bearer token contains invalid header characters".to_string())?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    for (name, value) in auth.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| format!("header {name:?} value contains invalid characters"))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        // Metrics endpoints don't redirect; refusing redirects also removes
        // any chance of auth headers leaking to another origin.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("vllmtop/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

fn endpoint_url(endpoint: &EndpointConfig, path: &str) -> String {
    let base = endpoint.url.as_str();
    if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Errors are reduced to short strings before leaving the collector; they
/// contain reqwest's message (URL host/port at most) and never headers.
async fn fetch_metrics(
    client: &reqwest::Client,
    endpoint: &EndpointConfig,
) -> Result<String, String> {
    let url = endpoint_url(endpoint, "metrics");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| http_error("/metrics", &e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("/metrics returned HTTP {}", status.as_u16()));
    }
    read_capped(resp, "/metrics", MAX_METRICS_BODY_BYTES).await
}

/// Read a body while enforcing a byte cap — EVERY response body goes through
/// this; `Response::json()`/`text()` would buffer without limit.
async fn read_capped(resp: reqwest::Response, what: &str, cap: usize) -> Result<String, String> {
    if let Some(len) = resp.content_length()
        && len as usize > cap
    {
        return Err(format!("{what} body is {len} bytes (limit {cap})"));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.map_err(|e| http_error(what, &e))? {
        if buf.len() + chunk.len() > cap {
            return Err(format!("{what} body exceeds {cap} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| format!("{what} body is not valid UTF-8"))
}

/// Health semantics: 2xx = healthy, 5xx and other server states = unhealthy,
/// but a server that simply doesn't HAVE /health (404/405/501) or that we
/// cannot ask (request error) is UNKNOWN — the endpoint is optional and its
/// absence must display as N/A, not as a red health flag.
async fn fetch_health(client: &reqwest::Client, endpoint: &EndpointConfig) -> Option<bool> {
    let url = endpoint_url(endpoint, "health");
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Some(true)
            } else if matches!(status.as_u16(), 404 | 405 | 501) {
                None
            } else {
                Some(false)
            }
        }
        Err(_) => None,
    }
}

async fn fetch_version(client: &reqwest::Client, endpoint: &EndpointConfig) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Version {
        version: String,
    }
    let url = endpoint_url(endpoint, "version");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = read_capped(resp, "/version", MAX_METADATA_BODY_BYTES)
        .await
        .ok()?;
    serde_json::from_str::<Version>(&body)
        .ok()
        .map(|v| v.version)
}

async fn fetch_models(client: &reqwest::Client, endpoint: &EndpointConfig) -> Option<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Models {
        data: Vec<ModelEntry>,
    }
    #[derive(serde::Deserialize)]
    struct ModelEntry {
        id: String,
    }
    let url = endpoint_url(endpoint, "v1/models");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = read_capped(resp, "/v1/models", MAX_METADATA_BODY_BYTES)
        .await
        .ok()?;
    serde_json::from_str::<Models>(&body)
        .ok()
        .map(|m| m.data.into_iter().map(|e| e.id).collect())
}

/// reqwest errors include the URL; keep host/path but make sure no query or
/// userinfo can appear by using `without_url` and appending our own path tag.
fn http_error(what: &str, e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "body error"
    } else {
        "request failed"
    };
    match source_chain_root(e) {
        Some(detail) => format!("{what}: {kind}: {detail}"),
        None => format!("{what}: {kind}"),
    }
}

/// The innermost error source is the useful part ("Connection refused");
/// the outer layers repeat the URL, which we deliberately avoid echoing so
/// no query string or userinfo can leak into displayed errors.
fn source_chain_root(e: &reqwest::Error) -> Option<String> {
    let mut cur: &(dyn std::error::Error + 'static) = e;
    let mut last = None;
    while let Some(next) = cur.source() {
        last = Some(next.to_string());
        cur = next;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use url::Url;

    fn ep(url: &str) -> EndpointConfig {
        EndpointConfig {
            name: "t".into(),
            url: Url::parse(url).unwrap(),
            bearer_token_env: None,
            header_env: BTreeMap::new(),
            max_running: None,
        }
    }

    #[test]
    fn url_joining_handles_trailing_slash_and_base_paths() {
        assert_eq!(
            endpoint_url(&ep("http://h:8000"), "metrics"),
            "http://h:8000/metrics"
        );
        assert_eq!(
            endpoint_url(&ep("http://h:8000/"), "metrics"),
            "http://h:8000/metrics"
        );
        assert_eq!(
            endpoint_url(&ep("http://h:8000/proxy/vllm"), "v1/models"),
            "http://h:8000/proxy/vllm/v1/models"
        );
    }

    #[test]
    fn fleet_phase_offsets_repeat_in_four_quarter_second_slots() {
        let expected_ms = [0, 250, 500, 750, 0, 250, 500, 750];
        for (idx, expected) in expected_ms.into_iter().enumerate() {
            assert_eq!(phase_offset(idx), Duration::from_millis(expected));
        }
    }

    #[test]
    fn anchored_scheduler_skips_every_slot_missed_by_a_slow_scrape() {
        let anchor = Instant::now();
        let cadence = Duration::from_secs(1);
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_millis(200)),
            anchor + Duration::from_secs(1)
        );
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_millis(2_300)),
            anchor + Duration::from_secs(3)
        );
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_secs(3)),
            anchor + Duration::from_secs(4)
        );
    }

    #[test]
    fn missing_auth_env_produces_client_error_not_panic() {
        let endpoint = EndpointConfig {
            name: "s".into(),
            url: Url::parse("https://h:1").unwrap(),
            bearer_token_env: Some("VLLMTOP_TEST_UNSET_VAR_XYZ".into()),
            header_env: BTreeMap::new(),
            max_running: None,
        };
        let err = build_client(&endpoint).unwrap_err();
        assert!(err.contains("VLLMTOP_TEST_UNSET_VAR_XYZ"));
    }
}
