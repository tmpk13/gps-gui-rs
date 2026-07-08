use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use walkers::{
    lat_lon, sources::OpenStreetMap, HttpOptions, HttpTiles, Map, MapMemory, Position,
};

use crate::gps::GpsFix;
use crate::marker::GpsLayer;

/// Where the map looks before the first GPS fix arrives.
fn default_position() -> Position {
    lat_lon(51.4779, -0.0015)
}

/// HTTP tile options caching to `cache_dir` (when writable). Tiles fetched once
/// are reused from disk, so previously viewed areas keep working without a
/// network. `None` disables the cache.
fn http_options(cache_dir: Option<PathBuf>) -> HttpOptions {
    HttpOptions {
        cache: cache_dir,
        ..Default::default()
    }
}

pub struct MyApp {
    tiles: HttpTiles,
    map_memory: MapMemory,
    gps_rx: Receiver<GpsFix>,
    current: Option<Position>,
    track: Vec<Position>,
}

impl MyApp {
    /// `cache_dir` is where tiles are cached to disk (`None` to disable). Desktop
    /// passes a local `.cache`; Android passes its writable data directory.
    pub fn new(ctx: egui::Context, gps_rx: Receiver<GpsFix>, cache_dir: Option<PathBuf>) -> Self {
        Self {
            tiles: HttpTiles::with_options(OpenStreetMap, http_options(cache_dir), ctx),
            map_memory: MapMemory::default(),
            gps_rx,
            current: None,
            track: Vec::new(),
        }
    }

    /// Pull every pending fix out of the channel, updating the current position
    /// and appending to the track.
    fn drain_gps(&mut self) {
        while let Ok(fix) = self.gps_rx.try_recv() {
            let pos = lat_lon(fix.lat, fix.lon);
            self.current = Some(pos);
            if self.track.last() != Some(&pos) {
                self.track.push(pos);
            }
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("gps-gui-rs");
            ui.separator();

            if ui.button("Center on GPS").clicked() {
                self.map_memory.follow_my_position();
            }
            if ui.button("Zoom in").clicked() {
                let _ = self.map_memory.zoom_in();
            }
            if ui.button("Zoom out").clicked() {
                let _ = self.map_memory.zoom_out();
            }
            if ui.button("Clear track").clicked() {
                self.track.clear();
            }

            ui.separator();
            match self.current {
                Some(pos) => ui.label(format!("lat {:.5}  lon {:.5}", pos.y(), pos.x())),
                None => ui.label("waiting for GPS fix..."),
            };
        });
    }

    fn map(&mut self, ui: &mut egui::Ui) {
        let my_position = self.current.unwrap_or_else(default_position);

        let layer = GpsLayer {
            current: self.current,
            track: self.track.clone(),
        };

        let map = Map::new(Some(&mut self.tiles), &mut self.map_memory, my_position)
            .with_plugin(layer);

        ui.add(map);
    }
}

impl eframe::App for MyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_gps();

        egui::Panel::top("controls").show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.map(ui);
        });
    }
}
