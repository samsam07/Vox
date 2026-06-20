//! vox — desktop platform + CLI over the vox-core engine (DESIGN §6, §11).
//!
//! Parses the CLI/TOML config, resolves cpal capture/playback devices, starts the
//! engine, wires the cpal stream callbacks to the engine's ring ports, and runs
//! until a stop signal (Ctrl+C / SIGINT / SIGTERM) or an optional `--duration`.
//! Output goes through `log` in plain/quiet mode (see `logging`); `--output tui`
//! renders a live dashboard instead. stdout is reserved for `--list-devices` /
//! `--print-config`.

mod cli;
mod config;
mod device;
mod logging;
mod timer;
mod tui;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream};
use log::{error, info, warn};

use cli::{Cli, OutputMode};
use config::{Config, DEFAULT_PORT};
use device::Role;
use vox_core::{CaptureSink, Engine, EngineConfig, EngineStats, PlaybackSource};

/// How often the plain status line is emitted.
const REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// Resolved, display-ready session facts shared by the plain summary and the TUI.
pub(crate) struct SessionInfo {
    pub mode: &'static str,
    pub capture: String,
    pub playback: String,
    pub peer: Option<SocketAddr>,
    pub bind: SocketAddr,
    pub bitrate: i32,
    pub jitter_ms: u32,
    pub fec: bool,
    pub dtx: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let output = cli.output;
    logging::init(output, cli.verbose);
    let host = cpal::default_host();

    if cli.list_devices {
        return device::list_devices(&host);
    }
    let print_config = cli.print_config;
    let config = Config::build(cli)?;
    if print_config {
        config.print();
        return Ok(());
    }

    let capture = device::resolve(&host, Role::Capture, &config.capture)?;
    let playback = device::resolve(&host, Role::Playback, &config.playback)?;
    if capture.is_none() && playback.is_none() {
        bail!("both capture and playback are 'none'; nothing to do");
    }

    // Choose each device's operating rate (prefer 48 kHz, else native + resample),
    // then negotiate channels at that rate. Disabled roles get a harmless default.
    let capture_rate = match capture.as_ref() {
        Some(device) => {
            device::pick_sample_rate(device, Role::Capture, config.capture_sample_rate)?
        }
        None => device::RATE,
    };
    let playback_rate = match playback.as_ref() {
        Some(device) => {
            device::pick_sample_rate(device, Role::Playback, config.playback_sample_rate)?
        }
        None => device::RATE,
    };
    let capture_channels = channels_for(
        capture.as_ref(),
        config.capture_channels,
        Role::Capture,
        capture_rate,
    )?;
    let playback_channels = channels_for(
        playback.as_ref(),
        config.playback_channels,
        Role::Playback,
        playback_rate,
    )?;

    let peer = match (&capture, &config.peer) {
        (Some(_), Some(spec)) => Some(vox_core::parse_peer(&with_default_port(spec))?),
        (Some(_), None) => bail!("--peer (host[:port]) is required to send captured audio"),
        (None, _) => None,
    };
    // Explicit bind wins; else default to 9680 when receiving; else ephemeral.
    let bind = match (playback.is_some(), config.bind) {
        (_, Some(port)) => Some(port),
        (true, None) => Some(DEFAULT_PORT),
        (false, None) => None,
    };

    // Finer process timer for the session → smoother send pacing, less jitter
    // (Windows only; restored on drop). Held until main returns.
    let _timer = timer::TimerResolution::highest();

    let (engine, ports) = Engine::start(EngineConfig {
        peer,
        bind,
        capture_channels,
        playback_channels,
        capture_sample_rate: capture_rate,
        playback_sample_rate: playback_rate,
        jitter_ms: config.jitter_ms,
        bitrate: config.bitrate,
        fec: config.fec,
        expected_loss: config.expected_loss,
        dtx: config.dtx,
    })?;

