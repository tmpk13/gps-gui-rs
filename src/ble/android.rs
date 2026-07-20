//! Android BLE worker.
//!
//! xbuild packages a NativeActivity APK with no Java build step, but GATT
//! callbacks require Java subclasses. So a small shim (android/BleBridge.java,
//! precompiled to assets/ble-bridge.dex by android/build-dex.sh) is embedded
//! into the native library, written to the app's code cache dir at startup,
//! loaded with DexClassLoader, and its `native*` callbacks are bound to the
//! Rust functions below with RegisterNatives.
//!
//! The Java callbacks arrive on Binder threads; they only push onto a channel
//! that this worker thread drains, so no locking beyond the channel is needed.

use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue};
use jni::sys::jint;
use jni::{JNIEnv, JavaVM, NativeMethod};

use gps_proto::packet::{self, PositionPacket};
use midair_proto::ble;
use midair_proto::link::Telemetry;

use super::{settings_event, BleCommand, BleEvent, BleHandle, ConfigWrite};

/// The compiled dex with rs.gps.gui.BleBridge (see android/build-dex.sh).
const BRIDGE_DEX: &[u8] = include_bytes!("../../assets/ble-bridge.dex");
const BRIDGE_CLASS: &str = "rs.gps.gui.BleBridge";

/// Events pushed by the Java callbacks (Binder threads) to the worker.
enum Cb {
    Scan { address: String },
    ConnectionState { new_state: i32 },
    ServicesDiscovered { status: i32 },
    Notify { uuid: String, value: Vec<u8> },
    WriteDone { status: i32 },
    DescriptorWrite { status: i32 },
}

/// Set once before RegisterNatives, so the Java callbacks can never fire
/// before it exists. Mutex because Binder threads push concurrently.
static CB_TX: OnceLock<Mutex<Sender<Cb>>> = OnceLock::new();

fn push_cb(event: Cb) {
    if let Some(tx) = CB_TX.get() {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(event);
        }
    }
}

fn jstring_or_empty(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(Into::into).unwrap_or_default()
}

// --- native callbacks (registered on the dex-loaded class) ---

extern "system" fn native_on_scan(
    mut env: JNIEnv,
    _class: JClass,
    address: JString,
    _name: JString,
    _rssi: jint,
) {
    let address = jstring_or_empty(&mut env, &address);
    push_cb(Cb::Scan { address });
}

extern "system" fn native_on_connection_state(
    _env: JNIEnv,
    _class: JClass,
    _status: jint,
    new_state: jint,
) {
    push_cb(Cb::ConnectionState { new_state });
}

extern "system" fn native_on_services_discovered(_env: JNIEnv, _class: JClass, status: jint) {
    push_cb(Cb::ServicesDiscovered { status });
}

extern "system" fn native_on_notify(
    mut env: JNIEnv,
    _class: JClass,
    uuid: JString,
    value: JByteArray,
) {
    let uuid = jstring_or_empty(&mut env, &uuid);
    let value = env.convert_byte_array(&value).unwrap_or_default();
    push_cb(Cb::Notify { uuid, value });
}

extern "system" fn native_on_write(_env: JNIEnv, _class: JClass, _uuid: JString, status: jint) {
    push_cb(Cb::WriteDone { status });
}

extern "system" fn native_on_descriptor_write(_env: JNIEnv, _class: JClass, status: jint) {
    push_cb(Cb::DescriptorWrite { status });
}

// --- worker ---

pub fn spawn(ctx: egui::Context, vm: usize, activity: usize) -> BleHandle {
    let (event_tx, event_rx) = channel();
    let (cmd_tx, cmd_rx) = channel();

    std::thread::spawn(move || {
        let report = Reporter {
            ctx,
            tx: event_tx.clone(),
        };
        if let Err(e) = worker(&report, cmd_rx, vm, activity) {
            report.status(format!("BLE stopped: {e}"));
        }
    });

    BleHandle {
        events: event_rx,
        commands: cmd_tx,
    }
}

