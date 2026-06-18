//! vox-core — the platform-agnostic vox engine (DESIGN §11): Opus codec, UDP
//! transport, packet format, jitter buffer, and the send/receive threads. No audio
//! device dependency (no cpal).
//!
//! A platform audio layer (cpal on desktop, Oboe/AAudio on Android) drives the
//! engine through the [`AudioPorts`] ring seam returned by [`Engine::start`]: its
//! record callback pushes captured interleaved PCM into the [`CaptureSink`], and
//! its play callback pulls PCM from the [`PlaybackSource`]. Both ops are
//! non-blocking, so the sacred-callback rule (DESIGN §2) is preserved. Downmix to
//! mono / upmix happen inside the engine; the platform only supplies channel counts.

mod audio;
mod engine;
mod net;
pub mod packet;
mod receive;
mod send;

pub use engine::{AudioPorts, CaptureSink, Engine, EngineConfig, EngineStats, PlaybackSource};
pub use net::parse_peer;
