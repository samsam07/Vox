//! Output/logging policy (M6c). Functional, user-facing messages go through `log`
//! at `info`; technical detail (`debug`/`trace`) appears only with `-v`/`-vv`.
//! `--output quiet` drops everything below `warn`. `RUST_LOG` overrides the level.
//!
//! Logs go to stderr; stdout is reserved for explicit data (`--list-devices`,
//! `--print-config`). `env_logger` ignores write errors, so a closed stderr won't
//! panic the process.

use std::io::Write;

use log::{Level, LevelFilter};

use crate::cli::OutputMode;

/// Initialise the global logger from the chosen output mode and verbosity.
pub fn init(mode: OutputMode, verbose: u8) {
    let level = match (mode, verbose) {
        // The TUI owns the screen — keep logs off so they don't corrupt it.
        (OutputMode::Tui, _) => LevelFilter::Off,
        (OutputMode::Quiet, _) => LevelFilter::Warn,
        (_, 0) => LevelFilter::Info,
        (_, 1) => LevelFilter::Debug,
        (_, _) => LevelFilter::Trace,
    };

    let mut builder = env_logger::Builder::new();
    builder.filter_level(level);

    if verbose == 0 {
        // Clean functional format: just the message, with a tag for warn/error.
        builder.format(|buf, record| match record.level() {
            Level::Info => writeln!(buf, "{}", record.args()),
            Level::Warn => writeln!(buf, "warning: {}", record.args()),
            Level::Error => writeln!(buf, "error: {}", record.args()),
            _ => Ok(()),
        });
    }
    // Let RUST_LOG override the computed level/filters for power users.
    builder.parse_default_env();
    builder.init();
}
