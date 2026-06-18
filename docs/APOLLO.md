# Apollo / Sunshine integration

vox is the mic backchannel for an Apollo/Moonlight remote-desktop session: start it
when a stream connects, stop it when the stream ends. vox runs until it receives a
stop signal (Ctrl+C / SIGINT / SIGTERM / Windows console close), so the disconnect
hook just terminates the process.

## Reference topology

- **Host (this Windows machine):** capture desktop audio from VB-Cable A, play the
  received client mic into VB-Cable B (apps see B as a microphone).
  ```
  vox --capture "CABLE-A Output (VB-Audio Virtual Cable A)" ^
      --playback "CABLE-B Input (VB-Audio Virtual Cable B)" ^
      --peer <client-ip>:9680 --bind 9680
  ```
- **Client:** capture the real mic, play received audio to headphones.
  ```
  vox --peer <host-ip>:9680 --bind 9680
  ```

Or put the settings in a TOML and run `vox host.toml` (see `samples/vox.toml`).

## Hooks (Sunshine/Apollo `prep-cmd`)

Add a global command pair: `do` runs on stream start, `undo` on stream end. Use the
full path to `vox.exe` and a config file.

- **do** (connect):  launch vox in the background, e.g.
  `cmd /c start "" "C:\vox\vox.exe" "C:\vox\host.toml"`
- **undo** (disconnect):  stop it
  `taskkill /IM vox.exe /T`            (Windows)
  `pkill -INT vox`                     (Linux)

`taskkill`/`pkill` deliver the signal vox catches for a clean shutdown. For UDP
voice there is nothing to flush, so even a forced kill (`taskkill /F`) is
functionally fine — the graceful path just avoids error output and joins threads.

> Exact `prep-cmd` wiring depends on your Apollo/Sunshine version and whether you
> attach it globally or per-app. This is a template — adjust paths and the
> connect/disconnect mechanism to your setup.
