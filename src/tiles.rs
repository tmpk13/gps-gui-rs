//! Selectable map tile layers.
//!
//! The standard [`OpenStreetMap`] raster and an [`OpenTopoMap`] topographic
//! raster. Both use the same slippy-map XYZ PNG scheme, so they share the
//! [`walkers::HttpTiles`] widget stack and the on-disk HTTP cache (keyed by
//! URL) with no special handling - the two layers' tiles simply live under
//! different cache keys.

use walkers::sources::{Attribution, OpenStreetMap, TileSource};
use walkers::TileId;

/// OpenTopoMap topographic tiles (<https://opentopomap.org>): OSM data with
/// contour lines and hillshading. Same XYZ PNG scheme as OSM; serves up to
/// zoom 17 (walkers upscales the deepest tile past that).
pub struct OpenTopoMap;

impl TileSource for OpenTopoMap {
    fn tile_url(&self, tile_id: TileId) -> String {
        format!(
            "https://tile.opentopomap.org/{}/{}/{}.png",
            tile_id.zoom, tile_id.x, tile_id.y
        )
    }

    fn attribution(&self) -> Attribution {
        Attribution {
            text: "OpenTopoMap (CC-BY-SA), OpenStreetMap contributors, SRTM",
            url: "https://opentopomap.org/about",
            logo_light: None,
            logo_dark: None,
        }
    }

    fn max_zoom(&self) -> u8 {
        17
    }
}

/// Which tile layer the map is showing. Also drives the offline download and
/// cache lookups so pre-fetching and the offline zoom fallback act on the
/// layer that is on screen (each layer caches under its own URLs).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MapLayer {
    /// Standard OpenStreetMap raster.
    Standard,
    /// OpenTopoMap topographic raster.
    Topo,
}

impl MapLayer {
    /// The tile URL for `tile` on this layer.
    pub fn tile_url(self, tile: TileId) -> String {
        match self {
            MapLayer::Standard => OpenStreetMap.tile_url(tile),
            MapLayer::Topo => OpenTopoMap.tile_url(tile),
        }
    }

    /// Highest zoom the layer's tile server offers.
    pub fn max_zoom(self) -> u8 {
        match self {
            MapLayer::Standard => OpenStreetMap.max_zoom(),
            MapLayer::Topo => OpenTopoMap.max_zoom(),
        }
    }
}
