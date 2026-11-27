# DESIGN.md — vox

Stable design. The "what and why." Changes here are decisions, not edits — treat
this as the source of truth. Volatile atomic detail (exact ranges, error strings)
is tagged `[CRYSTALLIZE]` and lives in PLAN.md per slice.

## 1. Model

Two symmetric instances, one per machine. There is no host and no client — plain
UDP has no connection. Each instance:

- captures from one local device → encodes → sends UDP to the peer
- receives UDP from the peer → decodes → plays to one local device

Both directions run at once, single invocation per machine. The only things that
differ between the two machines are the device names, the peer address, and tuning.

Reference topology (Apollo/Moonlight mic backchannel):
- Windows host: capture from VB-Cable A (desktop audio loopback), play received
  mic to VB-Cable B (apps see B as a microphone).
- Linux/Windows client: capture from real mic, play received audio to headphones.

## 2. Threads (locked)

Four threads per instance:

1. Capture callback (cpal-owned). Pushes raw PCM into the capture ring. Sacred.
2. Playback callback (cpal-owned). Pulls PCM from the jitter buffer. Sacred.
3. Send thread (ours). Drains capture ring → Opus-encode → UDP send.
4. Receive thread (ours). UDP recv → Opus-decode → push jitter buffer.

Why four, not two: capture and playback are always *different devices* with
*independent clocks*, so they must be two separate cpal streams (full-duplex
wants one device — impossible here). Encode/decode have variable timing and must
not run on the sacred callbacks, so they live on threads 3 and 4.

## 3. Buffers (locked)

- Capture ring: SPSC lock-free (`ringbuf`/`rtrb`). Writer = capture callback;
  reader = send thread.
- Jitter buffer (receive side): writer = receive thread; reader = playback
  callback. The ring is sized to `jitter-ms` (the depth **ceiling**, default 250 ms),
  but the **operating depth is adaptive** (M10): the receive thread measures network
  jitter (RFC3550, from the carried packet `timestamp`) and sizes a centre depth +
  band to it — shallow (low latency) on a clean link, deeper (more slack) on a bursty
  one. So `jitter-ms` is a max, not a fixed depth; vox runs below it when it can.
  - The ceiling defaults to 250 ms (was 150). The band's high watermark is clamped
    to the ceiling minus a frame, and its natural size is ~85 ms + 6 × jitter (+ a
    playback period). At 150 the clamp bit from ~3–6 ms of jitter, so busy-network
    bursts hit the pinned high edge and recenter-dropped (audible breaks); 250 lets
    the band follow jitter to ~20 ms. Clean-link latency is unchanged — the
    operating depth comes from measured jitter, not the ceiling.
  - Overrun → drop. Underrun → insert silence / Opus PLC frame.
  - Underrun handling (M10c): the playback callback fades out (~2 ms, no click),
    goes silent, and **rebuffers** — stays silent until the ring refills to the
    adaptive band centre — before fading back in. A break so restores full jitter
    slack instead of resuming at the floor and breaking again on the next hiccup.
    Playback also starts this way (silent until the first fill reaches the target),
    which replaces the startup prefill. Recenter inserts are suppressed while
    rebuffering (a repeated frame would replay as a stutter).
  - The band's low watermark (the underrun cushion) is one frame **plus one
    playback-callback period**, and the whole band sits that period higher: the
    callback drains the ring a period at a time (~25 ms on ALSA by default, ~10 ms
    on WASAPI), so a bare one-frame floor could be emptied by a single callback.
    The callback publishes its observed period for the receive thread to use.
  - Underruns (playback) and capture-ring overruns (mic audio dropped because the
    send thread fell behind) are counted and shown in the plain/TUI output. The
    capture ring is 200 ms deep — headroom, not latency (the send thread drains it
    as fast as it fills).
  - The recenter drop/hold (M8) is the band's edge enforcement: drop one in-order
    frame above the adaptive high watermark, repeat one below the low (loss
    concealment is never dropped). With the band sized to the jitter, normal swings
    free-run inside it without a (cutoff-causing) correction; only slow clock drift
    occasionally reaches an edge.
  - Long-term drift: by default the coarse recenter drop/hold is the (rare) stopgap.
    Opt into `--drift-correct` (M10b) for the smooth fix: the receive path runs a
    dynamic-ratio resampler trimmed by a proportional controller to hold occupancy at
    the band centre — no 20 ms cutoffs, and the buffer sits at the target instead of
    drifting to a rail (so latency stays near the target). It's off by default because
    it puts a resampler on the common path even at 48 kHz; enable it if the TUI shows
    sustained drift.

