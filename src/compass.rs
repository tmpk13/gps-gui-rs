//! Device-facing compass heading from the NDK rotation-vector sensor.
//!
//! This uses the native `ASensorManager` API directly, so it needs no JNI, no
//! Java, and no Gradle - it works under the same native-activity / xbuild build
//! as the rest of the app. It runs its own thread with its own `ALooper` and
//! emits the heading (degrees clockwise from north) whenever it changes.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use ndk_sys as ns;

/// Spawn the compass sensor loop. Emits the device heading in degrees clockwise
/// from north whenever it changes by more than ~0.2 degree.
pub fn spawn(ctx: egui::Context) -> Receiver<f32> {
    let (tx, rx) = channel();
    thread::spawn(move || unsafe { run(&tx, &ctx) });
    rx
}

unsafe fn run(tx: &Sender<f32>, ctx: &egui::Context) {
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

    ns::ASensorEventQueue_enableSensor(queue, sensor);
    ns::ASensorEventQueue_setEventRate(queue, sensor, 16_000); // ~60 Hz

    let mut last_sent: Option<f32> = None;

    loop {
        // Block up to 250 ms for sensor data.
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

    ns::ASensorEventQueue_disableSensor(queue, sensor);
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
