//! BLE central for the ESP32-C3 GPS beacon (esp32c3-gps firmware).
//!
//! Mirrors the gps source design: a background worker owns the platform BLE
//! stack and talks to the UI over channels. Events flow UI-ward (fixes, acks,
//! status), commands flow worker-ward (connect, config writes). Desktop uses
//! btleplug; Android drives the platform Bluetooth API through a small dex
//! shim loaded at runtime (see android.rs).
//!
//! The wire protocol lives in the shared gps-proto crate.

use std::sync::mpsc::{Receiver, Sender};

use gps_proto::packet::Ack;
pub use gps_proto::packet::PositionPacket;

#[cfg(not(target_os = "android"))]
mod desktop;

#[cfg(target_os = "android")]
mod android;

/// Worker -> UI.
pub enum BleEvent {
    /// Human-readable connection state for the Settings page.
    Status(String),
    /// Connection state changed; gates the config controls.
    Connected(bool),
    /// A decoded position packet from the beacon.
    Fix(PositionPacket),
    /// A config ack: the device confirmed (or rejected) a setting.
    Ack(Ack),
}

/// UI -> worker.
pub enum BleCommand {
    /// Start (or restart) connecting. `mac` pins a specific device; `None`
    /// scans for the first device advertising the GPS service.
    Connect { mac: Option<String> },
    /// Write the position notify interval (ms) to the config characteristic.
    /// The device answers on the ack characteristic.
    SetInterval(u32),
    /// Drop the connection and stay idle until the next `Connect`.
    Disconnect,
}

/// The UI's handle to the BLE worker.
pub struct BleHandle {
    pub events: Receiver<BleEvent>,
    pub commands: Sender<BleCommand>,
}

/// Spawn the BLE worker thread. It starts idle; send [`BleCommand::Connect`]
/// to begin. Desktop signature; Android needs the JVM/Activity pointers.
#[cfg(not(target_os = "android"))]
pub fn spawn(ctx: egui::Context) -> BleHandle {
    desktop::spawn(ctx)
}

/// Spawn the BLE worker thread (Android). `vm`/`activity` are the raw
/// pointers from `AndroidApp`, as with the GPS source.
#[cfg(target_os = "android")]
pub fn spawn(ctx: egui::Context, vm: usize, activity: usize) -> BleHandle {
    android::spawn(ctx, vm, activity)
}
