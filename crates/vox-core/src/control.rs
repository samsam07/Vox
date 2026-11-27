//! Live settings (DESIGN §12): a handle the front-end uses to change `bitrate`,
//! `fec`, `expected_loss`, `dtx` (send path) and `drift_correct` (receive path)
//! while a call runs. Each change travels over one `std::sync::mpsc` channel per
//! thread; the owning thread drains it between frames and applies it itself, so the
//! handle never touches codec state and the audio callbacks are untouched.
//! `jitter_ms` is not here: the jitter ring is allocated to it at start (§3).
//! Changes are session-only; nothing is written back to the config.

use std::sync::mpsc::{Receiver, Sender};

use anyhow::{bail, Result};

/// One live setting change. Names mirror the config keys (DESIGN §7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Setting {
    /// Opus bitrate, bits/s (send path).
    Bitrate(i32),
    /// In-band FEC on/off (send path).
    Fec(bool),
    /// Expected loss %, tuning FEC; only takes effect while `fec` is on (send path).
    ExpectedLoss(u8),
    /// Discontinuous transmission on/off (send path).
    Dtx(bool),
    /// Smooth clock-drift correction on/off (receive path).
    DriftCorrect(bool),
}

impl Setting {
    /// Whether this setting belongs to the send path (else the receive path).
    pub fn is_send_path(self) -> bool {
        !matches!(self, Setting::DriftCorrect(_))
    }
}

/// Send-thread end: encoder settings.
pub(crate) type EncoderControls = Receiver<Setting>;
/// Receive-thread end: receive-path settings.
pub(crate) type ReceiveControls = Receiver<Setting>;

/// A thread's starting settings plus the channel its live changes arrive on.
pub(crate) struct Tunable<T> {
    pub(crate) start: T,
    pub(crate) controls: Receiver<Setting>,
}

/// Changes settings on a running [`crate::Engine`]. Cheap to clone; holds only the
/// channel senders for the roles that are running.
#[derive(Clone)]
pub struct EngineControl {
    pub(crate) send: Option<Sender<Setting>>,
    pub(crate) receive: Option<Sender<Setting>>,
}

impl EngineControl {
    /// Whether a send path is running (its settings can be changed).
    pub fn sends(&self) -> bool {
        self.send.is_some()
    }

    /// Whether a receive path is running (its settings can be changed).
    pub fn receives(&self) -> bool {
        self.receive.is_some()
    }

    /// Queue `setting` for the thread that owns it. It is applied before that
    /// thread's next frame. Errors if that role isn't running or its thread has
    /// stopped; the setting is then unchanged.
    pub fn apply(&self, setting: Setting) -> Result<()> {
        let (channel, role) = if setting.is_send_path() {
            (&self.send, "send")
        } else {
            (&self.receive, "receive")
        };
        let Some(channel) = channel else {
            bail!("no {role} path is running");
        };
        if channel.send(setting).is_err() {
            bail!("the {role} path has stopped");
        }
        Ok(())
    }
}
