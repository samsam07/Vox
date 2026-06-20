//! Process timer-resolution control (jitter reduction).
//!
//! Windows' default scheduler tick is ~15.6 ms, so thread sleeps/wakeups snap to it
//! — our send thread's short sleep when the capture ring is momentarily empty
//! becomes ~15 ms, shipping frames in bursts (→ arrival jitter on the peer). Asking
//! for 1 ms resolution for the session smooths the pacing. Modern Windows scopes this
//! per-process; we pair it with `timeEndPeriod` on drop (good citizenship).
//!
//! Other platforms already honor sub-millisecond sleeps against a high-resolution
//! timer, so there is nothing to do — this is a no-op there.

/// RAII guard: raises the process timer resolution while held, restoring it on drop.
pub struct TimerResolution {
    #[cfg(windows)]
    active: bool,
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period_ms: u32) -> u32;
    fn timeEndPeriod(period_ms: u32) -> u32;
}

/// Finest period we request, in milliseconds.
#[cfg(windows)]
const PERIOD_MS: u32 = 1;
/// `TIMERR_NOERROR` — `timeBeginPeriod` success.
#[cfg(windows)]
const TIMERR_NOERROR: u32 = 0;

impl TimerResolution {
    /// Request the finest timer resolution for the process (1 ms on Windows; a no-op
    /// elsewhere). A failure is non-fatal — pacing just stays at the coarse default.
    pub fn highest() -> Self {
        #[cfg(windows)]
        {
            // SAFETY: `timeBeginPeriod` is a stable winmm call taking a period in ms;
            // it is paired with `timeEndPeriod` in `Drop`.
            let active = unsafe { timeBeginPeriod(PERIOD_MS) } == TIMERR_NOERROR;
            if active {
                log::debug!("raised timer resolution to {PERIOD_MS} ms");
            } else {
                log::debug!("could not raise timer resolution; using OS default");
            }
            TimerResolution { active }
        }
        #[cfg(not(windows))]
        TimerResolution {}
    }
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: pairs the successful `timeBeginPeriod(PERIOD_MS)` in `highest`.
            unsafe { timeEndPeriod(PERIOD_MS) };
        }
    }
}
