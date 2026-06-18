@echo off
setlocal
rem vox dev-run helper: load the Visual Studio C++ build environment (so cl/link
rem are present for any rebuild), put cargo/cmake/ninja on PATH, then `cargo run`
rem from the repo root. Any args are forwarded to cargo, e.g. `run.cmd --release`.
rem Configure the run via env vars first, e.g. `set VOX_SECS=20 && scripts\run.cmd`.

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
  echo ERROR: vswhere not found; is Visual Studio installed?
  exit /b 1
)

set "VSPATH="
for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%i"
if not defined VSPATH (
  echo ERROR: could not locate the Visual Studio C++ build tools via vswhere.
  exit /b 1
)

call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
set "PATH=%USERPROFILE%\.cargo\bin;C:\Program Files\CMake\bin;%LOCALAPPDATA%\Microsoft\WinGet\Links;%PATH%"

cd /d "%~dp0.."
cargo run %*
exit /b %ERRORLEVEL%
