# PLAN.md — vox

Milestones are slices. Each is independently runnable and has explicit exit
criteria. Ordering is risk-first (kill the build and dual-stream unknowns before
anything depends on them) and vertical (you can hear something as early as
possible). Per-slice `[CRYSTALLIZE]` notes are the volatile detail to fill in when
that slice begins — not before.

For every slice, run the verification ritual in CLAUDE.md
(restate → implement → self-check → human review).

---

## Phase 1 — MVP (minimum to use daily) — ✅ COMPLETE

All slices M0–M6 are done: vox is a working full-duplex LAN voice pipe (Opus over
UDP, clap CLI + TOML, live TUI), verified machine-to-machine. The per-milestone
detail below is the historical record of how it was built.

### M0 — Toolchain & skeleton proof
`cargo new`; add `cpal` and `opus`; get the Opus bundled-libopus build working
(cmake + C compiler present). Binary runs, prints cpal host + opus version.
- Exit: `cargo run` succeeds and prints versions on Windows.
- Risk retired: the C build (cmake/libopus, MSVC toolchain). Likely first failure
  is a missing C toolchain, not code.
- Applies: DESIGN §8.

### M1 — Device enumeration + dual-stream smoke test  `[VERIFY]`
List devices (names + indices) — this is also `--capture`/`--playback` name
resolution. Then open two SEPARATE cpal streams on two DIFFERENT devices at once
(capture one, playback another); confirm no underruns. Windows first.
- Exit: prints device list; both streams open and run cleanly for 60 s.
- Risk retired: the core architectural assumption (separate streams, separate
  devices). If cpal can't open VB-Cable A + B cleanly here, reconsider the backend
  NOW (PortAudio-sys fallback) before building on it.
- Applies: DESIGN §2, §6 (device naming).

### M2 — Loopback: hear yourself (no net, no codec)
One machine: capture cb → capture ring → playback cb. Raw PCM only.
- Exit: you hear your own mic in the headphones, low latency.
- Validates: sacred-callback discipline + SPSC ring in isolation.
- Applies: DESIGN §2, §3.

### M3 — Opus in the loop (still no net)
Insert encode→decode into the loopback path on one machine.
- Exit: still hear yourself; proves 48 kHz / 20 ms / bitrate config is right.
- Isolates codec bugs from network bugs.
- Applies: DESIGN §4.

### M4 — One-way over UDP (two machines, half the pipe)
Add send + receive threads, wire ONE direction (client mic → host playback).
Sequence-numbered packets, fixed jitter buffer. FEC OFF for now (happy path first).
- Exit: speak on client, hear on host across LAN.
- `[CRYSTALLIZE]` packet header layout (DESIGN §5): byte order, seq width,
  timestamp y/n.
- Applies: DESIGN §3 (jitter), §5.

### M5 — Full duplex
Mirror M4 into both directions: four threads, both rings, both machines.
- Exit: real two-way conversation over LAN.
- Applies: DESIGN §2 entire.

### M6 — CLI + config + Apollo + UX
Split into the `vox-core` library + `vox` binary workspace (DESIGN §11). Wrap the
engine in the locked CLI (`vox <config.toml>` / `vox --peer …`) with TOML config and
flag-override precedence; wire Apollo connect/disconnect hooks (run-until-signal —
see docs/APOLLO.md). UX: presentation modes (`--output quiet|plain|tui`), clear
functional logging (`-v`/`-vv` for technical detail), a live ratatui dashboard
(throughput / loss / jitter / uptime), and niceties (`--help` examples, richer
`--list-devices`, `--print-config`, short flags, broken-pipe robustness). The engine
exposes live metrics (byte counters + a non-consuming stats snapshot) for the plain
status and the TUI.
- Exit: vox starts/stops with a Moonlight session from a one-liner or TOML; the
  three output modes work; verified machine-to-machine.
- `[CRYSTALLIZE]` VOX_DEFAULT_PORT = 9680; TOML default values (DESIGN §6, §7).
- **MVP COMPLETE — usable daily for its real purpose.**

---

## Phase 2 — daily-driver polish — in progress (M7–M9 ✅, M10 next)

### M7 — FEC + graceful loss — ✅ COMPLETE
Enable Opus in-band FEC + gap-detection→FEC-decode (deferred from M4/M5). Test
under simulated loss.
- Exit: audio degrades gracefully, not glitchy, under induced drop.
- Done: encoder wires `fec`/`expected_loss`/`dtx`; receiver reconstructs loss (FEC
  for the last lost frame, PLC for earlier); `fec` default flipped on. Verified
  machine-to-machine under induced loss.

### M8 — Reconnection robustness + jitter recentering — ✅ COMPLETE
One side restarts without killing the other. Also add a minimal jitter-buffer
recentering stopgap for clock drift (drop a frame when the buffer sits high, hold
one when it sits low) — no resampler, just occasional frame add/drop — to blunt the
overrun glitching until the proper M10 resampling fix.
- Done: recentering drop/hold at ¾/¼ watermarks; peer-restart resync (large
  backward seq jump + decoder reset); send/recv survive transient peer-down socket
  errors. Verified machine-to-machine (restart one side, the other recovers).

