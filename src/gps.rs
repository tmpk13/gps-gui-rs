//! GPS position source.
//!
//! The rest of the app only depends on a `Receiver<GpsFix>`: something produces
//! fixes on a background thread and sends them over a channel, and the UI drains
//! that channel each frame. A real BLE source (btleplug) will slot in behind the
//! exact same interface later.

use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// A single GPS fix in decimal degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpsFix {
    pub lat: f64,
    pub lon: f64,
}

/// Spawn a simulated GPS source that emits a fix roughly once per second,
/// tracing a slow loop around a fixed point. Returns the receiving end of the
/// channel; when it is dropped the background thread exits.
pub fn spawn_simulated(ctx: egui::Context) -> Receiver<GpsFix> {
    let (tx, rx) = channel();

    thread::spawn(move || {
        // Greenwich observatory, a recognizable starting point.
        let base_lat = 51.4779;
        let base_lon = -0.0015;
        let start = Instant::now();

        loop {
            let t = start.elapsed().as_secs_f64();
            let fix = GpsFix {
                lat: base_lat + 0.0015 * (t * 0.12).sin(),
                lon: base_lon + 0.0015 * (t * 0.12).cos(),
            };

            if tx.send(fix).is_err() {
                break; // UI has gone away.
            }

            // Wake the UI thread so it drains the channel promptly.
            ctx.request_repaint();
            thread::sleep(Duration::from_secs(1));
        }
    });

    rx
}
