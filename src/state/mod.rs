//! Application state: one `AppState` owned by the main loop, updated by
//! reducing collector events, read by the renderer. No locks, no sharing.

pub mod history;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime};

use crate::metrics::histogram::{HistogramPoint, HistogramWindow, WindowEstimate};
use crate::metrics::model::ScrapeText;
use crate::metrics::normalize::{CuratedScrape, RawHistogram, SeriesKey, curate};
use crate::metrics::rates::{CounterBank, RateSample};
use history::RingSeries;

/// History series ids (per endpoint × series key).
pub mod series_id {
    pub const RUNNING: &str = "running";
    pub const WAITING: &str = "waiting";
    pub const KV_USAGE: &str = "kv_usage";
    pub const PROMPT_TPS: &str = "prompt_tps";
    pub const GENERATION_TPS: &str = "generation_tps";
    pub const REQUEST_RATE: &str = "request_rate";
    pub const TTFT_P95: &str = "ttft_p95";
    pub const ITL_P95: &str = "itl_p95";
    pub const E2E_P95: &str = "e2e_p95";
    pub const ERRORS: &str = "errors";
    pub const PREEMPTIONS: &str = "preemptions";

    pub const ALL: &[&str] = &[
        RUNNING,
        WAITING,
        KV_USAGE,
        PROMPT_TPS,
        GENERATION_TPS,
        REQUEST_RATE,
        TTFT_P95,
        ITL_P95,
        E2E_P95,
        ERRORS,
        PREEMPTIONS,
    ];
}

/// What a collector reports after one poll cycle.
#[derive(Debug)]
pub struct ScrapeOutcome {
    /// Monotonic completion time of the scrape.
    pub at: Instant,
    /// Wall-clock completion time (display/recording only).
    pub wall: SystemTime,
    /// How long the `/metrics` request took.
    pub duration: Duration,
    pub result: Result<ScrapePayload, String>,
}

/// Successful poll payload. Optional endpoints may be probed less often;
/// `None` means "not checked this round", not "absent".
#[derive(Debug, Default)]
pub struct ScrapePayload {
    pub metrics: Option<ScrapeText>,
    pub healthy: Option<bool>,
    pub version: Option<String>,
    pub models: Option<Vec<String>>,
}

/// Windowed/derived values for one (model, engine) series.
#[derive(Debug, Clone, Default)]
pub struct DerivedSeries {
    pub prompt_tps: Option<f64>,
    pub generation_tps: Option<f64>,
    /// Completions per second, summed over finish reasons with valid intervals.
    pub request_rate: Option<f64>,
    /// New error+abort finishes in the last interval.
    pub error_abort_delta: Option<f64>,
    pub preemption_delta: Option<f64>,
    pub preemption_rate: Option<f64>,
    /// Windowed prefix-cache hit rate (delta hits / delta queries).
    pub prefix_hit_rate_window: Option<f64>,
    /// Any curated counter for this series went backwards this scrape.
    pub reset_detected: bool,
    /// Histogram estimates keyed by canonical id (`metrics::normalize::hist`).
    pub estimates: BTreeMap<&'static str, WindowEstimate>,
}

/// Connection status, separate from data staleness.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnStatus {
    #[default]
    NeverConnected,
    Connected,
    Failing {
        error: String,
        consecutive: u32,
    },
}

/// How fresh an endpoint's data is, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Recent successful scrape.
    Fresh,
    /// Last good data is older than the staleness threshold; the values shown
    /// are the preserved last-good snapshot.
    Stale,
    /// No successful scrape yet.
    Never,
}

