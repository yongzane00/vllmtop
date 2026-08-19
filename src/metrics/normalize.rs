//! Capability-based extraction of curated vLLM metrics.
//!
//! This is the only place that knows concrete `vllm:*` metric name strings.
//! Each curated quantity has a canonical id plus an alias list, so a rename
//! upstream (e.g. `vllm:gpu_cache_usage_perc` → `vllm:kv_cache_usage_perc`)
//! is a one-line change here. Anything not curated stays visible in the Raw
//! Metrics view; anything missing surfaces as `None` (rendered `--`), never 0.
//!
//! Series are keyed by (model_name, engine) so multi-model and data-parallel
//! endpoints keep their label dimensions instead of being collapsed.

use std::collections::BTreeMap;

use super::model::{MetricFamily, MetricType, ScrapeText};

/// Identity of one exported series: model plus optional engine index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SeriesKey {
    pub model: String,
    /// `engine` label if present (data-parallel/multiprocess servers).
    pub engine: Option<String>,
}

impl SeriesKey {
    pub fn display(&self) -> String {
        match &self.engine {
            Some(e) => format!("{} [eng {}]", self.model, e),
            None => self.model.clone(),
        }
    }
}

/// Cumulative histogram data for one series, straight from a scrape.
#[derive(Debug, Clone, PartialEq)]
pub struct RawHistogram {
    /// `(le, cumulative_count)`, unsorted here; sorted/validated downstream.
    pub buckets: Vec<(f64, f64)>,
    pub sum: f64,
    pub count: f64,
}

/// Canonical ids for curated histograms (UI/state key on these).
pub mod hist {
    pub const TTFT: &str = "ttft";
    pub const INTER_TOKEN_LATENCY: &str = "itl";
    pub const E2E_LATENCY: &str = "e2e";
    pub const QUEUE_TIME: &str = "queue";
    pub const PREFILL_TIME: &str = "prefill";
    pub const DECODE_TIME: &str = "decode";
    pub const INFERENCE_TIME: &str = "inference";
    pub const PROMPT_TOKENS_PER_REQ: &str = "req_prompt_tokens";
    pub const GENERATION_TOKENS_PER_REQ: &str = "req_generation_tokens";
}

/// `(canonical id, exposition names newest-first)` for curated histograms.
const HISTOGRAM_SPECS: &[(&str, &[&str])] = &[
    (hist::TTFT, &["vllm:time_to_first_token_seconds"]),
    (
        hist::INTER_TOKEN_LATENCY,
        &[
            "vllm:inter_token_latency_seconds",
            // Pre-rename spelling used by older vLLM releases.
            "vllm:time_per_output_token_seconds",
        ],
    ),
    (hist::E2E_LATENCY, &["vllm:e2e_request_latency_seconds"]),
    (hist::QUEUE_TIME, &["vllm:request_queue_time_seconds"]),
    (hist::PREFILL_TIME, &["vllm:request_prefill_time_seconds"]),
    (hist::DECODE_TIME, &["vllm:request_decode_time_seconds"]),
    (
        hist::INFERENCE_TIME,
        &["vllm:request_inference_time_seconds"],
    ),
    (hist::PROMPT_TOKENS_PER_REQ, &["vllm:request_prompt_tokens"]),
    (
        hist::GENERATION_TOKENS_PER_REQ,
        &["vllm:request_generation_tokens"],
    ),
];

