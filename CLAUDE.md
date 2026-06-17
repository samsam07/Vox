# CLAUDE.md — vox coding contract

vox is a free, headless, bidirectional real-time voice pipe over LAN UDP. Two
symmetric instances, one per machine. Each captures from a local device, encodes,
sends UDP; receives UDP, decodes, plays to a local device. Original purpose: the
missing mic backchannel in an Apollo/Moonlight remote-desktop setup. Reusable for
anything.

This file is the contract. It is deliberately short so it survives a long session.
The stable "what and why" lives in `docs/DESIGN.md`; the per-slice plan and volatile
detail live in `docs/PLAN.md`; Rust style lives in `docs/CONVENTIONS.md`. Read the
relevant one when you start a slice — do not work from memory of it.

## Stack (locked)

- Language: Rust (edition 2021+).
- Audio I/O: `cpal` (maintained, callback-complete, pure-Rust). PortAudio was
  evaluated and dropped: its Rust binding is abandoned and callback-incomplete.
- Codec: `opus` crate (libopus binding via `opusic-sys`). Needs `cmake` + a C
  compiler to build bundled libopus.
- Transport: std UDP sockets. No RTP/RTCP.
- Build: Cargo. No CMake/Meson of our own.
- CLI: `clap`. Config: `toml` + `serde`.
- Ring buffer: `ringbuf` or `rtrb` (SPSC lock-free).

## Judgment vs work boundary

DECISIONS ALREADY MADE — do not relitigate, do not silently change:
the entire contents of `docs/DESIGN.md` (thread model, buffers, jitter, FEC,
packet format, CLI grammar, TOML schema) and the Stack list above.

IMPLEMENTATION LATITUDE — your call, within `docs/CONVENTIONS.md`:
internal helper structure, variable/function names, module file layout, test
layout, error-message wording.

NEITHER LIST — ask, do not assume. If a task needs a decision that is not in
DESIGN.md and is not pure implementation detail, stop and ask.

## Per-slice verification ritual (every milestone)

1. RESTATE: before coding, write the slice's goal and the invariants it must not
   break, in your own words. Name which DESIGN.md sections apply.
2. IMPLEMENT.
3. SELF-CHECK: verify against the slice's acceptance criteria in PLAN.md. Then run
   the anti-drift grep (below).
4. Hand to human for taste review. Do not start the next slice first.

## Anti-drift rule (hard)

Before using any name or value that was specified earlier (device-role flag names,
TOML keys, packet field names, default port, frame size, etc.), grep the repo +
DESIGN.md for the canonical spelling and use it verbatim. Never invent a synonym
for an already-named thing. (Prior project lost time to `state.json` being
silently substituted for a specified name — do not repeat that class of bug.)

## Hard invariants (violating these is a defect, not a style choice)

- The audio callbacks are SACRED. Inside a cpal capture/playback callback: no
  allocation, no I/O, no syscalls, no contended locks, no encode/decode. Only a
  non-blocking ring-buffer push/pop.
- Buffers are SPSC: exactly one writer thread, one reader thread, each. Do not
  share a buffer three ways. Do not reach for `Arc<Mutex<>>` to dodge the borrow
  checker here — if it fights you, the design intent is a single-owner handoff.
- One Opus encoder owned by the send thread; one decoder owned by the receive
  thread. Never shared across threads (Opus state is not thread-safe).
- Capture and playback are ALWAYS separate cpal streams on separate devices.
  Never open one device for both. Never assume full-duplex on one device.
- Virtual-device rule (Windows): one VB-Cable device is output-only, another is
  mic-only. Never open the same virtual device twice.
- MVP is 48 kHz only; the codec and UDP wire are always mono. A device stream may
  be opened stereo when the device offers no mono config (e.g., VB-Cable), with vox
  downmixing to mono before encode / upmixing after decode — the pipeline stays
  mono (see DESIGN §4). Arbitrary sample rates and native stereo on the wire remain
  out of scope for Phase 1.
- Device-role flag naming only (`--capture`/`--playback`). Never name a flag by
  network direction (no `--incoming`/`--outgoing`/`--send`/`--receive`).

## Tag conventions

- `[CRYSTALLIZE]` — atomic detail intentionally deferred; fill in when the owning
  slice begins, not before.
- `[PHASE-2]` / `[PHASE-3]` — out of MVP scope; do not implement in Phase 1.
- `[VERIFY]` — assumption to confirm empirically during its slice.
