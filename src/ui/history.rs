//! H: rolling history charts, filterable by endpoint and model/engine series.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::SeriesKey;
use crate::state::{EndpointState, series_id};
use crate::ui::format;

/// The charted metrics, in display order, with how to format their values.
const CHARTS: &[(&str, &str, ValueKind)] = &[
    (series_id::RUNNING, "running requests", ValueKind::Count),
    (series_id::WAITING, "waiting requests", ValueKind::Count),
    (series_id::KV_USAGE, "KV-cache usage", ValueKind::Fraction),
    (series_id::PROMPT_TPS, "prompt tokens/s", ValueKind::Count),
    (
        series_id::GENERATION_TPS,
        "generation tokens/s",
        ValueKind::Count,
    ),
    (series_id::REQUEST_RATE, "completions/s", ValueKind::Count),
    (series_id::TTFT_P95, "TTFT p95", ValueKind::Seconds),
    (series_id::ITL_P95, "inter-token p95", ValueKind::Seconds),
    (series_id::E2E_P95, "e2e latency p95", ValueKind::Seconds),
    (series_id::ERRORS, "errors+aborts (new)", ValueKind::Count),
    (
        series_id::PREEMPTIONS,
        "preemptions (new)",
        ValueKind::Count,
    ),
];

#[derive(Clone, Copy, PartialEq)]
enum ValueKind {
    Count,
    Fraction,
    Seconds,
}

impl ValueKind {
    fn fmt(self, v: Option<f64>) -> String {
        match self {
            ValueKind::Count => format::count(v),
            ValueKind::Fraction => format::percent(v),
            ValueKind::Seconds => format::seconds(v),
        }
    }
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let [filter_area, grid] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    draw_filter_line(frame, app, filter_area);

    // 2-column grid of charts; each chart needs ~8 rows to be readable.
    let chart_h = 8u16;
    let cols = if grid.width >= 100 { 2 } else { 1 };
    let visible_rows = (grid.height / chart_h).max(1) as usize;
    let total_rows = CHARTS.len().div_ceil(cols);
    let scroll = app.hist.scroll.min(total_rows.saturating_sub(visible_rows));

    for vis_row in 0..visible_rows {
        let row = vis_row + scroll;
        if row >= total_rows {
            break;
        }
        for col in 0..cols {
            let idx = row * cols + col;
            let Some(&(id, title, kind)) = CHARTS.get(idx) else {
                continue;
            };
            let w = grid.width / cols as u16;
            let cell = Rect {
                x: grid.x + col as u16 * w,
                y: grid.y + vis_row as u16 * chart_h,
                width: w,
                height: chart_h.min(grid.height - vis_row as u16 * chart_h),
            };
            draw_metric_chart(frame, app, cell, title, id, kind);
        }
    }
}