struct Reporter {
    ctx: egui::Context,
    tx: Sender<BleEvent>,
}

impl Reporter {
    fn send(&self, event: BleEvent) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }

    fn status(&self, s: impl Into<String>) {
        self.send(BleEvent::Status(s.into()));
    }
}

type AnyError = Box<dyn std::error::Error>;

/// The worker's JNI handle to the dex-loaded BleBridge class.
///
/// `env` is an unsafe clone of the thread's attach-guard env; the guard lives
/// for the whole `worker` call (the thread's lifetime), which makes the clone
/// sound. `activity` is the process-lifetime Activity reference from
/// `AndroidApp` (aliased by raw pointer, never owned or freed here).
struct Bridge<'a> {
    env: JNIEnv<'a>,
    class: GlobalRef,
    activity_raw: jni::sys::jobject,
}

fn worker(
    report: &Reporter,
    cmd_rx: Receiver<BleCommand>,
    vm: usize,
    activity: usize,
) -> Result<(), AnyError> {
    let (cb_tx, cb_rx) = channel();
    CB_TX
        .set(Mutex::new(cb_tx))
        .map_err(|_| "BLE worker started twice")?;

    // Pointers from AndroidApp, valid for the process lifetime.
    let vm = unsafe { JavaVM::from_raw(vm as *mut jni::sys::JavaVM) }?;
    let activity_raw = activity as jni::sys::jobject;
    let mut env = vm.attach_current_thread()?;

    let activity = unsafe { JObject::from_raw(activity_raw) };
    let class = load_bridge(&mut env, &activity)?;
    let mut bridge = Bridge {
        env: unsafe { env.unsafe_clone() },
        class,
        activity_raw,
    };

    // The shim is also the home of the keep-screen-on helper (runOnUiThread
    // needs a Java Runnable); apply it once, before anything below can fail.
    bridge.keep_screen_on();

    if !bridge.init() {
        report.status("Bluetooth unavailable on this device");
        return Ok(());
    }

    let mut wanted_connect = false;
    let mut mac: Option<String> = None;
    let mut chase = false;
    let mut writes: Vec<ConfigWrite> = Vec::new();
    let mut permissions_done = false;

    loop {
        if drain_commands(
            &cmd_rx,
            &mut wanted_connect,
            &mut mac,
            &mut chase,
            &mut writes,
        )
        .is_err()
        {
            return Ok(()); // UI has gone away
        }
        if !wanted_connect {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        if !permissions_done {
            report.status("waiting for Bluetooth permissions...");
            bridge.ensure_permissions()?;
            permissions_done = true;
        }

        match session(
            &mut bridge,
            report,
            &cb_rx,
            &cmd_rx,
            &mut wanted_connect,
            &mut mac,
            &mut chase,
            &mut writes,
        ) {
            Ok(()) => {} // clean stop requested by the UI
            Err(e) => {
                bridge.disconnect();
                report.send(BleEvent::Connected(false));
                report.status(format!("{e}; retrying"));
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn drain_commands(
    cmd_rx: &Receiver<BleCommand>,
    wanted_connect: &mut bool,
    mac: &mut Option<String>,
    chase: &mut bool,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), ()> {
    loop {
        match cmd_rx.try_recv() {
            Ok(BleCommand::Connect { mac: m, chase: c }) => {
                *wanted_connect = true;
                *mac = m;
                *chase = c;
            }
            Ok(BleCommand::Disconnect) => *wanted_connect = false,
            Ok(BleCommand::Config(w)) => writes.push(w),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
}

/// One scan+connect+subscribe+pump attempt. `Ok(())` means the UI asked to
/// stop; `Err` means retry.
#[allow(clippy::too_many_arguments)]
fn session(
    bridge: &mut Bridge,
    report: &Reporter,
    cb_rx: &Receiver<Cb>,
    cmd_rx: &Receiver<BleCommand>,
    wanted_connect: &mut bool,
    mac: &mut Option<String>,
    chase: &mut bool,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), String> {
    let session_mac = mac.clone();

    // Drop stale callback events from a previous session (e.g. a disconnect
    // that raced the teardown), so the waits below cannot match them.
    while cb_rx.try_recv().is_ok() {}

    // Resolve the target address. A pinned MAC normally connects straight
    // off, which is cheaper than scanning. That is the wrong primitive for a
    // board that may be asleep: `connect` is a bounded attempt, and retrying
    // it on a fixed cycle can stay out of phase with a 15 s advertising
    // window for a long time. Chasing therefore scans instead - always
    // listening, so any window is caught the moment it opens - and matches
    // the pinned address among the hits, exactly as the desktop worker does.
    let address = match session_mac.clone() {
        Some(m) if !*chase => m,
        pinned => {
            report.status(if pinned.is_some() {
                "waiting for a wake window..."
            } else {
                "scanning for GPS beacon..."
            });
            if !bridge.start_scan(packet::SERVICE_UUID) {
                return Err("scan failed (Bluetooth off?)".into());
            }
            let found = loop {
                if drain_commands(cmd_rx, wanted_connect, mac, chase, writes).is_err()
                    || !*wanted_connect
                {
                    bridge.stop_scan();
                    return Ok(());
                }
                match cb_rx.recv_timeout(Duration::from_millis(300)) {
                    Ok(Cb::Scan { address }) => match &pinned {
                        // Another GPS board answering the same service filter.
                        Some(m) if !address.eq_ignore_ascii_case(m) => {}
                        _ => break address,
                    },
                    Ok(_) => {}
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Err("worker gone".into()),
                }
            };
            bridge.stop_scan();
            found
        }
    };

    report.status(format!("connecting to {address}..."));
    if !bridge.connect(&address) {
        return Err("connect call failed".into());
    }
    wait_for(cb_rx, Duration::from_secs(20), |cb| {
        matches!(cb, Cb::ConnectionState { new_state: 2 })
    })
    .map_err(|_| "connect timed out")?;

    if !bridge.discover_services() {
        return Err("service discovery call failed".into());
    }
    wait_for(cb_rx, Duration::from_secs(10), |cb| {
        matches!(cb, Cb::ServicesDiscovered { status: 0 })
    })
    .map_err(|_| "service discovery timed out")?;

    // Subscribe to position + ack. Each CCCD write must complete before the
    // next GATT operation starts (Android runs one at a time).
    for chr in [packet::POSITION_UUID, packet::ACK_UUID] {
        if !bridge.set_notify(packet::SERVICE_UUID, chr, true) {
            return Err("subscribe call failed".into());
        }
        wait_for(cb_rx, Duration::from_secs(5), |cb| {
            matches!(cb, Cb::DescriptorWrite { status: 0 })
        })
        .map_err(|_| "subscribe timed out")?;
    }
    // Optional board-status subscriptions (esp32c6-gps; absent on the c3
    // beacon, so a missing characteristic is not an error). Settings is
    // subscribed before it is read below, so a change the board makes between
    // the two (a clamped interval, say) still reaches us.
    for chr in [ble::TELEMETRY_UUID, ble::LOG_UUID, ble::SETTINGS_UUID] {
        if bridge.set_notify(packet::SERVICE_UUID, chr, true) {
            let _ = wait_for(cb_rx, Duration::from_secs(5), |cb| {
                matches!(cb, Cb::DescriptorWrite { status: 0 })
            });
        }
    }

    report.send(BleEvent::Connected(true));
    report.status(format!("connected to {address}"));

    // Populate the board controls from the board itself rather than assuming
    // defaults for settings it holds in flash across power cycles. The shim
    // routes the read value through the notify callback, so the pump below
    // decodes it on the same path a change notification takes.
    bridge.read_characteristic(packet::SERVICE_UUID, ble::SETTINGS_UUID);

    // Pump: notifications out, commands in, until disconnect.
    loop {
        for w in writes.drain(..) {
            let (buf, n) = w.encode();
            if !bridge.write_characteristic(packet::SERVICE_UUID, packet::CONFIG_UUID, &buf[..n]) {
                report.status("config write failed");
            }
        }

        match cb_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Cb::Notify { uuid, value }) => {
                if uuid.eq_ignore_ascii_case(packet::POSITION_UUID) {
                    if let Some(p) = PositionPacket::decode(&value) {
                        report.send(BleEvent::Fix(p));
                    }
                } else if uuid.eq_ignore_ascii_case(packet::ACK_UUID) {
                    if let Some(a) = packet::parse_ack(&value) {
                        report.send(BleEvent::Ack(a));
                    }
                } else if uuid.eq_ignore_ascii_case(ble::TELEMETRY_UUID) {
                    if let Some(t) = Telemetry::decode(&value) {
                        report.send(BleEvent::Telemetry(t));
                    }
                } else if uuid.eq_ignore_ascii_case(ble::LOG_UUID) {
                    report.send(BleEvent::Log(String::from_utf8_lossy(&value).into_owned()));
                } else if uuid.eq_ignore_ascii_case(ble::SETTINGS_UUID) {
                    report.send(settings_event(&value));
                }
            }
            Ok(Cb::ConnectionState { new_state: 0 }) => return Err("connection lost".into()),
            Ok(Cb::WriteDone { status }) if status != 0 => {
                report.status(format!("config write rejected (status {status})"));
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err("worker gone".into()),
        }

        if drain_commands(cmd_rx, wanted_connect, mac, chase, writes).is_err() || !*wanted_connect {
            bridge.disconnect();
            report.send(BleEvent::Connected(false));
            report.status("disconnected");
            return Ok(());
        }
        if *mac != session_mac {
            // The UI pinned a different device (e.g. a config reload).
            bridge.disconnect();
            report.send(BleEvent::Connected(false));
            return Err("switching device".into());
        }
    }
}

/// Drain callback events until `pred` matches or the deadline passes.
fn wait_for(cb_rx: &Receiver<Cb>, timeout: Duration, pred: impl Fn(&Cb) -> bool) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.checked_duration_since(Instant::now()).ok_or(())?;
        match cb_rx.recv_timeout(left) {
            Ok(cb) if pred(&cb) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Err(()),
        }
    }
}

/// Write the embedded dex into the code cache dir, load it, and bind the
/// native callbacks.
fn load_bridge(env: &mut JNIEnv, activity: &JObject) -> Result<GlobalRef, AnyError> {
    let dir = env
        .call_method(activity, "getCodeCacheDir", "()Ljava/io/File;", &[])?
        .l()?;
    let dir: JString = env
        .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?
        .l()?
        .into();
    let dir: String = env.get_string(&dir)?.into();
    let dex_path = format!("{dir}/ble-bridge.dex");
    std::fs::write(&dex_path, BRIDGE_DEX)?;

    // new DexClassLoader(dexPath, codeCacheDir, null, activity.getClassLoader())
    // The optimized dir is ignored since API 26 but must be non-null before.
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

    let j_class_name = env.new_string(BRIDGE_CLASS)?;
    let class_obj = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&j_class_name)],
        )?
        .l()?;
    // Keep the class as a global ref: FindClass cannot see dex-loaded
    // classes, and jni's Desc machinery accepts a &GlobalRef directly.
    let class = env.new_global_ref(&JClass::from(class_obj))?;

    env.register_native_methods(
        &class,
        &[
            NativeMethod {
                name: "nativeOnScan".into(),
                sig: "(Ljava/lang/String;Ljava/lang/String;I)V".into(),
                fn_ptr: native_on_scan as *mut _,
            },
            NativeMethod {
                name: "nativeOnConnectionState".into(),
                sig: "(II)V".into(),
                fn_ptr: native_on_connection_state as *mut _,
            },
            NativeMethod {
                name: "nativeOnServicesDiscovered".into(),
                sig: "(I)V".into(),
                fn_ptr: native_on_services_discovered as *mut _,
            },
            NativeMethod {
                name: "nativeOnNotify".into(),
                sig: "(Ljava/lang/String;[B)V".into(),
                fn_ptr: native_on_notify as *mut _,
            },
            NativeMethod {
                name: "nativeOnWrite".into(),
                sig: "(Ljava/lang/String;I)V".into(),
                fn_ptr: native_on_write as *mut _,
            },
            NativeMethod {
                name: "nativeOnDescriptorWrite".into(),
                sig: "(I)V".into(),
                fn_ptr: native_on_descriptor_write as *mut _,
            },
        ],
    )?;

    Ok(class)
}

impl Bridge<'_> {
    /// Run `f` against the bridge class in a fresh local-reference frame (the
    /// worker thread never returns to Java, so per-call locals must not
    /// accumulate), clearing any pending exception on failure. The shim
    /// catches Throwable internally, so exceptions here are a backstop.
    /// `f` gets the class as a `&GlobalRef` because that is the only non-name
    /// `Desc<JClass>` jni offers (and FindClass cannot see dex classes).
    fn scoped<T>(
        &mut self,
        capacity: i32,
        f: impl FnOnce(&mut JNIEnv, &GlobalRef) -> Result<T, jni::errors::Error>,
    ) -> Option<T> {
        let class = self.class.clone();
        let result = self.env.with_local_frame(capacity, |env| f(env, &class));
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                if self.env.exception_check().unwrap_or(false) {
                    let _ = self.env.exception_describe();
                    let _ = self.env.exception_clear();
                }
                log::warn!("BleBridge call failed: {e}");
                None
            }
        }
    }

    fn init(&mut self) -> bool {
        let activity_raw = self.activity_raw;
        self.scoped(8, |env, class| {
            let activity = unsafe { JObject::from_raw(activity_raw) };
            env.call_static_method(
                class,
                "init",
                "(Landroid/content/Context;)Z",
                &[JValue::Object(&activity)],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    fn keep_screen_on(&mut self) {
        let activity_raw = self.activity_raw;
        self.scoped(8, |env, class| {
            let activity = unsafe { JObject::from_raw(activity_raw) };
            env.call_static_method(
                class,
                "keepScreenOn",
                "(Landroid/app/Activity;)V",
                &[JValue::Object(&activity)],
            )
            .map(|_| ())
        });
    }

    fn start_scan(&mut self, service_uuid: &str) -> bool {
        self.scoped(8, |env, class| {
            let uuid = env.new_string(service_uuid)?;
            env.call_static_method(
                class,
                "startScan",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&uuid)],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    fn stop_scan(&mut self) {
        self.scoped(4, |env, class| {
            env.call_static_method(class, "stopScan", "()V", &[]).map(|_| ())
        });
    }

    fn connect(&mut self, mac: &str) -> bool {
        self.scoped(8, |env, class| {
            // Android requires the colon-separated form in upper case.
            let mac = env.new_string(mac.to_uppercase())?;
            env.call_static_method(
                class,
                "connect",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&mac)],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    fn discover_services(&mut self) -> bool {
        self.scoped(4, |env, class| {
            env.call_static_method(class, "discoverServices", "()Z", &[])?.z()
        })
        .unwrap_or(false)
    }

    fn set_notify(&mut self, service: &str, chr: &str, enable: bool) -> bool {
        self.scoped(8, |env, class| {
            let service = env.new_string(service)?;
            let chr = env.new_string(chr)?;
            env.call_static_method(
                class,
                "setNotify",
                "(Ljava/lang/String;Ljava/lang/String;Z)Z",
                &[
                    JValue::Object(&service),
                    JValue::Object(&chr),
                    JValue::Bool(enable as u8),
                ],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    /// The value comes back through the notify callback, not a return value.
    fn read_characteristic(&mut self, service: &str, chr: &str) -> bool {
        self.scoped(8, |env, class| {
            let service = env.new_string(service)?;
            let chr = env.new_string(chr)?;
            env.call_static_method(
                class,
                "readCharacteristic",
                "(Ljava/lang/String;Ljava/lang/String;)Z",
                &[JValue::Object(&service), JValue::Object(&chr)],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    fn write_characteristic(&mut self, service: &str, chr: &str, value: &[u8]) -> bool {
        self.scoped(8, |env, class| {
            let service = env.new_string(service)?;
            let chr = env.new_string(chr)?;
            let bytes = env.byte_array_from_slice(value)?;
            env.call_static_method(
                class,
                "writeCharacteristic",
                "(Ljava/lang/String;Ljava/lang/String;[B)Z",
                &[
                    JValue::Object(&service),
                    JValue::Object(&chr),
                    JValue::Object(&bytes),
                ],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    fn disconnect(&mut self) {
        self.scoped(4, |env, class| {
            env.call_static_method(class, "disconnect", "()V", &[]).map(|_| ())
        });
    }

    /// Android 12+ makes BLUETOOTH_SCAN / BLUETOOTH_CONNECT runtime
    /// permissions. Request them (one dialog) and poll until granted, so a
    /// grant from Settings also unblocks us.
    fn ensure_permissions(&mut self) -> Result<(), AnyError> {
        let sdk = self
            .env
            .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
            .i()?;
        if sdk < 31 {
            // Pre-31 scanning rides on ACCESS_FINE_LOCATION, which the GPS
            // source already requests.
            return Ok(());
        }

        const PERMS: [&str; 2] = [
            "android.permission.BLUETOOTH_SCAN",
            "android.permission.BLUETOOTH_CONNECT",
        ];

        let mut last_request: Option<Instant> = None;
        loop {
            let activity_raw = self.activity_raw;
            let granted = {
                let activity = unsafe { JObject::from_raw(activity_raw) };
                PERMS.iter().all(|&p| {
                    crate::gps::check_permission(&mut self.env, &activity, p).unwrap_or(false)
                })
            };
            if granted {
                return Ok(());
            }
            // (Re-)request at most every 20s; the GPS thread may be showing
            // its own dialog first, in which case ours can get dropped.
            if last_request.map_or(true, |t| t.elapsed() > Duration::from_secs(20)) {
                last_request = Some(Instant::now());
                self.request_permissions(&PERMS);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn request_permissions(&mut self, perms: &[&str]) {
        let activity_raw = self.activity_raw;
        let perms: Vec<String> = perms.iter().map(|p| p.to_string()).collect();
        let result = self.env.with_local_frame(
            8 + perms.len() as i32,
            |env| -> Result<(), jni::errors::Error> {
                let array =
                    env.new_object_array(perms.len() as i32, "java/lang/String", JObject::null())?;
                for (i, p) in perms.iter().enumerate() {
                    let s = env.new_string(p)?;
                    env.set_object_array_element(&array, i as i32, &s)?;
                }
                let activity = unsafe { JObject::from_raw(activity_raw) };
                env.call_method(
                    &activity,
                    "requestPermissions",
                    "([Ljava/lang/String;I)V",
                    &[JValue::Object(&array), JValue::Int(2)],
                )?;
                Ok(())
            },
        );
        if self.env.exception_check().unwrap_or(false) {
            let _ = self.env.exception_describe();
            let _ = self.env.exception_clear();
        }
        if result.is_err() {
            log::warn!("requestPermissions failed; grant Bluetooth permissions manually");
        }
    }
}