#[derive(Debug)]
pub struct EndpointState {
    pub name: String,
    /// URL safe for display (no userinfo/query).
    pub display_url: String,
    pub status: ConnStatus,
    pub vllm_version: Option<String>,
    pub served_models: Vec<String>,
    /// `/health` result: None = never checked / unknown.
    pub healthy: Option<bool>,
    /// Monotonic start time of the most recent `/metrics` attempt.
    pub last_attempt_at: Option<Instant>,
    /// Present only while that endpoint's non-overlapping `/metrics` request
    /// is in flight.
    pub scraping_since: Option<Instant>,
    pub last_ok_at: Option<Instant>,
    pub last_ok_wall: Option<SystemTime>,
    pub last_scrape_duration: Option<Duration>,
    pub total_scrapes: u64,
    pub total_failures: u64,
    pub parse_issue_count: usize,
    /// Last good curated extraction, preserved while stale.
    pub curated: Option<CuratedScrape>,
    pub derived: BTreeMap<SeriesKey, DerivedSeries>,
    /// Set when a counter reset was seen (vLLM restart); cleared after display.
    pub restart_seen_at: Option<Instant>,
    /// Rolling history per (series, metric id).
    pub history: HashMap<(SeriesKey, &'static str), RingSeries>,

    counters: CounterBank<(SeriesKey, String)>,
    windows: HashMap<(SeriesKey, &'static str), HistogramWindow>,
    history_window: Duration,
    percentile_window: Duration,
}

impl EndpointState {
    pub fn new(
        name: String,
        display_url: String,
        history_window: Duration,
        percentile_window: Duration,
    ) -> Self {
        EndpointState {
            name,
            display_url,
            status: ConnStatus::default(),
            vllm_version: None,
            served_models: Vec::new(),
            healthy: None,
            last_attempt_at: None,
            scraping_since: None,
            last_ok_at: None,
            last_ok_wall: None,
            last_scrape_duration: None,
            total_scrapes: 0,
            total_failures: 0,
            parse_issue_count: 0,
            curated: None,
            derived: BTreeMap::new(),
            restart_seen_at: None,
            history: HashMap::new(),
            counters: CounterBank::new(),
            windows: HashMap::new(),
            history_window,
            percentile_window,
        }
    }

    /// Data freshness relative to `now`, given the configured refresh interval.
    pub fn freshness(&self, now: Instant, refresh_interval: Duration) -> Freshness {
        match self.last_ok_at {
            None => Freshness::Never,
            Some(at) => {
                let threshold = staleness_threshold(refresh_interval);
                if now.saturating_duration_since(at) > threshold {
                    Freshness::Stale
                } else {
                    Freshness::Fresh
                }
            }
        }
    }

    /// Apply one scrape outcome.
    pub fn apply(&mut self, outcome: ScrapeOutcome) {
        self.scraping_since = None;
        self.total_scrapes += 1;
        match outcome.result {
            Err(error) => {
                self.total_failures += 1;
                let consecutive = match &self.status {
                    ConnStatus::Failing { consecutive, .. } => consecutive + 1,
                    _ => 1,
                };
                self.status = ConnStatus::Failing { error, consecutive };
                // Deliberately keep raw/curated/derived: last good snapshot
                // stays visible, flagged stale via freshness().
            }
            Ok(payload) => {
                self.status = ConnStatus::Connected;
                if let Some(healthy) = payload.healthy {
                    self.healthy = Some(healthy);
                }
                if let Some(version) = payload.version {
                    self.vllm_version = Some(version);
                }
                if let Some(models) = payload.models {
                    self.served_models = models;
                }
                if let Some(text) = payload.metrics {
                    self.last_ok_at = Some(outcome.at);
                    self.last_ok_wall = Some(outcome.wall);
                    self.last_scrape_duration = Some(outcome.duration);
                    self.parse_issue_count = text.issues.len();
                    self.ingest_metrics(text, outcome.at);
                }
            }
        }
    }

    pub fn mark_attempt_started(&mut self, at: Instant) {
        self.last_attempt_at = Some(at);
        self.scraping_since = Some(at);
    }

    pub fn apply_optional(
        &mut self,
        healthy: Option<bool>,
        version: Option<String>,
        models: Option<Vec<String>>,
    ) {
        if let Some(healthy) = healthy {
            self.healthy = Some(healthy);
        }
        if let Some(version) = version {
            self.vllm_version = Some(version);
        }
        if let Some(models) = models {
            self.served_models = models;
        }
    }

    fn ingest_metrics(&mut self, text: ScrapeText, at: Instant) {
        let curated = curate(&text);
        self.counters.begin_scrape();
        let mut derived: BTreeMap<SeriesKey, DerivedSeries> = BTreeMap::new();

        for (key, series) in &curated.series {
            let mut d = DerivedSeries::default();
            let mut resets = false;

            let mut rate_of = |metric: &str, value: Option<f64>| -> RateSample {
                match value {
                    Some(v) => {
                        let s = self
                            .counters
                            .update((key.clone(), metric.to_string()), at, v);
                        resets |= s.is_reset();
                        s
                    }
                    None => RateSample::Unavailable,
                }
            };

            d.prompt_tps = rate_of("prompt_tokens", series.prompt_tokens).per_sec();
            d.generation_tps = rate_of("generation_tokens", series.generation_tokens).per_sec();

            let preemption = rate_of("preemptions", series.preemptions);
            d.preemption_rate = preemption.per_sec();
            d.preemption_delta = preemption.delta();

            // Completions: rate per finish reason, summed where available.
            // If ANY reason's counter reset this interval, the family is
            // inconsistent — summing the survivors would understate the rate
            // as a fake-but-plausible number. Report unavailable instead;
            // it recovers on the next interval like every other rate.
            let mut total_rate: Option<f64> = None;
            let mut error_abort_delta: Option<f64> = None;
            let mut success_reset = false;
            for (reason, value) in &series.success_by_reason {
                let s = rate_of(&format!("success:{reason}"), Some(*value));
                success_reset |= s.is_reset();
                if let RateSample::Rate { per_sec, delta } = s {
                    *total_rate.get_or_insert(0.0) += per_sec;
                    if reason == "error" || reason == "abort" {
                        *error_abort_delta.get_or_insert(0.0) += delta;
                    }
                }
            }
            if success_reset {
                total_rate = None;
                error_abort_delta = None;
            }
            d.request_rate = total_rate;
            d.error_abort_delta = error_abort_delta;

            // Windowed prefix-cache hit rate.
            let hits = rate_of("prefix_hits", series.prefix_cache_hits).delta();
            let queries = rate_of("prefix_queries", series.prefix_cache_queries).delta();
            d.prefix_hit_rate_window = match (hits, queries) {
                (Some(h), Some(q)) if q > 0.0 => Some((h / q).clamp(0.0, 1.0)),
                _ => None,
            };

            // Histogram windows.
            for (canonical, raw_histogram) in &series.histograms {
                if let Some(point) = to_point(raw_histogram, at) {
                    let window = self
                        .windows
                        .entry((key.clone(), canonical))
                        .or_insert_with(|| HistogramWindow::new(self.percentile_window));
                    window.push(point);
                    if let Some(estimate) = window.estimate() {
                        d.estimates.insert(canonical, estimate);
                    }
                }
            }

            d.reset_detected = resets;
            if resets {
                self.restart_seen_at = Some(at);
            }
            derived.insert(key.clone(), d);
        }
        self.counters.end_scrape();

        // Drop histogram windows for series that disappeared.
        self.windows
            .retain(|(key, _), _| curated.series.contains_key(key));

        // Drop history for series that disappeared entirely (bounded memory);
        // keep everything while the endpoint itself is merely stale.
        self.history
            .retain(|(key, _), _| curated.series.contains_key(key));

        self.derived = derived;
        self.curated = Some(curated);

        // Record history points from the same rows the recorder sees.
        let window = self.history_window;
        for (key, id, value) in self.current_samples() {
            self.history
                .entry((key, id))
                .or_insert_with(|| RingSeries::new(window))
                .push(at, value);
        }
    }

    /// The latest value for every (series, metric id) pair — the exact rows
    /// history charts and the SQLite recorder consume, so both always agree.
    pub fn current_samples(&self) -> Vec<(SeriesKey, &'static str, f64)> {
        let Some(curated) = &self.curated else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (key, series) in &curated.series {
            let d = self.derived.get(key);
            let est =
                |h: &str| -> Option<f64> { d.and_then(|d| d.estimates.get(h)).and_then(|e| e.p95) };
            let pairs: [(&'static str, Option<f64>); 11] = [
                (series_id::RUNNING, series.running),
                (series_id::WAITING, series.waiting),
                (series_id::KV_USAGE, series.kv_cache_usage),
                (series_id::PROMPT_TPS, d.and_then(|d| d.prompt_tps)),
                (series_id::GENERATION_TPS, d.and_then(|d| d.generation_tps)),
                (series_id::REQUEST_RATE, d.and_then(|d| d.request_rate)),
                (
                    series_id::TTFT_P95,
                    est(crate::metrics::normalize::hist::TTFT),
                ),
                (
                    series_id::ITL_P95,
                    est(crate::metrics::normalize::hist::INTER_TOKEN_LATENCY),
                ),
                (
                    series_id::E2E_P95,
                    est(crate::metrics::normalize::hist::E2E_LATENCY),
                ),
                (series_id::ERRORS, d.and_then(|d| d.error_abort_delta)),
                (series_id::PREEMPTIONS, d.and_then(|d| d.preemption_delta)),
            ];
            for (id, value) in pairs {
                if let Some(v) = value {
                    rows.push((key.clone(), id, v));
                }
            }
        }
        rows
    }

    /// Endpoint-level aggregate over all series (fleet view building block).
    pub fn aggregate(&self) -> EndpointAggregate {
        let mut agg = EndpointAggregate::default();
        let Some(curated) = &self.curated else {
            return agg;
        };
        let mut kv_pairs: Vec<(f64, Option<f64>)> = Vec::new();
        for (key, series) in &curated.series {
            let derived = self.derived.get(key);
            sum_opt(&mut agg.running, series.running);
            sum_opt(&mut agg.waiting, series.waiting);
            if let Some(d) = derived {
                sum_opt(&mut agg.prompt_tps, d.prompt_tps);
                sum_opt(&mut agg.generation_tps, d.generation_tps);
                sum_opt(&mut agg.request_rate, d.request_rate);
                sum_opt(&mut agg.error_abort_delta, d.error_abort_delta);
                sum_opt(&mut agg.preemption_delta, d.preemption_delta);
                if let Some(p95) = d
                    .estimates
                    .get(crate::metrics::normalize::hist::TTFT)
                    .and_then(|e| e.p95)
                {
                    agg.worst_ttft_p95 = Some(agg.worst_ttft_p95.map_or(p95, |w: f64| w.max(p95)));
                }
            }
            if let Some(usage) = series.kv_cache_usage {
                kv_pairs.push((usage, series.kv_cache_size_tokens));
            }
        }
        agg.kv_usage = aggregate_kv(&kv_pairs);
        agg.models = curated
            .series
            .keys()
            .map(|k| k.model.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        agg
    }
}

/// KV aggregation policy: capacity-weighted when every series exposes
/// capacity; otherwise an unweighted mean, explicitly labelled as such.
/// Percentages are never silently averaged as if globally meaningful.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KvAggregate {
    /// Weighted by KV capacity in tokens (comparable across series).
    CapacityWeighted(f64),
    /// Plain mean of per-series usage fractions; label it in the UI.
    UnweightedMean(f64),
}

impl KvAggregate {
    pub fn value(self) -> f64 {
        match self {
            KvAggregate::CapacityWeighted(v) | KvAggregate::UnweightedMean(v) => v,
        }
    }

    pub fn is_weighted(self) -> bool {
        matches!(self, KvAggregate::CapacityWeighted(_))
    }
}

pub fn aggregate_kv(pairs: &[(f64, Option<f64>)]) -> Option<KvAggregate> {
    if pairs.is_empty() {
        return None;
    }
    let total_capacity: Option<f64> = pairs.iter().map(|(_, c)| *c).sum();
    match total_capacity {
        Some(cap) if cap > 0.0 => {
            let weighted: f64 = pairs.iter().map(|(u, c)| u * c.unwrap_or(0.0)).sum::<f64>() / cap;
            Some(KvAggregate::CapacityWeighted(weighted))
        }
        _ => {
            let mean = pairs.iter().map(|(u, _)| u).sum::<f64>() / pairs.len() as f64;
            Some(KvAggregate::UnweightedMean(mean))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EndpointAggregate {
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub generation_tps: Option<f64>,
    pub request_rate: Option<f64>,
    pub error_abort_delta: Option<f64>,
    pub preemption_delta: Option<f64>,
    pub worst_ttft_p95: Option<f64>,
    pub kv_usage: Option<KvAggregate>,
    pub models: Vec<String>,
}

fn sum_opt(acc: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0.0) + v);
    }
}

fn to_point(raw: &RawHistogram, at: Instant) -> Option<HistogramPoint> {
    HistogramPoint::new(at, raw.buckets.clone(), raw.sum, raw.count)
}

/// Stale when no success for 3 intervals (min 5s) — tolerates one missed
/// scrape plus jitter without flapping.
pub fn staleness_threshold(refresh_interval: Duration) -> Duration {
    (refresh_interval * 3).max(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrape(at: Instant, secs_offset: f64, text: &str) -> ScrapeOutcome {
        ScrapeOutcome {
            at: at + Duration::from_secs_f64(secs_offset),
            wall: SystemTime::now(),
            duration: Duration::from_millis(10),
            result: Ok(ScrapePayload {
                metrics: Some(crate::metrics::parse::parse_text(text)),
                ..Default::default()
            }),
        }
    }

    fn metrics_text(prompt: f64, generated: f64, stop: f64, error: f64) -> String {
        format!(
            r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{{engine="0",model_name="m"}} 2.0
# TYPE vllm:kv_cache_usage_perc gauge
vllm:kv_cache_usage_perc{{engine="0",model_name="m"}} 0.4
# TYPE vllm:prompt_tokens_total counter
vllm:prompt_tokens_total{{engine="0",model_name="m"}} {prompt}
# TYPE vllm:generation_tokens_total counter
vllm:generation_tokens_total{{engine="0",model_name="m"}} {generated}
# TYPE vllm:request_success_total counter
vllm:request_success_total{{engine="0",finished_reason="stop",model_name="m"}} {stop}
vllm:request_success_total{{engine="0",finished_reason="error",model_name="m"}} {error}
"#
        )
    }

    fn ep() -> EndpointState {
        EndpointState::new(
            "test".into(),
            "http://localhost:8000".into(),
            Duration::from_secs(300),
            Duration::from_secs(60),
        )
    }

    fn key() -> SeriesKey {
        SeriesKey {
            model: "m".into(),
            engine: Some("0".into()),
        }
    }

    #[test]
    fn first_scrape_has_gauges_but_no_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.prompt_tps, None);
        assert_eq!(d.request_rate, None);
        let c = &e.curated.as_ref().unwrap().series[&key()];
        assert_eq!(c.running, Some(2.0));
        assert_eq!(c.kv_cache_usage, Some(0.4));
    }

    #[test]
    fn second_scrape_produces_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        e.apply(scrape(base, 2.0, &metrics_text(1200.0, 700.0, 14.0, 1.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.prompt_tps, Some(100.0));
        assert_eq!(d.generation_tps, Some(100.0));
        // (14-10)/2 + (1-0)/2 = 2.5 completions/sec
        assert_eq!(d.request_rate, Some(2.5));
        assert_eq!(d.error_abort_delta, Some(1.0));
    }

    #[test]
    fn counter_reset_marks_restart_and_suppresses_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(5000.0, 4000.0, 50.0, 0.0)));
        e.apply(scrape(base, 1.0, &metrics_text(100.0, 80.0, 1.0, 0.0)));
        let d = &e.derived[&key()];
        assert!(d.reset_detected);
        assert!(e.restart_seen_at.is_some());
        assert_eq!(d.prompt_tps, None);
        // Rates recover on the next interval.
        e.apply(scrape(base, 2.0, &metrics_text(200.0, 160.0, 2.0, 0.0)));
        assert_eq!(e.derived[&key()].prompt_tps, Some(100.0));
    }

