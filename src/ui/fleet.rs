//! Tab 1: fleet overview — the endpoint table, the past-30-days usage bar
//! charts, and the rolling history charts (PgUp/PgDn scrolls the grid).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::state::Freshness;
use crate::storage::usage::{DayUsage, USAGE_WINDOW_DAYS};
use crate::ui::{cards, format, freshness_badge};

/// A chart row needs this many terminal rows to be readable; below that the
/// charts section is dropped and the table gets everything.
const MIN_CHART_ROWS: u16 = 8;
/// The daily-usage bar section needs at least this many rows to read.
const MIN_USAGE_ROWS: u16 = 7;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    // The card tier is strictly ADDITIVE: it appears only when today's full
    // stack (table + usage bars + chart grid) still fits with a card row to
    // spare, so no size that renders something today loses anything.
    let table_needed = (app.endpoints.len() as u16).saturating_add(1);
    let table_h = table_needed.min((area.height / 2).max(1));
    let avail = area.height.saturating_sub(table_h);
    let usage0 = (avail / 3).clamp(MIN_USAGE_ROWS, 12);
    let cards_h =
        if cards::columns(area.width) > 0 && avail >= usage0 + MIN_CHART_ROWS + cards::CARD_H {
            cards::CARD_H
        } else {
            0
        };

    let [cards_area, body] =
        Layout::vertical([Constraint::Length(cards_h), Constraint::Min(0)]).areas(area);
    if cards_h > 0 {
        let now = Instant::now();
        let fleet = crate::state::aggregate_fleet(
            app.endpoints
                .iter()
                .map(|e| (e, e.freshness(now, app.refresh_interval))),
        );
        cards::draw_card_row(frame, &app.theme, cards_area, &fleet_cards(app, &fleet));
    }
    draw_body(frame, app, body);
}

/// Fleet-wide cards. Counts sum, KV is capacity-weighted where possible and
/// says so, and the worst latency names the endpoint that owns it.
fn fleet_cards(app: &App, fleet: &crate::state::FleetAggregate) -> Vec<cards::Card> {
    let t = &app.theme;

    let health_style = if fleet.endpoints_down > 0 {
        t.crit
    } else if fleet.endpoints_stale > 0 {
        t.warn
    } else {
        t.value
    };
    let mut health_notes: Vec<String> = Vec::new();
    if fleet.endpoints_stale > 0 {
        health_notes.push(format!("{} stale", fleet.endpoints_stale));
    }
    if fleet.endpoints_down > 0 {
        health_notes.push(format!("{} down", fleet.endpoints_down));
    }
    let endpoints = cards::Card::text(
        t,
        "ENDPOINTS",
        Some(format!("{}/{}", fleet.endpoints_up, fleet.endpoints_total)),
    )
    .style(health_style)
    .sub(
        (!health_notes.is_empty()).then(|| health_notes.join(" · ")),
        if fleet.endpoints_down > 0 {
            t.crit
        } else {
            t.warn
        },
    );

    let waiting = fleet.waiting.unwrap_or(0.0);
    let running = cards::Card::count(t, "RUNNING", fleet.running, None).sub(
        fleet
            .waiting
            .map(|w| format!("{} queued", format::count(Some(w)))),
        if waiting > 0.0 { t.warn } else { t.dim },
    );

    let generation = cards::Card::count(t, "GENERATION", fleet.generation_tps, Some("tok/s")).sub(
        (fleet.endpoints_up > 0).then(|| format!("over {} up", fleet.endpoints_up)),
        t.dim,
    );

    // The fleet table marks an unweighted KV figure with `~`; here there is
    // room to say it in words.
    let kv = cards::Card::percent(t, "KV CACHE", fleet.kv_usage.map(|k| k.value()), 0.75, 0.9).sub(
        fleet.kv_usage.map(|k| {
            if k.is_weighted() {
                "capacity-weighted".to_string()
            } else {
                "unweighted mean".to_string()
            }
        }),
        if fleet.kv_usage.is_some_and(|k| k.is_weighted()) {
            t.dim
        } else {
            t.warn
        },
    );

    let ttft = cards::Card::seconds(t, "WORST TTFT", fleet.worst_ttft_p95).sub(
        fleet
            .worst_ttft_endpoint
            .as_ref()
            .map(|name| format!("on {name}")),
        t.dim,
    );

    let prompt = cards::Card::count(t, "PROMPT", fleet.prompt_tps, Some("tok/s"));

    vec![endpoints, running, generation, kv, ttft, prompt]
}

