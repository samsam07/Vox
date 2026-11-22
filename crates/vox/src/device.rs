//! Desktop audio device layer (cpal): enumeration, `--capture`/`--playback` name
//! resolution, channel negotiation, and stream config. Naming is by local device
//! role, never network direction: `capture` is the local record (input) device,
//! `playback` is the local play (output) device.

use std::io::Write;

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, Device, Host, SampleRate, StreamConfig};

/// The wire/codec rate (DESIGN §4). Preferred when a device supports it (no
/// resampling); otherwise vox opens the device's native rate and resamples (M9).
pub const RATE: u32 = 48_000;

/// Local device role. Capture = input/record, playback = output/play.
#[derive(Clone, Copy)]
pub enum Role {
    Capture,
    Playback,
}

impl Role {
    /// Canonical role spelling, matching the `--capture`/`--playback` flags.
    pub fn label(self) -> &'static str {
        match self {
            Role::Capture => "capture",
            Role::Playback => "playback",
        }
    }
}

/// Print all capture (input) and playback (output) devices with indices, names,
/// native config, and a `[default]` marker. Writes to stdout, tolerating a closed
/// pipe (`vox --list-devices | head`).
pub fn list_devices(host: &Host) -> Result<()> {
    let cap_default = host.default_input_device().and_then(|d| d.name().ok());
    line("capture (input) devices:");
    print_devices(host.input_devices()?, Role::Capture, cap_default.as_deref());

    let pb_default = host.default_output_device().and_then(|d| d.name().ok());
    line("playback (output) devices:");
    print_devices(
        host.output_devices()?,
        Role::Playback,
        pb_default.as_deref(),
    );
    Ok(())
}

fn print_devices(devices: impl Iterator<Item = Device>, role: Role, default_name: Option<&str>) {
    let mut any = false;
    for (index, device) in devices.enumerate() {
        any = true;
        let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let marker = if Some(name.as_str()) == default_name {
            "  [default]"
        } else {
            ""
        };
        line(&format!(
            "  [{index}] {name}  {}{marker}",
            capability(&device, role)
        ));
    }
    if !any {
        line("  (none)");
    }
}

fn capability(device: &Device, role: Role) -> String {
    let config = match role {
        Role::Capture => device.default_input_config(),
        Role::Playback => device.default_output_config(),
    };
    match config {
        Ok(c) => format!(
            "[{} Hz, {} ch, {:?}]",
            c.sample_rate().0,
            c.channels(),
            c.sample_format()
        ),
        Err(_) => "[config unavailable]".to_string(),
    }
}

/// Write one line to stdout, ignoring a closed pipe (no panic).
fn line(s: &str) {
    let _ = writeln!(std::io::stdout(), "{s}");
}

/// Resolve a device spec to a concrete device. Spec strings mirror DESIGN §6:
/// `none` disables the role (`Ok(None)`), `default` selects the host default,
/// anything else is an exact device-name match.
pub fn resolve(host: &Host, role: Role, spec: &str) -> Result<Option<Device>> {
    let spec = spec.trim();
    if spec == "none" {
        return Ok(None);
    }
    let device = if spec == "default" {
        default_device(host, role)
            .ok_or_else(|| anyhow!("no default {} device available", role.label()))?
    } else {
        find_by_name(host, role, spec)?
    };
    Ok(Some(device))
}

fn default_device(host: &Host, role: Role) -> Option<Device> {
    match role {
        Role::Capture => host.default_input_device(),
        Role::Playback => host.default_output_device(),
    }
}

fn find_by_name(host: &Host, role: Role, name: &str) -> Result<Device> {
    let mut devices: Box<dyn Iterator<Item = Device>> = match role {
        Role::Capture => Box::new(host.input_devices()?),
        Role::Playback => Box::new(host.output_devices()?),
    };
    devices
        .find(|device| device.name().map(|n| n == name).unwrap_or(false))
        .ok_or_else(|| anyhow!("no {} device named {:?}", role.label(), name))
}

/// A cpal stream config at the given channel count and sample rate.
pub fn stream_config(channels: u16, rate: u32) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: SampleRate(rate),
        buffer_size: BufferSize::Default,
    }
}

/// Choose the device's operating stream config, `(rate, channels)`:
/// - rate: an explicit `forced` rate (must be supported); else 48 kHz when the device
///   offers it (the no-resample fast path); else the device's native default rate
///   (vox resamples to/from 48 kHz — M9).
/// - channels: an explicit `forced` count; else mono if the device opens it at that
///   rate, else stereo (DESIGN §4).
///
/// Each candidate config is probed once, in preference order — a mono 48 kHz device
/// costs a single probe stream (probing is a real stream open, slow on ALSA).
pub fn pick_stream(
    device: &Device,
    role: Role,
    forced_rate: Option<u32>,
    forced_channels: Option<u16>,
) -> Result<(u32, u16)> {
    let candidates = forced_channels.map_or_else(|| vec![1u16, 2], |c| vec![c]);
    let probe = |rate: u32| {
        candidates
            .iter()
            .copied()
            .find(|&ch| can_build(device, role, ch, rate))
            .map(|ch| (rate, ch))
    };
    let channels_label = match forced_channels {
        Some(c) => format!("{c} ch"),
        None => "mono or stereo".to_string(),
    };

    if let Some(rate) = forced_rate {
        return probe(rate).ok_or_else(|| {
            anyhow!(
                "{} device does not support {} Hz ({})",
                role.label(),
                rate,
                channels_label
            )
        });
    }
    if let Some(found) = probe(RATE) {
        return Ok(found);
    }
    let native = match role {
        Role::Capture => device.default_input_config(),
        Role::Playback => device.default_output_config(),
    }
    .map_err(|e| anyhow!("query default {} config: {e}", role.label()))?
    .sample_rate()
    .0;
    if native != RATE {
        if let Some(found) = probe(native) {
            return Ok(found);
        }
    }
    bail!(
        "{} device offers no {} Hz f32 {} config",
        role.label(),
        native,
        channels_label
    )
}

fn can_build(device: &Device, role: Role, channels: u16, rate: u32) -> bool {
    // Probe by building a throwaway stream — WASAPI under-reports supported configs.
    let config = stream_config(channels, rate);
    match role {
        Role::Capture => device
            .build_input_stream::<f32, _, _>(
                &config,
                |_: &[f32], _: &cpal::InputCallbackInfo| {},
                |_| {},
                None,
            )
            .is_ok(),
        Role::Playback => device
            .build_output_stream::<f32, _, _>(
                &config,
                |_: &mut [f32], _: &cpal::OutputCallbackInfo| {},
                |_| {},
                None,
            )
            .is_ok(),
    }
}
