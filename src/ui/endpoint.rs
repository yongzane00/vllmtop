//! Tabs 2…N: one endpoint in depth.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::{CuratedSeries, SeriesKey, hist};
use crate::state::{DerivedSeries, EndpointState};
use crate::ui::{format, freshness_badge};

pub fn draw(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let Some(e) = app.endpoints.get(index) else {
        return;
    };
    let [head, pulse, body] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(area);
    draw_head(frame, app, e, head);
    draw_pulse(frame, app, index, e, pulse);

    // Charts get the bottom 40% when there is room for them.
    if body.height >= 14 {
        let [tables, charts] =
            Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(body);
        draw_tables(frame, app, e, tables);
        draw_trends(frame, app, e, charts);
    } else {
        draw_tables(frame, app, e, body);
    }
}

/// The "is it alive and how fast" strip: an animated GENERATING indicator,
/// the two token rates, running (as an `n/max` bar when the config declares
/// the server's `--max-num-seqs`), waiting, and a full-width KV-cache bar
/// that visibly grows while a conversation is being generated.
fn draw_pulse(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let agg = e.aggregate();
    let generating = agg.running.unwrap_or(0.0) > 0.0;

    // Status with a spinner while requests are running. The UI redraws at
    // least every 500 ms tick, so the animation runs at ~2 fps — enough to
    // read as "alive". (Display-only wall-clock use.)
    let mut line1: Vec<Span> = Vec::new();
    if generating {
        let frames: &[&str] = if ascii {
            &["|", "/", "-", "\\"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        };
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let glyph = frames[(ms / 250) as usize % frames.len()];
        line1.push(Span::styled(format!(" {glyph} GENERATING"), t.value));
    } else {
        line1.push(Span::styled(" · IDLE      ", t.dim));
    }
    line1.push(Span::styled("   generation ", t.dim));
    line1.push(Span::styled(
        format!("{} tokens/s", format::count(agg.generation_tps)),
        t.value,
    ));
    line1.push(Span::styled("   prompt ", t.dim));
    line1.push(Span::styled(
        format!("{} tokens/s", format::count(agg.prompt_tps)),
        t.value,
    ));
    line1.push(Span::styled("   running ", t.dim));
    let max_running = app
        .config
        .endpoints
        .get(index)
        .and_then(|c| c.max_running)
        .filter(|m| *m > 0);
    match (agg.running, max_running) {
        (Some(run), Some(max)) => {
            let frac = run / f64::from(max);
            line1.push(Span::styled(
                format!(
                    "{}/{} {}",
                    format::count(Some(run)),
                    max,
                    format::bar(frac, 8, ascii)
                ),
                t.by_level(frac, 0.75, 0.95),
            ));
        }
        (run, _) => line1.push(Span::styled(format::count(run), t.value)),
    }
    line1.push(Span::styled("   waiting ", t.dim));
    let wait_style = if agg.waiting.unwrap_or(0.0) > 0.0 {
        t.warn
    } else {
        t.value
    };
    line1.push(Span::styled(format::count(agg.waiting), wait_style));

    // Full-width KV bar with absolute tokens when the server exposes its
    // capacity. This is the aggregate cache: with one conversation running
    // it is effectively that conversation's KV growing token by token.
    let mut line2: Vec<Span> = vec![Span::styled(" KV ", t.dim)];
    match agg.kv_usage {
        Some(kv) => {
            let frac = kv.value();
            // Reserve room for the trailing figures; the bar takes the rest.
            let bar_width = (area.width as usize).saturating_sub(42).clamp(10, 80);
            line2.push(Span::styled(
                format::bar(frac, bar_width, ascii),
                t.by_level(frac, 0.75, 0.9),
            ));
            line2.push(Span::styled(
                format!(" {}", format::percent(Some(frac))),
                t.by_level(frac, 0.75, 0.9),
            ));
            if !kv.is_weighted() {
                line2.push(Span::styled(" (unweighted)", t.warn));
            }
            // Absolute used/capacity, when every series reports capacity.
            if let Some(curated) = &e.curated {
                let caps: Vec<(f64, f64)> = curated
                    .series
                    .values()
                    .filter_map(|s| Some((s.kv_cache_usage?, s.kv_cache_size_tokens?)))
                    .collect();
                if !caps.is_empty() && caps.len() == curated.series.len() {
                    let total: f64 = caps.iter().map(|(_, c)| c).sum();
                    let used: f64 = caps.iter().map(|(u, c)| u * c).sum();
                    line2.push(Span::styled(
                        format!(
                            "  {} / {} tok",
                            format::count(Some(used)),
                            format::count(Some(total))
                        ),
                        t.secondary,
                    ));
                }
            }
        }
        None => line2.push(Span::styled(format::NA, t.na)),
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)])
            .block(Block::new().borders(Borders::BOTTOM).border_style(t.dim)),
        area,
    );
}

