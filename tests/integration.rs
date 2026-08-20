//! End-to-end pipeline tests: mock HTTP server → collector → parser →
//! normalization → state, including failure isolation and recovery.
//!
//! No test here touches a live network service; everything runs against
//! local httpmock servers or fixtures.

use std::time::Duration;

use clap::Parser;
use httpmock::prelude::*;
use tokio::sync::mpsc;

use vllmtop::cli::Cli;
use vllmtop::collector::spawn_all;
use vllmtop::config::{Config, load};
use vllmtop::event::AppEvent;
use vllmtop::metrics::normalize::{SeriesKey, curate, hist};
use vllmtop::metrics::parse::parse_text;
use vllmtop::state::{ConnStatus, EndpointState, Freshness};

const FIXTURE: &str = include_str!("fixtures/vllm_0_24_single_engine.txt");
const LEGACY_FIXTURE: &str = include_str!("fixtures/vllm_legacy_v0_names.txt");

fn config_for(urls: &[String]) -> Config {
    let mut args = vec![
        "vllmtop".to_string(),
        "--refresh-interval-ms".into(),
        "1000".into(),
    ];
    for (i, u) in urls.iter().enumerate() {
        args.push("--endpoint".into());
        args.push(format!("ep{i}={u}"));
    }
    load(&Cli::parse_from(args), |_| None).expect("valid test config")
}

fn state_for(config: &Config, idx: usize) -> EndpointState {
    EndpointState::new(
        config.endpoints[idx].name.clone(),
        config.endpoints[idx].display_url(),
        config.history_window,
        config.percentile_window,
    )
}

async fn next_scrape_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
) -> vllmtop::state::ScrapeOutcome {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for scrape event")
            .expect("event channel closed unexpectedly");
        match event {
            AppEvent::Scrape {
                endpoint: idx,
                outcome,
            } if idx == endpoint => return outcome,
            _ => {}
        }
    }
}

async fn next_metrics_started_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
) -> std::time::Instant {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event channel closed unexpectedly");
            match event {
                AppEvent::MetricsStarted { endpoint: idx, at } if idx == endpoint => return at,
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for metrics-start event")
}

async fn apply_next_optional_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
    state: &mut EndpointState,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event channel closed unexpectedly");
            match event {
                AppEvent::OptionalUpdate {
                    endpoint: idx,
                    healthy,
                    version,
                    models,
                } if idx == endpoint => {
                    state.apply_optional(healthy, version, models);
                    return;
                }
                AppEvent::Scrape {
                    endpoint: idx,
                    outcome,
                } if idx == endpoint => state.apply(outcome),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for optional-probe event");
}

fn serve_fixture(server: &MockServer) {
    server.mock(|when, then| {
        when.method(GET).path("/metrics");
        then.status(200)
            .header("content-type", "text/plain; version=0.0.4")
            .body(FIXTURE);
    });
    server.mock(|when, then| {
        when.method(GET).path("/health");
        then.status(200);
    });
    server.mock(|when, then| {
        when.method(GET).path("/version");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"version":"0.24.0"}"#);
    });
    server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"object":"list","data":[{"id":"example-org/example-model-27B","object":"model"}]}"#);
    });
}

