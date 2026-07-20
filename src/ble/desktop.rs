//! Desktop BLE worker built on btleplug (bluez on Linux).
//!
//! Runs a single-threaded tokio runtime on a dedicated thread. The worker is
//! a reconnect loop: scan (filtered by the GPS service UUID, or pinned to a
//! MAC), connect, subscribe to position + ack notifications, then pump
//! notifications and commands until something breaks, and start over.

use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Duration;

use btleplug::api::{
    Central, CharPropFlags, Manager as _, Peripheral as _, PeripheralProperties, ScanFilter,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use gps_proto::packet::{self, PositionPacket};
use midair_proto::ble;
use midair_proto::link::Telemetry;
use uuid::Uuid;

use super::{settings_event, BleCommand, BleEvent, BleHandle, ConfigWrite, DiscoveredDevice};

const SERVICE_UUID: Uuid = Uuid::from_u128(packet::SERVICE_UUID_U128);
const POSITION_UUID: Uuid = Uuid::from_u128(packet::POSITION_UUID_U128);
const CONFIG_UUID: Uuid = Uuid::from_u128(packet::CONFIG_UUID_U128);
const ACK_UUID: Uuid = Uuid::from_u128(packet::ACK_UUID_U128);
// Board-status characteristics served by the esp32c6-gps board on top of the
// shared gps-proto service. Absent on the older esp32c3 beacon, so treated as
// optional (see `connected`).
const TELEMETRY_UUID: Uuid = Uuid::from_u128(ble::TELEMETRY_UUID_U128);
const LOG_UUID: Uuid = Uuid::from_u128(ble::LOG_UUID_U128);
const SETTINGS_UUID: Uuid = Uuid::from_u128(ble::SETTINGS_UUID_U128);

pub fn spawn(ctx: egui::Context) -> BleHandle {
    let (event_tx, event_rx) = channel();
    let (cmd_tx, cmd_rx) = channel();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = event_tx.send(BleEvent::Status(format!("tokio runtime failed: {e}")));
                ctx.request_repaint();
                return;
            }
        };
        rt.block_on(worker(ctx, event_tx, cmd_rx));
    });

    BleHandle {
        events: event_rx,
        commands: cmd_tx,
    }
}

/// Sends an event and wakes the UI so it drains the channel promptly.
struct Reporter {
    ctx: egui::Context,
    tx: Sender<BleEvent>,
}

impl Reporter {
    fn send(&self, event: BleEvent) -> bool {
        let ok = self.tx.send(event).is_ok();
        self.ctx.request_repaint();
        ok
    }

    fn status(&self, s: impl Into<String>) -> bool {
        self.send(BleEvent::Status(s.into()))
    }
}

/// What the UI currently wants from us. `connect` and `scan` are mutually
/// exclusive: a discovery scan has no link, and a connected session does not
/// scan.
struct Wanted {
    connect: bool,
    /// Run a discovery scan for the device picker; see [`BleCommand::Scan`].
    scan: bool,
    mac: Option<String>,
    /// The board may be asleep; see [`BleCommand::Connect`]. This transport
    /// always finds its device by scanning, so chasing changes nothing about
    /// how it connects - only what it tells the user it is waiting for.
    chase: bool,
}

/// Drain pending commands. Config writes are queued into `writes` so a
/// request made while connected is applied in the pump loop (requests made
/// while disconnected are applied right after the next subscribe).
fn drain_commands(
    cmd_rx: &Receiver<BleCommand>,
    wanted: &mut Wanted,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), ()> {
    loop {
        match cmd_rx.try_recv() {
            Ok(BleCommand::Connect { mac, chase }) => {
                wanted.connect = true;
                wanted.scan = false;
                wanted.mac = mac;
                wanted.chase = chase;
            }
            Ok(BleCommand::Scan) => {
                wanted.connect = false;
                wanted.scan = true;
            }
            Ok(BleCommand::Disconnect) => {
                wanted.connect = false;
                wanted.scan = false;
            }
            Ok(BleCommand::Config(w)) => writes.push(w),
            Err(TryRecvError::Empty) => return Ok(()),
            Err(TryRecvError::Disconnected) => return Err(()),
        }
    }
}

