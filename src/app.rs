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
/// Fleet → Endpoint(0..n) → Fleet…
///
/// The fleet view embeds the rolling history charts below the endpoint
/// table (PgUp/PgDn scrolls them); there is no separate History or Raw view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Fleet,
    Endpoint(usize),
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
            FleetSort::GenTps => "gen tokens/s",
        }
    }
}

pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub endpoints: Vec<EndpointState>,
    pub view: View,
    pub fleet_selected: usize,
    pub fleet_sort: FleetSort,
    /// Scroll offset (in chart-grid rows) of the charts embedded in the
    /// fleet view; the renderer clamps it to the real row count.
    pub fleet_chart_scroll: usize,
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
            fleet_chart_scroll: 0,
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
            AppEvent::MetricsStarted { endpoint, at } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                state.mark_attempt_started(at);
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::OptionalUpdate {
                endpoint,
                healthy,
                version,
                models,
            } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                state.apply_optional(healthy, version, models);
                if !self.paused {
                    self.dirty = true;
                }
            }
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
                    MouseEventKind::ScrollUp => self.scroll_charts(-1),
                    MouseEventKind::ScrollDown => self.scroll_charts(1),
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
        // Ctrl+C always quits.
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
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            // The chart grid below the fleet table scrolls independently of
            // the row selection.
            KeyCode::PageDown => self.scroll_charts(1),
            KeyCode::PageUp => self.scroll_charts(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.fleet_selected = 0;
                self.fleet_chart_scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => self.move_selection(i64::MAX / 2),
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

    /// Move the fleet-table row selection (i128: G's huge jump plus a large
    /// value must clamp, never overflow).
    fn move_selection(&mut self, delta: i64) {
        if self.view != View::Fleet {
            return;
        }
        let max = self.endpoints.len().saturating_sub(1);
        let next = (self.fleet_selected as i128 + i128::from(delta)).clamp(0, max as i128);
        self.fleet_selected = next as usize;
    }

    /// Scroll the chart grid embedded in the fleet view. The bound here only
    /// needs to cover the 1-column layout; the renderer clamps to the real
    /// row count.
    fn scroll_charts(&mut self, delta: i64) {
        if self.view != View::Fleet {
            return;
        }
        let next = (self.fleet_chart_scroll as i128 + i128::from(delta)).clamp(0, 16);
        self.fleet_chart_scroll = next as usize;
    }

    pub fn next_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => {
                if n > 0 {
                    View::Endpoint(0)
                } else {
                    View::Fleet
                }
            }
            View::Endpoint(i) => {
                if i + 1 < n {
                    View::Endpoint(i + 1)
                } else {
                    View::Fleet
                }
            }
        }
    }

    pub fn prev_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => {
                if n > 0 {
                    View::Endpoint(n - 1)
                } else {
                    View::Fleet
                }
            }
            View::Endpoint(0) => View::Fleet,
            View::Endpoint(i) => View::Endpoint(i - 1),
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
        for _ in 0..3 {
            v = app.next_view(v);
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                View::Fleet,
                View::Endpoint(0),
                View::Endpoint(1),
                View::Fleet
            ]
        );
        // And backwards.
        assert_eq!(app.prev_view(View::Fleet), View::Endpoint(1));
        assert_eq!(app.prev_view(View::Endpoint(0)), View::Fleet);
        assert_eq!(app.prev_view(View::Endpoint(1)), View::Endpoint(0));
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
    fn removed_view_keys_do_not_navigate() {
        let mut app = test_app(&["http://h1:1"]);
        // 'R' (old Raw view), 'H' (old History view), and lowercase 'r'
        // (refresh) must all leave the view unchanged.
        for k in ['R', 'H', 'h', 'r', '/', 'e', 'm'] {
            app.handle_key(key(KeyCode::Char(k)));
            assert_eq!(app.view, View::Fleet, "key {k:?} must not navigate");
        }
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
    fn interval_adjustment_clamps_at_one_second_and_minus_means_slower() {
        let mut app = test_app(&["http://h1:1"]);
        assert_eq!(app.refresh_interval, Duration::from_millis(1000));
        // '+' = faster = shorter interval (documented direction).
        app.handle_key(key(KeyCode::Char('+')));
        assert_eq!(app.refresh_interval, Duration::from_millis(MIN_REFRESH_MS));
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
    fn repeated_end_key_never_overflows_selection() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Char('G')));
            app.handle_key(key(KeyCode::End));
        }
        // Clamped to the last row, no wrap/panic.
        assert_eq!(app.fleet_selected, 1);
    }

    #[test]
    fn chart_scroll_is_independent_of_row_selection_and_clamped() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.fleet_selected, 1);
        assert_eq!(app.fleet_chart_scroll, 0);
        for _ in 0..100 {
            app.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(app.fleet_chart_scroll, 16); // clamped
        assert_eq!(app.fleet_selected, 1); // selection untouched
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.fleet_chart_scroll, 0);
        assert_eq!(app.fleet_selected, 0);
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
    fn metrics_started_event_updates_attempt_state() {
        let mut app = test_app(&["http://h1:1"]);
        let started = Instant::now();
        app.handle_event(AppEvent::MetricsStarted {
            endpoint: 0,
            at: started,
        });
        assert_eq!(app.endpoints[0].last_attempt_at, Some(started));
        assert_eq!(app.endpoints[0].scraping_since, Some(started));
    }

    #[test]
    fn optional_probe_event_updates_metadata_without_a_metrics_scrape() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_event(AppEvent::OptionalUpdate {
            endpoint: 0,
            healthy: Some(false),
            version: Some("1.2.3".into()),
            models: Some(vec!["model-a".into()]),
        });
        let endpoint = &app.endpoints[0];
        assert_eq!(endpoint.healthy, Some(false));
        assert_eq!(endpoint.vllm_version.as_deref(), Some("1.2.3"));
        assert_eq!(endpoint.served_models, ["model-a"]);
        assert_eq!(endpoint.total_scrapes, 0);
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
}
