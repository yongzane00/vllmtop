//! The endpoint overview's lower band: key percentiles, server/model info,
//! and the observed-events feed.
//!
//! Honesty rules these panels exist to keep:
//! - the process metrics describe vLLM's **HTTP front-end** process, not the
//!   engine/GPU worker, and are labelled that way;
//! - `gpu_memory_utilization` is the **configured** target the operator asked
//!   for, never a measurement;
//! - KV capacity is in **tokens**; vLLM does not report KV bytes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::hist;
use crate::state::EndpointState;
use crate::ui::format;

fn panel<'a>(app: &App, title: &'a str) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(app.theme.dim)
        .title(Span::styled(format!(" {title} "), app.theme.heading))
}

/// The headline latency distributions, aggregated as "worst across series"
/// so a multi-model endpoint cannot hide its slowest model behind a mean.
pub fn draw_percentiles(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let window = app.config.percentile_window.as_secs();
    let title = format!("percentiles (~{window}s est)");
    let block = panel(app, &title);

    // Worst value across series for one (histogram, percentile) pair.
    let worst =
        |id: &'static str, pick: fn(&crate::metrics::histogram::WindowEstimate) -> Option<f64>| {
            e.derived
                .values()
                .filter_map(|d| d.estimates.get(id))
                .filter_map(pick)
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |cur: f64| cur.max(v)))
                })
        };

    let seconds_row = |label: &'static str, id: &'static str| {
        Row::new(vec![
            Cell::from(Span::styled(label, t.dim)),
            Cell::from(Span::styled(
                format::seconds(worst(id, |e| e.p50)),
                if worst(id, |e| e.p50).is_some() {
                    t.value
                } else {
                    t.na
                },
            )),
            Cell::from(Span::styled(
                format::seconds(worst(id, |e| e.p95)),
                if worst(id, |e| e.p95).is_some() {
                    t.value
                } else {
                    t.na
                },
            )),
            Cell::from(Span::styled(
                format::seconds(worst(id, |e| e.p99)),
                if worst(id, |e| e.p99).is_some() {
                    t.value
                } else {
                    t.na
                },
            )),
        ])
    };
    let count_row = |label: &'static str, id: &'static str| {
        Row::new(vec![
            Cell::from(Span::styled(label, t.dim)),
            Cell::from(Span::styled(
                format::count(worst(id, |e| e.p50)),
                t.secondary,
            )),
            Cell::from(Span::styled(
                format::count(worst(id, |e| e.p95)),
                t.secondary,
            )),
            Cell::from(Span::styled(
                format::count(worst(id, |e| e.p99)),
                t.secondary,
            )),
        ])
    };

    let rows = vec![
        seconds_row("TTFT", hist::TTFT),
        seconds_row("e2e", hist::E2E_LATENCY),
        seconds_row("inter-token", hist::INTER_TOKEN_LATENCY),
        seconds_row("queue", hist::QUEUE_TIME),
        count_row("prompt tok/req", hist::PROMPT_TOKENS_PER_REQ),
        count_row("gen tok/req", hist::GENERATION_TOKENS_PER_REQ),
    ];
    let header = Row::new(
        ["", "p50", "p95", "p99"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, t.heading))),
    );
    let widths = [
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .block(block),
        area,
    );
}

/// Server and model facts: what this endpoint is and how it was configured.
pub fn draw_info(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let block = panel(app, "server & model");
    let info = e.curated.as_ref().map(|c| &c.info);

    let mut lines: Vec<Line> = Vec::new();
    let mut row = |label: &str, value: String, style| {
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<15}"), t.dim),
            Span::styled(value, style),
        ]));
    };

    // Model identity.
    if let Some(model) = e.served_models.first() {
        row("model", format::truncate(&model.id, 28), t.value);
        if let Some(root) = &model.root {
            row("path", format::truncate(root, 28), t.secondary);
        }
        row(
            "context",
            model
                .max_model_len
                .map(|c| format::count(Some(c as f64)))
                .unwrap_or_else(|| format::NA.into()),
            t.secondary,
        );
    }
    if let Some(v) = &e.vllm_version {
        row("vLLM", v.clone(), t.secondary);
    }

    // KV geometry, in tokens — vLLM reports no KV byte figure.
    if let Some(info) = info {
        if let Some(cap) = info.cache_config_num("kv_cache_size_tokens") {
            row(
                "kv capacity",
                format!("{} tok", format::count(Some(cap))),
                t.value,
            );
        }
        if let Some(bs) = info.cache_config_num("block_size") {
            row("block size", format::count(Some(bs)), t.secondary);
        }
        if let Some(dtype) = info.cache_config.get("cache_dtype") {
            row("cache dtype", dtype.clone(), t.secondary);
        }
        if let Some(pc) = info.cache_config.get("enable_prefix_caching") {
            row("prefix caching", pc.clone(), t.secondary);
        }
        // The operator's requested fraction, NOT observed GPU memory.
        if let Some(util) = info.cache_config_num("gpu_memory_utilization") {
            row(
                "gpu mem target",
                format!("{} (configured)", format::percent(Some(util))),
                t.secondary,
            );
        }

        // The API-server process. Absent entirely when vLLM runs with
        // --api-server-count > 1 (multiprocess mode drops the collector).
        if let Some(rss) = info.front_end_rss_bytes {
            row("api proc mem", format::bytes(Some(rss)), t.secondary);
        }
        if let (Some(cpu), Some(up)) = (info.process_cpu_seconds, super::endpoint::uptime(e)) {
            let avg = if up.as_secs_f64() > 0.0 {
                format!(" ({})", format::percent(Some(cpu / up.as_secs_f64())))
            } else {
                String::new()
            };
            row(
                "api proc cpu",
                format!("{}{avg} avg", format::seconds(Some(cpu))),
                t.secondary,
            );
        }
        if let (Some(open), Some(max)) = (info.open_fds, info.max_fds) {
            let frac = if max > 0.0 { open / max } else { 0.0 };
            row(
                "open files",
                format!(
                    "{} / {}",
                    format::count(Some(open)),
                    format::count(Some(max))
                ),
                t.by_level(frac, 0.7, 0.9),
            );
        }
        // HTTP totals by status class (2xx/4xx/5xx; never exact codes).
        if !info.http_by_status.is_empty() {
            let text = info
                .http_by_status
                .iter()
                .map(|(class, n)| format!("{class} {}", format::count(Some(*n))))
                .collect::<Vec<_>>()
                .join("  ");
            let style = if info.http_by_status.keys().any(|k| k.starts_with('5')) {
                t.warn
            } else {
                t.secondary
            };
            row("http", text, style);
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" no metadata yet", t.na)));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// What vllmtop has observed about this endpoint — our own notes, not the
/// server's log.
pub fn draw_events(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let block = panel(app, "events");
    let now = Instant::now();

    if e.events.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" nothing noteworthy yet", t.na))).block(block),
            area,
        );
        return;
    }

    let visible = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = e
        .events
        .iter_newest_first()
        .take(visible.max(1))
        .map(|ev| {
            let age = format::brief_duration(now.saturating_duration_since(ev.at));
            Line::from(vec![
                Span::styled(format!(" {age:>6} "), t.dim),
                Span::styled(
                    format!("{:<11}", ev.kind.label()),
                    if ev.kind.is_bad() { t.crit } else { t.value },
                ),
                Span::styled(ev.detail.clone(), t.text),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
