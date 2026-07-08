//! Map overlay that draws the current GPS position and the track behind it.

use egui::{Color32, Response, Shape, Stroke, Ui};
use walkers::{MapMemory, Plugin, Position, Projector};

const TRACK_COLOR: Color32 = Color32::from_rgb(0, 120, 255);

/// A walkers [`Plugin`] rendering the live position marker and its trail.
///
/// It is rebuilt every frame from the app's state, so it just borrows the data
/// it needs to draw.
pub struct GpsLayer {
    pub current: Option<Position>,
    pub track: Vec<Position>,
}

impl Plugin for GpsLayer {
    fn run(
        self: Box<Self>,
        ui: &mut Ui,
        _response: &Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        // The travelled track as a polyline.
        if self.track.len() >= 2 {
            let points: Vec<_> = self
                .track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.add(Shape::line(points, Stroke::new(3.0, TRACK_COLOR)));
        }

        // The current position on top.
        if let Some(pos) = self.current {
            let screen = projector.project(pos).to_pos2();
            painter.circle_filled(screen, 8.0, TRACK_COLOR);
            painter.circle_stroke(screen, 8.0, Stroke::new(2.0, Color32::WHITE));
        }
    }
}
