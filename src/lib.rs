//! vllmtop — a terminal dashboard for vLLM servers.
//!
//! The crate is a library plus a thin binary so integration tests can drive
//! the collection pipeline directly. See `docs/ARCHITECTURE.md` for the data
//! flow: collectors → bounded channel → state reducer → renderer, with an
//! optional SQLite recorder on its own thread.

#![forbid(unsafe_code)]

pub mod app;
pub mod cli;
pub mod collector;
pub mod config;
pub mod event;
pub mod metrics;
pub mod state;
pub mod storage;
pub mod ui;
