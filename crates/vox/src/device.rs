//! Desktop audio device layer (cpal): enumeration, `--capture`/`--playback` name
//! resolution, channel negotiation, and stream config. Naming is by local device
//! role, never network direction: `capture` is the local record (input) device,
//! `playback` is the local play (output) device.

use anyhow::{anyhow, bail, Result};
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, Device, Host, SampleRate, StreamConfig};

/// MVP sample rate (DESIGN §4: 48 kHz only).
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

/// Print all capture (input) and playback (output) devices with indices + names,
/// marking each host default.
pub fn list_devices(host: &Host) -> Result<()> {
    let cap_default = host.default_input_device().and_then(|d| d.name().ok());
    println!("capture (input) devices:");
    print_devices(host.input_devices()?, cap_default.as_deref());

    let pb_default = host.default_output_device().and_then(|d| d.name().ok());
    println!("playback (output) devices:");
    print_devices(host.output_devices()?, pb_default.as_deref());
    Ok(())
}

fn print_devices(devices: impl Iterator<Item = Device>, default_name: Option<&str>) {
    let mut any = false;
    for (index, device) in devices.enumerate() {
        any = true;
        let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let marker = if Some(name.as_str()) == default_name {
            "  [default]"
        } else {
            ""
        };
        println!("  [{index}] {name}{marker}");
    }
    if !any {
        println!("  (none)");
    }
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

/// A 48 kHz cpal stream config at the given channel count.
pub fn stream_config(channels: u16) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: SampleRate(RATE),
        buffer_size: BufferSize::Default,
    }
}

/// Pick mono if the device can open it at 48 kHz / f32, else stereo (DESIGN §4).
pub fn pick_channels(device: &Device, role: Role) -> Result<u16> {
    for channels in [1u16, 2] {
        if can_build(device, role, channels) {
            return Ok(channels);
        }
    }
    bail!(
        "{} device offers no 48 kHz f32 mono or stereo config",
        role.label()
    )
}

fn can_build(device: &Device, role: Role, channels: u16) -> bool {
    // Probe by building a throwaway stream — WASAPI under-reports supported configs.
    let config = stream_config(channels);
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
