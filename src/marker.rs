//! Map overlay that draws the current GPS position, heading, the track, and
//! the BLE beacon (ESP32-C3 GPS) with its optional path.

use egui::{Align2, Color32, FontId, Response, Shape, Stroke, Ui, Vec2};
use walkers::{MapMemory, Plugin, Position, Projector};

use crate::config::{DistanceUnits, MarkerColors, MarkerSizes};

/// A walkers [`Plugin`] rendering the live position marker and its trail.
///
/// It is rebuilt every frame from the app's state, so it just borrows the data
/// it needs to draw.
pub struct GpsLayer {
    pub current: Option<Position>,
    pub track: Vec<Position>,
    /// Heading in degrees clockwise from north, if known.
    pub heading: Option<f32>,
    /// Live position of the BLE GPS beacon; a line is drawn to it from the
    /// current position.
    pub beacon: Option<Position>,
    /// The beacon's path so far; drawn dashed when `show_beacon_path` is on.
    pub beacon_track: Vec<Position>,
    pub show_beacon_path: bool,
    /// Colors for the drawn markers, from the loaded config.
    pub colors: MarkerColors,
    /// Independent sizes (points) for each drawn overlay.
    pub sizes: MarkerSizes,
    /// Draw the line to the beacon dotted rather than solid.
    pub distance_dotted: bool,
    /// Label the line to the beacon with the distance.
    pub show_distance: bool,
    /// Unit system for that label.
    pub distance_units: DistanceUnits,
    /// Great-circle distance to the beacon in meters, for the label.
    pub distance_m: Option<f64>,
}

impl Plugin for GpsLayer {
    fn run(
        self: Box<Self>,
        ui: &mut Ui,
        _response: &Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        // The distance label reads in the theme's text color so it stays legible
        // over either base map.
        let text_color = ui.visuals().text_color();
        let painter = ui.painter();
        let track_color = self.colors.track;
        let beacon_color = self.colors.fixed;
        let sizes = self.sizes;

        // The travelled track as a polyline.
        if self.track.len() >= 2 {
            let points: Vec<_> = self
                .track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.add(Shape::line(points, Stroke::new(sizes.track, track_color)));
        }

        // The beacon's path, dashed to tell it apart from the phone track.
        if self.show_beacon_path && self.beacon_track.len() >= 2 {
            let points: Vec<_> = self
                .beacon_track
                .iter()
                .map(|p| projector.project(*p).to_pos2())
                .collect();
            painter.extend(Shape::dashed_line(
                &points,
                Stroke::new(sizes.track, beacon_color),
                8.0,
                6.0,
            ));
        }

        // The beacon marker itself, plus the line from the current position.
        if let Some(beacon) = self.beacon {
            let beacon_screen = projector.project(beacon).to_pos2();
            if let Some(pos) = self.current {
                let screen = projector.project(pos).to_pos2();
                let stroke = Stroke::new(sizes.distance_line, beacon_color);
                if self.distance_dotted {
                    // A dash about as long as the line is wide, with a wider gap,
                    // reads as a row of dots.
                    painter.extend(Shape::dashed_line(
                        &[screen, beacon_screen],
                        stroke,
                        sizes.distance_line,
                        sizes.distance_line * 2.0,
                    ));
                } else {
                    painter.add(Shape::line_segment([screen, beacon_screen], stroke));
                }

                // Distance label centered above the midpoint of the line.
                if self.show_distance {
                    if let Some(m) = self.distance_m {
                        let mid = screen + (beacon_screen - screen) * 0.5;
                        let pad = sizes.distance_text * 0.4 + 4.0;
                        painter.text(
                            mid - Vec2::new(0.0, pad),
                            Align2::CENTER_BOTTOM,
                            self.distance_units.format(m),
                            FontId::proportional(sizes.distance_text),
                            text_color,
                        );
                    }
                }
            }
            painter.circle_filled(beacon_screen, sizes.beacon, beacon_color);
            painter.circle_stroke(beacon_screen, sizes.beacon, Stroke::new(2.0, Color32::WHITE));
        }

        let Some(pos) = self.current else { return };
        let screen = projector.project(pos).to_pos2();

        // Heading arrow (under the dot). North is up, angle increases clockwise.
        if let Some(deg) = self.heading {
            let a = deg.to_radians();
            let dir = Vec2::new(a.sin(), -a.cos());
            let perp = Vec2::new(-dir.y, dir.x);
            let tip = screen + dir * 26.0;
            let base = screen + dir * 8.0;
            painter.add(Shape::convex_polygon(
                vec![tip, base + perp * 7.0, base - perp * 7.0],
                track_color,
                Stroke::NONE,
            ));
        }

        // The current position on top.
        painter.circle_filled(screen, sizes.marker, track_color);
        painter.circle_stroke(screen, sizes.marker, Stroke::new(2.0, Color32::WHITE));
    }
}