fn draw_head(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let freshness = e.freshness(now, app.refresh_interval);
    let (badge, badge_style) = freshness_badge(app, freshness, e.healthy);

    let mut line1 = vec![
        Span::styled(format!(" {} ", e.name), t.heading),
        Span::styled(badge, badge_style),
        Span::styled(format!("  {}", e.display_url), t.dim),
        Span::styled("   vLLM ", t.dim),
        Span::styled(
            e.vllm_version.clone().unwrap_or_else(|| format::NA.into()),
            t.secondary,
        ),
    ];
    if e.restart_seen_at
        .is_some_and(|at| now.saturating_duration_since(at).as_secs() < 30)
    {
        line1.push(Span::styled("  RESTARTED", t.crit));
    }

    // Second line: only things worth flagging plus the served models.
    let mut line2: Vec<Span> = Vec::new();
    if e.parse_issue_count > 0 {
        line2.push(Span::styled(
            format!("   parse-issues {}", e.parse_issue_count),
            t.warn,
        ));
    }
    if !e.served_models.is_empty() {
        line2.push(Span::styled("   models ", t.dim));
        line2.push(Span::styled(
            format::truncate(&e.served_models.join(", "), 40),
            t.secondary,
        ));
    }
    if let crate::state::ConnStatus::Failing { error, consecutive } = &e.status {
        line2.push(Span::styled(
            format!("  ✗{consecutive} {}", format::truncate(error, 60)),
            t.crit,
        ));
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)])
            .block(Block::new().borders(Borders::BOTTOM).border_style(t.dim)),
        area,
    );
}

/// Not currently displayed (the header shows only the failure count), but
/// kept with its test for easy reinstatement.
#[allow(dead_code)]
fn attempt_status(endpoint: &EndpointState, now: Instant) -> String {
    if let Some(started) = endpoint.scraping_since {
        return format!(
            "scraping {}",
            format::brief_duration(now.saturating_duration_since(started))
        );
    }
    match endpoint.last_attempt_at {
        Some(started) => format!(
            "attempt {} ago",
            format::brief_duration(now.saturating_duration_since(started))
        ),
        None => "attempt never".into(),
    }
}

fn draw_tables(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(area);
    draw_activity(frame, app, e, left);
    draw_latency(frame, app, e, right);
}

