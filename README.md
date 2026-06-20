# vox

Free, headless, bidirectional real-time voice pipe over a local network.

vox runs as two symmetric instances, one per machine. Each captures audio from a
local device, Opus-encodes it, and sends it over plain UDP to the other; at the
same time it receives, decodes, and plays the other side's audio. There is no
"server" and no "client" — just two peers that know each other's address.

It was built to solve one specific gap: GPU desktop-streaming hosts like
**Apollo** (a Sunshine fork) with **Moonlight** carry video and speaker audio from
the host, but have no microphone backchannel. vox supplies that missing duplex —
and works as a general-purpose LAN voice pipe for anything else.

## Why

- **Free & FOSS.** No paid tiers, no accounts.
- **Headless.** Fully scriptable from arguments or a config file. No GUI.
- **Low latency.** Tuned for real-time voice (Teams, Discord, in-game comms).
- **Cross-platform.** Windows ↔ Linux and Windows ↔ Windows.
- **No extra audio servers.** Uses what's already there — WASAPI on Windows,
  ALSA/PipeWire on Linux. No JACK, no Steam, no intermediary daemon.

## How it works

Each instance runs four threads: a capture callback and a playback callback
(audio I/O), plus a send thread (encode → UDP) and a receive thread (UDP →
decode). A lock-free ring buffer hands audio off the real-time callbacks, and a
jitter buffer on the receive side smooths out network timing and adapts its depth to
the measured jitter. Opus runs at 48 kHz, 20 ms frames, mono; in-band FEC for
graceful packet loss is available (opt-in via `--fec`, off by default — on a clean
link it costs more than it saves).

See [`docs/DESIGN.md`](docs/DESIGN.md) for the full architecture and rationale.

## Usage

Config-file mode (used by Apollo's command hooks):

```
vox config.toml
```

Ad-hoc mode:

```
vox --peer <host[:port]> [--bind <port>] [--capture <dev>] [--playback <dev>]
```

`--capture` / `--playback` name the *local* device to record from / play to.
Accepts `default`, `none` (disable that direction), or a device name. Omitted
means `default`. Port defaults to 9680 when omitted.

Run `vox --list-devices` to see exact device names (`--capture`/`--playback`
match them exactly). Other handy flags: `--output tui` for a live dashboard
(throughput, loss, jitter), `--output quiet` for silent headless runs, `-v` for
technical logs, and `--print-config` to dump the resolved settings.

Example — Windows host (VB-Cable A = desktop audio, B = virtual mic):

```
vox --peer 192.168.1.20 --capture "CABLE-A Output" --playback "CABLE-B Input"
```

Example — client (real mic and headphones):

```
vox --peer 192.168.1.10
```

Audio tuning (`--bitrate`, `--fec`, `--jitter-ms`, per-device sample rate, etc.)
lives in the TOML config and can be overridden by flags. See
[`docs/DESIGN.md`](docs/DESIGN.md) §6–§7.

## Building

Rust + Cargo. Requires a C compiler and `cmake` (to build bundled libopus); on
Linux also `alsa-lib-devel`.

```
cargo build --release
```

MVP builds natively per target (build Windows on Windows, Linux on Linux).
Cross-compiling from Fedora is a later-phase convenience.

## Status

**Working beta** (pre-1.0; no public release cut yet). Phase 1 MVP and most of Phase 2 are done: full-duplex LAN
voice with a clap CLI + TOML config and a live TUI, plus in-band FEC + graceful loss,
reconnection robustness, non-48k device resampling, and an adaptive jitter buffer —
verified machine-to-machine. Remaining for Phase 2: smooth clock-drift compensation
and the Linux client; Phase 3 covers packaging, encryption, and an Android front-end.
Not a 1.0 release yet. See [`docs/PLAN.md`](docs/PLAN.md) for milestones.

## Scope

In: two-peer duplex voice over LAN, device selection, adjustable quality, graceful
loss. Planned for Phase 3: payload encryption. Out: more than two peers, GUI,
audio mixing/effects.
