//! Resolved configuration: defaults < TOML file < CLI flags (DESIGN §6, §7).

use std::io::Write;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::cli::Cli;

/// VOX_DEFAULT_PORT (DESIGN §6).
pub const DEFAULT_PORT: u16 = 9680;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_BITRATE: i32 = 24_000;
const DEFAULT_JITTER_MS: u32 = 100;
const DEFAULT_FEC: bool = true;
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

        let capture_sample_rate = cli
            .capture_sample_rate
            .or(file.capture_sample_rate)
            .unwrap_or(DEFAULT_SAMPLE_RATE);
        let playback_sample_rate = cli
            .playback_sample_rate
            .or(file.playback_sample_rate)
            .unwrap_or(DEFAULT_SAMPLE_RATE);
        if capture_sample_rate != DEFAULT_SAMPLE_RATE || playback_sample_rate != DEFAULT_SAMPLE_RATE
        {
            bail!("Phase 1 is 48 kHz only; non-48 kHz needs the Phase-2 resampler");
        }

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
        let lines = [
            format!(
                "peer              = {}",
                self.peer.as_deref().unwrap_or("(none)")
            ),
            format!(
                "bind              = {}",
                self.bind.map_or("auto".to_string(), |p| p.to_string())
            ),
            format!("capture           = {}", self.capture),
            format!("playback          = {}", self.playback),
            format!("capture_channels  = {}", channels(self.capture_channels)),
            format!("playback_channels = {}", channels(self.playback_channels)),
            format!("bitrate           = {}", self.bitrate),
            format!("fec               = {}", self.fec),
            format!("expected_loss     = {}", self.expected_loss),
            format!("dtx               = {}", self.dtx),
            format!("jitter_ms         = {}", self.jitter_ms),
        ];
        let mut out = std::io::stdout();
        for l in lines {
            let _ = writeln!(out, "{l}");
        }
    }
}
