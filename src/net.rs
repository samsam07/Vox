//! UDP socket helpers. One bound socket per instance, shared (via `Arc`) by the
//! send and receive threads — std UDP, no RTP/RTCP (DESIGN §5, §8).

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Bind a UDP socket on all interfaces at `port` (0 lets the OS pick). A read
/// timeout lets the receive thread wake periodically to observe its stop flag.
pub fn bind(port: u16) -> Result<Arc<UdpSocket>> {
    let socket =
        UdpSocket::bind(("0.0.0.0", port)).with_context(|| format!("bind UDP port {port}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .context("set UDP read timeout")?;
    Ok(Arc::new(socket))
}

/// Resolve a `host:port` peer spec to a single socket address.
pub fn parse_peer(spec: &str) -> Result<SocketAddr> {
    spec.to_socket_addrs()
        .with_context(|| format!("resolve peer {spec:?}"))?
        .next()
        .ok_or_else(|| anyhow!("peer {spec:?} resolved to no address"))
}
