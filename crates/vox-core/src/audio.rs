//! Engine audio constants (DESIGN §4). Device/cpal concerns live in the binary.

/// MVP sample rate: 48 kHz only.
pub(crate) const RATE: u32 = 48_000;
/// Samples in one 20 ms mono frame at 48 kHz.
pub(crate) const FRAME: usize = 960;
/// Generous upper bound on one encoded 20 ms Opus packet, in bytes.
pub(crate) const MAX_PACKET: usize = 4000;
/// Capture ring depth (capture callback → send thread). Internal, not user-tunable
/// — only the jitter buffer (`jitter_ms`) is exposed (DESIGN §6).
pub(crate) const CAPTURE_RING_MS: u32 = 50;

/// Ring capacity in samples for `ms` of audio at `channels` (floored at one frame).
pub(crate) fn ring_capacity(ms: u32, channels: u16) -> usize {
    ((RATE as usize * ms as usize / 1000) * channels as usize).max(channels as usize * FRAME)
}
