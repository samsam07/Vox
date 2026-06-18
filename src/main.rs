//! vox — M2 loopback: hear yourself.
//!
//! Routes raw PCM from a capture device through an SPSC ring to a playback device
//! on one machine — no network, no codec (DESIGN §2, §3). The locked CLI (DESIGN
//! §6) lands at M6; until then inputs come from environment variables, defaulting
//! to the host default devices.

mod device;
mod loopback;

use std::str::FromStr;

use anyhow::{anyhow, Result};

use device::Role;

fn main() -> Result<()> {
    let host = cpal::default_host();
    println!("cpal host: {}", host.id().name());
    device::list_devices(&host)?;
    println!();

    // Temporary M2 inputs (replaced by the locked CLI at M6). VOX_CAPTURE /
    // VOX_PLAYBACK accept the same `none|default|name` specs as the future flags.
    let cap_spec = env_or("VOX_CAPTURE", "default");
    let play_spec = env_or("VOX_PLAYBACK", "default");
    let ring_ms: u32 = env_parse("VOX_RING_MS", 50);
    let secs: u64 = env_parse("VOX_SECS", 30);

    let capture = device::resolve(&host, Role::Capture, &cap_spec)?
        .ok_or_else(|| anyhow!("capture is 'none'; loopback needs a capture device"))?;
    let playback = device::resolve(&host, Role::Playback, &play_spec)?
        .ok_or_else(|| anyhow!("playback is 'none'; loopback needs a playback device"))?;

    loopback::run(&capture, &playback, loopback::Params { ring_ms, secs })
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
