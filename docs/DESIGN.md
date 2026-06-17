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
  callback. Fixed ring, ~40–60 ms, CLI-tunable (`jitter-ms`).
  - Overrun → drop. Underrun → insert silence / Opus PLC frame.
  - Handles short-term network jitter. Does NOT handle long-term clock drift
    between the independent capture/playback/peer clocks — over a long session the
    buffer slowly fills or drains, causing an occasional audible blip. Accepted for
    MVP. `[PHASE-2]` drift compensation (resampling) fixes it.

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
- FEC enabled (in-band). Encoder: FEC on, expected-packet-loss ~5–10%
  (justified by WiFi on the client leg), DTX on.
- FEC ↔ jitter buffer are coupled: FEC reconstructs a lost frame N from a
  redundant copy carried in frame N+1, which only works if the jitter buffer held
  N+1 long enough. The ~40–60 ms buffer provides that look-ahead.
- MVP is 48 kHz only. Non-48k capture/playback requires a resampler — `[PHASE-2]`,
  shares its resampler with drift compensation.
- `[PHASE-2]` evaluate `opus-rs` (pure-Rust Opus) to drop the libopus C build.
  Not for MVP — too new for the latency-critical FEC path.

## 5. Packet format (locked)

UDP datagram = small header + Opus payload.
- Sequence number (2–4 bytes) — drives gap detection for jitter ordering and FEC.
- `[CRYSTALLIZE]` exact header layout (byte order, whether a timestamp field is
  included) — fix at the M4 slice.

## 6. CLI (locked)

```
vox <config.toml>                                  # config mode (Apollo hooks)
vox --peer <host[:port]> [--bind <port>] [flags]   # ad-hoc mode
```

Connection:
- `--peer <host[:port]>` — send target. Port omitted → VOX_DEFAULT_PORT.
- `--bind <port>` — local receive port. Omitted → VOX_DEFAULT_PORT.
- No host/connect verb. Symmetric peers.
- `[CRYSTALLIZE]` VOX_DEFAULT_PORT numeric value — fix at M6.

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

## 7. TOML schema (locked shape, values `[CRYSTALLIZE]` at M6)

Keys mirror flags using role naming: `peer`, `bind`, `capture`, `playback`,
`capture_sample_rate`, `playback_sample_rate`, `capture_channels`,
`playback_channels`, `bitrate`, `fec`, `expected_loss`, `dtx`, `jitter_ms`.
Flags override TOML.

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

Encryption; >2 peers; GUI; audio mixing/effects; half-duplex-by-default (duplex is
the definition; one direction is achieved via `--capture none` / `--playback none`).
