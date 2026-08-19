//! Tab 1: fleet overview — all endpoints in one dense table plus totals.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::state::{Freshness, KvAggregate, aggregate_kv};
use crate::ui::{format, freshness_badge};

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let [summary_area, table_area] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(0)]).areas(area);
    draw_summary(frame, app, summary_area);
    draw_table(frame, app, table_area);
}

fn draw_summary(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();

    let mut healthy = 0usize;
    let mut running: Option<f64> = None;
    let mut waiting: Option<f64> = None;
    let mut prompt: Option<f64> = None;
    let mut generation: Option<f64> = None;
    let mut completions: Option<f64> = None;
    // (usage, capacity) per series across the whole fleet for KV aggregation.
    let mut kv_pairs: Vec<(f64, Option<f64>)> = Vec::new();

    for e in &app.endpoints {
        let fresh = e.freshness(now, app.refresh_interval) == Freshness::Fresh;
        if fresh && e.healthy != Some(false) {
            healthy += 1;
        }
        // Totals only include fresh data: a stale snapshot must not inflate
        // fleet activity silently.
        if !fresh {
            continue;
        }
        let agg = e.aggregate();
        sum(&mut running, agg.running);
        sum(&mut waiting, agg.waiting);
        sum(&mut prompt, agg.prompt_tps);
        sum(&mut generation, agg.generation_tps);
        sum(&mut completions, agg.request_rate);
        if let Some(curated) = &e.curated {
            for series in curated.series.values() {
                if let Some(usage) = series.kv_cache_usage {
                    kv_pairs.push((usage, series.kv_cache_size_tokens));
                }
            }
        }
    }

    let kv_span: Vec<Span> = match aggregate_kv(&kv_pairs) {
        Some(KvAggregate::CapacityWeighted(v)) => vec![
            Span::styled(format::percent(Some(v)), t.by_level(v, 0.75, 0.9)),
            Span::styled(" capacity-weighted", t.dim),
        ],
        Some(KvAggregate::UnweightedMean(v)) => vec![
            Span::styled(format::percent(Some(v)), t.by_level(v, 0.75, 0.9)),
            Span::styled(" unweighted mean (capacity unknown)", t.warn),
        ],
        None => vec![Span::styled(format::NA, t.na)],
    };

    let n = app.endpoints.len();
    let health_style = if healthy == n {
        t.value
    } else if healthy == 0 {
        t.crit
    } else {
        t.warn
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(" FLEET ", t.heading),
            Span::styled(format!("{healthy}/{n} healthy"), health_style),
            Span::styled("   running ", t.dim),
            Span::styled(format::count(running), t.value),
            Span::styled("   waiting ", t.dim),
            styled_waiting(app, waiting),
            Span::styled("   completions ", t.dim),
            Span::styled(format::rate(completions), t.value),
        ]),
        Line::from(vec![
            Span::styled("        prompt ", t.dim),
            Span::styled(format!("{} t/s", format::count(prompt)), t.value),
            Span::styled("   generation ", t.dim),
            Span::styled(format!("{} t/s", format::count(generation)), t.value),
        ]),
        Line::from([vec![Span::styled("        fleet KV ", t.dim)], kv_span].concat()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::BOTTOM)
                .border_style(app.theme.dim),
        ),
        area,
    );
}

fn styled_waiting(app: &App, waiting: Option<f64>) -> Span<'static> {
    let t = &app.theme;
    match waiting {
        Some(w) if w > 0.0 => Span::styled(format::count(waiting), t.warn),
        _ => Span::styled(format::count(waiting), t.value),
    }
}

fn sum(acc: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0.0) + v);
    }
}