#[tokio::test]
async fn full_pipeline_against_mock_server() {
    let server = MockServer::start_async().await;
    serve_fixture(&server);

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    let outcome = next_scrape_for(&mut rx, 0).await;
    state.apply(outcome);
    apply_next_optional_for(&mut rx, 0, &mut state).await;

    // Connection + optional endpoints.
    assert_eq!(state.status, ConnStatus::Connected);
    assert_eq!(state.healthy, Some(true));
    assert_eq!(state.vllm_version.as_deref(), Some("0.24.0"));
    assert_eq!(state.served_models, vec!["example-org/example-model-27B"]);
    assert_eq!(
        state.freshness(std::time::Instant::now(), config.refresh_interval),
        Freshness::Fresh
    );

    // Curated values from the real vLLM 0.24 capture.
    let key = SeriesKey {
        model: "example-org/example-model-27B".into(),
        engine: Some("0".into()),
    };
    let curated = state.curated.as_ref().unwrap();
    let series = &curated.series[&key];
    assert_eq!(series.running, Some(0.0));
    assert_eq!(series.waiting, Some(0.0));
    assert_eq!(series.kv_cache_usage, Some(0.0));
    assert_eq!(series.prompt_tokens, Some(3_763_908.0));
    assert_eq!(series.generation_tokens, Some(54_939.0));
    assert_eq!(series.success_by_reason["stop"], 91.0);
    assert_eq!(series.kv_cache_size_tokens, Some(342_803.0));
    assert!(series.histograms.contains_key(hist::TTFT));
    assert!(series.histograms.contains_key(hist::DECODE_TIME));

    // Second scrape: rates become available (deltas are zero here).
    let outcome = next_scrape_for(&mut rx, 0).await;
    state.apply(outcome);
    let derived = &state.derived[&key];
    assert_eq!(derived.prompt_tps, Some(0.0));
    assert_eq!(derived.generation_tps, Some(0.0));
    assert_eq!(derived.request_rate, Some(0.0));
}

#[tokio::test]
async fn failing_endpoint_does_not_block_healthy_one() {
    let server = MockServer::start_async().await;
    serve_fixture(&server);
    // Port 9 (discard) on localhost: refused immediately on any sane host.
    let dead = "http://127.0.0.1:9".to_string();

    let config = config_for(&[server.base_url(), dead]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut healthy = state_for(&config, 0);
    let mut failing = state_for(&config, 1);

    healthy.apply(next_scrape_for(&mut rx, 0).await);
    failing.apply(next_scrape_for(&mut rx, 1).await);

    assert_eq!(healthy.status, ConnStatus::Connected);
    assert!(healthy.curated.is_some());
    match &failing.status {
        ConnStatus::Failing { error, .. } => {
            assert!(!error.is_empty());
            // Failure text must not leak any auth material (none set here,
            // but the invariant is that errors never carry header values).
            assert!(!error.to_lowercase().contains("authorization"));
        }
        s => panic!("expected failing endpoint, got {s:?}"),
    }
    // The healthy endpoint keeps scraping at its interval.
    healthy.apply(next_scrape_for(&mut rx, 0).await);
    assert_eq!(healthy.total_scrapes, 2);
    assert_eq!(healthy.total_failures, 0);
}

#[tokio::test]
async fn endpoint_recovers_after_coming_back() {
    let server = MockServer::start_async().await;
    let broken = server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(500);
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/health");
            then.status(200);
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    state.apply(next_scrape_for(&mut rx, 0).await);
    assert!(matches!(state.status, ConnStatus::Failing { .. }));
    assert!(state.curated.is_none());

    // Server comes back.
    broken.delete_async().await;
    serve_fixture(&server);

    // Collector backs off after a failure, so allow a few cycles.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        state.apply(next_scrape_for(&mut rx, 0).await);
        if state.status == ConnStatus::Connected || std::time::Instant::now() > deadline {
            break;
        }
    }
    assert_eq!(state.status, ConnStatus::Connected);
    assert!(state.curated.is_some());
    assert_eq!(
        state.freshness(std::time::Instant::now(), config.refresh_interval),
        Freshness::Fresh
    );
}

#[tokio::test]
async fn missing_optional_endpoints_leave_metadata_unknown() {
    let server = MockServer::start_async().await;
    // Only /metrics exists; /health /version /v1/models all 404.
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    state.apply(next_scrape_for(&mut rx, 0).await);

    // Monitoring continues; the missing endpoints degrade to unknown/N/A.
    assert_eq!(state.status, ConnStatus::Connected);
    assert!(state.curated.is_some());
    assert_eq!(state.vllm_version, None);
    assert!(state.served_models.is_empty());
    // A server without /health (404) is UNKNOWN, not unhealthy: the endpoint
    // is optional and its absence must show as N/A.
    assert_eq!(state.healthy, None);
}

