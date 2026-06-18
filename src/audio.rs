//! Shared audio constants and helpers for the send and receive paths.

use anyhow::{bail, Result};
use cpal::traits::DeviceTrait;
use cpal::{BufferSize, Device, SampleRate, StreamConfig};

use crate::device::Role;

/// MVP sample rate (DESIGN §4: 48 kHz only).
pub const RATE: u32 = 48_000;
/// Samples in one 20 ms mono frame at 48 kHz (DESIGN §4).
pub const FRAME: usize = 960;
/// Generous upper bound on one encoded 20 ms Opus packet, in bytes.
pub const MAX_PACKET: usize = 4000;

/// A 48 kHz cpal stream config at the given channel count.
pub fn stream_config(channels: u16) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: SampleRate(RATE),
        buffer_size: BufferSize::Default,
    }
}

/// Ring capacity in samples for `ms` of audio at `channels` (floored at one frame).
pub fn ring_capacity(ms: u32, channels: u16) -> usize {
    ((RATE as usize * ms as usize / 1000) * channels as usize).max(channels as usize * FRAME)
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