Single-owner discipline is enforced by the borrow checker. Do not defeat it with
shared mutability.

## 4. Codec (locked)

- Opus via the `opus` crate (libopus). 48 kHz internal.
- Frame size 20 ms (standard voice).
- Mono, 1 channel end-to-end: the Opus codec and the UDP wire format are always
  mono. Preferred path is to request a mono stream from cpal directly (works on
  real hardware). Where a device offers no mono config — notably VB-Cable, which on
  WASAPI shared mode exposes only 48 kHz *stereo* — vox opens the stream at the
  device's native channel count and does its own deterministic mix: downmix to mono
  before encode, upmix mono→stereo after decode. The mix stays under our control
  (not the OS mixer — the original intent), and the wire/codec stay uniformly mono,
  so the connectionless symmetric peers (§1) need no channel-count negotiation and
  the packet header (§5) carries no channel field.
  - Revisit (post-MVP): native stereo end-to-end (no mix) is deferred — it would
    require a channel field in the header or a handshake the connectionless model
    lacks. Decision established empirically at M1 (VB-Cable is stereo-only here).
- FEC is in-band, available, but **off by default** (opt-in via `--fec`). M7 wired
  the encode/decode path: when on, `fec`/`expected_loss`/`dtx` drive the encoder and
  the receiver reconstructs loss (FEC + PLC, below). The earlier default-on was
  reverted after machine-to-machine testing: in-band FEC carries a redundant copy of
  the previous frame, which both **steals bitrate from the primary signal** (an
  audible quality/wobble cost at 24 kbps) and **enlarges packets** (more arrival
  jitter → more recentering). That only pays off when real loss is high; on a clean
  LAN or 5 GHz WiFi it is a net loss, so the default is off and you enable it for
  genuinely lossy links. `[PHASE-3]` make FEC adaptive (auto-enable on measured
  loss). DTX shrinks silent frames but vox still transmits every frame so the
  sequence stays contiguous (a receiver gap always means real loss).
- FEC ↔ jitter buffer are coupled: FEC reconstructs a lost frame N from a
  redundant copy carried in frame N+1, which only works if the jitter buffer held
  N+1 long enough. The buffer (≥ one frame deep, ~100 ms by default) provides that
  look-ahead.
- The wire/codec is always 48 kHz. A device that doesn't run at 48 kHz is opened at
  its native rate and resampled at the edge (M9, `rubato`): capture rate → 48 kHz
  before encode, 48 kHz → playback rate after decode. A 48 kHz device is a
  passthrough (no resampler). `[PHASE-2]` M10 reuses this seam for drift correction.
- `[PHASE-2]` evaluate `opus-rs` (pure-Rust Opus) to drop the libopus C build.
  Not for MVP — too new for the latency-critical FEC path.

## 5. Packet format (locked)

UDP datagram = small header + Opus payload.
- Sequence number (2–4 bytes) — drives gap detection for jitter ordering and FEC.
- Crystallized at M4 — 8-byte big-endian (network order) header, then payload:
  - bytes 0..4: `seq` — u32, increments by 1 per 20 ms frame. Drives gap detection
    and ordering; wrap-aware comparison (never wraps in practice: ~2.7 years). A
    short forward gap is concealed and a short backward step is a late/duplicate
    drop; a large jump either way is a discontinuity that resyncs — a large backward
    one specifically means the peer restarted (seq reset), so the decoder is reset
    too (M8 reconnection robustness). Transient socket errors from a peer being down
    (e.g. ICMP→ConnectionReset on Windows) are tolerated, not fatal, on both threads.
  - bytes 4..8: `timestamp` — u32, sample count of the frame's first sample
    (increments by 960). The receiver reads it for adaptive jitter sizing (M10): the
    timestamp delta gives the expected inter-arrival spacing that arrival-time jitter
    is measured against (RFC3550), so a lost-packet gap is not mistaken for jitter.
  - bytes 8..: Opus payload (one encoded 20 ms mono frame).

## 6. CLI (locked)

```
vox <config.toml>                                  # config mode (Apollo hooks)
vox --peer <host[:port]> [--bind <port>] [flags]   # ad-hoc mode
```

Connection:
- `--peer <host[:port]>` — send target. Port omitted → VOX_DEFAULT_PORT.
- `--bind <port>` — local receive port. Omitted → VOX_DEFAULT_PORT *when receiving*
  (a playback device is set). A send-only instance (`--playback none`) needs no
  fixed bind: it sends from an OS-assigned ephemeral source port and opens no
  listener (crystallized at M6).