/// Left column: activity and cache metrics, one section per series.
fn draw_activity(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let mut lines: Vec<Line> = Vec::new();

    let Some(curated) = &e.curated else {
        lines.push(Line::from(Span::styled(" no data yet", t.na)));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    };

    for (key, series) in &curated.series {
        let d = e.derived.get(key);
        push_series_activity(
            &mut lines,
            app,
            key,
            series,
            d,
            curated.series.len() > 1,
            ascii,
        );
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn push_series_activity(
    lines: &mut Vec<Line>,
    app: &App,
    key: &SeriesKey,
    s: &CuratedSeries,
    d: Option<&DerivedSeries>,
    multi: bool,
    ascii: bool,
) {
    let t = &app.theme;
    if multi {
        lines.push(Line::from(Span::styled(
            format!(" ▸ {}", key.display()),
            t.heading,
        )));
    } else {
        lines.push(Line::from(Span::styled(" ACTIVITY", t.heading)));
    }

    let kv = |label: &str, value: String, style| {
        Line::from(vec![
            // Width fits the longest label, "generation tokens/s" (19).
            Span::styled(format!("   {label:<20}"), t.dim),
            Span::styled(value, style),
        ])
    };

    // With one series, running/waiting/KV/token rates already live in the
    // pulse strip above — repeating them here is noise. Multi-series
    // (multi-model / data-parallel) endpoints keep the full per-series rows
    // because the pulse strip only shows aggregates.
    if multi {
        let wait_style = if s.waiting.unwrap_or(0.0) > 0.0 {
            t.warn
        } else {
            t.value
        };
        lines.push(kv("running", format::count(s.running), t.value));
        let mut waiting_text = format::count(s.waiting);
        if !s.waiting_by_reason.is_empty() {
            let reasons: Vec<String> = s
                .waiting_by_reason
                .iter()
                .filter(|(_, v)| **v > 0.0)
                .map(|(k, v)| format!("{k}:{}", format::count(Some(*v))))
                .collect();
            if !reasons.is_empty() {
                waiting_text = format!("{waiting_text} ({})", reasons.join(" "));
            }
        }
        lines.push(kv("waiting", waiting_text, wait_style));

        match s.kv_cache_usage {
            Some(u) => {
                let style = t.by_level(u, 0.75, 0.9);
                let mut text =
                    format!("{} {}", format::bar(u, 20, ascii), format::percent(Some(u)));
                if let Some(cap) = s.kv_cache_size_tokens {
                    text.push_str(&format!("  of {} tok", format::count(Some(cap))));
                }
                lines.push(kv("KV cache", text, style));
            }
            None => lines.push(kv("KV cache", format::NA.into(), t.na)),
        }

        lines.push(kv(
            "prompt tokens/s",
            format::count(d.and_then(|d| d.prompt_tps)),
            t.value,
        ));
        lines.push(kv(
            "generation tokens/s",
            format::count(d.and_then(|d| d.generation_tps)),
            t.value,
        ));
    } else if !s.waiting_by_reason.is_empty() {
        // Waiting reasons have no home in the pulse strip; show them when
        // any are non-zero.
        let reasons: Vec<String> = s
            .waiting_by_reason
            .iter()
            .filter(|(_, v)| **v > 0.0)
            .map(|(k, v)| format!("{k}:{}", format::count(Some(*v))))
            .collect();
        if !reasons.is_empty() {
            lines.push(kv("waiting reasons", reasons.join(" "), t.warn));
        }
    }
    lines.push(kv(
        "completions/s",
        format::count(d.and_then(|d| d.request_rate)),
        t.value,
    ));

    // Lifetime finish-reason breakdown.
    if !s.success_by_reason.is_empty() {
        let text: Vec<String> = s
            .success_by_reason
            .iter()
            .map(|(k, v)| format!("{k}:{}", format::count(Some(*v))))
            .collect();
        lines.push(kv("finished (life)", text.join(" "), t.secondary));
    }

    let err_delta = d.and_then(|d| d.error_abort_delta);
    lines.push(kv(
        "errors+aborts",
        format!(
            "{} new  {} life",
            format::count(err_delta),
            format::count(s.error_abort_total())
        ),
        if err_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.text
        },
    ));

    let pre_delta = d.and_then(|d| d.preemption_delta);
    lines.push(kv(
        "preemptions",
        format!(
            "{} new  {} life",
            format::count(pre_delta),
            format::count(s.preemptions)
        ),
        if pre_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.text
        },
    ));

    let window_rate = d.and_then(|d| d.prefix_hit_rate_window);
    lines.push(kv(
        "prefix cache hit",
        format!(
            "{} now  {} life",
            format::percent(window_rate),
            format::percent(s.prefix_cache_hit_rate())
        ),
        t.secondary,
    ));
    if s.external_prefix_cache_queries.unwrap_or(0.0) > 0.0 {
        let rate = match (
            s.external_prefix_cache_hits,
            s.external_prefix_cache_queries,
        ) {
            (Some(h), Some(q)) if q > 0.0 => Some(h / q),
            _ => None,
        };
        lines.push(kv("ext prefix hit", format::percent(rate), t.secondary));
    }
    lines.push(Line::default());
}