#[tokio::test]
async fn slow_metrics_scrape_skips_missed_tick_and_stays_anchored() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200)
                .delay(Duration::from_millis(1_500))
                .body(FIXTURE);
        })
        .await;

    let args = [
        "vllmtop".to_string(),
        "--refresh-interval-ms".into(),
        "1000".into(),
        "--endpoint".into(),
        format!("ep0={}", server.base_url()),
    ];
    let config = load(&Cli::parse_from(args), |_| None).unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let first = next_metrics_started_for(&mut rx, 0).await;
    let second = next_metrics_started_for(&mut rx, 0).await;
    let gap = second.duration_since(first);
    assert!(gap >= Duration::from_millis(1_800), "gap was {gap:?}");
    assert!(gap <= Duration::from_millis(2_250), "gap was {gap:?}");
}

#[tokio::test]
async fn slow_optional_probe_cannot_delay_metrics_publication() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/health");
            then.status(200).delay(Duration::from_secs(2));
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);
    let _started = next_metrics_started_for(&mut rx, 0).await;
    let outcome = tokio::time::timeout(Duration::from_millis(500), next_scrape_for(&mut rx, 0))
        .await
        .expect("slow /health delayed /metrics publication");
    assert!(outcome.result.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn first_metrics_attempts_use_fixed_quarter_second_fleet_phases() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;
    let urls = vec![server.base_url(); 4];
    let config = config_for(&urls);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let starts = tokio::time::timeout(Duration::from_secs(2), async {
        let mut starts = [None; 4];
        while starts.iter().any(Option::is_none) {
            if let Some(AppEvent::MetricsStarted { endpoint, at }) = rx.recv().await
                && endpoint < starts.len()
                && starts[endpoint].is_none()
            {
                starts[endpoint] = Some(at);
            }
        }
        starts.map(Option::unwrap)
    })
    .await
    .expect("fleet did not start all first attempts");

    for (idx, expected_ms) in [0_u64, 250, 500, 750].into_iter().enumerate() {
        let actual = starts[idx].duration_since(starts[0]);
        let expected = Duration::from_millis(expected_ms);
        assert!(
            actual.abs_diff(expected) <= Duration::from_millis(200),
            "endpoint {idx}: expected {expected:?}, got {actual:?}"
        );
    }
}

#[test]
fn live_fixture_parses_cleanly_and_completely() {
    let scrape = parse_text(FIXTURE);
    assert!(
        scrape.issues.is_empty(),
        "real vLLM output must parse without issues: {:?}",
        scrape.issues
    );
    // The capture contains 678 lines: 502 samples plus HELP/TYPE metadata.
    assert_eq!(scrape.total_samples(), 502);
    assert!(scrape.families.len() > 80, "got {}", scrape.families.len());
    // The awkward histogram-named-_total family parses as ONE histogram.
    let iteration = scrape.family("vllm:iteration_tokens_total").unwrap();
    assert_eq!(
        iteration.kind,
        vllmtop::metrics::model::MetricType::Histogram
    );
    // Summary families with no quantiles survive.
    assert!(scrape.family("http_request_size_bytes").is_some());
}

#[test]
fn legacy_v0_names_map_to_same_capabilities() {
    let curated = curate(&parse_text(LEGACY_FIXTURE));
    let key = SeriesKey {
        model: "example-org/legacy-model".into(),
        engine: None,
    };
    let series = &curated.series[&key];
    assert_eq!(series.kv_cache_usage, Some(0.62));
    assert!(series.histograms.contains_key(hist::INTER_TOKEN_LATENCY));
    assert_eq!(series.running, Some(3.0));
}

#[test]
fn unknown_backend_specific_metrics_survive_to_raw_view() {
    let text = "\
# TYPE habana_specific_thing gauge
habana_specific_thing{device=\"hpu0\"} 42.0
vllm:future_metric_without_type 7.0
";
    let scrape = parse_text(text);
    assert_eq!(scrape.total_samples(), 2);
    assert!(scrape.family("habana_specific_thing").is_some());
    assert!(scrape.family("vllm:future_metric_without_type").is_some());
    // They contribute no curated series but are not lost.
    let curated = curate(&scrape);
    assert!(curated.series.is_empty());
}
