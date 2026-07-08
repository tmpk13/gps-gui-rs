//! GPS position source.
//!
//! The rest of the app only depends on a `Receiver<GpsFix>`: something produces
//! fixes on a background thread and sends them over a channel, and the UI drains
//! that channel each frame. Desktop uses a simulated source; Android reads the
//! phone's GNSS via LocationManager. A BLE source could slot in the same way.

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

/// Spawn a GPS source backed by the phone's Android LocationManager.
///
/// Requests the fine-location permission if needed, then polls the freshest
/// last-known fix across providers once per second and emits it on change.
/// Feeds the same channel as [`spawn_simulated`].
#[cfg(target_os = "android")]
pub fn spawn_android_location(ctx: egui::Context) -> Receiver<GpsFix> {
    let (tx, rx) = channel();

    thread::spawn(move || {
        if let Err(err) = android_location_loop(&tx, &ctx) {
            log::error!("android location source stopped: {err}");
        }
    });

    rx
}

#[cfg(target_os = "android")]
fn android_location_loop(
    tx: &std::sync::mpsc::Sender<GpsFix>,
    ctx: &egui::Context,
) -> Result<(), Box<dyn std::error::Error>> {
    use jni::objects::{JObject, JValue};
    use jni::JavaVM;

    // Pointers provided by android-activity, valid for the process lifetime.
    let native = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(native.vm().cast()) }?;
    let activity = unsafe { JObject::from_raw(native.context().cast()) };
    let mut env = vm.attach_current_thread()?;

    // Ensure the fine-location permission is granted (poll after prompting;
    // the native activity has no easy onRequestPermissionsResult callback).
    const PERMISSION: &str = "android.permission.ACCESS_FINE_LOCATION";
    if !check_permission(&mut env, &activity, PERMISSION)? {
        request_permission(&mut env, &activity, PERMISSION)?;
        while !check_permission(&mut env, &activity, PERMISSION)? {
            thread::sleep(Duration::from_millis(500));
        }
    }

    // LocationManager lm = activity.getSystemService("location");
    let service = env.new_string("location")?;
    let location_manager = env
        .call_method(
            &activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&service)],
        )?
        .l()?;

    let providers = ["gps", "fused", "network", "passive"];
    let mut last: Option<GpsFix> = None;

    loop {
        // Pick the most recent last-known location across providers.
        let mut best: Option<(GpsFix, i64)> = None;

        for provider in providers {
            let name = env.new_string(provider)?;
            let location = match env.call_method(
                &location_manager,
                "getLastKnownLocation",
                "(Ljava/lang/String;)Landroid/location/Location;",
                &[JValue::Object(&name)],
            ) {
                Ok(value) => value.l()?,
                Err(_) => continue, // provider not present on this device
            };
            if location.is_null() {
                continue;
            }

            let lat = env.call_method(&location, "getLatitude", "()D", &[])?.d()?;
            let lon = env.call_method(&location, "getLongitude", "()D", &[])?.d()?;
            let time = env.call_method(&location, "getTime", "()J", &[])?.j()?;

            if best.map_or(true, |(_, t)| time > t) {
                best = Some((GpsFix { lat, lon }, time));
            }
        }

        if let Some((fix, _)) = best {
            if last != Some(fix) {
                last = Some(fix);
                if tx.send(fix).is_err() {
                    break; // UI has gone away.
                }
                ctx.request_repaint();
            }
        }

        thread::sleep(Duration::from_secs(1));
    }

    Ok(())
}

/// `Context.checkSelfPermission(name) == PERMISSION_GRANTED (0)`.
#[cfg(target_os = "android")]
fn check_permission(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    permission: &str,
) -> Result<bool, jni::errors::Error> {
    use jni::objects::JValue;
    let name = env.new_string(permission)?;
    let result = env
        .call_method(
            activity,
            "checkSelfPermission",
            "(Ljava/lang/String;)I",
            &[JValue::Object(&name)],
        )?
        .i()?;
    Ok(result == 0)
}

/// `Activity.requestPermissions({ name }, requestCode)`.
#[cfg(target_os = "android")]
fn request_permission(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    permission: &str,
) -> Result<(), jni::errors::Error> {
    use jni::objects::JValue;
    let name = env.new_string(permission)?;
    let string_class = env.find_class("java/lang/String")?;
    let array = env.new_object_array(1, &string_class, &name)?;
    env.call_method(
        activity,
        "requestPermissions",
        "([Ljava/lang/String;I)V",
        &[JValue::Object(&array), JValue::Int(1)],
    )?;
    Ok(())
}
