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
    /// Course over ground: degrees clockwise from true north. Only present when
    /// moving (GPS cannot derive a bearing while stationary).
    pub bearing: Option<f32>,
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

        let w = 0.12;

        loop {
            let t = start.elapsed().as_secs_f64();
            // Velocity direction along the circular path (derivative of the
            // position below) gives a plausible course over ground.
            let d_north = (t * w).cos();
            let d_east = -(t * w).sin();
            let bearing = d_east.atan2(d_north).to_degrees().rem_euclid(360.0);

            let fix = GpsFix {
                lat: base_lat + 0.0015 * (t * w).sin(),
                lon: base_lon + 0.0015 * (t * w).cos(),
                bearing: Some(bearing as f32),
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
/// Requests the fine-location permission if needed, seeds the map with the
/// freshest last-known fix, then registers for active location updates and
/// emits each fresh fix on change. Feeds the same channel as [`spawn_simulated`].
///
/// Active updates matter: `getLastKnownLocation` is passive and never wakes the
/// GNSS hardware, so on its own it returns a stale fix that never changes. The
/// live updates come through a small `LocationListener` dex shim
/// (android/LocationBridge.java), which is what actually powers up the GNSS.
///
/// `vm` and `activity` are the raw `JavaVM` and Activity pointers from
/// `AndroidApp` (passed as `usize` so they can cross the thread boundary). The
/// Activity is required: `requestPermissions` is an Activity method, and
/// `ndk_context`'s context is the Application, which does not have it.
#[cfg(target_os = "android")]
pub fn spawn_android_location(ctx: egui::Context, vm: usize, activity: usize) -> Receiver<GpsFix> {
    let (tx, rx) = channel();

    thread::spawn(move || {
        if let Err(err) = android_location_loop(&tx, &ctx, vm, activity) {
            log::error!("android location source stopped: {err}");
        }
    });

    rx
}

#[cfg(target_os = "android")]
fn android_location_loop(
    tx: &std::sync::mpsc::Sender<GpsFix>,
    ctx: &egui::Context,
    vm: usize,
    activity: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use jni::objects::{JObject, JValue};
    use jni::JavaVM;

    // Pointers from AndroidApp, valid for the process lifetime.
    let vm = unsafe { JavaVM::from_raw(vm as *mut jni::sys::JavaVM) }?;
    let activity = unsafe { JObject::from_raw(activity as jni::sys::jobject) };
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

    let mut last: Option<GpsFix> = None;

    // Seed the map with the freshest last-known fix so it is not empty before
    // the first live update arrives (a cold GPS fix can take tens of seconds).
    if let Some(fix) =
        last_known_fix(&mut env, &location_manager, &["gps", "fused", "network", "passive"])?
    {
        last = Some(fix);
        if tx.send(fix).is_err() {
            return Ok(()); // UI has gone away.
        }
        ctx.request_repaint();
    }

    // Register for active updates through the dex shim. This is what powers up
    // the GNSS hardware; the last-known sweep above is passive and never
    // refreshes on its own. Fixes land on `loc_rx` via `native_on_location`.
    let (loc_tx, loc_rx) = channel();
    LOC_TX
        .set(std::sync::Mutex::new(loc_tx))
        .map_err(|_| "android location started twice")?;
    let class = load_location_bridge(&mut env, &activity)?;

    // LocationBridge.start(activity, minTimeMs = 1000, minDistanceM = 0)
    let started = env
        .call_static_method(
            &class,
            "start",
            "(Landroid/app/Activity;JF)Z",
            &[JValue::Object(&activity), JValue::Long(1000), JValue::Float(0.0)],
        )?
        .z()?;
    if !started {
        log::warn!("no location provider available; showing last-known fix only");
    }

    // The Sender in LOC_TX lives forever, so recv only ends when we break out
    // ourselves: on shutdown, when the UI channel has closed.
    while let Ok(fix) = loc_rx.recv() {
        if last != Some(fix) {
            last = Some(fix);
            if tx.send(fix).is_err() {
                let _ = env.call_static_method(&class, "stop", "()V", &[]);
                break; // UI has gone away.
            }
            ctx.request_repaint();
        }
    }

    Ok(())
}

/// Freshest last-known location across the given providers, or `None` if none
/// have one cached. Passive: this never wakes the GNSS hardware, so it only
/// seeds an initial position; live movement comes from the LocationListener.
#[cfg(target_os = "android")]
fn last_known_fix(
    env: &mut jni::JNIEnv,
    location_manager: &jni::objects::JObject,
    providers: &[&str],
) -> Result<Option<GpsFix>, jni::errors::Error> {
    use jni::objects::JValue;

    // Scope JNI local references: the caller runs on a long-lived thread that
    // never returns to Java, so per-call refs must not accumulate.
    env.with_local_frame(16, |env| -> Result<Option<GpsFix>, jni::errors::Error> {
        // Pick the most recent last-known location across providers.
        let mut best: Option<(GpsFix, i64)> = None;

        for provider in providers {
            let name = env.new_string(provider)?;
            let location = match env.call_method(
                location_manager,
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

            // Course over ground, only valid while moving.
            let bearing = if env.call_method(&location, "hasBearing", "()Z", &[])?.z()? {
                Some(env.call_method(&location, "getBearing", "()F", &[])?.f()?)
            } else {
                None
            };

            if best.map_or(true, |(_, t)| time > t) {
                best = Some((GpsFix { lat, lon, bearing }, time));
            }
        }

        Ok(best.map(|(fix, _)| fix))
    })
}

/// Internal channel from the Java `LocationListener` callback (main thread) to
/// the location worker. Set once before `LocationBridge.start`, so the callback
/// can never fire before it exists.
#[cfg(target_os = "android")]
static LOC_TX: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<GpsFix>>> =
    std::sync::OnceLock::new();

/// `LocationBridge.nativeOnLocation`: one live fix. Runs on the main thread and
/// only pushes onto the channel the worker drains, so no locking is needed
/// beyond the channel's.
#[cfg(target_os = "android")]
extern "system" fn native_on_location(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    lat: jni::sys::jdouble,
    lon: jni::sys::jdouble,
    bearing: jni::sys::jfloat,
    has_bearing: jni::sys::jboolean,
) {
    let fix = GpsFix {
        lat,
        lon,
        bearing: (has_bearing != 0).then_some(bearing),
    };
    if let Some(tx) = LOC_TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(fix);
        }
    }
}

/// Write the embedded dex holding `rs.gps.gui.LocationBridge` into the code
/// cache dir, load it with DexClassLoader (FindClass cannot see dex classes),
/// and bind `nativeOnLocation`. Mirrors the BLE bridge loader.
#[cfg(target_os = "android")]
fn load_location_bridge(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
) -> Result<jni::objects::GlobalRef, Box<dyn std::error::Error>> {
    use jni::objects::{JClass, JObject, JString, JValue};
    use jni::NativeMethod;

    const LOCATION_DEX: &[u8] = include_bytes!("../assets/location-bridge.dex");
    const LOCATION_CLASS: &str = "rs.gps.gui.LocationBridge";

    let dir = env
        .call_method(activity, "getCodeCacheDir", "()Ljava/io/File;", &[])?
        .l()?;
    let dir: JString = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?
        .l()?
        .into();
    let dir: String = env.get_string(&dir)?.into();
    let dex_path = format!("{dir}/location-bridge.dex");
    std::fs::write(&dex_path, LOCATION_DEX)?;

    // new DexClassLoader(dexPath, codeCacheDir, null, activity.getClassLoader())
    let parent = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let j_dex_path = env.new_string(&dex_path)?;
    let j_opt_dir = env.new_string(&dir)?;
    let loader = env.new_object(
        "dalvik/system/DexClassLoader",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/ClassLoader;)V",
        &[
            JValue::Object(&j_dex_path),
            JValue::Object(&j_opt_dir),
            JValue::Object(&JObject::null()),
            JValue::Object(&parent),
        ],
    )?;

    let j_class_name = env.new_string(LOCATION_CLASS)?;
    let class_obj = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&j_class_name)],
        )?
        .l()?;
    // Keep the class as a global ref: FindClass cannot see dex-loaded classes,
    // and jni's Desc machinery accepts a &GlobalRef directly.
    let class = env.new_global_ref(&JClass::from(class_obj))?;

    env.register_native_methods(
        &class,
        &[NativeMethod {
            name: "nativeOnLocation".into(),
            sig: "(DDFZ)V".into(),
            fn_ptr: native_on_location as *mut _,
        }],
    )?;

    Ok(class)
}