- No host/connect verb. Symmetric peers.
- VOX_DEFAULT_PORT = 9680 (UDP). Clear of VBAN (6980) and Moonlight/Apollo
  (~47984–48010), and below the ephemeral range.

Devices (local-role naming only):
- `--capture <none|default|"name">` — local record device. Omitted → `default`.
  `none` → receive-only.
- `--playback <none|default|"name">` — local play device. Omitted → `default`.
  `none` → send-only.
- both `none` → error.

Audio options (TOML, each overridable by an identically-named flag). Split by what
they belong to:
- Device properties (capture/playback prefixed): `--capture-sample-rate`,
  `--playback-sample-rate`, `--capture-channels`, `--playback-channels`.
- Codec / send-path (encoder only, NOT capture/playback-prefixed): `--bitrate`,
  `--fec`, `--expected-loss`, `--dtx`.
- Receive-path: `--jitter-ms`.

Rationale: bitrate/FEC/DTX are encoder settings on the send path; playback has no
bitrate (decode is parameter-free). Naming them per-device would be a category error.

Operational flags (tooling, not audio config; added at M6c): `--list-devices`,
`--print-config`, `--output <quiet|plain|tui>` (default `plain`), `-v`/`-vv`
(verbosity), `--duration <secs>` (run for N seconds then exit; default: until a
stop signal).

## 7. TOML schema (crystallized at M6)

Keys mirror flags using role naming: `peer`, `bind`, `capture`, `playback`,
`capture_sample_rate`, `playback_sample_rate`, `capture_channels`,
`playback_channels`, `bitrate`, `fec`, `expected_loss`, `dtx`, `jitter_ms`,
`drift_correct`.
Precedence: flag > TOML > default. See `samples/vox.toml`.

Defaults: `bind` 9680 (when receiving), `capture`/`playback` `default`,
`*_sample_rate` auto (prefer 48 kHz when the device supports it, else its native
rate + resample — M9), `*_channels` auto-negotiated (mono if the device supports it,
else stereo), `bitrate` 24000,
`fec` false, `expected_loss` 10, `dtx` false, `jitter_ms` 250 (a ceiling; vox adapts
below it), `drift_correct` false (M10b, §3). `fec` /
`expected_loss` / `dtx` take effect on the encoder as of M7; `expected_loss` only
applies when `fec` is on.

## 8. Build (locked)

- Cargo only. cpal is pure-Rust (Linux needs `alsa-lib-devel`).
- Opus (`opus` crate -> `audiopus_sys`):
  - Windows: libopus built from source via `cmake` + a C compiler, linked
    statically.
  - Linux: system libopus found via pkg-config (`opus-devel`), linked
    dynamically; runtime needs the `opus` package. `audiopus_sys` 0.2.2's bundled
    path is broken on Fedora (asks for a dylib it does not build; searches
    `out/lib` while cmake installs to `out/lib64`).
  - `[PHASE-2]` bundle libopus into the Linux binary (static) so no system
    Opus package is required.
- Strategy A-then-B:
  - A: native build per target — Windows→Windows (MSVC toolchain, cmake +
    MSVC C compiler), Linux→Linux (M11).
  - B (M12): cross-compile from Fedora, Linux→Windows only (Windows→Linux is the
    hard direction, not pursued), via plain `cargo` + the MinGW-w64 toolchain
    (`--target x86_64-pc-windows-gnu`, Fedora `mingw64-gcc`). `cargo-zigbuild`
    was the original plan; dropped because system MinGW needs no extra tools and
    zig can only target windows-gnu anyway, so the output is the same.
    - The cross build forces `LIBOPUS_STATIC=1 LIBOPUS_NO_PKG=1`:
      `audiopus_sys` chooses linkage by the *build host* (Linux → dynamic +
      pkg-config), not the target. Set per-invocation (`make build-windows`),
      never in `.cargo/config.toml`, so the native Linux build stays dynamic.
    - The result is a standalone `vox.exe` (libopus static; only Windows system
      DLLs). GNU ABI rather than MSVC — functionally equivalent for vox.
- macOS deferred.

## 9. Data flow (one instance)

```
 send path:     capture cb ─push→ [capture ring] ─pull→ send thread ─encode→ UDP out ⇒ peer
 receive path:  peer ⇒ UDP in ─→ receive thread ─decode→ [jitter buf] ─pull→ playback cb
```
Mirror instance on the peer runs the same two paths.

## 10. Out of scope (kept here so it is not "rediscovered")

