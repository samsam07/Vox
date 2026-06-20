//! Engine audio constants (DESIGN §4). Device/cpal concerns live in the binary.

/// The wire/codec sample rate: always 48 kHz mono (DESIGN §4). Device streams may
/// run at other rates and are resampled to/from this at the edge (`crate::resample`).
pub(crate) const RATE: u32 = 48_000;
/// Samples in one 20 ms mono frame at 48 kHz.
pub(crate) const FRAME: usize = 960;
/// Generous upper bound on one encoded 20 ms Opus packet, in bytes.
pub(crate) const MAX_PACKET: usize = 4000;
/// Capture ring depth (capture callback → send thread). Internal, not user-tunable
/// — only the jitter buffer (`jitter_ms`) is exposed (DESIGN §6).
pub(crate) const CAPTURE_RING_MS: u32 = 50;

/// Ring capacity in samples for `ms` of audio at `rate`/`channels` (floored at one
/// 48 kHz frame). `rate` is the device rate the ring carries, not necessarily 48 kHz.
pub(crate) fn ring_capacity(rate: u32, ms: u32, channels: u16) -> usize {
    ((rate as usize * ms as usize / 1000) * channels as usize).max(channels as usize * FRAME)
}