/// Today's ladder, unchanged: table, then the 30-day usage bars, then the
/// rolling chart grid, dropping the grid first and the bars second.
fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    // Table gets exactly what it needs (header + one row per endpoint, at
    // most half the space). Below it: daily usage bars, then the rolling
    // chart grid. Degrade order as the terminal shrinks: grid first, then
    // the usage bars, the table last.
    let table_needed = (app.endpoints.len() as u16).saturating_add(1);
    let table_h = table_needed.min((area.height / 2).max(1));
    let rest = area.height.saturating_sub(table_h);
    let usage_h = (rest / 3).clamp(MIN_USAGE_ROWS, 12);

    if rest >= usage_h + MIN_CHART_ROWS {
        let [table_area, usage_area, charts_area] = Layout::vertical([
            Constraint::Length(table_h),
            Constraint::Length(usage_h),
            Constraint::Min(0),
        ])
        .areas(area);
        draw_table(frame, app, table_area);
        draw_daily_usage(frame, app, usage_area);
        super::charts::draw_grid(frame, app, charts_area, app.fleet_chart_scroll);
    } else if rest >= MIN_USAGE_ROWS {
        let [table_area, usage_area] =
            Layout::vertical([Constraint::Length(table_h), Constraint::Min(0)]).areas(area);
        draw_table(frame, app, table_area);
        draw_daily_usage(frame, app, usage_area);
    } else {
        draw_table(frame, app, area);
    }
}

