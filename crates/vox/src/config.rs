//! Resolved configuration: defaults < TOML file < CLI flags (DESIGN §6, §7).

use std::io::Write;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::cli::Cli;

/// VOX_DEFAULT_PORT (DESIGN §6).
pub const DEFAULT_PORT: u16 = 9680;
const DEFAULT_BITRATE: i32 = 24_000;
const DEFAULT_JITTER_MS: u32 = 150;
const DEFAULT_FEC: bool = false;
const DEFAULT_EXPECTED_LOSS: u8 = 10;
const DEFAULT_DTX: bool = false;

/// The TOML file shape (every key optional; `deny_unknown_fields` catches typos).
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    peer: Option<String>,
    bind: Option<u16>,
    capture: Option<String>,
    playback: Option<String>,
    capture_sample_rate: Option<u32>,
    playback_sample_rate: Option<u32>,
    capture_channels: Option<u16>,
    playback_channels: Option<u16>,
    bitrate: Option<i32>,
    fec: Option<bool>,
    expected_loss: Option<u8>,
    dtx: Option<bool>,
    jitter_ms: Option<u32>,
}

/// Fully resolved configuration the binary acts on.
pub struct Config {
    pub peer: Option<String>,
    pub bind: Option<u16>,
    pub capture: String,
    pub playback: String,
    pub capture_channels: Option<u16>,
    pub playback_channels: Option<u16>,
    /// Device sample rates (Hz); `None` → auto (prefer 48 kHz, else device native).
    pub capture_sample_rate: Option<u32>,
    pub playback_sample_rate: Option<u32>,
    pub bitrate: i32,
    pub fec: bool,
    pub expected_loss: u8,
    pub dtx: bool,
    pub jitter_ms: u32,
    pub duration: Option<u64>,
}

impl Config {
    /// Merge defaults, the optional TOML file, and the CLI flags (flag > file >
    /// default).
    pub fn build(cli: Cli) -> Result<Config> {
        let file = match &cli.config {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("read config {}", path.display()))?;
                toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?
            }
            None => FileConfig::default(),
        };

        Ok(Config {
            peer: cli.peer.or(file.peer),
            bind: cli.bind.or(file.bind),
            capture: cli
                .capture
                .or(file.capture)
                .unwrap_or_else(|| "default".into()),
            playback: cli
                .playback
                .or(file.playback)
                .unwrap_or_else(|| "default".into()),
            capture_channels: cli.capture_channels.or(file.capture_channels),
            playback_channels: cli.playback_channels.or(file.playback_channels),
            capture_sample_rate: cli.capture_sample_rate.or(file.capture_sample_rate),
            playback_sample_rate: cli.playback_sample_rate.or(file.playback_sample_rate),
            bitrate: cli.bitrate.or(file.bitrate).unwrap_or(DEFAULT_BITRATE),
            fec: cli.fec.or(file.fec).unwrap_or(DEFAULT_FEC),
            expected_loss: cli
                .expected_loss
                .or(file.expected_loss)
                .unwrap_or(DEFAULT_EXPECTED_LOSS),
            dtx: cli.dtx.or(file.dtx).unwrap_or(DEFAULT_DTX),
            jitter_ms: cli
                .jitter_ms
                .or(file.jitter_ms)
                .unwrap_or(DEFAULT_JITTER_MS),
            duration: cli.duration,
        })
    }

    /// Dump the resolved config to stdout (for `--print-config`), tolerating a
    /// closed pipe.
    pub fn print(&self) {
        let channels = |c: Option<u16>| c.map_or("auto".to_string(), |n| n.to_string());
        let rate = |r: Option<u32>| r.map_or("auto".to_string(), |n| n.to_string());
        // Labels mirror the TOML keys verbatim (so they copy-paste); pad to align `=`.
        let line = |key: &str, value: String| format!("{key:<20} = {value}");
        let lines = [
            line("peer", self.peer.clone().unwrap_or_else(|| "(none)".into())),
            line(
                "bind",
                self.bind.map_or("auto".to_string(), |p| p.to_string()),
            ),
            line("capture", self.capture.clone()),
            line("playback", self.playback.clone()),
            line("capture_channels", channels(self.capture_channels)),
            line("playback_channels", channels(self.playback_channels)),
            line("capture_sample_rate", rate(self.capture_sample_rate)),
            line("playback_sample_rate", rate(self.playback_sample_rate)),
            line("bitrate", self.bitrate.to_string()),
            line("fec", self.fec.to_string()),
            line("expected_loss", self.expected_loss.to_string()),
            line("dtx", self.dtx.to_string()),
            line("jitter_ms", self.jitter_ms.to_string()),
        ];
        let mut out = std::io::stdout();
        for l in lines {
            let _ = writeln!(out, "{l}");
        }
    }
}
