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
use midair_proto::ble;
pub use midair_proto::ble::Settings;
pub use midair_proto::link::Telemetry;

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
    /// Board telemetry (LoRa link, GPS, SD) from the esp32c6-gps board.
    Telemetry(Telemetry),
    /// The latest WIO status/log line (ASCII) relayed by the board.
    Log(String),
    /// The board's own view of its power and sleep settings, read on connect
    /// and notified on every change - including changes the board makes by
    /// itself, such as clamping an interval or disarming stow on connect.
    /// This, not the UI, is the authority on what the board is set to.
    Settings(Settings),
    /// The settings blob did not decode: the board's layout version is newer
    /// than this build knows. Its settings are unreadable, not defaulted.
    SettingsUnsupported,
    /// The board acked a stow arm and then dropped the link on purpose. It is
    /// now asleep for `secs`, reachable only in its brief wake windows.
    Stowed { secs: u32 },
}

/// UI -> worker.
pub enum BleCommand {
    /// Start (or restart) connecting. `mac` pins a specific device; `None`
    /// scans for the first device advertising the GPS service.
    ///
    /// `chase` says the board may be asleep, advertising for only about 15 s
    /// per wake. That rules out bounded connect attempts, which can keep
    /// missing a window they are out of phase with, so a chasing transport
    /// scans continuously instead - always listening, whenever the window
    /// happens to open.
    Connect { mac: Option<String>, chase: bool },
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
    /// A board interval in seconds: the wake-check cadence or the stow arm.
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

/// The applied stow interval an ack carries, when it is a stow arm the board
/// accepted. Such an ack is followed by a deliberate disconnect, so both
/// transports watch for it to tell that sleep apart from a dropped link.
fn stow_armed(ack: &Ack) -> Option<u32> {
    if ack.id != ble::CFG_ESP_STOW_S || ack.status != packet::ACK_OK {
        return None;
    }
    ack.value_u32.filter(|&s| s > 0)
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

        // 900 s = 0x0384, little endian.
        let (b, n) = ConfigWrite::Seconds {
            id: ble::CFG_ESP_STOW_S,
            secs: 900,
        }
        .encode();
        assert_eq!(&b[..n], &[ble::CFG_ESP_STOW_S, 4, 0x84, 0x03, 0, 0]);
    }

    #[test]
    fn interval_matches_gps_proto() {
        let (mine, n) = ConfigWrite::Interval(1500).encode();
        let (theirs, m) = packet::encode_config(packet::ConfigCommand::UpdateIntervalMs(1500));
        assert_eq!((&mine[..n], n), (&theirs[..m], m));
    }

    /// Only an accepted, non-zero stow arm predicts the deliberate disconnect.
    /// Anything else that drops the link is a real fault to retry.
    #[test]
    fn stow_armed_only_on_accepted_arm() {
        let ack = |id, status, value| Ack {
            id,
            status,
            value_u32: Some(value),
        };
        assert_eq!(
            stow_armed(&ack(ble::CFG_ESP_STOW_S, packet::ACK_OK, 43200)),
            Some(43200)
        );
        // Disarm: acked, but the board stays awake and connected.
        assert_eq!(stow_armed(&ack(ble::CFG_ESP_STOW_S, packet::ACK_OK, 0)), None);
        assert_eq!(
            stow_armed(&ack(ble::CFG_ESP_STOW_S, packet::ACK_BAD_VALUE, 0)),
            None
        );
        // The wake-check interval sleeps too, but never drops a live link.
        assert_eq!(
            stow_armed(&ack(ble::CFG_ESP_SLEEP_S, packet::ACK_OK, 300)),
            None
        );
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
