// SPDX-License-Identifier: GPL-3.0-only
//! Show / hide cycle driver for the overlay backends.
//!
//! Reproduces, with no audio stack and no daemon, the sequence that
//! exposed "the overlay only appears on the first dictation": the
//! states `session.rs` drives for a dictation, run several times over,
//! with fake audio pushed in so the renderer repaints like a real
//! capture.
//!
//! It is the client half of `tests/overlay-show-hide-check.sh`, which
//! runs it inside a nested headless compositor and screenshots each
//! phase. The two halves talk over stdout:
//!
//! ```text
//! BACKEND wlr-layer-shell     # which backend actually spawned
//! GEOM 640 100 48             # panel width, height, bottom margin
//! CUE baseline                # nothing shown: reference frame
//! CUE shown-1                 # steady state reached; capture now
//! CUE polishing-1
//! CUE hidden-1
//! …
//! DONE
//! ```
//!
//! After every `CUE` the driver holds that state for
//! `FONO_PROBE_SETTLE_MS` (default 1500), which is the window the
//! harness has to take its screenshot in. `FONO_PROBE_CYCLES` sets how
//! many dictations to imitate.
//!
//! Dev-only; never built into the shipped binary.

use std::io::Write;
use std::thread::sleep;
use std::time::Duration;

use fono_core::config::WaveformStyle;
use fono_overlay::renderer::{self, RendererState};
use fono_overlay::{spawn_overlay, OverlayState, PolishingPhase};

/// How long each phase is held after its cue is printed.
const DEFAULT_SETTLE_MS: u64 = 1_500;
/// Fake level pushes during a shown phase, ~30 fps.
const LEVEL_TICK: Duration = Duration::from_millis(33);

fn env_ms(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn cue(name: &str) {
    println!("CUE {name}");
    let _ = std::io::stdout().flush();
}

/// Push the audio taps a real capture would deliver — levels, raw
/// samples and FFT bins (the heatmap / spectrum styles paint from the
/// last two) — for `dur`, at roughly the daemon's cadence.
fn feed(overlay: &fono_overlay::OverlayHandle, phase: &mut f32, dur: Duration) {
    let mut t = Duration::ZERO;
    while t < dur {
        *phase += 0.35;
        let amp = 0.2f32.mul_add(phase.sin(), 0.25);
        overlay.push_level(amp);
        overlay.push_samples(
            (0..256u16).map(|k| amp * f32::from(k).mul_add(0.19, *phase).sin()).collect(),
        );
        overlay.push_fft_bins((0..64).map(|k| amp * (1.0 - k as f32 / 64.0)).collect());
        sleep(LEVEL_TICK);
        t += LEVEL_TICK;
    }
}

fn main() {
    // Deliberately not `EnvFilter` — that feature would pull regex &
    // friends into the lockfile for a dev-only example. `RUST_LOG` is
    // read as a bare level instead.
    let level = match std::env::var("RUST_LOG").unwrap_or_default().to_ascii_lowercase().as_str() {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" => tracing::Level::WARN,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

    let cycles = env_ms("FONO_PROBE_CYCLES", 3) as usize;
    let settle = Duration::from_millis(env_ms("FONO_PROBE_SETTLE_MS", DEFAULT_SETTLE_MS));

    let style = WaveformStyle::Heatmap;
    let overlay = spawn_overlay(style).expect("spawn_overlay never fails");
    println!("BACKEND {}", overlay.backend_id().as_str());
    // Where the panel is supposed to land, straight from the renderer's
    // own constants, so the harness can check that rectangle without
    // hardcoding a copy of them: width, height, bottom margin. The
    // harness turns that into screen coordinates once it knows the
    // output size.
    println!(
        "GEOM {} {} {}",
        renderer::WIN_WIDTH.round() as u32,
        RendererState::new(style).target_logical_height().round() as u32,
        renderer::BOTTOM_OFFSET,
    );
    let _ = std::io::stdout().flush();

    // Reference frame: whatever the screen looks like with the overlay
    // hidden. Every later frame is compared against it, so the check
    // works the same on an empty nested desktop and on one with a
    // wallpaper and a Plasma panel in the way.
    cue("baseline");
    sleep(settle);

    let mut phase = 0.0f32;
    for i in 1..=cycles {
        // The order below is the one `session.rs` drives for a real
        // dictation: Recording (levels + FFT taps) → Processing →
        // Polishing → Hidden. Cheaper sequences hid the bug once
        // already, so the probe follows the daemon rather than the
        // minimum needed to map a surface.

        // --- recording, with audio data arriving like a real capture.
        overlay.set_state(OverlayState::Recording { db: -18 });
        feed(&overlay, &mut phase, Duration::from_millis(250));
        cue(&format!("shown-{i}"));
        feed(&overlay, &mut phase, settle);

        // --- post-release: batch STT, then the polish walk.
        overlay.set_state(OverlayState::Processing);
        sleep(Duration::from_millis(250));
        for step in 0..5u16 {
            overlay.set_state(OverlayState::Polishing {
                phase: PolishingPhase::Transcribing,
                walk_progress: step * 2_000,
            });
            sleep(Duration::from_millis(60));
        }
        cue(&format!("polishing-{i}"));
        sleep(settle);

        // --- hidden: the state the daemon leaves the overlay in
        //     between dictations.
        overlay.set_state(OverlayState::Hidden);
        cue(&format!("hidden-{i}"));
        sleep(settle);
    }

    overlay.shutdown();
    println!("DONE");
    let _ = std::io::stdout().flush();
    // Give the backend thread a beat to tear the surface down before
    // the process (and the wayland connection) goes away.
    sleep(Duration::from_millis(200));
}