async fn worker(ctx: egui::Context, tx: Sender<BleEvent>, cmd_rx: Receiver<BleCommand>) {
    let report = Reporter { ctx, tx };

    let manager = match Manager::new().await {
        Ok(m) => m,
        Err(e) => {
            report.status(format!("BLE unavailable: {e}"));
            return;
        }
    };

    let mut wanted = Wanted {
        connect: false,
        scan: false,
        mac: None,
        chase: false,
    };
    let mut writes: Vec<ConfigWrite> = Vec::new();

    loop {
        if drain_commands(&cmd_rx, &mut wanted, &mut writes).is_err() {
            return; // UI has gone away.
        }
        if !wanted.connect && !wanted.scan {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }

        let adapter = match manager.adapters().await.ok().and_then(|a| a.into_iter().next()) {
            Some(a) => a,
            None => {
                report.status("no Bluetooth adapter found");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        if wanted.scan {
            if let Err(e) = discover(&adapter, &report, &cmd_rx, &mut wanted, &mut writes).await {
                report.status(format!("{e}; retrying"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            continue;
        }

        // One connect attempt; on any failure fall through, wait, retry.
        match session(&adapter, &report, &cmd_rx, &mut wanted, &mut writes).await {
            Ok(()) => {} // clean disconnect requested by the UI
            Err(e) => {
                report.send(BleEvent::Connected(false));
                report.status(format!("{e}; retrying"));
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Scan without connecting, reporting every board that answers, until the UI
/// asks for something else. Unlike the scan inside [`session`] this never stops
/// at the first hit: the picker wants the whole list, and a board that starts
/// advertising late still has to appear.
///
/// Boards are re-reported for as long as the scan runs rather than only on
/// first sight, so the signal strength the picker shows keeps up with a board
/// being carried around.
async fn discover(
    adapter: &Adapter,
    report: &Reporter,
    cmd_rx: &Receiver<BleCommand>,
    wanted: &mut Wanted,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), String> {
    report.status("scanning for boards...");
    adapter
        .start_scan(ScanFilter {
            services: vec![SERVICE_UUID],
        })
        .await
        .map_err(|e| format!("scan failed: {e}"))?;

    let result = loop {
        if drain_commands(cmd_rx, wanted, writes).is_err() || !wanted.scan {
            break Ok(());
        }
        let peripherals = match adapter.peripherals().await {
            Ok(p) => p,
            Err(e) => break Err(format!("scan failed: {e}")),
        };
        for p in peripherals {
            // The adapter remembers devices from earlier scans, so match on
            // what is being advertised now rather than trusting the cache.
            if let Ok(Some(props)) = p.properties().await {
                if is_beacon(&props) {
                    report.send(BleEvent::Discovered(DiscoveredDevice {
                        address: p.address().to_string(),
                        name: props.local_name.clone(),
                        rssi: props.rssi,
                    }));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };

    let _ = adapter.stop_scan().await;
    if result.is_ok() {
        report.status("scan stopped");
    }
    result
}

/// Scan for the beacon, connect, run one connected session, then always
/// disconnect so the next reconnect starts from clean device state.
async fn session(
    adapter: &Adapter,
    report: &Reporter,
    cmd_rx: &Receiver<BleCommand>,
    wanted: &mut Wanted,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), String> {
    let session_mac = wanted.mac.clone();

    report.status(if wanted.chase {
        "waiting for a wake window..."
    } else {
        "scanning for GPS beacon..."
    });
    let filter = ScanFilter {
        services: vec![SERVICE_UUID],
    };
    adapter
        .start_scan(filter)
        .await
        .map_err(|e| format!("scan failed: {e}"))?;

    // Poll discovered peripherals until one matches (by MAC when pinned,
    // otherwise by advertised service or name).
    let peripheral = loop {
        if drain_commands(cmd_rx, wanted, writes).is_err() || !wanted.connect {
            let _ = adapter.stop_scan().await;
            return Ok(());
        }
        if let Some(p) = find_match(adapter, wanted.mac.as_deref()).await {
            break p;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let _ = adapter.stop_scan().await;

    // Clear any half-open connection left from a previous session. bluez keeps
    // the device object across disconnects; connecting to one it still believes
    // is connected wedges (the central never completes the link) until the
    // process restarts, so force a clean slate first.
    if peripheral.is_connected().await.unwrap_or(false) {
        let _ = peripheral.disconnect().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Run the connected session, then unconditionally disconnect. bluez does
    // not tear the link down for us on error, and a lingering half-open device
    // is exactly what blocks the next reconnect.
    let result = connected(&peripheral, report, cmd_rx, wanted, &session_mac, writes).await;
    let _ = peripheral.disconnect().await;
    report.send(BleEvent::Connected(false));
    result
}

/// Connect to `peripheral`, subscribe, and pump notifications until the UI
/// disconnects (`Ok`) or the link fails (`Err`). The caller disconnects the
/// peripheral afterward regardless of the outcome.
async fn connected(
    peripheral: &Peripheral,
    report: &Reporter,
    cmd_rx: &Receiver<BleCommand>,
    wanted: &mut Wanted,
    session_mac: &Option<String>,
    writes: &mut Vec<ConfigWrite>,
) -> Result<(), String> {
    let addr = peripheral.address();
    report.status(format!("connecting to {addr}..."));
    peripheral
        .connect()
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    peripheral
        .discover_services()
        .await
        .map_err(|e| format!("discovery failed: {e}"))?;

    let chars = peripheral.characteristics();
    let position = chars
        .iter()
        .find(|c| c.uuid == POSITION_UUID && c.properties.contains(CharPropFlags::NOTIFY))
        .cloned()
        .ok_or("position characteristic missing")?;
    let ack = chars
        .iter()
        .find(|c| c.uuid == ACK_UUID)
        .cloned()
        .ok_or("ack characteristic missing")?;
    let config = chars
        .iter()
        .find(|c| c.uuid == CONFIG_UUID)
        .cloned()
        .ok_or("config characteristic missing")?;
    // Optional board-status characteristics (esp32c6-gps only).
    let telemetry = chars
        .iter()
        .find(|c| c.uuid == TELEMETRY_UUID && c.properties.contains(CharPropFlags::NOTIFY))
        .cloned();
    let log = chars
        .iter()
        .find(|c| c.uuid == LOG_UUID && c.properties.contains(CharPropFlags::NOTIFY))
        .cloned();
    let settings = chars.iter().find(|c| c.uuid == SETTINGS_UUID).cloned();

    peripheral
        .subscribe(&position)
        .await
        .map_err(|e| format!("subscribe failed: {e}"))?;
    peripheral
        .subscribe(&ack)
        .await
        .map_err(|e| format!("subscribe failed: {e}"))?;
    if let Some(c) = &telemetry {
        let _ = peripheral.subscribe(c).await;
    }
    if let Some(c) = &log {
        let _ = peripheral.subscribe(c).await;
    }
    // Subscribe before the read below, so a change the board makes between the
    // two (a clamped interval, say) still reaches us.
    if let Some(c) = &settings {
        let _ = peripheral.subscribe(c).await;
    }

    let mut notifications = peripheral
        .notifications()
        .await
        .map_err(|e| format!("notification stream failed: {e}"))?;

    report.send(BleEvent::Connected(true));
    report.status(format!("connected to {addr}"));

    // Populate the board controls from the board itself rather than assuming
    // defaults for settings it holds in flash across power cycles.
    if let Some(c) = &settings {
        match peripheral.read(c).await {
            Ok(v) => {
                report.send(settings_event(&v));
            }
            Err(e) => {
                report.status(format!("settings read failed: {e}"));
            }
        }
    }

    let mut since_check = 0u32;
    loop {
        // Apply queued config writes.
        for w in writes.drain(..) {
            let (buf, n) = w.encode();
            if let Err(e) = peripheral
                .write(&config, &buf[..n], WriteType::WithResponse)
                .await
            {
                report.status(format!("config write failed: {e}"));
            }
        }

        // Wait briefly for a notification, then service commands again.
        match tokio::time::timeout(Duration::from_millis(250), notifications.next()).await {
            Ok(Some(n)) => {
                if n.uuid == POSITION_UUID {
                    if let Some(p) = PositionPacket::decode(&n.value) {
                        report.send(BleEvent::Fix(p));
                    }
                } else if n.uuid == ACK_UUID {
                    if let Some(a) = packet::parse_ack(&n.value) {
                        report.send(BleEvent::Ack(a));
                    }
                } else if n.uuid == TELEMETRY_UUID {
                    if let Some(t) = Telemetry::decode(&n.value) {
                        report.send(BleEvent::Telemetry(t));
                    }
                } else if n.uuid == LOG_UUID {
                    report.send(BleEvent::Log(String::from_utf8_lossy(&n.value).into_owned()));
                } else if n.uuid == SETTINGS_UUID {
                    report.send(settings_event(&n.value));
                }
            }
            Ok(None) => return Err("connection lost".into()),
            Err(_) => {
                // Timeout: periodically confirm the link is still up (the
                // stream does not always end on disconnect).
                since_check += 1;
                if since_check >= 8 {
                    since_check = 0;
                    if !peripheral.is_connected().await.unwrap_or(false) {
                        return Err("connection lost".into());
                    }
                }
            }
        }

        if drain_commands(cmd_rx, wanted, writes).is_err() || !wanted.connect {
            report.status("disconnected");
            return Ok(());
        }
        if wanted.mac != *session_mac {
            // The UI pinned a different device (e.g. a config reload).
            return Err("switching device".into());
        }
    }
}

/// Whether this advertisement is one of our boards: it offers the GPS service,
/// or it goes by the firmware's name.
fn is_beacon(props: &PeripheralProperties) -> bool {
    props.services.contains(&SERVICE_UUID)
        || props.local_name.as_deref() == Some(packet::DEVICE_NAME)
}

/// Find a discovered peripheral matching the pinned MAC (case-insensitive) or,
/// with no MAC, the first board that answers.
async fn find_match(adapter: &Adapter, mac: Option<&str>) -> Option<Peripheral> {
    let peripherals = adapter.peripherals().await.ok()?;
    for p in peripherals {
        if let Some(mac) = mac {
            if p.address().to_string().eq_ignore_ascii_case(mac) {
                return Some(p);
            }
            continue;
        }
        if let Ok(Some(props)) = p.properties().await {
            if is_beacon(&props) {
                return Some(p);
            }
        }
    }
    None
}
