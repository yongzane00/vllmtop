//! Events flowing into the main loop.

use crate::state::ScrapeOutcome;
use ratatui::crossterm::event::{KeyEvent, MouseEvent};

/// Everything the main select loop can receive, from collectors and the
/// input thread alike. One bounded channel carries them all so backpressure
/// applies uniformly.
#[derive(Debug)]
pub enum AppEvent {
    /// A collector finished one poll cycle (success or failure).
    Scrape {
        endpoint: usize,
        outcome: ScrapeOutcome,
    },
    /// Keyboard input from the terminal.
    Key(KeyEvent),
    /// Mouse input (scrolling in lists).
    Mouse(MouseEvent),
    /// Terminal was resized; re-layout on next draw.
    Resize,
    /// The input thread died (stdin closed); exit gracefully.
    InputClosed,
}