/// Right column: latency percentile table per series.
fn draw_latency(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let Some(curated) = &e.curated else {
        return;
    };

    // "inference time" (≈ prefill + decode) is deliberately omitted as
    // redundant with the phase split below it.
    const ROWS: [(&str, &str); 6] = [
        (hist::TTFT, "TTFT"),
        (hist::INTER_TOKEN_LATENCY, "inter-token"),
        (hist::E2E_LATENCY, "e2e latency"),
        (hist::QUEUE_TIME, "queue time"),
        (hist::PREFILL_TIME, "prefill time"),
        (hist::DECODE_TIME, "decode time"),
    ];

    let mut rows: Vec<Row> = Vec::new();
    let window = app.config.percentile_window.as_secs();
    for key in curated.series.keys() {
        let Some(d) = e.derived.get(key) else {
            continue;
        };
        if curated.series.len() > 1 {
            rows.push(Row::new(vec![Cell::from(Span::styled(
                format!("▸ {}", key.display()),
                t.heading,
            ))]));
        }
        for (id, label) in ROWS {
            let est = d.estimates.get(id);
            let cell = |v: Option<f64>| {
                Cell::from(Span::styled(
                    format::seconds(v),
                    if v.is_some() { t.value } else { t.na },
                ))
            };
            let obs = est.map(|e| e.observations).unwrap_or(0.0);
            rows.push(Row::new(vec![
                Cell::from(Span::styled(format!("  {label}"), t.dim)),
                cell(est.and_then(|e| e.p50)),
                cell(est.and_then(|e| e.p95)),
                cell(est.and_then(|e| e.p99)),
                cell(est.and_then(|e| e.mean)),
                Cell::from(Span::styled(
                    if est.is_some() {
                        format::count(Some(obs))
                    } else {
                        format::NA.into()
                    },
                    t.dim,
                )),
            ]));
        }
        // Tokens-per-request distributions, same estimator.
        for (id, label) in [
            (hist::PROMPT_TOKENS_PER_REQ, "prompt tok/req"),
            (hist::GENERATION_TOKENS_PER_REQ, "gen tok/req"),
        ] {
            let est = d.estimates.get(id);
            let cell = |v: Option<f64>| {
                Cell::from(Span::styled(
                    format::count(v),
                    if v.is_some() { t.secondary } else { t.na },
                ))
            };
            rows.push(Row::new(vec![
                Cell::from(Span::styled(format!("  {label}"), t.dim)),
                cell(est.and_then(|e| e.p50)),
                cell(est.and_then(|e| e.p95)),
                cell(est.and_then(|e| e.p99)),
                cell(est.and_then(|e| e.mean)),
                Cell::from(Span::raw("")),
            ]));
        }
    }

    let header = Row::new(
        [
            format!("LATENCY (~{window}s est)"),
            "p50".into(),
            "p95".into(),
            "p99".into(),
            "mean".into(),
            "obs".into(),
        ]
        .into_iter()
        .map(|h: String| Cell::from(Span::styled(h, t.heading))),
    );
    let widths = [
        Constraint::Length(22),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
    ];
    frame.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        area,
    );
}

/// Bottom charts: recent trends for generation throughput and queue depth.
fn draw_trends(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(area);
    super::charts::draw_single_chart(
        frame,
        app,
        left,
        "generation tokens/s",
        &[crate::state::series_id::GENERATION_TPS],
        Some(e),
    );
    super::charts::draw_single_chart(
        frame,
        app,
        right,
        "running / waiting",
        &[
            crate::state::series_id::RUNNING,
            crate::state::series_id::WAITING,
        ],
        Some(e),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn attempt_status_distinguishes_idle_and_scraping() {
        let now = Instant::now();
        let mut endpoint = EndpointState::new(
            "ep".into(),
            "http://example.test".into(),
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert_eq!(attempt_status(&endpoint, now), "attempt never");
        endpoint.mark_attempt_started(now - Duration::from_millis(300));
        assert!(attempt_status(&endpoint, now).starts_with("scraping "));
        endpoint.scraping_since = None;
        assert_eq!(attempt_status(&endpoint, now), "attempt 0.3s ago");
    }
}
