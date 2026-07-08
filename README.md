# gps-gui-rs

A GPS tracking GUI written in Rust: an interactive slippy map that plots a live
position and the track behind it.

- **GUI**: [egui](https://github.com/emilk/egui) / eframe
- **Map**: [walkers](https://github.com/podusowski/walkers) slippy-map widget
- **Tiles**: OpenStreetMap over HTTP, cached to disk (`.cache/`) so previously
  viewed areas keep rendering offline
- **GPS**: the phone's built-in GNSS (Android LocationManager over JNI) on
  device; a simulated source on desktop. Both feed the same channel, so an
  external BLE ([btleplug](https://github.com/deviceplug/btleplug)) source could
  slot in the same way

## Run (desktop)

```sh
cargo run
```

The map opens on Greenwich and a simulated fix traces a slow loop. Use
**Center on GPS** to follow the position, **Zoom in/out**, and **Clear track**.

## Run (Android)

One crate builds both: the desktop `[[bin]]` and an Android `cdylib` loaded from
a NativeActivity via `android_main` (`src/lib.rs`). The Android build uses the
`wgpu` renderer, reads the phone's GNSS via LocationManager, and caches tiles to
the app's writable data dir for offline reuse.

Prerequisites: Android SDK + NDK, and `rustup target add aarch64-linux-android`.

```sh
# no-Java flow with xbuild (recommended)
cargo install xbuild
x doctor                              # verify SDK/NDK are found
adb devices -l
x run --release --device adb:<serial> # build APK, install, launch

# or with cargo-apk
cargo install cargo-apk
cargo apk run --release
```

Point `ANDROID_HOME` / `ANDROID_NDK_HOME` at your installs (or let `x doctor`
locate them). Permissions (INTERNET for tiles, LOCATION for GPS) are declared in
`manifest.yaml`, which is what xbuild reads — it ignores Cargo.toml's
`[package.metadata.android]`.

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
