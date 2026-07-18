// Desktop entry point. On Android the crate is built as a cdylib and started
// through `android_main` in lib.rs instead, so this binary is empty there.

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    use gps_gui_rs::{app::MyApp, ble};

    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_title("gps-gui-rs"),
        ..Default::default()
    };

    eframe::run_native(
        "gps-gui-rs",
        options,
        Box::new(|cc| {
            // No live GPS source on desktop yet: the app shows a manual
            // position entry bar (passing `None` here).
            let ble = ble::spawn(cc.egui_ctx.clone());
            let cache_dir = Some(std::path::PathBuf::from(".cache"));
            Ok(Box::new(MyApp::new(
                cc.egui_ctx.clone(),
                None,
                cache_dir,
                None,
                None,
                ble,
            )))
        }),
    )
}

#[cfg(target_os = "android")]
fn main() {}
