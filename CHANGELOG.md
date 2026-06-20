# Changelog

All notable changes to vox are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and vox uses
[Semantic Versioning](https://semver.org/). `vox` (the CLI) and `vox-core` (the
engine) are versioned independently; an entry notes which crate it affects when that
matters.

## [Unreleased]

Nothing released yet — everything below is the content of the upcoming **0.1.0**
(Phases 1–2). On the first release, rename this section to `[0.1.0] - YYYY-MM-DD`.

### Added

- **Full-duplex LAN voice pipe.** Two symmetric peers: each captures from a local
  device, Opus-encodes (48 kHz / 20 ms / mono), and sends over plain UDP, while
  decoding and playing the peer's stream. Four threads with sacred, allocation-free
  audio callbacks and lock-free SPSC ring buffers.
- **CLI + TOML config** (`clap` + `serde`) with `flag > file > default` precedence.
  Role-named device selection (`--capture` / `--playback`, each `none` | `default` |
  exact name); `--list-devices`, `--print-config`, `--duration`,
  `--output quiet|plain|tui`, `-v` / `-vv`.
- **Live TUI dashboard** (`--output tui`): throughput graph, jitter-buffer gauge with
  live latency, and loss / recenter / drift quality readouts.
- **In-band FEC + PLC loss concealment** (opt-in via `--fec`, off by default — on a
  clean link it costs more than it saves). The receiver reconstructs a lost frame
  from the redundant copy carried in the next packet; earlier losses use PLC.
- **Reconnection robustness.** One side restarts without killing the other:
  peer-restart resync (sequence reset + decoder reset) and tolerance of transient
  peer-down socket errors (e.g. Windows ICMP → ConnectionReset).
- **Adaptive jitter buffer.** Sizes its operating band to measured network jitter
  (RFC3550, from the packet timestamp) — shallow / low-latency on clean links, deeper
  on bursty ones — with a recenter drop/hold backstop for slow clock drift.
  `--jitter-ms` is the depth ceiling (default 150).
- **Smooth clock-drift compensation** (opt-in via `--drift-correct`, off by default):
  a dynamic-ratio resampler trimmed by a controller holds the buffer at its target,
  removing drift cutoffs and keeping latency near the target. A TUI **drift** readout
  (buffer-latency trend, ms/min) flags when to enable it.
- **Non-48 kHz device resampling** (`rubato`): a device that doesn't run at 48 kHz is
  opened at its native rate and resampled at the edge; the wire stays 48 kHz mono.
- **Apollo / Sunshine integration**: runs until a stop signal so connect/disconnect
  hooks start and stop it (`docs/APOLLO.md`); sample configs `samples/host.toml`,
  `samples/client.toml`, and the annotated `samples/vox.toml`.
- **Windows timer-resolution bump** (1 ms for the session) for smoother send pacing.

[Unreleased]: https://github.com/samsam07/Vox/commits/master
