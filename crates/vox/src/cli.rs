//! Command-line grammar (DESIGN §6). Every audio option mirrors a TOML key and
//! overrides it. Two modes: `vox <config.toml>` (config) and `vox --peer …`
//! (ad-hoc); they share the same flags, all optional so TOML can supply them.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "vox", version, about = "Headless bidirectional voice pipe over LAN UDP")]
pub struct Cli {
    /// TOML config file. Flags override its values.
    pub config: Option<PathBuf>,

    /// List capture/playback devices and exit.
    #[arg(long)]
    pub list_devices: bool,

    // --- Connection ---
    /// Send target, host[:port]. Port omitted → 9680.
    #[arg(long)]
    pub peer: Option<String>,
    /// Local UDP receive port (omitted → 9680 when receiving; send-only uses an
    /// ephemeral source port).
    #[arg(long)]
    pub bind: Option<u16>,

    // --- Devices (none | default | "exact name") ---
    /// Local record device: none | default | "exact name". none = receive-only.
    #[arg(long)]
    pub capture: Option<String>,
    /// Local play device: none | default | "exact name". none = send-only.
    #[arg(long)]
    pub playback: Option<String>,
    /// Capture sample rate (Phase 1: 48000 only).
    #[arg(long = "capture-sample-rate")]
    pub capture_sample_rate: Option<u32>,
    /// Playback sample rate (Phase 1: 48000 only).
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
    /// In-band FEC (takes effect at M7).
    #[arg(long)]
    pub fec: Option<bool>,
    /// Expected packet loss %, to tune FEC (takes effect at M7).
    #[arg(long = "expected-loss")]
    pub expected_loss: Option<u8>,
    /// Discontinuous transmission / silence suppression (takes effect at M7).
    #[arg(long)]
    pub dtx: Option<bool>,

    // --- Receive-path ---
    /// Jitter buffer depth, ms (~40-60).
    #[arg(long = "jitter-ms")]
    pub jitter_ms: Option<u32>,

    /// Run for N seconds then exit (default: until Ctrl+C). For tests/ops.
    #[arg(long)]
    pub duration: Option<u64>,
}
