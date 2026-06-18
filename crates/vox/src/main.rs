//! vox — desktop platform + CLI over the vox-core engine (DESIGN §6, §11).
//!
//! Parses the CLI/TOML config, resolves cpal capture/playback devices, starts the
//! engine, wires the cpal stream callbacks to the engine's ring ports, and runs
//! until a stop signal (Ctrl+C / SIGINT / SIGTERM) or an optional `--duration`.

mod cli;
mod config;
mod device;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream};

use cli::Cli;
use config::{Config, DEFAULT_PORT};
use device::Role;
use vox_core::{CaptureSink, Engine, EngineConfig, PlaybackSource};

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    let host = cpal::default_host();

    if cli.list_devices {
        println!("cpal host: {}", host.id().name());
        return device::list_devices(&host);
    }

    let config = Config::build(cli)?;

    let capture = device::resolve(&host, Role::Capture, &config.capture)?;
    let playback = device::resolve(&host, Role::Playback, &config.playback)?;
    if capture.is_none() && playback.is_none() {
        bail!("both capture and playback are 'none'; nothing to do");
    }

    let capture_channels = channels_for(capture.as_ref(), config.capture_channels, Role::Capture)?;
    let playback_channels =
        channels_for(playback.as_ref(), config.playback_channels, Role::Playback)?;

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

    let (engine, ports) = Engine::start(EngineConfig {
        peer,
        bind,
        capture_channels,
        playback_channels,
        jitter_ms: config.jitter_ms,
        bitrate: config.bitrate,
    })?;
    print_summary(&config, &capture, &playback, &engine, peer)?;

    // Wire the cpal stream callbacks to the engine's ring ports. Keep the streams
    // in scope for the session; dropping them stops the audio.
    let cap_stream = match (capture.as_ref(), ports.capture) {
        (Some(device), Some(sink)) => Some(build_capture(device, capture_channels.unwrap(), sink)?),
        _ => None,
    };
    let play_stream = match (playback.as_ref(), ports.playback) {
        (Some(device), Some(source)) => {
            Some(build_playback(device, playback_channels.unwrap(), source)?)
        }
        _ => None,
    };

    wait_for_stop(config.duration)?;

    drop(cap_stream);
    drop(play_stream);
    let stats = engine.stop()?;
    println!("stopped.");
    if capture.is_some() {
        println!("  send: {} packets", stats.packets_sent);
    }
    if playback.is_some() {
        println!(
            "  recv: {} packets, {} gap frames, {} late/dup dropped",
            stats.packets_received, stats.gap_frames, stats.dropped_late
        );
    }
    Ok(())
}

/// Forced channel count if given, else auto-negotiate, else `None` (role disabled).
fn channels_for(device: Option<&Device>, forced: Option<u16>, role: Role) -> Result<Option<u16>> {
    match (device, forced) {
        (Some(_), Some(channels)) => Ok(Some(channels)),
        (Some(device), None) => Ok(Some(device::pick_channels(device, role)?)),
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

fn print_summary(
    config: &Config,
    capture: &Option<Device>,
    playback: &Option<Device>,
    engine: &Engine,
    peer: Option<std::net::SocketAddr>,
) -> Result<()> {
    let mode = match (capture.is_some(), playback.is_some()) {
        (true, true) => "full duplex",
        (true, false) => "send-only",
        (false, true) => "receive-only",
        (false, false) => unreachable!("guarded above"),
    };
    println!("vox: {mode}");
    println!("  capture:  {}", describe_device(capture));
    println!("  playback: {}", describe_device(playback));
    if let Some(peer) = peer {
        println!("  peer:     {peer}");
    }
    println!("  bind:     {}", engine.local_addr()?);
    println!(
        "  codec:    {} bps, jitter {} ms, fec={}, dtx={}, expected_loss={}%",
        config.bitrate, config.jitter_ms, config.fec, config.dtx, config.expected_loss
    );
    if config.fec || config.dtx {
        println!("  note: fec/dtx are parsed but take effect at M7.");
    }
    Ok(())
}

/// Block until Ctrl+C / SIGINT / SIGTERM, or until `duration` elapses.
fn wait_for_stop(duration: Option<u64>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))
            .context("install signal handler")?;
    }
    match duration {
        Some(secs) => println!("running for {secs}s (Ctrl+C to stop early)..."),
        None => println!("running (Ctrl+C to stop)..."),
    }
    let deadline = duration.map(|secs| Instant::now() + Duration::from_secs(secs));
    while !stop.load(Ordering::SeqCst) {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn build_capture(device: &Device, channels: u16, mut sink: CaptureSink) -> Result<Stream> {
    let stream = device
        .build_input_stream::<f32, _, _>(
            &device::stream_config(channels),
            // SACRED: non-blocking ring push only.
            move |data: &[f32], _| sink.push(data),
            move |err| eprintln!("capture stream error: {err}"),
            None,
        )
        .context("build capture stream")?;
    stream.play().context("start capture stream")?;
    Ok(stream)
}

fn build_playback(device: &Device, channels: u16, mut source: PlaybackSource) -> Result<Stream> {
    let stream = device
        .build_output_stream::<f32, _, _>(
            &device::stream_config(channels),
            // SACRED: non-blocking ring pop (+ silence on underrun) only.
            move |data: &mut [f32], _| source.fill(data),
            move |err| eprintln!("playback stream error: {err}"),
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
