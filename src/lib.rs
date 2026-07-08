pub mod app;
pub mod gps;
pub mod marker;

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
    // options. Passed as usize so they can cross into the GPS thread. The
    // Activity (not ndk_context's Application) is needed for requestPermissions.
    let vm_ptr = android_app.vm_as_ptr() as usize;
    let activity_ptr = android_app.activity_as_ptr() as usize;

    let mut options = NativeOptions::default();
    options.renderer = Renderer::Wgpu;
    options.android_app = Some(android_app);

    let _ = eframe::run_native(
        "gps-gui-rs",
        options,
        Box::new(move |cc| {
            let gps_rx = gps::spawn_android_location(cc.egui_ctx.clone(), vm_ptr, activity_ptr);
            let compass_rx = Some(compass::spawn(cc.egui_ctx.clone()));
            Ok(Box::new(app::MyApp::new(
                cc.egui_ctx.clone(),
                gps_rx,
                cache_dir,
                compass_rx,
            )))
        }),
    );
}