    let info = SessionInfo {
        mode: mode_label(&capture, &playback),
        capture: format!(
            "{}{}",
            describe_device(&capture),
            stream_label(capture_channels, capture_rate)
        ),
        playback: format!(
            "{}{}",
            describe_device(&playback),
            stream_label(playback_channels, playback_rate)
        ),
        peer,
        bind: engine.local_addr()?,
        bitrate: config.bitrate,
        jitter_ms: config.jitter_ms,
        fec: config.fec,
        dtx: config.dtx,
    };

    // Wire the cpal stream callbacks to the engine's ring ports. Keep the streams
    // in scope for the session; dropping them stops the audio.
    let cap_stream = match (capture.as_ref(), ports.capture) {
        (Some(device), Some(sink)) => Some(build_capture(
            device,
            capture_channels.unwrap(),
            capture_rate,
            sink,
        )?),
        _ => None,
    };
    let play_stream = match (playback.as_ref(), ports.playback) {
        (Some(device), Some(source)) => Some(build_playback(
            device,
            playback_channels.unwrap(),
            playback_rate,
            source,
        )?),
        _ => None,
    };

    match output {
        OutputMode::Tui => tui::run(&engine, &info, config.duration)?,
        _ => {
            log_summary(&info);
            run_session(&engine, config.duration)?;
        }
    }

    drop(cap_stream);
    drop(play_stream);
    let stats = engine.stop()?;
    report_final(output, capture.is_some(), playback.is_some(), &stats);
    Ok(())
}

/// Forced channel count if given, else auto-negotiate at `rate`, else `None` (role
/// disabled).
fn channels_for(
    device: Option<&Device>,
    forced: Option<u16>,
    role: Role,
    rate: u32,
) -> Result<Option<u16>> {
    match (device, forced) {
        (Some(_), Some(channels)) => Ok(Some(channels)),
        (Some(device), None) => Ok(Some(device::pick_channels(device, role, rate)?)),
        (None, _) => Ok(None),
    }
}

fn with_default_port(spec: &str) -> String {
    if spec.contains(':') {
        spec.to_string()
    } else {
        format!("{spec}:{DEFAULT_PORT}")
    }
}

fn mode_label(capture: &Option<Device>, playback: &Option<Device>) -> &'static str {
    match (capture.is_some(), playback.is_some()) {
        (true, true) => "full duplex",
        (true, false) => "send-only",
        (false, true) => "receive-only",
        (false, false) => unreachable!("guarded above"),
    }
}

fn log_summary(info: &SessionInfo) {
    info!("vox {} — {}", env!("CARGO_PKG_VERSION"), info.mode);
    info!("capture  {}", info.capture);
    info!("playback {}", info.playback);
    if let Some(peer) = info.peer {
        info!("sending to {peer}");
    }
    info!("listening {}", info.bind);
    info!(
        "codec {} bps, jitter {} ms, fec={}, dtx={}",
        info.bitrate, info.jitter_ms, info.fec, info.dtx
    );
}

fn report_final(output: OutputMode, sending: bool, receiving: bool, stats: &EngineStats) {
    let send = format!(
        "sent {} packets ({} KiB)",
        stats.packets_sent,
        stats.bytes_sent / 1024
    );
    let recv = format!(
        "received {} packets ({} KiB), {} gap frames, {} late/dup, {} overruns, \
         {} recenter (drop {} / hold {})",
        stats.packets_received,
        stats.bytes_received / 1024,
        stats.gap_frames,
        stats.dropped_late,
        stats.overruns,
        stats.recenter_drops + stats.recenter_inserts,
        stats.recenter_drops,
        stats.recenter_inserts
    );
    match output {
        // The TUI suppresses the logger; print a plain summary after it restores.
        OutputMode::Tui => {
            println!("stopped");
            if sending {
                println!("{send}");
            }
            if receiving {
                println!("{recv}");
            }
        }
        OutputMode::Quiet => {}
        OutputMode::Plain => {
            info!("stopped");
            if sending {
                info!("{send}");
            }
            if receiving {
                info!("{recv}");
            }
        }
    }
}

