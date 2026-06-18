<#
.SYNOPSIS
  Dev-run helper for vox: set the run knobs from friendly params, then launch.

.DESCRIPTION
  Maps params to the temporary VOX_* environment variables the binary reads (the
  locked CLI arrives at M6). Only the params you pass are applied; the rest keep
  their defaults. Env changes are restored on exit so they don't leak into your
  shell session. Delegates the VS build-env setup + `cargo run` to run.cmd.

.EXAMPLE
  scripts\run.ps1 -Bind 5000 -Capture none -Secs 60
  # receiver: bind :5000, receive-only, play to default device

.EXAMPLE
  scripts\run.ps1 -Peer 127.0.0.1:5000 -Playback none -Bind 5001 -Secs 60
  # sender: capture default device, send-only to 127.0.0.1:5000
#>
[CmdletBinding()]
param(
    [string]$Peer,
    [int]$Bind,
    [string]$Capture,
    [string]$Playback,
    [int]$Secs,
    [int]$RingMs,
    [int]$JitterMs,
    [int]$Bitrate
)

$map = [ordered]@{
    Peer     = 'VOX_PEER'
    Bind     = 'VOX_BIND'
    Capture  = 'VOX_CAPTURE'
    Playback = 'VOX_PLAYBACK'
    Secs     = 'VOX_SECS'
    RingMs   = 'VOX_RING_MS'
    JitterMs = 'VOX_JITTER_MS'
    Bitrate  = 'VOX_BITRATE'
}

# Remember prior values so we can restore them and not leak into the session.
$saved = @{}
foreach ($var in $map.Values) { $saved[$var] = [Environment]::GetEnvironmentVariable($var) }

try {
    foreach ($param in $map.Keys) {
        if ($PSBoundParameters.ContainsKey($param)) {
            Set-Item "Env:$($map[$param])" ([string]$PSBoundParameters[$param])
        }
    }
    & "$PSScriptRoot\run.cmd"
}
finally {
    foreach ($var in $saved.Keys) {
        if ($null -eq $saved[$var]) { Remove-Item "Env:$var" -ErrorAction SilentlyContinue }
        else { Set-Item "Env:$var" $saved[$var] }
    }
}

exit $LASTEXITCODE