/// Curated values for one (model, engine) series. Cumulative counters stay
/// cumulative here; rate computation happens in the state layer where
/// monotonic scrape times live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CuratedSeries {
    // Gauges.
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub waiting_by_reason: BTreeMap<String, f64>,
    /// 0.0..=1.0 fraction.
    pub kv_cache_usage: Option<f64>,
    /// KV capacity in tokens, from `cache_config_info` labels when exposed.
    pub kv_cache_size_tokens: Option<f64>,
    // Counters (cumulative).
    pub prompt_tokens: Option<f64>,
    pub generation_tokens: Option<f64>,
    pub preemptions: Option<f64>,
    /// finished_reason → cumulative count (`stop`, `length`, `abort`, `error`, ...).
    pub success_by_reason: BTreeMap<String, f64>,
    pub prefix_cache_queries: Option<f64>,
    pub prefix_cache_hits: Option<f64>,
    pub external_prefix_cache_queries: Option<f64>,
    pub external_prefix_cache_hits: Option<f64>,
    // Histograms (cumulative snapshots), keyed by canonical id from [`hist`].
    pub histograms: BTreeMap<&'static str, RawHistogram>,
}

impl CuratedSeries {
    /// Total successful completions across finish reasons.
    pub fn success_total(&self) -> Option<f64> {
        if self.success_by_reason.is_empty() {
            None
        } else {
            Some(self.success_by_reason.values().sum())
        }
    }

    /// Cumulative count of error + abort finishes.
    pub fn error_abort_total(&self) -> Option<f64> {
        if self.success_by_reason.is_empty() {
            return None;
        }
        Some(
            self.success_by_reason
                .iter()
                .filter(|(k, _)| k.as_str() == "error" || k.as_str() == "abort")
                .map(|(_, v)| v)
                .sum(),
        )
    }

    /// Lifetime prefix-cache hit rate (hits/queries), if both are exposed.
    pub fn prefix_cache_hit_rate(&self) -> Option<f64> {
        match (self.prefix_cache_hits, self.prefix_cache_queries) {
            (Some(h), Some(q)) if q > 0.0 => Some(h / q),
            _ => None,
        }
    }
}

/// The curated view of one whole scrape.
#[derive(Debug, Clone, Default)]
pub struct CuratedScrape {
    pub series: BTreeMap<SeriesKey, CuratedSeries>,
    /// Canonical ids of curated metrics that were actually found — the
    /// endpoint's detected capabilities.
    pub capabilities: Vec<&'static str>,
}

impl CuratedScrape {
    pub fn has(&self, capability: &'static str) -> bool {
        self.capabilities.contains(&capability)
    }
}

fn series_key(labels: &super::model::LabelSet) -> Option<SeriesKey> {
    let model = labels.get("model_name")?.to_string();
    Some(SeriesKey {
        model,
        engine: labels.get("engine").map(str::to_string),
    })
}