/// Block until Ctrl+C / SIGINT / SIGTERM, or until `duration` elapses, emitting a
/// periodic throughput line and connection-liveness messages.
fn run_session(engine: &Engine, duration: Option<u64>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("install signal handler")?;
    }
    match duration {
        Some(secs) => info!("running for {secs}s (Ctrl+C to stop early)"),
        None => info!("running (Ctrl+C to stop)"),
    }
    let deadline = duration.map(|secs| Instant::now() + Duration::from_secs(secs));

    let mut last = engine.stats();
    let mut last_report = Instant::now();
    let mut peer_seen = last.packets_received > 0;
    let mut warned_silent = false;

    while !stop.load(Ordering::SeqCst) {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));

        if last_report.elapsed() < REPORT_INTERVAL {
            continue;
        }
        let now = engine.stats();
        let secs = last_report.elapsed().as_secs_f64();
        let recv_delta = now.packets_received - last.packets_received;

        if now.packets_received > 0 && !peer_seen {
            peer_seen = true;
            info!("peer connected");
        }
        if peer_seen {
            if recv_delta == 0 && !warned_silent {
                warn!("peer silent (no packets in last {secs:.0}s)");
                warned_silent = true;
            } else if recv_delta > 0 {
                warned_silent = false;
            }
        }

        info!(
            "tx {:.0} kbps {:.0} pkt/s | rx {:.0} kbps {:.0} pkt/s | gaps {} drops {} overruns {} recenter {}/{}",
            kbps(now.bytes_sent - last.bytes_sent, secs),
            (now.packets_sent - last.packets_sent) as f64 / secs,
            kbps(now.bytes_received - last.bytes_received, secs),
            recv_delta as f64 / secs,
            now.gap_frames,
            now.dropped_late,
            now.overruns,
            now.recenter_drops,
            now.recenter_inserts
        );

        last = now;
        last_report = Instant::now();
    }
    Ok(())
}

pub(crate) fn kbps(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        0.0
    } else {
        bytes as f64 * 8.0 / 1000.0 / secs
    }
}

fn build_capture(
    device: &Device,
    channels: u16,
    rate: u32,
    mut sink: CaptureSink,
) -> Result<Stream> {
    let stream = device
        .build_input_stream::<f32, _, _>(
            &device::stream_config(channels, rate),
            // SACRED: non-blocking ring push only.
            move |data: &[f32], _| sink.push(data),
            move |err| error!("capture stream: {err}"),
            None,
        )
        .context("build capture stream")?;
    stream.play().context("start capture stream")?;
    Ok(stream)
}

fn build_playback(
    device: &Device,
    channels: u16,
    rate: u32,
    mut source: PlaybackSource,
) -> Result<Stream> {
    let stream = device
        .build_output_stream::<f32, _, _>(
            &device::stream_config(channels, rate),
            // SACRED: non-blocking ring pop (+ silence on underrun) only.
            move |data: &mut [f32], _| source.fill(data),
            move |err| error!("playback stream: {err}"),
            None,
        )
        .context("build playback stream")?;
    stream.play().context("start playback stream")?;
    Ok(stream)
}

fn describe_device(device: &Option<Device>) -> String {
    match device {
        Some(device) => device.name().unwrap_or_else(|_| "<unknown>".to_string()),
        None => "none".to_string(),
    }
}

/// Device stream label for the summary, e.g. `  [44100 Hz, mono]` (empty if the
/// role is disabled). A rate other than 48000 means vox is resampling (M9).
fn stream_label(channels: Option<u16>, rate: u32) -> String {
    match channels {
        Some(1) => format!("  [{rate} Hz, mono]"),
        Some(2) => format!("  [{rate} Hz, stereo]"),
        Some(n) => format!("  [{rate} Hz, {n} ch]"),
        None => String::new(),
    }
}
