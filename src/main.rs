//! vox — M1 device enumeration + dual-stream smoke test.
//!
//! Prints the cpal host and every capture/playback device, then opens two
//! separate streams on two separate devices at once and runs them for a fixed
//! window to prove the core architectural assumption (DESIGN §2). The locked CLI
//! (DESIGN §6) lands at M6; until then the smoke harness takes its inputs from
//! environment variables, defaulting to the host default devices.

mod device;
mod smoke;

use anyhow::{anyhow, Result};

use device::Role;

fn main() -> Result<()> {
    let host = cpal::default_host();
    println!("cpal host: {}", host.id().name());
    println!("libopus:   {}", opus::version());

    device::list_devices(&host)?;
    println!();

    // Temporary M1 inputs (replaced by the locked CLI at M6). VOX_CAPTURE /
    // VOX_PLAYBACK accept the same `none|default|name` specs as the future flags.
    let cap_spec = env_or("VOX_CAPTURE", "default");
    let play_spec = env_or("VOX_PLAYBACK", "default");
    let secs: u64 = std::env::var("VOX_SMOKE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let capture = device::resolve(&host, Role::Capture, &cap_spec)?
        .ok_or_else(|| anyhow!("capture is 'none'; the M1 smoke test needs a capture device"))?;
    let playback = device::resolve(&host, Role::Playback, &play_spec)?
        .ok_or_else(|| anyhow!("playback is 'none'; the M1 smoke test needs a playback device"))?;

    let passed = smoke::run(&capture, &playback, secs)?;
    if !passed {
        std::process::exit(1);
    }
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