/// Extract curated metrics from a parsed scrape.
pub fn curate(scrape: &ScrapeText) -> CuratedScrape {
    let mut out = CuratedScrape::default();
    // `cache_config_info` carries `engine` but NO `model_name` (verified on
    // live vLLM 0.24), so capacity is collected per engine here and joined
    // onto the model series afterwards.
    let mut kv_capacity_by_engine: BTreeMap<Option<String>, f64> = BTreeMap::new();

    for family in &scrape.families {
        match family.name.as_str() {
            "vllm:num_requests_running" => {
                gauge_into(family, &mut out, "running", |s, v| s.running = Some(v));
            }
            "vllm:num_requests_waiting" => {
                gauge_into(family, &mut out, "waiting", |s, v| s.waiting = Some(v));
            }
            "vllm:num_requests_waiting_by_reason" => {
                labelled_gauge_into(
                    family,
                    &mut out,
                    "waiting_by_reason",
                    "reason",
                    |s, k, v| {
                        s.waiting_by_reason.insert(k, v);
                    },
                );
            }
            // Current name first; `gpu_cache_usage_perc` is the pre-V1 name.
            "vllm:kv_cache_usage_perc" | "vllm:gpu_cache_usage_perc" => {
                gauge_into(family, &mut out, "kv_cache_usage", |s, v| {
                    s.kv_cache_usage = Some(v)
                });
            }
            "vllm:prompt_tokens_total" => {
                gauge_into(family, &mut out, "prompt_tokens", |s, v| {
                    s.prompt_tokens = Some(v)
                });
            }
            "vllm:generation_tokens_total" => {
                gauge_into(family, &mut out, "generation_tokens", |s, v| {
                    s.generation_tokens = Some(v)
                });
            }
            "vllm:num_preemptions_total" => {
                gauge_into(family, &mut out, "preemptions", |s, v| {
                    s.preemptions = Some(v)
                });
            }
            "vllm:request_success_total" => {
                labelled_gauge_into(
                    family,
                    &mut out,
                    "request_success",
                    "finished_reason",
                    |s, k, v| {
                        s.success_by_reason.insert(k, v);
                    },
                );
            }
            // gpu_prefix_cache_* are the pre-v0.9.2 names.
            "vllm:prefix_cache_queries_total" | "vllm:gpu_prefix_cache_queries_total" => {
                gauge_into(family, &mut out, "prefix_cache", |s, v| {
                    s.prefix_cache_queries = Some(v)
                });
            }
            "vllm:prefix_cache_hits_total" | "vllm:gpu_prefix_cache_hits_total" => {
                gauge_into(family, &mut out, "prefix_cache", |s, v| {
                    s.prefix_cache_hits = Some(v)
                });
            }
            "vllm:external_prefix_cache_queries_total" => {
                gauge_into(family, &mut out, "external_prefix_cache", |s, v| {
                    s.external_prefix_cache_queries = Some(v)
                });
            }
            "vllm:external_prefix_cache_hits_total" => {
                gauge_into(family, &mut out, "external_prefix_cache", |s, v| {
                    s.external_prefix_cache_hits = Some(v)
                });
            }
            "vllm:cache_config_info" => {
                // Info gauge: interesting data lives in the labels, whose
                // exact set varies by vLLM version — read defensively.
                for sample in &family.samples {
                    let get_num = |name: &str| -> Option<f64> {
                        sample.labels.get(name).and_then(|v| v.parse::<f64>().ok())
                    };
                    // Newer vLLM exposes capacity directly; older versions
                    // expose block geometry instead.
                    let capacity = get_num("kv_cache_size_tokens").or_else(|| {
                        match (get_num("num_gpu_blocks"), get_num("block_size")) {
                            (Some(blocks), Some(size)) if blocks > 0.0 && size > 0.0 => {
                                Some(blocks * size)
                            }
                            _ => None,
                        }
                    });
                    if let Some(tokens) = capacity {
                        let engine = sample.labels.get("engine").map(str::to_string);
                        kv_capacity_by_engine.insert(engine, tokens);
                    }
                }
            }
            _ => {
                curate_histogram(family, &mut out);
            }
        }
    }

    // Join per-engine KV capacity onto the model series — but ONLY when
    // exactly one series claims a capacity entry. Multiple models on one
    // engine share that engine's cache; attributing the full capacity to
    // each would double-count it in capacity-weighted fleet aggregation, so
    // those series keep capacity unknown (the UI then falls back to the
    // labelled unweighted mean instead of a silently wrong weighted one).
    if !kv_capacity_by_engine.is_empty() {
        let single_entry_key = (kv_capacity_by_engine.len() == 1)
            .then(|| kv_capacity_by_engine.keys().next().cloned())
            .flatten();
        // Which capacity entry would each series use? Exact engine match
        // first; the single-entry fallback bridges old servers where one
        // side lacks the engine label — but never across two DIFFERENT
        // explicit engines (an engine-1 series must not claim engine-0's
        // capacity).
        let assignment: Vec<(SeriesKey, Option<String>)> = out
            .series
            .keys()
            .filter_map(|k| {
                if kv_capacity_by_engine.contains_key(&k.engine) {
                    return Some((k.clone(), k.engine.clone()));
                }
                match &single_entry_key {
                    Some(entry) if entry.is_none() || k.engine.is_none() => {
                        Some((k.clone(), entry.clone()))
                    }
                    _ => None,
                }
            })
            .collect();
        let mut claims: BTreeMap<Option<String>, usize> = BTreeMap::new();
        for (_, entry) in &assignment {
            *claims.entry(entry.clone()).or_insert(0) += 1;
        }
        let mut any = false;
        for (key, entry) in assignment {
            if claims.get(&entry) == Some(&1)
                && let Some(tokens) = kv_capacity_by_engine.get(&entry)
                && let Some(series) = out.series.get_mut(&key)
            {
                series.kv_cache_size_tokens = Some(*tokens);
                any = true;
            }
        }
        if any {
            push_capability(&mut out.capabilities, "kv_capacity");
        }
    }
    out
}

