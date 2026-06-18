//! vox — M4 one-way over UDP.
//!
//! Symmetric instance: if a capture device is selected it captures → encodes →
//! sends to the peer; if a playback device is selected it receives → decodes →
//! plays. One direction is achieved by setting the other role to `none` (DESIGN
//! §1, §2). The locked CLI (DESIGN §6) lands at M6; until then inputs come from
//! environment variables.

mod audio;
mod device;
mod net;
mod packet;
mod receive;
mod send;

use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use device::Role;

fn main() -> Result<()> {
    let host = cpal::default_host();
    println!("cpal host: {}", host.id().name());
    device::list_devices(&host)?;
    println!();

    // Temporary M4 inputs (replaced by the locked CLI at M6). VOX_CAPTURE /
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

    let socket = net::bind(bind_port)?;
    println!("bound {}", socket.local_addr()?);

    let sender = match &capture {
        Some(device) => {
            let spec = peer_spec
                .as_deref()
                .ok_or_else(|| anyhow!("set VOX_PEER (host:port) to send captured audio"))?;
            let peer = net::parse_peer(spec)?;
            println!("sending to {peer}");
            Some(send::start(Arc::clone(&socket), peer, device, ring_ms, bitrate)?)
        }
        None => None,
    };
    let receiver = match &playback {
        Some(device) => Some(receive::start(Arc::clone(&socket), device, jitter_ms)?),
        None => None,
    };

    println!("running for {secs}s ...");
    thread::sleep(Duration::from_secs(secs));

    println!("results:");
    if let Some(sender) = sender {
        sender.stop_and_join()?;
    }
    if let Some(receiver) = receiver {
        receiver.stop_and_join()?;
    }
    Ok(())
}

/// Read an env var, trimmed; fall back to `default` if unset or blank. Trimming
/// guards against the cmd.exe `set X=Y && ...` trailing-space footgun.
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