### M9 — `[PHASE-2]` resampling — ✅ COMPLETE
Non-48k devices via a resampler (`rubato`). Lifts the 48k-only constraint.
- Done: edge resampling (capture→48k, 48k→playback), passthrough at 48 kHz; rate
  auto-selection (prefer 48 kHz, else native). Verified on a non-48 kHz device.

### M10 — `[PHASE-2]` adaptive jitter — ✅ COMPLETE
Originally drift compensation (resampling) + adaptive buffer sizing. The
resampling-drift half was built (M10p1) and **shelved** (dropped; design preserved in
M10b) after machine-to-machine testing: it taxed the common 48 kHz path without
reducing real-world recenter, because **jitter, not drift, dominates** on actual
links. So M10 became **adaptive jitter sizing**: the receive thread measures arrival
jitter (RFC3550, from the carried `timestamp`) and sizes a centre depth + band to it,
gliding the recenter watermarks (fast-attack / slow-release). `jitter-ms` is now the
depth ceiling (default 150 ms; adaptive below it).
- Done: verified machine-to-machine — recenter fell from ~116/499 to ~3/8, audio
  clean. TUI shows live buffered latency + the adaptive `target`. The residual is
  slow drift (buffer creeps high over a session) → M10b is the seamless fix and would
  also drop latency from ~ceiling to ~target.

Phase-2 tuning that came out of M10 testing (smaller fixes, done ahead of M10
proper): `fec` default reverted to **off** (it costs primary-signal quality + adds
jitter; only helps on lossy links — see DESIGN §4); recenter watermarks widened to a
near-rail/adaptive backstop so they stop cutting off on normal jitter; Windows timer
resolution raised to 1 ms; TUI codec/config line.

### M10b — smooth clock-drift compensation (promoted from Phase 3)
The adaptive jitter buffer (M10) holds the band against jitter, but slow clock drift
(peer capture clock vs local playback clock) still creeps the buffer toward a rail
over a session, corrected only coarsely by the recenter drop/hold (the residual seen
in M10 testing). The smooth fix: resample the receive stream at a ratio nudged by a
control loop so the buffer holds its target with no discrete cutoffs — which also
drops latency from ~ceiling back toward the target.

Built once as M10p1 and shelved (it taxed the common 48 kHz path and didn't help
jitter, which dominates). Revival plan: do **not** make it always-on — gate it
(`--drift-correct`, default off) or auto-engage only once drift actually accumulates,
keeping the 48 kHz passthrough otherwise; pair with M10 (jitter handled separately).
Implementation that worked (reimplement from this):
- Receive-side rubato `SincFixedIn::new(out/in ratio, max_relative = 1 + 2*MAX_TRIM,
  params, chunk = 256, channels = 1)`; `params` = sinc_len 128 / f_cutoff 0.95 /
  oversampling 256 / Linear / BlackmanHarris2. chunk 256 ≈ 5 ms added latency.
- Per packet: `set_resample_ratio_relative(1.0 + trim, ramp = true)`, trim clamped to
  ±MAX_TRIM = 0.005 (±0.5 %, ~8 cents — inaudible).
- Proportional controller on EMA-smoothed occupancy: `smoothed += 0.05*(occ -
  smoothed)`; `trim = -0.02*(smoothed - setpoint)/setpoint`; setpoint = capacity/2.

### M11 — Fedora native build + Linux client
Bring up native Linux build (`alsa-lib-devel`); validate Windows↔Linux. Re-run the
M1 dual-stream smoke test on ALSA/PipeWire.

---

## Phase 3 — production for others

### M12 — `cargo-zigbuild` cross-compile from Fedora (Linux→Windows).
### M13 — packaging, logging/diagnostics, config validation, external-user docs.
### M13b — `[PHASE-3]` evaluate `opus-rs` to drop the libopus C dependency.
### M13c — `[PHASE-3]` adaptive FEC — auto-enable in-band FEC when the receiver reports real packet loss, so lossy links self-heal without the clean-link quality tax (FEC is opt-in/off by default as of Phase 2).
### M13e — `[PHASE-3]` cpal buffer-size tuning — possible jitter optimization
Request a smaller fixed cpal buffer instead of `BufferSize::Default` (often ~10 ms),
so the capture/playback callbacks deliver/consume audio in finer chunks → less
chunking jitter feeding the rings. Portable (WASAPI + ALSA/PipeWire). Risk: too-small
buffers cause xruns/instability and vary by device, so it needs a safe floor and
per-device validation. Deferred from the M10-era jitter work: the `timeBeginPeriod`
half (committed) showed self-inflicted *timing* jitter is minor here, so this is
speculative — revisit only if a link proves chunking-bound.
### M14 — `[PHASE-3]` Android front-end on `vox-core` (Oboe/AAudio + JNI/uniffi, libopus via NDK) — walkie-talkie app.
### M15 — `[PHASE-3]` Encryption
Optional authenticated encryption of the UDP payload (e.g. ChaCha20-Poly1305 with a
pre-shared key) so vox is safe on untrusted networks. Adds a nonce to the packet
header (§5) and a key/PSK config knob. `[CRYSTALLIZE]` cipher, key handling, and the
exact header change when the slice begins.
