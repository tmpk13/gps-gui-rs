mod app;
mod gps;
mod marker;

use app::MyApp;

fn main() -> eframe::Result<()> {
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
            // The GPS source runs on its own thread and pushes fixes over a
            // channel. Today it is simulated; swapping in a BLE-backed source
            // later only means changing this one line.
            let gps_rx = gps::spawn_simulated(cc.egui_ctx.clone());
            Ok(Box::new(MyApp::new(cc.egui_ctx.clone(), gps_rx)))
        }),
    )
}
