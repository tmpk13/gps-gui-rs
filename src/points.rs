//! Recorded GPS points: which source produced them and when. Powers both the
//! map tracks and the searchable points list page.

use std::time::SystemTime;

use walkers::Position;

/// Where a recorded point came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PointSource {
    /// The phone's own GNSS (a simulated loop on desktop).
    Phone,
    /// The ESP32-C3 BLE GPS beacon.
    Esp,
}

impl PointSource {
    /// How the source is named in the points list and its filter.
    ///
    /// The phone is the BLE central of the link the ESP beacon sits on the far
    /// end of, which is the name it goes by everywhere else in the system.
    pub fn label(self) -> &'static str {
        match self {
            PointSource::Phone => "Central",
            PointSource::Esp => "esp",
        }
    }
}

/// One recorded track point.
#[derive(Clone, Copy)]
pub struct TrackPoint {
    pub pos: Position,
    pub source: PointSource,
    pub time: SystemTime,
}

impl TrackPoint {
    /// "lat lon" with 5 decimals (about meter precision); what the points
    /// list shows and what the search matches against.
    pub fn coord_text(&self) -> String {
        format!("{:.5} {:.5}", self.pos.y(), self.pos.x())
    }

    /// Substring search across the source label and the coordinates. `query`
    /// must already be lowercase; the label is folded to match, so searching is
    /// case-insensitive however the labels are capitalized.
    pub fn matches(&self, query: &str) -> bool {
        self.source.label().to_lowercase().contains(query) || self.coord_text().contains(query)
    }
}

/// Compact "how long ago" text for the points list.
pub fn age_text(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or_default().as_secs();
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86400 {
        format!("{} h", secs / 3600)
    } else {
        format!("{} d", secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use walkers::lat_lon;

    fn point(source: PointSource) -> TrackPoint {
        TrackPoint {
            pos: lat_lon(51.4779, -0.0015),
            source,
            time: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn search_matches_source_and_coordinates() {
        let p = point(PointSource::Esp);
        assert!(p.matches(""));
        assert!(p.matches("esp"));
        assert!(p.matches("51.477"));
        assert!(p.matches("-0.0015"));
        assert!(!p.matches("central"));
        assert!(!p.matches("52."));
    }

    #[test]
    fn source_search_ignores_label_case() {
        // The query arrives lowercased; a capitalized label still matches.
        assert!(point(PointSource::Phone).matches("central"));
    }

    #[test]
    fn ages_scale_units() {
        let base = SystemTime::UNIX_EPOCH;
        let at = |secs| base + Duration::from_secs(secs);
        assert_eq!(age_text(at(5), base), "5 s");
        assert_eq!(age_text(at(120), base), "2 min");
        assert_eq!(age_text(at(7200), base), "2 h");
        assert_eq!(age_text(at(200_000), base), "2 d");
        // Clock skew must not panic.
        assert_eq!(age_text(base, at(5)), "0 s");
    }
}
