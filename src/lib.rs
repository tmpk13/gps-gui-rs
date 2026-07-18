pub mod app;
pub mod ble;
pub mod config;
pub mod gps;
pub mod marker;
pub mod offline;
pub mod points;

#[cfg(target_os = "android")]
mod compass;

/// Android entry point.
///
/// The Android activity glue calls the exported `android_main` symbol.
/// Desktop uses `src/main.rs` instead; both build the same `app::MyApp`.
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(android_app: egui_winit::winit::platform::android::activity::AndroidApp) {
    use eframe::{NativeOptions, Renderer};

    android_logger::init_once(
        android_logger::Config::default()
            .with_tag("gps-gui-rs")
            .with_max_level(log::LevelFilter::Info),
    );

    // The app's private data dir is writable, so tiles can be cached there for
    // offline reuse (unlike the working directory).
    let cache_dir = android_app
        .internal_data_path()
        .map(|p| p.join("tile-cache"));
    if let Some(dir) = &cache_dir {
        let _ = std::fs::create_dir_all(dir);
    }

    // Grab the JVM + Activity pointers before `android_app` is moved into the
    // options. Passed as usize so they can cross into the GPS/BLE threads. The
    // Activity (not ndk_context's Application) is needed for requestPermissions.
    let vm_ptr = android_app.vm_as_ptr() as usize;
    let activity_ptr = android_app.activity_as_ptr() as usize;

    // Safe-area insets: `content_rect` is the region inside the system bars.
    // Updates on rotation via the InsetsChanged event, so query it each frame.
    let insets_app = android_app.clone();
    let insets: Option<Box<dyn Fn() -> [f32; 4]>> = Some(Box::new(move || {
        let rect = insets_app.content_rect();
        let (w, h) = insets_app
            .native_window()
            .map(|win| (win.width(), win.height()))
            .unwrap_or((0, 0));
        let top = rect.top.max(0) as f32;
        let left = rect.left.max(0) as f32;
        let right = if w > 0 { (w - rect.right).max(0) as f32 } else { 0.0 };
        let bottom = if h > 0 { (h - rect.bottom).max(0) as f32 } else { 0.0 };
        [top, right, bottom, left]
    }));

    let mut options = NativeOptions::default();
    options.renderer = Renderer::Wgpu;
    options.android_app = Some(android_app);

    let _ = eframe::run_native(
        "gps-gui-rs",
        options,
        Box::new(move |cc| {
            let gps_rx = gps::spawn_android_location(cc.egui_ctx.clone(), vm_ptr, activity_ptr);
            let compass_rx = Some(compass::spawn(cc.egui_ctx.clone()));
            // The BLE worker also loads the dex shim and applies
            // keep-screen-on through it.
            let ble = ble::spawn(cc.egui_ctx.clone(), vm_ptr, activity_ptr);
            Ok(Box::new(app::MyApp::new(
                cc.egui_ctx.clone(),
                Some(gps_rx),
                cache_dir,
                compass_rx,
                insets,
                ble,
            )))
        }),
    );
}