fn draw_table(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let wide = area.width >= 110;

    let header = Row::new(
        [
            "NAME",
            "ST",
            "MODEL",
            "RUN",
            "WAIT",
            "KV",
            "PROMPT/s",
            "GEN/s",
            "REQ/s",
            // Worst p95 across the endpoint's series — never a merged
            // estimate (bucket merging across series is not assumed).
            "WORST TTFT",
            "ERR",
            "PRE",
            "AGE",
            if wide { "GEN t/s TREND" } else { "" },
        ]
        .into_iter()
        .map(|h| Cell::from(Span::styled(h, t.heading))),
    );

    let mut rows: Vec<Row> = Vec::new();
    for (row_idx, &i) in app.sorted_endpoint_indices().iter().enumerate() {
        let e = &app.endpoints[i];
        let agg = e.aggregate();
        let freshness = e.freshness(now, app.refresh_interval);
        let (badge, badge_style) = freshness_badge(app, freshness, e.healthy);

        let model = if agg.models.is_empty() {
            format::NA.to_string()
        } else {
            format::truncate(&agg.models.join(","), 24)
        };

        let kv_cell = match agg.kv_usage {
            Some(kv) => {
                let v = kv.value();
                let marker = if kv.is_weighted() { "" } else { "~" };
                Span::styled(
                    format!(
                        "{}{} {}",
                        format::bar(v, 8, ascii),
                        marker,
                        format::percent(Some(v))
                    ),
                    t.by_level(v, 0.75, 0.9),
                )
            }
            None => Span::styled(format::NA.to_string(), t.na),
        };

        let err_style = if agg.error_abort_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.dim
        };
        let pre_style = if agg.preemption_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.dim
        };
        let wait_style = if agg.waiting.unwrap_or(0.0) > 0.0 {
            t.warn
        } else {
            t.value
        };

        // Endpoint-level generation-throughput sparkline: sum across series
        // is not directly stored, so show the first series' trend.
        let trend: String = if wide {
            e.history
                .iter()
                .find(|((_, id), _)| *id == crate::state::series_id::GENERATION_TPS)
                .map(|(_, s)| format::spark(&s.tail_values(20), 20, ascii))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let age = match freshness {
            Freshness::Never => "never".to_string(),
            _ => format::ago(e.last_ok_at, now),
        };
        let age_style = if freshness == Freshness::Stale {
            t.crit
        } else {
            t.dim
        };

        let mut row = Row::new(vec![
            Cell::from(Span::styled(format::truncate(&e.name, 14), t.text)),
            Cell::from(Span::styled(badge, badge_style)),
            Cell::from(Span::styled(model, t.secondary)),
            Cell::from(Span::styled(format::count(agg.running), t.value)),
            Cell::from(Span::styled(format::count(agg.waiting), wait_style)),
            Cell::from(kv_cell),
            Cell::from(Span::styled(format::count(agg.prompt_tps), t.value)),
            Cell::from(Span::styled(format::count(agg.generation_tps), t.value)),
            Cell::from(Span::styled(format::count(agg.request_rate), t.value)),
            Cell::from(Span::styled(
                format::seconds(agg.worst_ttft_p95),
                t.secondary,
            )),
            Cell::from(Span::styled(
                format::count(agg.error_abort_delta),
                err_style,
            )),
            Cell::from(Span::styled(format::count(agg.preemption_delta), pre_style)),
            Cell::from(Span::styled(age, age_style)),
            Cell::from(Span::styled(trend, t.secondary)),
        ]);
        if row_idx == app.fleet_selected {
            row = row.style(t.selected);
        }
        rows.push(row);
    }

    let widths = [
        Constraint::Length(14), // NAME
        Constraint::Length(6),  // ST
        Constraint::Length(24), // MODEL
        Constraint::Length(5),  // RUN
        Constraint::Length(5),  // WAIT
        Constraint::Length(16), // KV
        Constraint::Length(9),  // PROMPT/s
        Constraint::Length(8),  // GEN/s
        Constraint::Length(6),  // REQ/s
        Constraint::Length(10), // WORST TTFT
        Constraint::Length(4),  // ERR
        Constraint::Length(4),  // PRE
        Constraint::Length(9),  // AGE
        Constraint::Min(0),     // TREND
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}
