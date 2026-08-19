//! R: searchable raw-metrics browser. Every family the endpoint exposed is
//! visible here, curated or not, with type, labels, value, and staleness.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::state::Freshness;
use crate::ui::format;

/// Hard cap on rendered rows: keeps pathological cardinality from freezing
/// the UI. The status line reports when truncation happens.
const MAX_ROWS: usize = 20_000;

struct RawRow<'a> {
    endpoint: &'a str,
    metric: &'a str,
    kind: &'static str,
    labels: String,
    value: f64,
    age: String,
    stale: bool,
}

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let [status_area, table_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    // Build the row set deterministically: endpoints in config order,
    // families in exposition order per endpoint would flap across scrapes,
    // so sort by (endpoint, family, sample name, labels).
    let needle = app.raw.filter.to_lowercase();
    let mut rows: Vec<RawRow> = Vec::new();
    let mut total_series = 0usize;
    let mut truncated = false;

    'endpoints: for (i, e) in app.endpoints.iter().enumerate() {
        if let Some(filter) = app.raw.endpoint_filter
            && filter != i
        {
            continue;
        }
        let Some(scrape) = &e.raw else { continue };
        let stale = e.freshness(now, app.refresh_interval) == Freshness::Stale;
        let age = format::ago(e.last_ok_at, now);
        let endpoint_matches = e.name.to_lowercase().contains(&needle);
        for family in &scrape.families {
            for sample in &family.samples {
                total_series += 1;
                // Cap check BEFORE any per-row allocation, so pathological
                // cardinality costs counting only. Keep walking (cheaply)
                // just to finish the total_series count.
                if rows.len() >= MAX_ROWS {
                    truncated = true;
                    continue;
                }
                if !needle.is_empty()
                    && !endpoint_matches
                    && !sample.name.to_lowercase().contains(&needle)
                    && !sample.labels.iter().any(|(k, v)| {
                        k.to_lowercase().contains(&needle) || v.to_lowercase().contains(&needle)
                    })
                {
                    continue;
                }
                rows.push(RawRow {
                    endpoint: &e.name,
                    metric: &sample.name,
                    kind: family.kind.as_str(),
                    labels: sample.labels.to_string(),
                    value: sample.value,
                    age: age.clone(),
                    stale,
                });
            }
        }
        // With a single-endpoint filter and the cap hit, nothing more to do.
        if truncated && app.raw.endpoint_filter.is_some() {
            break 'endpoints;
        }
    }
    rows.sort_by(|a, b| (a.endpoint, a.metric, &a.labels).cmp(&(b.endpoint, b.metric, &b.labels)));

    // Status / filter line.
    let filter_display = if app.raw.editing {
        format!("/{}_", app.raw.filter)
    } else if app.raw.filter.is_empty() {
        "(press / to filter)".into()
    } else {
        format!("/{}", app.raw.filter)
    };
    let endpoint_display = match app.raw.endpoint_filter {
        None => "ALL".to_string(),
        Some(i) => app
            .endpoints
            .get(i)
            .map(|e| e.name.clone())
            .unwrap_or_default(),
    };
    let mut status = vec![
        Span::styled(" RAW ", t.heading),
        Span::styled(
            format!("{} shown / {} series", rows.len(), total_series),
            t.text,
        ),
        Span::styled("   endpoint(e): ", t.dim),
        Span::styled(endpoint_display, t.value),
        Span::styled("   filter: ", t.dim),
        Span::styled(
            filter_display,
            if app.raw.editing { t.warn } else { t.value },
        ),
    ];
    if truncated {
        status.push(Span::styled(
            format!("   display capped at {MAX_ROWS} rows"),
            t.warn,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(status)), status_area);

    // Visible slice under scroll.
    let visible = table_area.height.saturating_sub(1) as usize; // minus header
    let max_scroll = rows.len().saturating_sub(visible);
    let scroll = app.raw.scroll.min(max_scroll);

    let header = Row::new(
        ["ENDPOINT", "METRIC", "TYPE", "LABELS", "VALUE", "AGE"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, t.heading))),
    );

    let label_width = (table_area.width as usize)
        .saturating_sub(14 + 40 + 9 + 12 + 10 + 6)
        .max(16);

    let body: Vec<Row> = rows[scroll..(scroll + visible.min(rows.len() - scroll))]
        .iter()
        .map(|r| {
            let age_style = if r.stale { t.crit } else { t.dim };
            let age_text = if r.stale {
                format!("{} STALE", r.age)
            } else {
                r.age.clone()
            };
            Row::new(vec![
                Cell::from(Span::styled(format::truncate(r.endpoint, 13), t.text)),
                Cell::from(Span::styled(format::truncate(r.metric, 40), t.value)),
                Cell::from(Span::styled(r.kind, t.secondary)),
                Cell::from(Span::styled(
                    format::truncate(&r.labels, label_width),
                    t.dim,
                )),
                Cell::from(Span::styled(format::raw_value(r.value), t.text)),
                Cell::from(Span::styled(age_text, age_style)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(14),
        Constraint::Length(40),
        Constraint::Length(9),
        Constraint::Min(16),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    frame.render_widget(
        Table::new(body, widths).header(header).column_spacing(1),
        table_area,
    );
}
