//! vox — M0 toolchain proof.
//!
//! Compiles `cpal` and `opus` (the latter forces the bundled-libopus C build via
//! cmake + a C compiler) and prints the audio host and libopus version. The point
//! of this slice is to retire the C-build risk, not to do any audio work yet.

fn main() {
    let host = cpal::default_host();
    println!("cpal default host: {}", host.id().name());
    println!("libopus version:   {}", opus::version());
}