\>2 peers; GUI; audio mixing/effects; half-duplex-by-default (duplex is the
definition; one direction is achieved via `--capture none` / `--playback none`).

Encryption was out of scope for the MVP but is now a planned Phase-3 feature
(PLAN M15) — optional payload encryption so vox is safe on untrusted networks.

## 11. Crate structure (locked at M6)

vox is a Cargo workspace, split so the engine can be reused behind other
front-ends (e.g. an Android walkie-talkie) without dragging in the desktop CLI:

- `vox-core` (library): the platform-agnostic engine — Opus codec, UDP transport,
  packet format, send/receive threads, the capture ring and the jitter buffer. It
  does NOT depend on cpal. It exposes the SPSC ring ends as the audio seam: a
  capture sink (the platform's record callback pushes device-native interleaved
  PCM in, non-blocking) and a playback source (the platform's play callback pulls
  PCM out, non-blocking). Downmix-to-mono / upmix stay inside the core (send /
  receive threads); the platform only supplies the channel count.
- `vox` (binary): the desktop platform + UI — cpal device enumeration/resolution
  and stream construction (whose sacred callbacks call the core's sink/source),
  plus the clap CLI, TOML config, and Apollo hooks.

The seam is exactly §2/§3's ring boundary made public; the sacred-callback rule is
unchanged (callbacks do only the non-blocking sink-push / source-pop, plus the
relaxed-atomic counters / playout state and the underrun fade — §3, M10c).

`[PHASE-3]` an Android front-end is a second `vox-core` consumer: Oboe/AAudio (via
JNI/uniffi) feeds the same sink/source, libopus builds via the NDK. Not pursued in
Phase 1 — the split only keeps it possible without re-architecting later.

## 12. Live settings: the TUI control channel (decided for the TUI redesign, M16)

The TUI's Options overlay changes some audio settings while a call is running, so
they can be tuned while watching loss/drift on the dashboard instead of restarting
both ends (which breaks the call). Live: `bitrate`, `fec`, `expected_loss`, `dtx`
(send path) and `drift_correct` (receive path). `jitter_ms` stays start-only — the
jitter ring is allocated to it (§3) — and the TUI shows it read-only ("needs
restart").

- **Shape.** `Engine` hands the front-end a control handle (`Engine::control()` →
  `EngineControl`; a change is a `Setting`). Changes travel over one
  `std::sync::mpsc` channel per thread: one to the send thread, one to the receive
  thread. A direction that isn't running has no channel; the handle offers only the
  settings of the roles that exist (`EngineControl::apply` refuses the others).
- **Send thread.** Drains its channel with `try_recv` once per loop pass, at the top
  of the pass, so changes land *between* frames, never mid-encode. It applies them
  with the Opus encoder ctls: `set_bitrate`, `set_inband_fec`,
  `set_packet_loss_perc`, `set_dtx`. Same rule as at start-up (§7): the packet-loss
  percentage is `expected_loss` while `fec` is on and 0 while it is off, so changing
  `expected_loss` with `fec` off only takes effect once `fec` is turned on.
- **Receive thread.** Drains its channel once per loop pass (every datagram, and at
  least every socket read timeout, so a silent peer doesn't stall it). Toggling
  `drift_correct` rebuilds its `ReceiveResampler` (`Static` ↔ `Drift`) and resets
  the drift controller (a fresh `DriftController` when on, none when off). The
  decoder, sequence state, and jitter estimate are untouched.
  - `[VERIFY]` the swap discards the old resampler's buffered remainder (up to one
    256-sample drift chunk, ~5 ms) and the new one starts with its own delay, so a
    few samples drop or shift at the switch — possibly an audible click. Check by ear
    machine-to-machine; if it is audible, a mitigation is a separate decision.
- **Errors.** A ctl or rebuild that fails is logged and the thread keeps running with
  its previous setting — a live-tuning mistake must never end the call.
- **The peer needs nothing.** Nothing goes on the wire (§5 unchanged): the peer's
  Opus decoder accepts whatever bitrate / FEC / DTX arrives, and `drift_correct` is
  purely local.
- **Session-only.** Changes are never written to the config file; a restart uses the
  configured values (§7 precedence).
- **Invariants unchanged.** The audio callbacks are untouched (§2, §11). Each codec
  and resampler is still owned and mutated by its one thread — the control handle
  only sends messages, never touches codec state (§2, §4). The SPSC rings stay
  exactly two-ended (§3); the control channels connect the front-end to our own
  threads only, never to a callback.
