//! The main application: owns all state, reduces events, routes input,
//! and drives rendering. Runs entirely on one task — collectors and the
//! recorder communicate exclusively through channels.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc;

use crate::collector::CollectorControl;
use crate::config::{Config, MAX_REFRESH_MS, MIN_REFRESH_MS};
use crate::event::AppEvent;
use crate::state::EndpointState;
use crate::storage::{Recorder, SampleRow, now_ms};
use crate::ui::theme::Theme;

/// Which screen is showing. Ordering for Tab cycling:
/// Fleet → Endpoint(0..n) → History → Raw → Fleet…
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Fleet,
    Endpoint(usize),
    History,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetSort {
    Name,
    Running,
    Waiting,
    GenTps,
}

impl FleetSort {
    pub fn next(self) -> Self {
        match self {
            FleetSort::Name => FleetSort::Running,
            FleetSort::Running => FleetSort::Waiting,
            FleetSort::Waiting => FleetSort::GenTps,
            FleetSort::GenTps => FleetSort::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FleetSort::Name => "name",
            FleetSort::Running => "running",
            FleetSort::Waiting => "waiting",
            FleetSort::GenTps => "gen t/s",
        }
    }
}

#[derive(Debug, Default)]
pub struct RawViewState {
    pub filter: String,
    pub editing: bool,
    pub scroll: usize,
    /// None = all endpoints.
    pub endpoint_filter: Option<usize>,
}

#[derive(Debug, Default)]
pub struct HistoryViewState {
    /// None = all endpoints overlaid.
    pub endpoint_filter: Option<usize>,
    /// Index into the union of observed series keys; None = all/aggregate.
    pub model_filter: Option<usize>,
    /// Scroll offset in chart-grid rows.
    pub scroll: usize,
}

pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub endpoints: Vec<EndpointState>,
    pub view: View,
    pub fleet_selected: usize,
    pub fleet_sort: FleetSort,
    pub raw: RawViewState,
    pub hist: HistoryViewState,
    pub paused: bool,
    pub show_help: bool,
    /// Runtime-adjustable; starts at config.refresh_interval.
    pub refresh_interval: Duration,
    pub recorder: Option<Recorder>,
    /// Recording failed to start (shown in the header; app keeps running).
    pub recorder_error: Option<String>,
    control: CollectorControl,
    should_quit: bool,
    dirty: bool,
}

