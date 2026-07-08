# gps-gui-rs

A GPS tracking GUI written in Rust: an interactive slippy map that plots a live
position and the track behind it.

- **GUI**: [egui](https://github.com/emilk/egui) / eframe
- **Map**: [walkers](https://github.com/podusowski/walkers) slippy-map widget
- **Tiles**: OpenStreetMap over HTTP, cached to disk (`.cache/`) so previously
  viewed areas keep rendering offline
- **GPS**: currently a simulated source on a background thread; a BLE
  ([btleplug](https://github.com/deviceplug/btleplug)) source is planned behind
  the same channel interface

## Run

```sh
cargo run
```

The map opens on Greenwich and a simulated fix traces a slow loop. Use
**Center on GPS** to follow the position, **Zoom in/out**, and **Clear track**.

## Architecture

- `src/gps.rs` - GPS source. Produces `GpsFix` values on a background thread and
  sends them over an `mpsc` channel. Swapping in BLE later means replacing
  `spawn_simulated` with a BLE-backed producer feeding the same channel.
- `src/app.rs` - eframe app: drains the channel each frame, holds the tile
  source, map state, current position, and track.
- `src/marker.rs` - a walkers `Plugin` that draws the track polyline and the
  current-position marker.

## Offline maps

HTTP tiles are cached to `.cache/`, so areas you have already viewed load
without a network. For fully offline use, walkers also supports a local tile
directory (`LocalTiles`) or a bundled `.pmtiles` file.
