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
  callback. The ring is sized to `jitter-ms` (the depth **ceiling**, default 150 ms),
  but the **operating depth is adaptive** (M10): the receive thread measures network
  jitter (RFC3550, from the carried packet `timestamp`) and sizes a centre depth +
  band to it — shallow (low latency) on a clean link, deeper (more slack) on a bursty
  one. So `jitter-ms` is a max, not a fixed depth; vox runs below it when it can.
  - Overrun → drop. Underrun → insert silence / Opus PLC frame.
  - The recenter drop/hold (M8) is the band's edge enforcement: drop one in-order
    frame above the adaptive high watermark, repeat one below the low (loss
    concealment is never dropped). With the band sized to the jitter, normal swings
    free-run inside it without a (cutoff-causing) correction; only slow clock drift
    occasionally reaches an edge. Long-term drift is otherwise **not** smoothly
    corrected — `[PHASE-3]` M13d (shelved resampling) is the seamless fix; the coarse
    20 ms drop/hold is the accepted stopgap for the rare drift event.

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
    (increments by 960). Carried for Phase-2 clock-drift/playout work; the MVP
    receiver does not consume it.
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
`playback_channels`, `bitrate`, `fec`, `expected_loss`, `dtx`, `jitter_ms`.
Precedence: flag > TOML > default. See `samples/vox.toml`.

Defaults: `bind` 9680 (when receiving), `capture`/`playback` `default`,
`*_sample_rate` auto (prefer 48 kHz when the device supports it, else its native
rate + resample — M9), `*_channels` auto-negotiated (mono if the device supports it,
else stereo), `bitrate` 24000,
`fec` false, `expected_loss` 10, `dtx` false, `jitter_ms` 150 (a ceiling; vox adapts
below it). `fec` /
`expected_loss` / `dtx` take effect on the encoder as of M7; `expected_loss` only
applies when `fec` is on.

## 8. Build (locked)

- Cargo only. cpal is pure-Rust (Linux needs `alsa-lib-devel`). Opus needs `cmake`
  + a C compiler for bundled libopus.
- Strategy A-then-B:
  - Phase 1: native build per target. Currently Windows→Windows (MSVC toolchain).
    Fedora native build comes at M11.
  - `[PHASE-2/3]` cross-compile from Fedora via `cargo-zigbuild`, Linux→Windows
    only (Windows→Linux is the hard direction, not pursued).
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
unchanged (callbacks do only the non-blocking sink-push / source-pop).

`[PHASE-3]` an Android front-end is a second `vox-core` consumer: Oboe/AAudio (via
JNI/uniffi) feeds the same sink/source, libopus builds via the NDK. Not pursued in
Phase 1 — the split only keeps it possible without re-architecting later.