    #[test]
    fn partial_reset_within_success_family_reports_unavailable_not_partial_sum() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 50.0, 10.0)));
        // 'stop' keeps growing but 'error' resets (e.g. label re-registration):
        // summing only the survivor would fake a plausible-but-wrong rate.
        e.apply(scrape(base, 1.0, &metrics_text(1100.0, 600.0, 60.0, 2.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.request_rate, None);
        assert_eq!(d.error_abort_delta, None);
        // Unrelated rates in other families are unaffected.
        assert_eq!(d.prompt_tps, Some(100.0));
        // Next interval recovers.
        e.apply(scrape(base, 2.0, &metrics_text(1200.0, 700.0, 70.0, 3.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.request_rate, Some(11.0)); // (70-60) + (3-2)
        assert_eq!(d.error_abort_delta, Some(1.0));
    }

    #[test]
    fn failure_preserves_last_good_snapshot() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        e.apply(ScrapeOutcome {
            at: base + Duration::from_secs(1),
            wall: SystemTime::now(),
            duration: Duration::from_millis(5),
            result: Err("connection refused".into()),
        });
        assert!(matches!(e.status, ConnStatus::Failing { .. }));
        // Snapshot survives.
        assert!(e.curated.is_some());
        assert_eq!(
            e.curated.as_ref().unwrap().series[&key()].running,
            Some(2.0)
        );
        assert_eq!(e.total_failures, 1);
    }

    #[test]
    fn consecutive_failures_counted() {
        let base = Instant::now();
        let mut e = ep();
        for i in 0..3 {
            e.apply(ScrapeOutcome {
                at: base + Duration::from_secs(i),
                wall: SystemTime::now(),
                duration: Duration::ZERO,
                result: Err("timeout".into()),
            });
        }
        match &e.status {
            ConnStatus::Failing { consecutive, .. } => assert_eq!(*consecutive, 3),
            s => panic!("unexpected status {s:?}"),
        }
    }

    #[test]
    fn freshness_transitions() {
        let base = Instant::now();
        let mut e = ep();
        let interval = Duration::from_secs(1);
        assert_eq!(e.freshness(base, interval), Freshness::Never);
        e.apply(scrape(base, 0.0, &metrics_text(1.0, 1.0, 1.0, 0.0)));
        assert_eq!(
            e.freshness(base + Duration::from_secs(2), interval),
            Freshness::Fresh
        );
        assert_eq!(
            e.freshness(base + Duration::from_secs(30), interval),
            Freshness::Stale
        );
    }

    #[test]
    fn attempt_state_distinguishes_in_flight_from_last_success() {
        let base = Instant::now();
        let mut e = ep();
        e.mark_attempt_started(base);
        assert_eq!(e.last_attempt_at, Some(base));
        assert_eq!(e.scraping_since, Some(base));
        assert_eq!(e.last_ok_at, None);

        e.apply(scrape(base, 2.0, &metrics_text(1.0, 1.0, 1.0, 0.0)));
        assert_eq!(e.scraping_since, None);
        assert_eq!(e.last_attempt_at, Some(base));
        assert_eq!(e.last_ok_at, Some(base + Duration::from_secs(2)));
    }

    #[test]
    fn history_accumulates() {
        let base = Instant::now();
        let mut e = ep();
        for i in 0..5 {
            let v = 1000.0 + i as f64 * 100.0;
            e.apply(scrape(base, i as f64, &metrics_text(v, v, 10.0, 0.0)));
        }
        let running = &e.history[&(key(), series_id::RUNNING)];
        assert_eq!(running.len(), 5);
        // Rates only exist from the second scrape on.
        let tps = &e.history[&(key(), series_id::PROMPT_TPS)];
        assert_eq!(tps.len(), 4);
        assert_eq!(tps.latest().unwrap().value, 100.0);
    }

    #[test]
    fn kv_aggregation_weighted_vs_unweighted() {
        // Both capacities known: weighted.
        let agg = aggregate_kv(&[(0.9, Some(1000.0)), (0.1, Some(9000.0))]).unwrap();
        assert!(agg.is_weighted());
        assert!((agg.value() - 0.18).abs() < 1e-9);
        // Any capacity missing: unweighted mean, flagged.
        let agg = aggregate_kv(&[(0.9, Some(1000.0)), (0.1, None)]).unwrap();
        assert!(!agg.is_weighted());
        assert!((agg.value() - 0.5).abs() < 1e-9);
        assert_eq!(aggregate_kv(&[]), None);
    }

    #[test]
    fn zero_vs_unavailable_distinction() {
        let base = Instant::now();
        let mut e = ep();
        // Endpoint exposes running but nothing else.
        let text = "# TYPE vllm:num_requests_running gauge\nvllm:num_requests_running{model_name=\"m\"} 0.0\n";
        e.apply(scrape(base, 0.0, text));
        let k = SeriesKey {
            model: "m".into(),
            engine: None,
        };
        let c = &e.curated.as_ref().unwrap().series[&k];
        assert_eq!(c.running, Some(0.0)); // real zero
        assert_eq!(c.waiting, None); // unavailable, distinct from zero
        assert_eq!(c.kv_cache_usage, None);
    }
}
