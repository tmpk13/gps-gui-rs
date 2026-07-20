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

use gps_proto::packet::{self, Ack};
pub use gps_proto::packet::PositionPacket;
pub use midair_proto::ble::Settings;
pub use midair_proto::link::Telemetry;

#[cfg(not(target_os = "android"))]
mod desktop;

#[cfg(target_os = "android")]
mod android;

/// One board seen while scanning, for the Beacon page's device picker.
///
/// Every board runs the same firmware and so advertises the same name, which
/// makes `name` near-useless for telling two apart - the address is the
/// identity, and the readable label comes from the nicknames in the app config.
/// `rssi` is what actually distinguishes them in the field: the board in your
/// hand is the loud one.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveredDevice {
    pub address: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

/// Worker -> UI.
pub enum BleEvent {
    /// Human-readable connection state for the Beacon page.
    Status(String),
    /// A board seen during a discovery scan. Sent repeatedly for the same
    /// board as the scan runs, so the signal strength stays current; the UI
    /// keys them by address.
    Discovered(DiscoveredDevice),
    /// Connection state changed; gates the config controls.
    Connected(bool),
    /// A decoded position packet from the beacon.
    Fix(PositionPacket),
    /// A config ack: the device confirmed (or rejected) a setting.
    Ack(Ack),
    /// Board telemetry (LoRa link, GPS, SD) from the esp32c6-gps board.
    Telemetry(Telemetry),
    /// The latest WIO status/log line (ASCII) relayed by the board.
    Log(String),
    /// The board's own view of its power and sleep settings, read on connect
    /// and notified on every change - including changes the board makes by
    /// itself, such as clamping an interval. This, not the UI, is the
    /// authority on what the board is set to.
    Settings(Settings),
    /// The settings blob did not decode: the board's layout version is newer
    /// than this build knows. Its settings are unreadable, not defaulted.
    SettingsUnsupported,
}

/// UI -> worker.
pub enum BleCommand {
    /// Start (or restart) connecting. `mac` pins a specific device; `None`
    /// scans for the first device advertising the GPS service.
    ///
    /// `chase` says the board may be asleep, advertising for only a short
    /// window per wake (configurable on the board, seconds rather than
    /// minutes). That rules out bounded connect attempts, which can keep
    /// missing a window they are out of phase with, so a chasing transport
    /// scans continuously instead - always listening, whenever the window
    /// happens to open.
    Connect { mac: Option<String>, chase: bool },
    /// Scan without connecting, reporting every board that answers as a
    /// [`BleEvent::Discovered`], until a `Connect` or `Disconnect` arrives.
    ///
    /// Separate from the scan a `Connect` does because the two want opposite
    /// things: connecting takes the first board that matches and stops, while
    /// the picker needs to keep looking so a board that advertises late still
    /// turns up in the list. Only one board is ever connected at a time, so
    /// this drops any live link first.
    Scan,
    /// Write one setting to the config characteristic. The device answers on
    /// the ack characteristic with the value it actually applied.
    Config(ConfigWrite),
    /// Drop the connection and stay idle until the next `Connect`.
    Disconnect,
}

/// One config-characteristic write, `[id, len, value...]`. The gps-proto
/// notify interval and the esp32c6-gps board ids share the characteristic and
/// differ only in the id and the width of the value.
#[derive(Clone, Copy, Debug)]
pub enum ConfigWrite {
    /// Position notify interval in ms (gps-proto `CFG_UPDATE_INTERVAL_MS`).
    Interval(u32),
    /// A board on/off setting: the power rail, WIO sleep or GPS backup mode.
    Flag { id: u8, on: bool },
    /// A board interval in seconds: the wake-check cadence.
    Seconds { id: u8, secs: u32 },
}

impl ConfigWrite {
    /// The encoded write and its length.
    pub fn encode(&self) -> ([u8; 6], usize) {
        match *self {
            ConfigWrite::Interval(ms) => {
                packet::encode_config(packet::ConfigCommand::UpdateIntervalMs(ms))
            }
            ConfigWrite::Flag { id, on } => {
                let mut b = [0u8; 6];
                b[0] = id;
                b[1] = 1;
                b[2] = on as u8;
                (b, 3)
            }
            ConfigWrite::Seconds { id, secs } => {
                let mut b = [0u8; 6];
                b[0] = id;
                b[1] = 4;
                b[2..6].copy_from_slice(&secs.to_le_bytes());
                (b, 6)
            }
        }
    }
}

/// Decode a settings blob into the event that describes it. A blob that fails
/// to decode is a version mismatch, which is its own event rather than a
/// silent fall back to defaults the board never reported.
fn settings_event(bytes: &[u8]) -> BleEvent {
    match Settings::decode(bytes) {
        Some(s) => BleEvent::Settings(s),
        None => BleEvent::SettingsUnsupported,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use midair_proto::ble;

    /// The board parses `[id, len, value...]`, so the framing is what has to
    /// be right - a wrong length byte is read as a different value entirely.
    #[test]
    fn flag_and_seconds_framing() {
        let (b, n) = ConfigWrite::Flag {
            id: ble::CFG_PWR_EN,
            on: true,
        }
        .encode();
        assert_eq!(&b[..n], &[ble::CFG_PWR_EN, 1, 1]);

        let (b, n) = ConfigWrite::Flag {
            id: ble::CFG_WIO_SLEEP,
            on: false,
        }
        .encode();
        assert_eq!(&b[..n], &[ble::CFG_WIO_SLEEP, 1, 0]);

        // 300 s = 0x012C, little endian.
        let (b, n) = ConfigWrite::Seconds {
            id: ble::CFG_ESP_SLEEP_S,
            secs: 300,
        }
        .encode();
        assert_eq!(&b[..n], &[ble::CFG_ESP_SLEEP_S, 4, 0x2C, 0x01, 0, 0]);
    }

    #[test]
    fn interval_matches_gps_proto() {
        let (mine, n) = ConfigWrite::Interval(1500).encode();
        let (theirs, m) = packet::encode_config(packet::ConfigCommand::UpdateIntervalMs(1500));
        assert_eq!((&mine[..n], n), (&theirs[..m], m));
    }

    #[test]
    fn settings_event_rejects_a_newer_layout() {
        let good = Settings {
            pwr_en: true,
            sleep_interval_s: 30,
            ..Settings::default()
        };
        assert!(matches!(
            settings_event(&good.encode()),
            BleEvent::Settings(s) if s == good
        ));

        let mut newer = good.encode();
        newer[0] = ble::SETTINGS_VERSION + 1;
        assert!(matches!(
            settings_event(&newer),
            BleEvent::SettingsUnsupported
        ));
    }
}
