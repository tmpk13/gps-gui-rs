//! Device-facing compass heading from the NDK rotation-vector sensor.
//!
//! This uses the native `ASensorManager` API directly, so it needs no JNI, no
//! Java, and no Gradle - it works under the same native-activity / xbuild build
//! as the rest of the app. It runs its own thread with its own `ALooper` and
//! emits the heading (degrees clockwise from north) whenever it changes.
//!
//! The sensor is powered only while the UI asks for it through
//! [`CompassHandle::wanted`], and at the rate it asks for through
//! [`CompassHandle::interval_us`]. The rotation vector is fused from the
//! accelerometer, gyroscope and magnetometer, so leaving it enabled keeps all
//! three awake: heading-up turns the whole map and needs a fast rate, while the
//! marker's heading arrow reads fine from a few updates a second.

use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// The UI's handle to the compass thread.
pub struct CompassHandle {
    /// Headings in degrees clockwise from north, sent as they change.
    pub headings: Receiver<f32>,
    /// Whether the UI currently needs headings. The thread enables the sensor
    /// while this is set and disables it again when it clears, so nothing is
    /// measured for a heading no one is drawing.
    pub wanted: Arc<AtomicBool>,
    /// Requested delivery interval in microseconds. The thread applies changes
    /// while the sensor is running, and clamps to the sensor's minimum delay
    /// (asking for faster than the hardware allows is an error).
    pub interval_us: Arc<AtomicI32>,
}

/// The requested interval in microseconds for a rate in Hz, for
/// [`CompassHandle::interval_us`]. Non-positive or unrepresentable rates fall
/// back to 1 Hz rather than dividing by zero into an absurd interval; the rest
/// are capped at ten seconds, which is slower than any rate worth asking for.
pub fn interval_us(hz: f32) -> i32 {
    if hz <= 0.0 || !hz.is_finite() {
        return 1_000_000;
    }
    (1_000_000.0 / hz).clamp(1.0, 10_000_000.0) as i32
}

#[cfg(test)]
mod tests {
    use super::interval_us;

    #[test]
    fn rates_become_intervals_and_bad_ones_a_second() {
        assert_eq!(interval_us(60.0), 16_666);
        assert_eq!(interval_us(4.0), 250_000);
        // The slowest rate the settings allow still gets its full interval.
        assert_eq!(interval_us(0.5), 2_000_000);
        assert_eq!(interval_us(0.0), 1_000_000);
        assert_eq!(interval_us(f32::NAN), 1_000_000);
    }
}

