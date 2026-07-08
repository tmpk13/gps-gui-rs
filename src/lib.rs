pub mod app;
pub mod gps;
pub mod marker;

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

    let mut options = NativeOptions::default();
    options.renderer = Renderer::Wgpu;
    options.android_app = Some(android_app);

    let _ = eframe::run_native(
        "gps-gui-rs",
        options,
        Box::new(|cc| {
            let gps_rx = gps::spawn_simulated(cc.egui_ctx.clone());
            Ok(Box::new(app::MyApp::new(cc.egui_ctx.clone(), gps_rx)))
        }),
    );
}
