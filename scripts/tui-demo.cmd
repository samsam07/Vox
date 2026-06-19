@echo off
rem Single-machine TUI demo: vox sends to itself (127.0.0.1:9680) and receives its
rem own packets, so tx + rx + the jitter buffer all populate — handy for iterating
rem on the TUI without a second machine. Builds + runs via run.cmd; quit with
rem q / Esc / Ctrl+C. Extra args are forwarded (e.g. a quieter playback sink).
rem
rem   scripts\tui-demo.cmd
rem   scripts\tui-demo.cmd --playback "CABLE-B Input (VB-Audio Virtual Cable B)"
rem
rem Tip: use HEADPHONES (default playback is your speakers, so the mic would feed
rem back). Talking into the mic makes the throughput graph move (Opus is VBR).
call "%~dp0run.cmd" --peer 127.0.0.1:9680 --bind 9680 --output tui %*