#[cfg(target_os = "android")]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::mpsc::{channel, Sender};
    use std::sync::Arc;
    use std::thread;

    use ndk_sys as ns;

    use super::{interval_us, CompassHandle};

    /// Rate the sensor starts at until the UI asks for another.
    const DEFAULT_HZ: f32 = 60.0;

    /// Spawn the compass sensor loop. It starts with the sensor off; set
    /// [`CompassHandle::wanted`] to power it up. Emits the device heading in
    /// degrees clockwise from north whenever it changes by more than ~0.2
    /// degree.
    pub fn spawn(ctx: egui::Context) -> CompassHandle {
        let (tx, rx) = channel();
        let wanted = Arc::new(AtomicBool::new(false));
        let interval = Arc::new(AtomicI32::new(interval_us(DEFAULT_HZ)));
        let thread_wanted = wanted.clone();
        let thread_interval = interval.clone();
        thread::spawn(move || unsafe { run(&tx, &thread_wanted, &thread_interval, &ctx) });
        CompassHandle {
            headings: rx,
            wanted,
            interval_us: interval,
        }
    }

    unsafe fn run(
        tx: &Sender<f32>,
        wanted: &AtomicBool,
        interval: &AtomicI32,
        ctx: &egui::Context,
    ) {
        let package = match std::ffi::CString::new("rs.gps.gui") {
            Ok(p) => p,
            Err(_) => return,
        };

        let manager = ns::ASensorManager_getInstanceForPackage(package.as_ptr());
        if manager.is_null() {
            log::error!("compass: no sensor manager");
            return;
        }

        let sensor =
            ns::ASensorManager_getDefaultSensor(manager, ns::ASENSOR_TYPE_ROTATION_VECTOR as i32);
        if sensor.is_null() {
            log::error!("compass: no rotation-vector sensor");
            return;
        }

        let looper = ns::ALooper_prepare(ns::ALOOPER_PREPARE_ALLOW_NON_CALLBACKS as i32);
        if looper.is_null() {
            log::error!("compass: no looper");
            return;
        }

        const IDENT: i32 = 1;
        let queue = ns::ASensorManager_createEventQueue(
            manager,
            looper,
            IDENT,
            None,
            std::ptr::null_mut(),
        );
        if queue.is_null() {
            log::error!("compass: no event queue");
            return;
        }

        // Asking for a shorter interval than the hardware delivers is an error,
        // so every requested rate is floored at the sensor's own minimum.
        let min_delay = ns::ASensor_getMinDelay(sensor).max(1);

        let mut last_sent: Option<f32> = None;
        // The sensor starts off and is only enabled while the UI wants it.
        let mut enabled = false;
        // The rate last handed to the queue, so a change is applied once rather
        // than re-sent every poll.
        let mut applied_us = 0;

        loop {
            // Follow the UI's request. Enabling costs a fresh `setEventRate`
            // (the rate is per enable), disabling drops the last heading so a
            // stale one is not re-sent when the sensor comes back.
            let want = wanted.load(Ordering::Relaxed);
            let want_us = interval.load(Ordering::Relaxed).max(min_delay);
            if want != enabled {
                if want {
                    ns::ASensorEventQueue_enableSensor(queue, sensor);
                    ns::ASensorEventQueue_setEventRate(queue, sensor, want_us);
                    applied_us = want_us;
                } else {
                    ns::ASensorEventQueue_disableSensor(queue, sensor);
                    last_sent = None;
                }
                enabled = want;
            } else if enabled && want_us != applied_us {
                // Rate changes take effect on a running sensor, so switching
                // between heading-up and the slow marker-arrow rate does not
                // interrupt the readings.
                ns::ASensorEventQueue_setEventRate(queue, sensor, want_us);
                applied_us = want_us;
            }

            // Block up to 250 ms for sensor data. With the sensor disabled this
            // is just how often the request flag above is re-read.
            ns::ALooper_pollOnce(
                250,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );

            // Drain the queue, keeping only the most recent heading.
            let mut event: ns::ASensorEvent = std::mem::zeroed();
            let mut latest: Option<f32> = None;
            while ns::ASensorEventQueue_getEvents(queue, &mut event, 1) > 0 {
                if event.type_ == ns::ASENSOR_TYPE_ROTATION_VECTOR as i32 {
                    let v = event.__bindgen_anon_1.__bindgen_anon_1.data;
                    latest = Some(azimuth_degrees(v[0], v[1], v[2], v[3]));
                }
            }

            // Events queued just before a disable are dropped rather than sent
            // as a heading the UI would then hold on to indefinitely.
            if !enabled {
                continue;
            }

            if let Some(az) = latest {
                if last_sent.map_or(true, |prev| angle_diff(prev, az) > 0.2) {
                    last_sent = Some(az);
                    if tx.send(az).is_err() {
                        break; // UI has gone away.
                    }
                    ctx.request_repaint();
                }
            }
        }

        if enabled {
            ns::ASensorEventQueue_disableSensor(queue, sensor);
        }
        ns::ASensorManager_destroyEventQueue(manager, queue);
    }

    /// Azimuth in degrees clockwise from north from a rotation-vector quaternion
    /// `(x, y, z, w)`. Mirrors Android's `getRotationMatrixFromVector` followed by
    /// `getOrientation` (azimuth = `atan2(R[1], R[4])`).
    fn azimuth_degrees(x: f32, y: f32, z: f32, w: f32) -> f32 {
        let r1 = 2.0 * (x * y - z * w);
        let r4 = 1.0 - 2.0 * (x * x + z * z);
        (r1.atan2(r4).to_degrees() + 360.0) % 360.0
    }

    /// Smallest absolute difference between two headings, in degrees (0..=180).
    fn angle_diff(a: f32, b: f32) -> f32 {
        let d = (a - b).abs() % 360.0;
        if d > 180.0 {
            360.0 - d
        } else {
            d
        }
    }
}

#[cfg(target_os = "android")]
pub use imp::spawn;
