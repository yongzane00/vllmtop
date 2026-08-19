//! Command-line interface definitions.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug, Default)]
#[command(
    name = "vllmtop",
    version,
    about = "A colorful terminal dashboard for monitoring vLLM servers",
    long_about = "Monitors one or more vLLM servers by polling their read-only HTTP endpoints\n\
                  (/metrics, /health, /version, /v1/models). It never proxies or inspects\n\
                  inference traffic and needs no accelerator libraries.\n\n\
                  With no arguments it monitors http://127.0.0.1:8000."
)]
pub struct Cli {
    /// Endpoint to monitor, as NAME=URL or bare URL. Repeatable.
    /// When given, replaces the endpoint list from the config file entirely.
    #[arg(short, long = "endpoint", value_name = "NAME=URL")]
    pub endpoints: Vec<String>,

    /// Path to a TOML config file (default: ~/.config/vllmtop/config.toml).
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Record metric history to this SQLite database (off unless given).
    #[arg(long, value_name = "PATH")]
    pub record: Option<PathBuf>,

    /// Refresh interval in milliseconds (min 250, max 60000).
    #[arg(long, value_name = "MS")]
    pub refresh_interval_ms: Option<u64>,

    /// In-memory history window in seconds (min 30, max 86400).
    #[arg(long, value_name = "SECS")]
    pub history_seconds: Option<u64>,

    /// Rolling window for latency percentile estimates, in seconds.
    #[arg(long, value_name = "SECS")]
    pub percentile_window_seconds: Option<u64>,

    /// Days of recorded history to retain (with --record).
    #[arg(long, value_name = "DAYS")]
    pub retention_days: Option<u32>,

    /// Disable colors (the NO_COLOR environment variable also works).
    #[arg(long)]
    pub no_color: bool,

    /// Print shell completions to stdout and exit.
    #[arg(long, value_name = "SHELL", value_enum)]
    pub completions: Option<clap_complete::Shell>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cli = Cli::parse_from(["vllmtop"]);
        assert!(cli.endpoints.is_empty());
        assert!(cli.config.is_none());
        assert!(cli.record.is_none());
    }

    #[test]
    fn repeatable_endpoints() {
        let cli = Cli::parse_from([
            "vllmtop",
            "--endpoint",
            "local=http://127.0.0.1:8000",
            "--endpoint",
            "spark-a=http://10.0.0.21:8000",
            "-e",
            "https://example.com:8443",
        ]);
        assert_eq!(cli.endpoints.len(), 3);
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