impl App {
    pub fn new(config: Config, theme: Theme, control: CollectorControl) -> App {
        let endpoints = config
            .endpoints
            .iter()
            .map(|e| {
                EndpointState::new(
                    e.name.clone(),
                    e.display_url(),
                    config.history_window,
                    config.percentile_window,
                )
            })
            .collect();
        let (recorder, recorder_error) = match &config.record_path {
            Some(path) => match Recorder::start(path, config.retention_days) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(e)),
            },
            None => (None, None),
        };
        App {
            refresh_interval: config.refresh_interval,
            theme,
            endpoints,
            view: View::Fleet,
            fleet_selected: 0,
            fleet_sort: FleetSort::Name,
            raw: RawViewState::default(),
            hist: HistoryViewState::default(),
            paused: false,
            show_help: false,
            recorder,
            recorder_error,
            control,
            should_quit: false,
            dirty: true,
            config,
        }
    }

    /// Main loop: reduce events, redraw when dirty (throttled), exit on
    /// quit keys or SIGTERM. Never busy-loops: it always parks on the
    /// channel/tick select.
    pub async fn run(
        mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut events: mpsc::Receiver<AppEvent>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Backdated so the first frame paints immediately instead of waiting
        // for the first tick.
        let mut min_frame = Instant::now()
            .checked_sub(Duration::from_millis(200))
            .unwrap_or_else(Instant::now);

        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            if self.dirty && min_frame.elapsed() >= Duration::from_millis(90) {
                terminal.draw(|frame| crate::ui::draw(frame, &self))?;
                self.dirty = false;
                min_frame = Instant::now();
            }
            #[cfg(unix)]
            let term_signal = sigterm.recv();
            #[cfg(not(unix))]
            let term_signal = std::future::pending::<Option<()>>();

            tokio::select! {
                maybe = events.recv() => match maybe {
                    Some(event) => self.handle_event(event),
                    None => break, // all senders gone; nothing left to show
                },
                _ = tick.tick() => {
                    // Ages/staleness advance even without data; skip while
                    // paused to honor "no automatic refresh".
                    if !self.paused {
                        self.dirty = true;
                    }
                }
                _ = term_signal => break,
            }
            if self.should_quit {
                break;
            }
        }
        if let Some(recorder) = self.recorder.take() {
            recorder.shutdown();
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Scrape { endpoint, outcome } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                let succeeded = outcome.result.is_ok();
                state.apply(outcome);
                if succeeded {
                    self.record_samples(endpoint);
                }
                // State always updates; pausing only freezes the display.
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::Key(key) => {
                self.handle_key(key);
                self.dirty = true;
            }
            AppEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_by(-1),
                    MouseEventKind::ScrollDown => self.scroll_by(1),
                    _ => {}
                }
                self.dirty = true;
            }
            AppEvent::Resize => self.dirty = true,
            AppEvent::InputClosed => self.should_quit = true,
        }
    }

    fn record_samples(&mut self, endpoint: usize) {
        let Some(recorder) = &self.recorder else {
            return;
        };
        let state = &self.endpoints[endpoint];
        let ts_ms = now_ms();
        let rows: Vec<SampleRow> = state
            .current_samples()
            .into_iter()
            .map(|(key, metric, value)| SampleRow {
                ts_ms,
                endpoint: state.name.clone(),
                model: key.model,
                engine: key.engine,
                metric: metric.to_string(),
                value,
            })
            .collect();
        recorder.record(rows);
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits, even mid-filter-edit.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if self.show_help {
            // Any of the dismissal keys closes the overlay.
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
            ) {
                self.show_help = false;
            }
            return;
        }
        if self.raw.editing {
            self.handle_filter_edit(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Tab => self.view = self.next_view(self.view),
            KeyCode::BackTab => self.view = self.prev_view(self.view),
            KeyCode::Char('1') => self.view = View::Fleet,
            KeyCode::Char(c @ '2'..='9') => {
                let idx = (c as usize) - ('2' as usize);
                if idx < self.endpoints.len() {
                    self.view = View::Endpoint(idx);
                }
            }
            KeyCode::Char('h') | KeyCode::Char('H') => self.view = View::History,
            // Case matters: R is Raw metrics, r is refresh (documented in ?).
            KeyCode::Char('R') => self.view = View::Raw,
            KeyCode::Char('r') => self.control.force_refresh(),
            KeyCode::Char('p') => self.paused = !self.paused,
            // '+' = faster refresh = SHORTER interval (matches README).
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust_interval(-1),
            KeyCode::Char('-') => self.adjust_interval(1),
            KeyCode::Char('s') => {
                if self.view == View::Fleet {
                    self.fleet_sort = self.fleet_sort.next();
                }
            }
            KeyCode::Char('/') => {
                if self.view == View::Raw {
                    self.raw.editing = true;
                }
            }
            KeyCode::Esc => {
                if self.view == View::Raw && !self.raw.filter.is_empty() {
                    self.raw.filter.clear();
                    self.raw.scroll = 0;
                }
            }
            KeyCode::Char('e') => self.cycle_endpoint_filter(),
            KeyCode::Char('m') => {
                if self.view == View::History {
                    self.cycle_model_filter();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::PageDown => self.scroll_by(10),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::Char('g') | KeyCode::Home => self.scroll_home(),
            KeyCode::Char('G') | KeyCode::End => self.scroll_by(i64::MAX / 2),
            KeyCode::Enter if self.view == View::Fleet => {
                let idx = self
                    .sorted_endpoint_indices()
                    .get(self.fleet_selected)
                    .copied();
                if let Some(idx) = idx {
                    self.view = View::Endpoint(idx);
                }
            }
            _ => {}
        }
    }

    fn handle_filter_edit(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.raw.filter.clear();
                self.raw.editing = false;
                self.raw.scroll = 0;
            }
            KeyCode::Enter => self.raw.editing = false,
            KeyCode::Backspace => {
                self.raw.filter.pop();
                self.raw.scroll = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.raw.filter.len() < 128 =>
            {
                self.raw.filter.push(c);
                self.raw.scroll = 0;
            }
            _ => {}
        }
    }

    fn adjust_interval(&mut self, direction: i64) {
        let current_ms = self.refresh_interval.as_millis() as i64;
        // Steps: 250ms below 1s, 500ms up to 5s, 1s beyond.
        let step = if current_ms < 1000 {
            250
        } else if current_ms < 5000 {
            500
        } else {
            1000
        };
        let next =
            (current_ms + direction * step).clamp(MIN_REFRESH_MS as i64, MAX_REFRESH_MS as i64);
        self.refresh_interval = Duration::from_millis(next as u64);
        self.control.set_interval(self.refresh_interval);
    }

    fn cycle_endpoint_filter(&mut self) {
        let n = self.endpoints.len();
        let slot = match self.view {
            View::History => &mut self.hist.endpoint_filter,
            View::Raw => &mut self.raw.endpoint_filter,
            _ => return,
        };
        *slot = match *slot {
            None => Some(0),
            Some(i) if i + 1 < n => Some(i + 1),
            Some(_) => None,
        };
        self.raw.scroll = 0;
        self.hist.scroll = 0;
    }

    /// All (model, engine) series keys observed across endpoints, sorted —
    /// the domain for the History view's model filter.
    pub fn observed_series(&self) -> Vec<crate::metrics::normalize::SeriesKey> {
        let mut set = std::collections::BTreeSet::new();
        for e in &self.endpoints {
            if let Some(c) = &e.curated {
                set.extend(c.series.keys().cloned());
            }
        }
        set.into_iter().collect()
    }

    fn cycle_model_filter(&mut self) {
        let n = self.observed_series().len();
        if n == 0 {
            self.hist.model_filter = None;
            return;
        }
        self.hist.model_filter = match self.hist.model_filter {
            None => Some(0),
            Some(i) if i + 1 < n => Some(i + 1),
            Some(_) => None,
        };
    }

    fn scroll_by(&mut self, delta: i64) {
        // i128 arithmetic: G's huge jump delta plus an already-large scroll
        // must clamp, never overflow.
        let apply = |v: &mut usize, max: usize| {
            let next = (*v as i128 + i128::from(delta)).clamp(0, max as i128);
            *v = next as usize;
        };
        match self.view {
            View::Fleet => {
                let max = self.endpoints.len().saturating_sub(1);
                apply(&mut self.fleet_selected, max);
            }
            View::Raw => {
                // Upper bound enforced during render (it knows the row count);
                // a huge cap here just lets G jump far.
                apply(&mut self.raw.scroll, usize::MAX / 2);
            }
            View::History => {
                // Enough for the 1-column layout on a short terminal; the
                // renderer clamps to the real row count.
                apply(&mut self.hist.scroll, 16);
            }
            View::Endpoint(_) => {}
        }
    }

    fn scroll_home(&mut self) {
        match self.view {
            View::Fleet => self.fleet_selected = 0,
            View::Raw => self.raw.scroll = 0,
            View::History => self.hist.scroll = 0,
            View::Endpoint(_) => {}
        }
    }

    pub fn next_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => {
                if n > 0 {
                    View::Endpoint(0)
                } else {
                    View::History
                }
            }
            View::Endpoint(i) => {
                if i + 1 < n {
                    View::Endpoint(i + 1)
                } else {
                    View::History
                }
            }
            View::History => View::Raw,
            View::Raw => View::Fleet,
        }
    }

    pub fn prev_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => View::Raw,
            View::Endpoint(0) => View::Fleet,
            View::Endpoint(i) => View::Endpoint(i - 1),
            View::History => {
                if n > 0 {
                    View::Endpoint(n - 1)
                } else {
                    View::Fleet
                }
            }
            View::Raw => View::History,
        }
    }

    /// Endpoint indices in fleet-table order under the active sort.
    /// Sorting is deterministic: ties fall back to name.
    pub fn sorted_endpoint_indices(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.endpoints.len()).collect();
        let sort = self.fleet_sort;
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&self.endpoints[a], &self.endpoints[b]);
            let name = ea.name.cmp(&eb.name);
            let by = |v: Option<f64>| v.unwrap_or(f64::NEG_INFINITY);
            match sort {
                FleetSort::Name => name,
                FleetSort::Running => by(eb.aggregate().running)
                    .partial_cmp(&by(ea.aggregate().running))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
                FleetSort::Waiting => by(eb.aggregate().waiting)
                    .partial_cmp(&by(ea.aggregate().waiting))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
                FleetSort::GenTps => by(eb.aggregate().generation_tps)
                    .partial_cmp(&by(ea.aggregate().generation_tps))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
            }
        });
        idx
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::CollectorControl;
    use crate::config::Config;
    use tokio::sync::watch;

    fn test_app(endpoint_urls: &[&str]) -> App {
        use clap::Parser;
        let mut args = vec!["vllmtop".to_string()];
        for (i, u) in endpoint_urls.iter().enumerate() {
            args.push("--endpoint".into());
            args.push(format!("ep{i}={u}"));
        }
        let cli = crate::cli::Cli::parse_from(args);
        let config: Config = crate::config::load(&cli, |_| None).unwrap();
        let (interval_tx, _rx) = watch::channel(config.refresh_interval);
        let (force_tx, _rx2) = watch::channel(0);
        let control = CollectorControl {
            interval_tx,
            force_tx,
        };
        App::new(config, Theme::mono(), control)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn tab_cycles_through_all_views() {
        let app = test_app(&["http://h1:1", "http://h2:2"]);
        let mut v = View::Fleet;
        let mut seen = vec![v];
        for _ in 0..4 {
            v = app.next_view(v);
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                View::Fleet,
                View::Endpoint(0),
                View::Endpoint(1),
                View::History,
                View::Raw
            ]
        );
        assert_eq!(app.next_view(View::Raw), View::Fleet);
        // And backwards.
        assert_eq!(app.prev_view(View::Fleet), View::Raw);
        assert_eq!(app.prev_view(View::Endpoint(0)), View::Fleet);
        assert_eq!(app.prev_view(View::History), View::Endpoint(1));
    }

    #[test]
    fn number_keys_select_endpoints_and_ignore_out_of_range() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.view, View::Endpoint(0));
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.view, View::Endpoint(1));
        // '9' is out of range with 2 endpoints: view unchanged.
        app.handle_key(key(KeyCode::Char('9')));
        assert_eq!(app.view, View::Endpoint(1));
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.view, View::Fleet);
    }

    #[test]
    fn more_than_nine_endpoints_reachable_via_tab() {
        let urls: Vec<String> = (0..12).map(|i| format!("http://h{i}:80")).collect();
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        let app = test_app(&refs);
        // Endpoint 11 has no number key but Tab reaches it.
        let mut v = View::Fleet;
        for _ in 0..12 {
            v = app.next_view(v);
        }
        assert_eq!(v, View::Endpoint(11));
    }

    #[test]
    fn case_sensitive_r_bindings() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('R')));
        assert_eq!(app.view, View::Raw);
        // Lowercase r must NOT navigate (it forces a refresh instead).
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.view, View::Fleet);
    }

    #[test]
    fn filter_editing_captures_q_and_esc_clears() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('R')));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.raw.editing);
        for c in "quest".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // 'q' went into the filter, not quit.
        assert!(!app.should_quit());
        assert_eq!(app.raw.filter, "quest");
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.raw.editing);
        assert_eq!(app.raw.filter, "quest");
        // Esc outside editing clears the filter.
        app.handle_key(key(KeyCode::Esc));
        assert!(app.raw.filter.is_empty());
    }

    #[test]
    fn quit_keys() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit());

        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn interval_adjustment_clamps_and_plus_means_faster() {
        let mut app = test_app(&["http://h1:1"]);
        assert_eq!(app.refresh_interval, Duration::from_millis(1000));
        // '+' = faster = shorter interval (documented direction).
        app.handle_key(key(KeyCode::Char('+')));
        assert_eq!(app.refresh_interval, Duration::from_millis(500));
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Char('+')));
        }
        assert_eq!(app.refresh_interval, Duration::from_millis(MIN_REFRESH_MS));
        for _ in 0..500 {
            app.handle_key(key(KeyCode::Char('-')));
        }
        assert_eq!(app.refresh_interval, Duration::from_millis(MAX_REFRESH_MS));
    }

    #[test]
    fn repeated_end_key_never_overflows_scroll() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('R')));
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Char('G')));
            app.handle_key(key(KeyCode::End));
        }
        // Clamped to the raw view's cap, no wrap/panic.
        assert!(app.raw.scroll > 0);
    }

    #[test]
    fn pause_freezes_display_not_collection() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.paused);
        app.dirty = false;
        // A scrape while paused still updates state but does not mark dirty.
        let outcome = crate::state::ScrapeOutcome {
            at: Instant::now(),
            wall: std::time::SystemTime::now(),
            duration: Duration::from_millis(1),
            result: Err("x".into()),
        };
        app.handle_event(AppEvent::Scrape {
            endpoint: 0,
            outcome,
        });
        assert!(!app.dirty);
        assert_eq!(app.endpoints[0].total_scrapes, 1);
    }

    #[test]
    fn help_overlay_swallows_navigation() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Fleet); // unchanged
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.show_help);
    }

    #[test]
    fn endpoint_filter_cycles_all_then_each_then_all() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        app.handle_key(key(KeyCode::Char('R')));
        assert_eq!(app.raw.endpoint_filter, None);
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.raw.endpoint_filter, Some(0));
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.raw.endpoint_filter, Some(1));
        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.raw.endpoint_filter, None);
    }
}