/// The past-30-days usage section: three bar charts (output tokens, input
/// tokens, requests per local day), fleet-wide, fed by the recorded history.
fn draw_daily_usage(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;

    // No data cases explain themselves instead of showing empty axes.
    let mut note = |text: String, style| {
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(t.dim)
            .title(Span::styled(" daily usage (30d) ", t.heading));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))).block(block),
            area,
        );
    };
    if let Some(reason) = &app.usage.disabled {
        note(format!(" usage history off — {reason}"), t.na);
        return;
    }
    let Some(data) = &app.usage.data else {
        match &app.usage.last_error {
            Some(e) => note(format!(" usage query failed: {e}"), t.crit),
            None => note(" loading usage history…".into(), t.na),
        }
        return;
    };

    type Getter = fn(&DayUsage) -> Option<f64>;
    let specs: [(&str, Getter); 3] = [
        ("output tokens/day", |d| d.generation_tokens),
        ("input tokens/day", |d| d.prompt_tokens),
        ("requests/day", |d| d.requests),
    ];

    // Three side-by-side charts when wide, stacked when narrow.
    if area.width >= 100 {
        let [a, b, c] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(area);
        for ((title, get), cell) in specs.into_iter().zip([a, b, c]) {
            draw_usage_chart(frame, app, cell, title, get, &data.days);
        }
    } else {
        let h = (area.height / 3).max(3);
        let [a, b, c] = Layout::vertical([
            Constraint::Length(h),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .areas(area);
        for ((title, get), cell) in specs.into_iter().zip([a, b, c]) {
            draw_usage_chart(frame, app, cell, title, get, &data.days);
        }
    }
}

/// Hand-rendered daily-usage tape. Every slot of the window is visibly
/// accounted for — a bar (observed), a low mark (measured zero), or a faint
/// baseline dot (unobserved) — because ratatui's `BarChart` draws nothing at
/// all for zero-height bars, which made absent days look broken instead of
/// quiet. Date ticks sit at a fixed 5-day interval, anchored on today, using
/// the same relative vocabulary as the rolling charts' x-axis.
fn draw_usage_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    get: fn(&DayUsage) -> Option<f64>,
    days: &[DayUsage],
) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_w == 0 || inner_h < 2 {
        return;
    }
    // Last inner row is the date-tick axis; the rest is bar area.
    let chart_rows = inner_h - 1;

    // Bars size dynamically with the terminal: wide terminals get wider
    // bars with gaps; narrow ones pack width-1 bars and, when even those
    // cannot fit, show only the most recent days (labelled in the title).
    let per = (inner_w / USAGE_WINDOW_DAYS).max(1);
    let (bar_w, gap) = if per >= 3 { (per - 1, 1) } else { (per, 0) };
    let step = (bar_w + gap).max(1);
    let fit = (inner_w / step).clamp(1, USAGE_WINDOW_DAYS).min(days.len());
    if fit == 0 {
        // No recorded days yet. Everything below indexes from `fit - 1`, so
        // bail before that underflows.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" no usage recorded yet", t.na))).block(
                Block::new()
                    .borders(Borders::ALL)
                    .border_style(t.dim)
                    .title(Span::styled(format!(" {title} "), t.heading)),
            ),
            area,
        );
        return;
    }
    let shown = &days[days.len() - fit..];

    let total: Option<f64> = shown
        .iter()
        .filter_map(get)
        .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v));
    let peak = shown.iter().filter_map(get).fold(0.0_f64, f64::max);

    let mut title_spans = vec![Span::styled(format!(" {title} "), t.heading)];
    title_spans.push(Span::styled(format!("Σ {}", format::count(total)), t.value));
    if peak > 0.0 {
        // The y-scale: bars are normalized to the busiest shown day.
        title_spans.push(Span::styled(
            format!("  pk {}", format::count(Some(peak))),
            t.dim,
        ));
    }
    if fit < USAGE_WINDOW_DAYS {
        title_spans.push(Span::styled(format!("  last {fit}d"), t.dim));
    }
    title_spans.push(Span::raw(" "));

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Per-day column height in eighth-rows; None = unobserved.
    let heights: Vec<Option<usize>> = shown
        .iter()
        .map(|d| {
            get(d).map(|v| {
                if peak <= 0.0 || v <= 0.0 {
                    0
                } else {
                    // Anything observed and non-zero shows at least ▁.
                    (((v / peak) * (chart_rows * 8) as f64).round() as usize)
                        .clamp(1, chart_rows * 8)
                }
            })
        })
        .collect();

    const EIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let mut lines: Vec<Line> = Vec::with_capacity(chart_rows + 1);
    for row in 0..chart_rows {
        // Rows render top-down; `floor` is how many eighths sit below this row.
        let floor = (chart_rows - 1 - row) * 8;
        let mut spans: Vec<Span> = Vec::with_capacity(fit + 1);
        for (i, h) in heights.iter().enumerate() {
            let is_baseline = row == chart_rows - 1;
            let (cell, style) = match h {
                // Unobserved day: a quiet dot on the baseline, never a bar.
                None if is_baseline => (if ascii { "." } else { "·" }, t.na),
                // Measured zero: an explicit low mark, distinct from absent.
                Some(0) if is_baseline => (if ascii { "_" } else { "▁" }, t.dim),
                Some(h) if *h > floor => {
                    let filled = h - floor;
                    if filled >= 8 {
                        (if ascii { "#" } else { "█" }, t.value)
                    } else if ascii {
                        // No partial blocks in ASCII: round at half a row.
                        (if filled >= 4 { "#" } else { " " }, t.value)
                    } else {
                        (EIGHTHS[filled - 1], t.value)
                    }
                }
                _ => (" ", t.dim),
            };
            spans.push(Span::styled(cell.repeat(bar_w.max(1)), style));
            if gap > 0 && i + 1 < fit {
                spans.push(Span::raw(" ".repeat(gap)));
            }
        }
        lines.push(Line::from(spans));
    }

    // Date ticks: every 5 days, anchored on today (rightmost slot), in the
    // rolling charts' relative vocabulary: `-25d … -10d -5d today`.
    // "today" is placed first and always wins; older ticks yield to it.
    let today_text = "today";
    let today_end = ((fit - 1) * step + bar_w).min(inner_w);
    let today_start = today_end.saturating_sub(today_text.len());
    let mut axis: Vec<Span> = Vec::new();
    let mut cursor = 0usize;
    for i in 0..fit.saturating_sub(1) {
        let days_ago = fit - 1 - i;
        if !days_ago.is_multiple_of(5) {
            continue;
        }
        let text = format!("-{days_ago}d");
        let start = (i * step).min(inner_w.saturating_sub(text.len()));
        // Skip ticks that would collide with a neighbor or with "today".
        if start < cursor || start + text.len() + 1 > today_start {
            continue;
        }
        axis.push(Span::raw(" ".repeat(start - cursor)));
        cursor = start + text.len();
        axis.push(Span::styled(text, t.dim));
    }
    if today_text.len() <= inner_w && today_start >= cursor {
        axis.push(Span::raw(" ".repeat(today_start - cursor)));
        axis.push(Span::styled(today_text, t.secondary));
    }
    lines.push(Line::from(axis));

    frame.render_widget(Paragraph::new(lines), inner);
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
            if wide { "GEN tokens/s TREND" } else { "" },
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
