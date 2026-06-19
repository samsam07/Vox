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

## Phase 2 — daily-driver polish — next up

### M7 — FEC + graceful loss
Enable Opus in-band FEC + gap-detection→FEC-decode (deferred from M4/M5). Test
under simulated loss.
- Exit: audio degrades gracefully, not glitchy, under induced drop.

### M8 — Reconnection robustness
One side restarts without killing the other.

### M9 — `[PHASE-2]` resampling
Non-48k devices via a resampler (`rubato`). Lifts the 48k-only constraint.

### M10 — `[PHASE-2]` drift compensation + adaptive jitter
Resampling-based clock-drift correction (shares M9 resampler) + adaptive buffer
sizing. Kills the long-session blip.

### M11 — Fedora native build + Linux client
Bring up native Linux build (`alsa-lib-devel`); validate Windows↔Linux. Re-run the
M1 dual-stream smoke test on ALSA/PipeWire.

---

## Phase 3 — production for others

### M12 — `cargo-zigbuild` cross-compile from Fedora (Linux→Windows).
### M13 — packaging, logging/diagnostics, config validation, external-user docs.
### M13b — `[PHASE-3]` evaluate `opus-rs` to drop the libopus C dependency.
### M14 — `[PHASE-3]` Android front-end on `vox-core` (Oboe/AAudio + JNI/uniffi, libopus via NDK) — walkie-talkie app.