fn push_capability(caps: &mut Vec<&'static str>, cap: &'static str) {
    if !caps.contains(&cap) {
        caps.push(cap);
    }
}

/// Route a single-valued (per series) family into a field setter.
fn gauge_into(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    set: impl Fn(&mut CuratedSeries, f64),
) {
    let mut any = false;
    for sample in &family.samples {
        let Some(key) = series_key(&sample.labels) else {
            continue;
        };
        set(out.series.entry(key).or_default(), sample.value);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// Route a family whose samples fan out over one extra label (reason,
/// finished_reason) into a map inserter.
fn labelled_gauge_into(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    label: &str,
    insert: impl Fn(&mut CuratedSeries, String, f64),
) {
    let mut any = false;
    for sample in &family.samples {
        let Some(key) = series_key(&sample.labels) else {
            continue;
        };
        let Some(reason) = sample.labels.get(label) else {
            continue;
        };
        insert(
            out.series.entry(key).or_default(),
            reason.to_string(),
            sample.value,
        );
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// Match a family against the curated histogram table and collect buckets.
fn curate_histogram(family: &MetricFamily, out: &mut CuratedScrape) {
    if family.kind != MetricType::Histogram {
        return;
    }
    let Some((canonical, _)) = HISTOGRAM_SPECS
        .iter()
        .find(|(_, names)| names.contains(&family.name.as_str()))
    else {
        return;
    };

    // Group this family's samples by series key.
    let mut partial: BTreeMap<SeriesKey, RawHistogram> = BTreeMap::new();
    let bucket_name = format!("{}_bucket", family.name);
    let sum_name = format!("{}_sum", family.name);
    let count_name = format!("{}_count", family.name);

    for sample in &family.samples {
        let Some(key) = series_key(&sample.labels) else {
            continue;
        };
        let entry = partial.entry(key).or_insert_with(|| RawHistogram {
            buckets: Vec::new(),
            sum: 0.0,
            count: 0.0,
        });
        if sample.name == bucket_name {
            if let Some(le) = sample.labels.get("le").and_then(parse_le) {
                entry.buckets.push((le, sample.value));
            }
        } else if sample.name == sum_name {
            entry.sum = sample.value;
        } else if sample.name == count_name {
            entry.count = sample.value;
        }
    }

    let mut any = false;
    for (key, histogram) in partial {
        if histogram.buckets.is_empty() {
            continue;
        }
        out.series
            .entry(key)
            .or_default()
            .histograms
            .insert(canonical, histogram);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, canonical);
    }
}

fn parse_le(raw: &str) -> Option<f64> {
    match raw {
        "+Inf" | "+inf" | "inf" | "Inf" => Some(f64::INFINITY),
        _ => raw.parse::<f64>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::parse::parse_text;

    const SNIPPET: &str = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="m1"} 4.0
vllm:num_requests_running{engine="1",model_name="m1"} 2.0
# TYPE vllm:num_requests_waiting gauge
vllm:num_requests_waiting{engine="0",model_name="m1"} 1.0
# TYPE vllm:num_requests_waiting_by_reason gauge
vllm:num_requests_waiting_by_reason{engine="0",model_name="m1",reason="capacity"} 1.0
vllm:num_requests_waiting_by_reason{engine="0",model_name="m1",reason="deferred"} 0.0
# TYPE vllm:kv_cache_usage_perc gauge
vllm:kv_cache_usage_perc{engine="0",model_name="m1"} 0.25
# TYPE vllm:prompt_tokens_total counter
vllm:prompt_tokens_total{engine="0",model_name="m1"} 1000.0
# TYPE vllm:request_success_total counter
vllm:request_success_total{engine="0",finished_reason="stop",model_name="m1"} 90.0
vllm:request_success_total{engine="0",finished_reason="abort",model_name="m1"} 3.0
vllm:request_success_total{engine="0",finished_reason="error",model_name="m1"} 2.0
# TYPE vllm:time_to_first_token_seconds histogram
vllm:time_to_first_token_seconds_bucket{engine="0",le="0.5",model_name="m1"} 10.0
vllm:time_to_first_token_seconds_bucket{engine="0",le="+Inf",model_name="m1"} 12.0
vllm:time_to_first_token_seconds_count{engine="0",model_name="m1"} 12.0
vllm:time_to_first_token_seconds_sum{engine="0",model_name="m1"} 6.5
# TYPE vllm:cache_config_info gauge
vllm:cache_config_info{engine="0",model_name="m1",kv_cache_size_tokens="342803",block_size="784"} 1.0
"#;

    #[test]
    fn extracts_per_engine_series() {
        let curated = curate(&parse_text(SNIPPET));
        assert_eq!(curated.series.len(), 2);
        let k0 = SeriesKey {
            model: "m1".into(),
            engine: Some("0".into()),
        };
        let k1 = SeriesKey {
            model: "m1".into(),
            engine: Some("1".into()),
        };
        assert_eq!(curated.series[&k0].running, Some(4.0));
        assert_eq!(curated.series[&k1].running, Some(2.0));
        // Engine 1 exposes nothing else: everything stays None, not zero.
        assert_eq!(curated.series[&k1].waiting, None);
        assert_eq!(curated.series[&k1].kv_cache_usage, None);
    }

    #[test]
    fn waiting_reasons_and_finish_reasons() {
        let curated = curate(&parse_text(SNIPPET));
        let k0 = SeriesKey {
            model: "m1".into(),
            engine: Some("0".into()),
        };
        let s = &curated.series[&k0];
        assert_eq!(s.waiting_by_reason["capacity"], 1.0);
        assert_eq!(s.success_total(), Some(95.0));
        assert_eq!(s.error_abort_total(), Some(5.0));
    }

    #[test]
    fn histogram_and_capacity_extraction() {
        let curated = curate(&parse_text(SNIPPET));
        let k0 = SeriesKey {
            model: "m1".into(),
            engine: Some("0".into()),
        };
        let s = &curated.series[&k0];
        let h = &s.histograms[hist::TTFT];
        assert_eq!(h.count, 12.0);
        assert_eq!(h.sum, 6.5);
        assert_eq!(h.buckets.len(), 2);
        assert_eq!(s.kv_cache_size_tokens, Some(342803.0));
    }

    #[test]
    fn capability_detection_reports_what_was_found() {
        let curated = curate(&parse_text(SNIPPET));
        assert!(curated.has("running"));
        assert!(curated.has(hist::TTFT));
        assert!(curated.has("kv_capacity"));
        assert!(!curated.has(hist::E2E_LATENCY));
        assert!(!curated.has("preemptions"));
    }

    #[test]
    fn legacy_alias_gpu_cache_usage_perc() {
        let text = r#"
# TYPE vllm:gpu_cache_usage_perc gauge
vllm:gpu_cache_usage_perc{model_name="old"} 0.5
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "old".into(),
            engine: None,
        };
        assert_eq!(curated.series[&key].kv_cache_usage, Some(0.5));
    }

    #[test]
    fn legacy_alias_gpu_prefix_cache_counters() {
        let text = r#"
# TYPE vllm:gpu_prefix_cache_queries_total counter
vllm:gpu_prefix_cache_queries_total{model_name="old"} 100.0
# TYPE vllm:gpu_prefix_cache_hits_total counter
vllm:gpu_prefix_cache_hits_total{model_name="old"} 40.0
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "old".into(),
            engine: None,
        };
        assert_eq!(curated.series[&key].prefix_cache_hit_rate(), Some(0.4));
    }

    #[test]
    fn kv_capacity_falls_back_to_block_geometry() {
        // cache_config_info has no model_name label (matches live vLLM), so
        // capacity joins onto series created by other metrics.
        let text = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{model_name="old"} 0.0
# TYPE vllm:cache_config_info gauge
vllm:cache_config_info{num_gpu_blocks="442",block_size="16"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "old".into(),
            engine: None,
        };
        assert_eq!(curated.series[&key].kv_cache_size_tokens, Some(7072.0));
    }

    #[test]
    fn kv_capacity_joins_per_engine_and_single_engine_fallback() {
        // Two engines with different capacities: exact engine match.
        let text = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="m"} 0.0
vllm:num_requests_running{engine="1",model_name="m"} 0.0
# TYPE vllm:cache_config_info gauge
vllm:cache_config_info{engine="0",kv_cache_size_tokens="1000"} 1.0
vllm:cache_config_info{engine="1",kv_cache_size_tokens="2000"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let k = |e: &str| SeriesKey {
            model: "m".into(),
            engine: Some(e.into()),
        };
        assert_eq!(curated.series[&k("0")].kv_cache_size_tokens, Some(1000.0));
        assert_eq!(curated.series[&k("1")].kv_cache_size_tokens, Some(2000.0));

        // Engine label mismatch (old server without engine on info metric):
        // a single capacity entry applies to all series.
        let text = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="m"} 0.0
# TYPE vllm:cache_config_info gauge
vllm:cache_config_info{kv_cache_size_tokens="5000"} 1.0
"#;
        let curated = curate(&parse_text(text));
        assert_eq!(curated.series[&k("0")].kv_cache_size_tokens, Some(5000.0));
    }

    #[test]
    fn shared_engine_capacity_is_not_double_attributed() {
        // Two MODELS on one engine share that engine's cache: assigning the
        // full capacity to both would double-count it in fleet weighting.
        let text = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{engine="0",model_name="model-a"} 0.0
vllm:num_requests_running{engine="0",model_name="model-b"} 0.0
# TYPE vllm:cache_config_info gauge
vllm:cache_config_info{engine="0",kv_cache_size_tokens="1000"} 1.0
"#;
        let curated = curate(&parse_text(text));
        for (key, series) in &curated.series {
            assert_eq!(
                series.kv_cache_size_tokens, None,
                "series {key:?} must not claim shared capacity"
            );
        }
        assert!(!curated.has("kv_capacity"));
    }

    #[test]
    fn legacy_alias_time_per_output_token() {
        let text = r#"
# TYPE vllm:time_per_output_token_seconds histogram
vllm:time_per_output_token_seconds_bucket{model_name="old",le="+Inf"} 5.0
vllm:time_per_output_token_seconds_count{model_name="old"} 5.0
vllm:time_per_output_token_seconds_sum{model_name="old"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "old".into(),
            engine: None,
        };
        assert!(
            curated.series[&key]
                .histograms
                .contains_key(hist::INTER_TOKEN_LATENCY)
        );
    }

    #[test]
    fn missing_metrics_mean_empty_not_zero() {
        let curated = curate(&parse_text("up 1\n"));
        assert!(curated.series.is_empty());
        assert!(curated.capabilities.is_empty());
    }

    #[test]
    fn samples_without_model_label_are_ignored_for_curation() {
        let text = "# TYPE vllm:num_requests_running gauge\nvllm:num_requests_running 3.0\n";
        let curated = curate(&parse_text(text));
        assert!(curated.series.is_empty());
    }
}