fn draw_filter_line(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let endpoint = match app.hist.endpoint_filter {
        None => "ALL".to_string(),
        Some(i) => app
            .endpoints
            .get(i)
            .map(|e| e.name.clone())
            .unwrap_or_default(),
    };
    let series = app.observed_series();
    let model = match app.hist.model_filter.and_then(|i| series.get(i)) {
        None => "ALL".to_string(),
        Some(k) => k.display(),
    };
    let line = Line::from(vec![
        Span::styled(" HISTORY ", t.heading),
        Span::styled("endpoint(e): ", t.dim),
        Span::styled(endpoint, t.value),
        Span::styled("   series(m): ", t.dim),
        Span::styled(model, t.value),
        Span::styled(
            format!(
                "   window {}   scroll j/k",
                format::brief_duration(app.config.history_window)
            ),
            t.dim,
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Which series key filter applies (History view only).
fn active_series_filter(app: &App) -> Option<SeriesKey> {
    let series = app.observed_series();
    app.hist.model_filter.and_then(|i| series.get(i).cloned())
}

/// `(seconds relative to now, value)` chart points for one plotted line.
type LinePoints = Vec<(f64, f64)>;

fn draw_metric_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    id: &'static str,
    kind: ValueKind,
) {
    let now = Instant::now();
    let window = app.config.history_window.as_secs_f64();
    let series_filter = active_series_filter(app);

    // One dataset per (endpoint, model/engine series): merging different
    // series into one line would draw a meaningless sawtooth.
    let mut plotted: Vec<(usize, LinePoints, Option<f64>)> = Vec::new();
    for (i, e) in app.endpoints.iter().enumerate() {
        if let Some(filter) = app.hist.endpoint_filter
            && filter != i
        {
            continue;
        }
        for ((key, sid), ring) in &e.history {
            if *sid != id {
                continue;
            }
            if let Some(f) = &series_filter
                && key != f
            {
                continue;
            }
            let points: LinePoints = ring
                .iter()
                .filter_map(|p| {
                    let x = -(now.saturating_duration_since(p.at).as_secs_f64());
                    (x >= -window).then_some((x, p.value))
                })
                .collect();
            if !points.is_empty() {
                plotted.push((i, points, ring.latest().map(|p| p.value)));
            }
        }
    }

    let t = &app.theme;
    // Headline value must aggregate honestly per value kind: counts sum;
    // fractions and latencies never do (a sum of percentages or p95s is
    // meaningless). Label anything that isn't a plain single value.
    let latest_values: Vec<f64> = plotted.iter().filter_map(|(_, _, l)| *l).collect();
    let headline: Vec<Span> = if latest_values.is_empty() {
        vec![Span::styled(crate::ui::format::NA, t.na)]
    } else if latest_values.len() == 1 {
        vec![Span::styled(kind.fmt(Some(latest_values[0])), t.value)]
    } else {
        match kind {
            ValueKind::Count => vec![
                Span::styled(kind.fmt(Some(latest_values.iter().sum::<f64>())), t.value),
                Span::styled(" total", t.dim),
            ],
            ValueKind::Fraction => vec![
                Span::styled(
                    kind.fmt(Some(
                        latest_values.iter().sum::<f64>() / latest_values.len() as f64,
                    )),
                    t.value,
                ),
                Span::styled(" unweighted mean", t.dim),
            ],
            ValueKind::Seconds => vec![
                Span::styled(
                    kind.fmt(Some(latest_values.iter().copied().fold(f64::MIN, f64::max))),
                    t.value,
                ),
                Span::styled(" worst", t.dim),
            ],
        }
    };

    let mut title_spans = vec![Span::styled(format!(" {title} "), t.heading)];
    title_spans.extend(headline);
    title_spans.push(Span::raw(" "));
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));

    if plotted.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  no data", t.na))).block(block),
            area,
        );
        return;
    }

    let mut y_max = plotted
        .iter()
        .flat_map(|(_, pts, _)| pts.iter().map(|p| p.1))
        .fold(f64::NEG_INFINITY, f64::max);
    if !y_max.is_finite() || y_max <= 0.0 {
        y_max = 1.0;
    }
    let y_max = if kind == ValueKind::Fraction {
        1.0f64.max(y_max)
    } else {
        y_max * 1.1
    };

    let datasets: Vec<Dataset> = plotted
        .iter()
        .map(|(i, pts, _)| {
            Dataset::default()
                .name(app.endpoints[*i].name.clone())
                .data(pts)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.series_color(*i))
        })
        .collect();

    let x_axis = Axis::default()
        .bounds([-window, 0.0])
        .labels(vec![
            Span::styled(
                format!("-{}", format::brief_duration(app.config.history_window)),
                t.dim,
            ),
            Span::styled("now", t.dim),
        ])
        .style(t.dim);
    let y_axis = Axis::default()
        .bounds([0.0, y_max])
        .labels(vec![
            Span::styled("0", t.dim),
            Span::styled(kind.fmt(Some(y_max)), t.dim),
        ])
        .style(t.dim);

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(x_axis)
        .y_axis(y_axis)
        .hidden_legend_constraints((Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)));
    frame.render_widget(chart, area);
}

/// A single standalone chart for one or two series ids of ONE endpoint
/// (used by the endpoint detail view's trend row).
pub fn draw_single_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    ids: &[&'static str],
    endpoint: Option<&EndpointState>,
) {
    let t = &app.theme;
    let now = Instant::now();
    let window = app.config.history_window.as_secs_f64().min(120.0);

    let mut lines_data: Vec<(String, LinePoints)> = Vec::new();
    if let Some(e) = endpoint {
        for (li, id) in ids.iter().enumerate() {
            let mut pts: LinePoints = Vec::new();
            for ((_key, sid), ring) in &e.history {
                if sid != id {
                    continue;
                }
                for p in ring.iter() {
                    let x = -(now.saturating_duration_since(p.at).as_secs_f64());
                    if x >= -window {
                        pts.push((x, p.value));
                    }
                }
            }
            pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if !pts.is_empty() {
                lines_data.push((format!("{id}#{li}"), pts));
            }
        }
    }

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Span::styled(format!(" {title} "), t.heading));

    if lines_data.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  no data", t.na))).block(block),
            area,
        );
        return;
    }

    let mut y_max = lines_data
        .iter()
        .flat_map(|(_, pts)| pts.iter().map(|p| p.1))
        .fold(f64::NEG_INFINITY, f64::max);
    if !y_max.is_finite() || y_max <= 0.0 {
        y_max = 1.0;
    }
    let y_max = y_max * 1.1;

    let datasets: Vec<Dataset> = lines_data
        .iter()
        .enumerate()
        .map(|(i, (_name, pts))| {
            Dataset::default()
                .data(pts)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.series_color(i))
        })
        .collect();

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([-window, 0.0])
                .labels(vec![
                    Span::styled(format!("-{:.0}s", window), t.dim),
                    Span::styled("now", t.dim),
                ])
                .style(t.dim),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(vec![
                    Span::styled("0", t.dim),
                    Span::styled(format::count(Some(y_max)), t.dim),
                ])
                .style(t.dim),
        );
    frame.render_widget(chart, area);
}
