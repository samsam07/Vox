//! End-to-end engine loopback over localhost UDP with simulated device callbacks:
//! a "capture device" thread pushes a tone into the CaptureSink and a "playback
//! device" thread pulls from the PlaybackSource, both in ALSA-sized 25 ms periods
//! on a real-time schedule. Exercises the send/receive threads, the wire, the jitter
//! buffer and the playout rebuffer (M10c) without audio hardware.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use vox_core::{Engine, EngineConfig, Setting};

const RATE: u32 = 48_000;
/// cpal's ALSA default period (25 ms) at 48 kHz mono.
const PERIOD: usize = 1200;
const PERIOD_TIME: Duration = Duration::from_millis(25);

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("find a free UDP port")
}

fn start() -> (Engine, vox_core::AudioPorts) {
    start_at(24_000)
}

fn start_at(bitrate: i32) -> (Engine, vox_core::AudioPorts) {
    let port = free_port();
    Engine::start(EngineConfig {
        peer: Some(format!("127.0.0.1:{port}").parse().unwrap()),
        bind: Some(port),
        capture_channels: Some(1),
        playback_channels: Some(1),
        capture_sample_rate: RATE,
        playback_sample_rate: RATE,
        jitter_ms: 150,
        bitrate,
        fec: false,
        expected_loss: 10,
        dtx: false,
        drift_correct: false,
    })
    .expect("engine starts")
}

/// Run capture + playback "devices" for `run`; capture goes quiet for `stall` at
/// `stall_at` and then catches up in one burst (a stalled send path draining its
/// ring). Returns the played samples.
fn drive(
    ports: vox_core::AudioPorts,
    run: Duration,
    stall_at: Duration,
    stall: Duration,
) -> Vec<f32> {
    let mut sink = ports.capture.expect("capture port");
    let mut source = ports.playback.expect("playback port");
    let done = Arc::new(AtomicBool::new(false));

    let capture = {
        let done = Arc::clone(&done);
        thread::spawn(move || {
            let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
            let mut phase = 0.0f32;
            let mut chunk = vec![0.0f32; PERIOD];
            let start = Instant::now();
            let mut next = start;
            let mut owed = 0usize; // periods withheld during the stall
            while !done.load(Ordering::Relaxed) {
                next += PERIOD_TIME;
                thread::sleep(next.saturating_duration_since(Instant::now()));
                let t = start.elapsed();
                if t >= stall_at && t < stall_at + stall {
                    owed += 1;
                    continue;
                }
                for _ in 0..=owed {
                    for s in chunk.iter_mut() {
                        *s = phase.sin() * 0.5;
                        phase += step;
                    }
                    sink.push(&chunk);
                }
                owed = 0;
            }
        })
    };

    let mut played = Vec::new();
    let mut buf = vec![0.0f32; PERIOD];
    let start = Instant::now();
    let mut next = start;
    while start.elapsed() < run {
        next += PERIOD_TIME;
        thread::sleep(next.saturating_duration_since(Instant::now()));
        source.fill(&mut buf);
        played.extend_from_slice(&buf);
    }
    done.store(true, Ordering::Relaxed);
    capture.join().unwrap();
    played
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

/// Steady ALSA-sized periods on both ends: after the startup fill, playback never
/// runs dry and carries the tone.
#[test]
fn steady_periods_play_without_underrun() {
    let (engine, ports) = start();
    let played = drive(ports, Duration::from_secs(2), Duration::MAX, Duration::ZERO);
    let stats = engine.stop().unwrap();

    assert_eq!(stats.underruns, 0, "steady stream must not break");
    assert_eq!(stats.capture_overruns, 0);
    let tail = &played[played.len() / 2..];
    assert!(rms(tail) > 0.2, "tone plays through (rms {})", rms(tail));
}

/// A 150 ms sender stall (inside the 200 ms capture-ring headroom): playback breaks once, rebuffers, and plays on — no
/// cascade of follow-up breaks — and the burst after the stall fits the capture ring.
#[test]
fn stall_breaks_once_then_recovers() {
    let (engine, ports) = start();
    let played = drive(
        ports,
        Duration::from_millis(2500),
        Duration::from_millis(800),
        Duration::from_millis(150),
    );
    let stats = engine.stop().unwrap();

    assert_eq!(stats.underruns, 1, "one stall → one break");
    assert_eq!(stats.capture_overruns, 0, "catch-up burst fits the ring");
    let tail = &played[played.len() * 4 / 5..];
    assert!(rms(tail) > 0.2, "playback resumed (rms {})", rms(tail));
}

/// Average bytes per sent packet between two stats snapshots.
fn bytes_per_packet(a: &vox_core::EngineStats, b: &vox_core::EngineStats) -> f64 {
    (b.bytes_sent - a.bytes_sent) as f64 / (b.packets_sent - a.packets_sent).max(1) as f64
}

/// Live settings (DESIGN §12): a bitrate change on a running engine reaches the
/// encoder (packets shrink), and toggling drift correction on and off mid-call keeps
/// playback going without a break.
#[test]
fn live_settings_apply_mid_call() {
    let (engine, ports) = start_at(64_000);
    let control = engine.control();
    assert!(control.sends() && control.receives());
    let devices = thread::spawn(move || {
        drive(
            ports,
            Duration::from_millis(4200),
            Duration::MAX,
            Duration::ZERO,
        )
    });

    thread::sleep(Duration::from_millis(800));
    let a = engine.stats();
    thread::sleep(Duration::from_millis(800));
    let b = engine.stats();
    control.apply(Setting::Bitrate(12_000)).unwrap();
    control.apply(Setting::DriftCorrect(true)).unwrap();
    thread::sleep(Duration::from_millis(300));
    let c = engine.stats();
    thread::sleep(Duration::from_millis(800));
    let d = engine.stats();
    control.apply(Setting::DriftCorrect(false)).unwrap();

    let played = devices.join().unwrap();
    let stats = engine.stop().unwrap();
    let (before, after) = (bytes_per_packet(&a, &b), bytes_per_packet(&c, &d));
    assert!(
        after < before / 2.0,
        "bitrate change shrinks packets ({before:.0} → {after:.0} bytes)"
    );
    assert_eq!(
        stats.underruns, 0,
        "switching drift correction must not break playback"
    );
    let tail = &played[played.len() * 4 / 5..];
    assert!(rms(tail) > 0.2, "tone still plays (rms {})", rms(tail));
}

/// A send-only engine's handle refuses receive-path settings instead of silently
/// dropping them.
#[test]
fn control_refuses_settings_for_roles_not_running() {
    let port = free_port();
    let (engine, _ports) = Engine::start(EngineConfig {
        peer: Some(format!("127.0.0.1:{port}").parse().unwrap()),
        bind: None,
        capture_channels: Some(1),
        playback_channels: None,
        capture_sample_rate: RATE,
        playback_sample_rate: RATE,
        jitter_ms: 150,
        bitrate: 24_000,
        fec: false,
        expected_loss: 10,
        dtx: false,
        drift_correct: false,
    })
    .expect("engine starts");
    let control = engine.control();
    assert!(control.sends() && !control.receives());
    assert!(control.apply(Setting::Fec(true)).is_ok());
    assert!(control.apply(Setting::DriftCorrect(true)).is_err());
    engine.stop().unwrap();
}
