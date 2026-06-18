//! vox UDP wire format (DESIGN §5): an 8-byte big-endian header followed by one
//! Opus payload. Header is `seq` (u32) then `timestamp` (u32, sample count). The
//! timestamp is carried for Phase-2 clock-drift/playout work; the MVP receiver
//! reads only `seq`.

/// Size of the fixed header in bytes.
pub const HEADER_LEN: usize = 8;

/// A parsed datagram: header fields plus a borrowed view of the payload.
pub struct Packet<'a> {
    pub seq: u32,
    // Parsed from the wire but not consumed by the MVP receiver; Phase-2 clock-drift
    // / playout scheduling will read it (DESIGN §5).
    #[allow(dead_code)]
    pub timestamp: u32,
    pub payload: &'a [u8],
}

/// Write the 8-byte header into the front of `out`. The payload is expected to
/// already occupy `out[HEADER_LEN..]`; the caller sends `HEADER_LEN + payload_len`.
pub fn write_header(seq: u32, timestamp: u32, out: &mut [u8]) {
    out[0..4].copy_from_slice(&seq.to_be_bytes());
    out[4..8].copy_from_slice(&timestamp.to_be_bytes());
}

/// Parse a received datagram. Returns `None` if it is too short to hold a header.
pub fn parse(buf: &[u8]) -> Option<Packet<'_>> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    let seq = u32::from_be_bytes(buf[0..4].try_into().ok()?);
    let timestamp = u32::from_be_bytes(buf[4..8].try_into().ok()?);
    Some(Packet {
        seq,
        timestamp,
        payload: &buf[HEADER_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trips() {
        let mut buf = vec![0u8; HEADER_LEN + 3];
        write_header(0x0102_0304, 0x0A0B_0C0D, &mut buf);
        buf[HEADER_LEN..].copy_from_slice(&[1, 2, 3]);

        let pkt = parse(&buf).expect("parses");
        assert_eq!(pkt.seq, 0x0102_0304);
        assert_eq!(pkt.timestamp, 0x0A0B_0C0D);
        assert_eq!(pkt.payload, &[1, 2, 3]);
    }

    #[test]
    fn rejects_short_datagram() {
        assert!(parse(&[0, 1, 2]).is_none());
    }
}
