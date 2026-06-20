//! Command-line grammar (DESIGN §6). Every audio option mirrors a TOML key and
//! overrides it. Two modes: `vox <config.toml>` (config) and `vox --peer …`
//! (ad-hoc); they share the same flags, all optional so TOML can supply them.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

const EXAMPLES: &str = "\
EXAMPLES:
  vox --list-devices
  vox --peer 192.168.1.50                       # full duplex, default devices
  vox --peer 192.168.1.50 --playback none       # send-only (mic -> peer)
  vox --capture none --bind 9680                # receive-only (peer -> speakers)
  vox host.toml                                 # config file
  vox host.toml --output tui                    # with the live dashboard
";

/// How vox presents itself while running.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// Errors only — for headless / Apollo use.
    Quiet,
    /// Clear functional output (default).
    #[default]
    Plain,
    /// Live full-screen dashboard (requires a terminal).
    Tui,
}

#[derive(Parser, Debug)]
#[command(
    name = "vox",
    version,
    about = "Headless bidirectional voice pipe over LAN UDP",
    after_help = EXAMPLES
)]
pub struct Cli {
    /// TOML config file. Flags override its values.
    pub config: Option<PathBuf>,

    /// List capture/playback devices and exit.
    #[arg(long)]
    pub list_devices: bool,

    /// Print the resolved configuration and exit.
    #[arg(long)]
    pub print_config: bool,

    /// Output mode.
    #[arg(short, long, value_enum, default_value_t = OutputMode::Plain)]
    pub output: OutputMode,

    /// Increase verbosity: -v technical detail, -vv trace.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    // --- Connection ---
    /// Send target, host[:port]. Port omitted → 9680.
    #[arg(short, long)]
    pub peer: Option<String>,
    /// Local UDP receive port (omitted → 9680 when receiving; send-only uses an
    /// ephemeral source port).
    #[arg(short, long)]
    pub bind: Option<u16>,

    // --- Devices (none | default | "exact name") ---
    /// Local record device: none | default | "exact name". none = receive-only.
    #[arg(long)]
    pub capture: Option<String>,
    /// Local play device: none | default | "exact name". none = send-only.
    #[arg(long)]
    pub playback: Option<String>,
    /// Capture device rate, Hz (omit = auto: prefer 48000, else native + resample).
    #[arg(long = "capture-sample-rate")]
    pub capture_sample_rate: Option<u32>,
    /// Playback device rate, Hz (omit = auto: prefer 48000, else native + resample).
    #[arg(long = "playback-sample-rate")]
    pub playback_sample_rate: Option<u32>,
    /// Force capture channels (omit = auto-negotiate mono, else stereo).
    #[arg(long = "capture-channels")]
    pub capture_channels: Option<u16>,
    /// Force playback channels (omit = auto-negotiate mono, else stereo).
    #[arg(long = "playback-channels")]
    pub playback_channels: Option<u16>,

    // --- Codec / send-path ---
    /// Opus target bitrate, bits/s.
    #[arg(long)]
    pub bitrate: Option<i32>,
    /// In-band FEC: redundant copy of each frame recovers a single drop (default on).
    #[arg(long)]
    pub fec: Option<bool>,
    /// Expected packet loss %, to tune FEC (only applied when fec is on).
    #[arg(long = "expected-loss")]
    pub expected_loss: Option<u8>,
    /// Discontinuous transmission / silence suppression.
    #[arg(long)]
    pub dtx: Option<bool>,

    // --- Receive-path ---
    /// Jitter buffer depth, ms (default 100; lower on a clean wired LAN, higher on WiFi).
    #[arg(long = "jitter-ms")]
    pub jitter_ms: Option<u32>,

    /// Run for N seconds then exit (default: until Ctrl+C). For tests/ops.
    #[arg(long)]
    pub duration: Option<u64>,
}