/// `Context.checkSelfPermission(name) == PERMISSION_GRANTED (0)`.
/// Also used by the BLE worker for the Bluetooth runtime permissions.
#[cfg(target_os = "android")]
pub(crate) fn check_permission(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    permission: &str,
) -> Result<bool, jni::errors::Error> {
    use jni::objects::JValue;
    // Scoped frame: this is polled in a loop, so per-call refs must not leak.
    env.with_local_frame(8, |env| -> Result<bool, jni::errors::Error> {
        let name = env.new_string(permission)?;
        let granted = env
            .call_method(
                activity,
                "checkSelfPermission",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&name)],
            )?
            .i()?
            == 0;
        Ok(granted)
    })
}

/// `Activity.requestPermissions({ name }, requestCode)`.
///
/// Best-effort: if the call throws (e.g. some devices insist it run on the UI
/// thread), the pending exception is cleared and logged rather than left to
/// crash the process. The caller polls `checkSelfPermission` regardless, so the
/// user can also grant the permission from Settings or via `adb`.
#[cfg(target_os = "android")]
fn request_permission(
    env: &mut jni::JNIEnv,
    activity: &jni::objects::JObject,
    permission: &str,
) -> Result<(), jni::errors::Error> {
    use jni::objects::JValue;
    let name = env.new_string(permission)?;
    let array = env.new_object_array(1, "java/lang/String", &name)?;

    let result = env.call_method(
        activity,
        "requestPermissions",
        "([Ljava/lang/String;I)V",
        &[JValue::Object(&array), JValue::Int(1)],
    );

    if env.exception_check()? {
        let _ = env.exception_describe();
        env.exception_clear()?;
    }
    if let Err(err) = result {
        log::warn!("requestPermissions failed ({err}); grant ACCESS_FINE_LOCATION manually");
    }

    Ok(())
}
