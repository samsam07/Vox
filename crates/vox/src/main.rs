//! vox — desktop platform + CLI over the vox-core engine (DESIGN §11).
//!
//! Resolves cpal capture/playback devices, starts the engine, and wires the cpal
//! stream callbacks to the engine's ring ports (capture cb → CaptureSink::push,
//! playback cb → PlaybackSource::fill). The locked CLI (DESIGN §6) lands at M6b;
//! until then inputs come from environment variables.

mod device;

use std::str::FromStr;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream};

use device::Role;
use vox_core::{CaptureSink, Engine, EngineConfig, PlaybackSource};

fn main() -> Result<()> {
    let host = cpal::default_host();
    println!("cpal host: {}", host.id().name());
    device::list_devices(&host)?;
    println!();

    // Temporary M6a inputs (replaced by the locked CLI at M6b). VOX_CAPTURE /
    // VOX_PLAYBACK accept the same `none|default|name` specs as the future flags.
    let cap_spec = env_or("VOX_CAPTURE", "default");
    let play_spec = env_or("VOX_PLAYBACK", "default");
    let ring_ms: u32 = env_parse("VOX_RING_MS", 50);
    let jitter_ms: u32 = env_parse("VOX_JITTER_MS", 50);
    let bitrate: i32 = env_parse("VOX_BITRATE", 24_000);
    let secs: u64 = env_parse("VOX_SECS", 30);
    let bind_port: u16 = env_parse("VOX_BIND", 0);
    let peer_spec = std::env::var("VOX_PEER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let capture = device::resolve(&host, Role::Capture, &cap_spec)?;
    let playback = device::resolve(&host, Role::Playback, &play_spec)?;
    if capture.is_none() && playback.is_none() {
        bail!("both --capture and --playback are 'none'; nothing to do");
    }
    if playback.is_some() && bind_port == 0 {
        bail!("set VOX_BIND to the local port the peer sends to (receiving needs a known port)");
    }
    println!("capture:  {}", describe_device(&capture));
    println!("playback: {}", describe_device(&playback));

    // Negotiate channel counts (cpal probe) for whichever roles are active.
    let capture_channels = capture
        .as_ref()
        .map(|d| device::pick_channels(d, Role::Capture))
        .transpose()?;
    let playback_channels = playback
        .as_ref()
        .map(|d| device::pick_channels(d, Role::Playback))
        .transpose()?;

    let peer = match (&capture, &peer_spec) {
        (Some(_), Some(spec)) => Some(vox_core::parse_peer(spec)?),
        (Some(_), None) => return Err(anyhow!("set VOX_PEER (host:port) to send captured audio")),
        (None, _) => None,
    };

    let (engine, ports) = Engine::start(EngineConfig {
        peer,
        bind_port,
        capture_channels,
        playback_channels,
        ring_ms,
        jitter_ms,
        bitrate,
    })?;
    println!("bound {}", engine.local_addr()?);
    if let Some(peer) = peer {
        println!("sending to {peer}");
    }

    // Wire the cpal stream callbacks to the engine's ring ports. The streams must
    // outlive the run, so keep them in scope; dropping them stops the audio.
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

    let mode = match (cap_stream.is_some(), play_stream.is_some()) {
        (true, true) => "full duplex",
        (true, false) => "send-only",
        (false, true) => "receive-only",
        (false, false) => unreachable!("guarded above: not both none"),
    };
    println!("mode: {mode}; running for {secs}s ...");
    thread::sleep(Duration::from_secs(secs));

    // Stop audio first, then the engine threads.
    drop(cap_stream);
    drop(play_stream);
    let stats = engine.stop()?;

    println!("results:");
    if capture.is_some() {
        println!("  send: {} packets sent", stats.packets_sent);
    }
    if playback.is_some() {
        println!(
            "  recv: {} packets, {} gap frames (silence), {} late/dup dropped",
            stats.packets_received, stats.gap_frames, stats.dropped_late
        );
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

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_parse<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}
