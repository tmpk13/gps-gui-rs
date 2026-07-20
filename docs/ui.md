# UI architecture

How the GUI is put together, for anyone changing a page or adding a control.
The UI is [egui](https://docs.rs/egui) in immediate mode, driven each frame by
`eframe::App::ui`.

## Module layout

- `src/app.rs` - owns **state** and the per-frame update loop. `MyApp` holds
  every field the UI reads or writes; `eframe::App::ui` drains the input
  channels and then dispatches to one page renderer based on `self.page`.
- `src/app/ui/` - owns **rendering**. It is a submodule of `app`, so its
  `impl MyApp` blocks can reach `MyApp`'s private fields directly.
  - `mod.rs` - shared scaffolding (the helpers every page builds on), the
    layout constants, the page-menu dropdown, and the floating corner toggle.
  - `map.rs` - the interactive map page: the map itself, the controls bar,
    marker info popups, and the offline region-download selection/progress.
  - `pages.rs` - the non-map pages: Points, Status, Beacon, Settings,
    Radio, and the desktop manual-position bar.

The page renderers read state that lives outside the UI too: `src/config.rs`
holds the app's own TOML settings, and `src/radio.rs` holds the WIO-E5
RADIO.TOML model the Radio page edits (see below).

The split is deliberate: `app.rs` reads as state + logic, the `ui` modules read
as "how each page is drawn". Add new state to `MyApp`; add new drawing to a
`ui` module.

## The frame loop

`MyApp::ui` (in `app.rs`) runs once per frame:

1. `drain_sources()` pulls every pending message off the channels (phone GPS,
   compass, BLE beacon events, and the offline zoom-probe result) and updates
   state. Nothing in the render path blocks on IO.
2. It reads the viewport rect (`screen`) and matches on `self.page` to call the
   one page renderer.
3. After the page, it draws the always-on overlays: the corner page toggle (on
   every page but the map), the download-progress readout, and - on desktop
   only - the manual position bar.

Because egui is immediate mode, there is no retained widget tree: each renderer
re-emits its whole page every frame from current state. To change what shows,
change state (usually a `MyApp` field) and let the next frame redraw.

## Layering with Areas and Order

The screen is composed from `egui::Area`s, not panels, so their clip rects can
overrun the screen (needed for the rotating map's overscan). Stacking is
controlled by `egui::Order`, lowest first:

- `Background` - the full-screen page content (the map, or a `content_page`).
- `Middle` - the region-select box-drag layer, so drags draw a box instead of
  panning the map, while the controls above stay clickable.
- `Foreground` - the controls bar, floating popups, the manual GPS bar.
- `Tooltip` - the floating corner page toggle on non-map pages.

Pointer priority follows the same order: a higher layer under the pointer wins
the click. This is why the controls sit in `Foreground` over an interactive
(`Background`) map.

## Shared scaffolding (`mod.rs`)

Every page is built from a few helpers so each renderer reads as its own
content rather than boilerplate:

- `content_page(ctx, id, screen, top, add)` - a full-screen `Background` area
  filled with the panel color, a `PAGE_MARGIN` margin, with the top safe-area
  inset already skipped. The closure supplies the heading and body. Used by
  Points, Status, Beacon, Settings, Radio.

It **pins the body's width** (`set_width`), and that is load-bearing rather
than cosmetic. An `Area` sizes itself to whatever it held last frame, so its
`Ui` has no width to wrap text against: a long label lays out as one endless
line and widens the page instead of wrapping, and it never shrinks back. Pinning
the width to the screen less the two margins is what makes the paragraphs on
Status, Beacon and Settings wrap - and it also makes the frame exactly
screen-wide. Rows that pair a long label with an input use `horizontal_wrapped`
for the same reason: a plain `horizontal` never wraps, so on a phone the input
is pushed off the edge.
- `floating(ctx, id, order, pos, pivot, constrain, add)` - a popup `Frame` in
  its own area, for the transient overlays (selection hint, download confirm
  and progress, marker info bubble, manual position bar).
- `feedback_label` / `status_bool` / `color_swatch` - the small repeated result
  and status rows.
- `icon_button` / `icon_button_pulse` - a square icon button. Icons are white
  SVGs tinted to the current text color, so they follow the light/dark theme.
  `icon_button_pulse` oscillates the background red to flag an action with no
  target (the center button when there is no marker).
- `icon_size_for(screen)` - icon side length as a fraction of the smaller
  screen dimension, clamped, so the toolbar stays proportional on phone and
  desktop.
- `icon_size_for_row(screen, avail, spacing, count)` - the same size, but capped
  so `count` buttons still fit `avail` points: no button may exceed a `1/count`
  share of the width, counting its padding and the gaps between buttons. The
  controls bar counts its buttons before laying any out and sizes itself with
  this, so adding one shrinks the row instead of pushing it off the edge.

Constants at the top of `mod.rs` (`ICON_SIZE_*`, `BUTTON_PAD_*_FRAC`,
`CORNER_MARGIN_FRAC`) and the `OK_GREEN` / `ERR_RED` colors are the tuning knobs
for sizing and feedback.

## Pages and navigation

`Page` (in `app.rs`) is the enum of screens. `page_items()` in `mod.rs` lists
every page with its label and icon, in menu order, and drives both:

- `page_menu` - the hamburger dropdown that switches pages. On the map it sits
  inline at the right end of the controls bar; the trigger glyph crossfades
  between the hamburger and an X while open.
- `page_toggle` - a floating copy of that menu in the top-right corner, drawn
  on every page *except* the map (the map uses the inline one).

To add a page: add a `Page` variant, a `match` arm in `MyApp::ui`, a renderer
`impl MyApp` method, and an entry in `page_items()`.

## Safe-area insets

On Android the status bar and gesture bar overlap the window. `top_inset` and
`bottom_inset` (in `app.rs`) convert the platform-reported insets to egui
points; renderers add that space so content clears the system bars. Both return
`0.0` on desktop, where there are no bars.

## The map page (`map.rs`)

The map is a full-bleed `Background` area so it can overscan past the screen
edges. The key wrinkles:

- **Heading-up rotation.** When enabled and a heading is known, the map is
  drawn into a square `map_rect` sized to the screen diagonal (so the corners
  stay filled at any angle), then every painted shape is rotated about the
  screen center and clipped back to the screen. The drawn angle eases toward
  the live heading each frame (`smoothed_heading`) so it glides. On mobile,
  heading-up also locks the view centered on you (pan becomes a no-op).
  Heading-up is also the only thing that powers the compass: the sensor behind
  `CompassHandle` is off until this button turns it on (see "The compass" below),
  so the button's visibility is keyed off `MyApp::has_direction` - "a heading
  exists *or* a compass could supply one" - rather than off a live reading.
- **The center button.** A plain tap centers on you (falling back to the first
  beacon with no fix yet); holding it - or right-clicking on desktop, both being
  `Response::secondary_clicked` - opens `center_menu_ui`, a list of every marker
  with a known position (`MyApp::center_targets`). Either path goes through
  `MyApp::center_on`, which leaves tracking mode, centers (following the live
  position only for you), and kicks off the offline zoom probe.
- **Beacon heartbeat.** While the BLE link is up, a ring expands out of the
  beacon marker and fades, one beat per `PULSE_PERIOD`. The phase is computed in
  `MyApp::map` and handed to `GpsLayer` as `beacon_pulse`; the animation runs on
  `request_repaint_after(PULSE_FRAME)` rather than a per-frame repaint, so an
  otherwise idle map is not pinned at full frame rate.
- **Tracking mode.** The track button (`tracking_beacon: Option<usize>` is the
  active beacon index) frames the user and a beacon together: `tracking_orientation`
  centers the view on their midpoint, picks a zoom that fits the pair inside the
  screen height less a top/bottom margin (`TRACK_MARGIN_FRAC`), and returns the
  user->beacon bearing that feeds the same rotation easing as heading-up - so the
  beacon rides near the top and the user near the bottom. It reuses
  `smoothed_heading` (tracking bearing and heading-up are mutually exclusive) and
  locks pan/zoom on every platform (the center and zoom are recomputed each
  frame). The track button is the only way in and out: tapping it enters the
  mode, walks along the beacon list, and leaves the mode on the press after the
  last beacon. The heading button is hidden while tracking (which owns the map's
  orientation), as it is whenever no heading source is available at all.
- **Zoom is driven manually.** The map lives in a `Background` area, and
  walkers' built-in zoom only fires when the map is the top interactable layer
  under the pointer - which a background area never is. So walkers' zoom gesture
  is turned off (`zoom_gesture(false)`, `zoom_with_ctrl(false)`) and we drive
  zoom ourselves: mouse-wheel on desktop, pinch on Android (both gated by
  `cfg!(target_os = "android")`). The **+/- zoom buttons are desktop-only**;
  mobile relies on pinch, so the buttons would only crowd the small toolbar.
- **Panning** is by primary-button drag, suppressed while pinching or while a
  download box is being picked.
- **Marker info.** A double-click/tap projects each marker to screen space (the
  same projection + rotation the marker layer draws with) and selects the
  closest one within a hit radius; a miss dismisses the popup.
- **Overlay drawing (`marker.rs`).** `GpsLayer` draws the track, beacon, and the
  line between them. Sizes come from the config `[sizes]` table (each overlay is
  independent). The user->beacon line is dotted when `[distance] dotted` - both
  it and the distance label below are toggleable on the Settings page.
- **The distance label** (`MyApp::distance_label`, units from `[distance]`, shown
  when `[distance] show`) is the one overlay NOT drawn by the plugin. Text needs
  an angle as well as a position, and leaving that to the rotation pass left the
  glyphs level, so the label is painted after the pass with both set outright: it
  projects the user and beacon the same way the plugin does, turns the midpoint
  about the same pivot, and hands the map's angle to the text. It is painted as
  eight offset copies in the contrasting theme color (the outline) under the
  label itself, all sharing one laid-out galley.

## Offline region download flow

`RegionSelect` (in `app.rs`) is the state machine: `Inactive -> Picking ->
Confirm`.

- Started from the **Settings page** ("Offline maps" -> Download region), which
  sets `self.page = Page::Map` and `self.select = Picking`, jumping to the map
  with selection active. (It used to be a toolbar button on the map.) The
  section only appears when tiles are cached to disk (`cache_dir.is_some()`).
- `select_overlay` (Middle order) captures the drag and draws the box; on
  release it unprojects the box corners to lat/lon and moves to `Confirm`.
- `select_ui` (Foreground) shows the "drag a box" hint (with a Cancel button,
  since there is no longer a toolbar toggle to cancel) and then the confirm
  panel: a max-zoom stepper and the tile-count/size estimate, gated by
  `MAX_REGION_TILES`.
- Confirming calls `offline::spawn_download`; `download_ui` shows progress
  floating bottom-left on every page until dismissed.

The actual tiling/fetching lives in `src/offline.rs`; the UI only drives the
selection and shows progress.

## The Settings page (`pages.rs` + `config.rs`)

The app's own TOML settings are edited here, not just loaded. Every widget binds
straight to the live `AppConfig` on `MyApp`, so a change shows on the map at
once; the file is only touched by the buttons.

**The split with the Beacon page is by who owns the setting**, not by subject.
Settings holds what the app owns and can save: the config file itself, the
marker colors and overlay sizes, what the map draws (including the beacon path
and the distance read-out), track recording, and the offline-map download.
Everything the *board* owns, plus the link that reaches it, is on the Beacon
page below. Two beacon-related app settings (`[ble] enabled` and `mac`) live
there anyway, because they decide how the link is made and are useless apart
from it; they repeat the Save button rather than sending you back here for it,
writing the same file and sharing the same `config_feedback` line.

- **Save** (`MyApp::save_config` -> `AppConfig::save`) edits an existing file in
  place with `toml_edit`: comments, key order, and any keys the app knows nothing
  about survive, and only the values it owns are replaced (each keeps the decor
  of the value it replaced). With no file there, it generates a documented one
  from `AppConfig::to_toml`, which doubles as the "generate a config" action.
- **Reset to defaults** drops `AppConfig` back to its built-in defaults in memory
  only; the file is untouched until the next Save, so a Load undoes it.
- **The default path** (`default_config_path`) is the config file beside the tile
  cache, which on Android is the app's private data directory - the working
  directory there can be read but never written, so a bare filename could never
  be saved. On desktop the cache is relative, leaving the plain filename in the
  working directory. It is both what starts loaded and what Save writes back to.
- `[ble] show_path` is the single source of truth for the beacon-path overlay
  (there is no separate runtime flag), and the `mac` input keeps its own text
  buffer, `ble_mac_text`, since the setting itself is an `Option<String>` where
  empty means "scan by service". Changing `enabled`/`mac` takes effect on
  Reconnect, not per keystroke.

## The Beacon page (`pages.rs` + `ble/`)

Everything about the beacon that is not drawing: the link (`ble_link_ui`), the
two connection settings above, the notify interval, and the board's own power
and sleep settings.

### Board power and sleep (`board_power_ui`)

The bottom section of the Beacon page drives the ESP32-C6's own power rail and
deep sleep. Unlike everything above it, **none of it is app state**: the board
holds these in flash and is the authority on them.

- **The board tells us, we do not tell the board.** The worker reads
  `ble::SETTINGS_UUID` on connect and subscribes to it; each blob decodes into
  `BleEvent::Settings` and lands in `MyApp::board_settings`. Every checkbox
  binds to a copy of that blob, so a click sends a write and the checkbox only
  moves once the board reports it moved. This matters because the board changes
  these by itself: it clamps an out-of-range interval.
- **One write in flight.** `MyApp::send_config` sets `ble_ack_pending`, which
  disables the controls until the ack arrives. The text inputs are seeded from
  the first settings blob of a session only, so a later notification cannot
  overwrite something half-typed.
- **A newer firmware is said out loud.** `Settings::decode` returning `None` is
  a layout-version mismatch, which becomes `BleEvent::SettingsUnsupported` and
  hides the controls behind an explanation - never a fall back to defaults the
  board never reported.
- **One sleep control, and it never disarms itself.** The board has a single
  wake-check interval (`CFG_ESP_SLEEP_S`), clamped to 5 s - 5 min. Connecting
  does not clear it, so the app needs no memory of what the board is doing:
  auto-connect is simply `config.ble.enabled`, and `sync_ble_to_config` has no
  special case. The 5 min ceiling is what makes that safe - deep sleep has no
  wake source but the timer, so the ceiling is the longest the board can be out
  of reach, and a wait that long needs no confirmation, no persisted state and
  no way back in beyond waiting.
- **The advertising window has no Disable, unlike the interval next to it.**
  `CFG_ESP_ADV_WINDOW_S` sets how long each wake advertises, clamped to
  3 s - 60 s. The wake-check interval takes 0 to mean "never sleep", which is
  the safe direction; a 0-length window is the opposite, leaving a sleeping
  board unreachable by anything but a physical reset, so the board clamps 0 up
  to the floor and the page offers no button that asks for it. The two controls
  sit together because they are the same decision - the interval and the window
  are the duty cycle, and so the battery life - but only one of them can be
  turned off.
- **Text that quotes a board value reads it from the board.** The window used
  to be a fixed 15 s and several strings said so; now that it is configurable
  only the strings shown while connected quote `adv_window_s`, and the ones
  shown while trying to connect describe the window without a number, because
  at that point the app has no live value to quote.
- **The link is three explicit buttons, not a toggle** (`ble_link_ui`,
  `MyApp::ble_intent`). Connect / Connect to sleeping / Disconnect map one to
  one onto `BleIntent::{Connect, ConnectSleeping, Idle}`, and each button sends
  exactly one `BleCommand`. Disconnect is not a nicety: the board only
  deep-sleeps while nothing is connected, so an app that reconnects on its own
  keeps it awake and its sleep interval never does anything. There was no way
  to express "leave it alone" while connecting was a checkbox the app
  re-applied itself.
- **Buttons must never compose commands.** `drain_commands` empties the whole
  channel in one pass, so a Disconnect queued just before a Connect is
  overwritten and never happens. `set_ble_intent` therefore sends a single
  command per press, and there is no "reconnect" that is secretly two.
- **Intent is session state, not config.** `[ble] enabled` seeds it at startup
  and is never written back, so a Disconnect lasts until the next launch rather
  than quietly becoming a saved preference. The checkbox is labelled for what
  it actually is: connect automatically at startup.
- **Intent survives a connect.** It says what to do when there is *no* link, so
  a board that goes back to sleep is still chased if that is what was asked
  for. Only Disconnect clears it.
- **Two status lines, and they say different things.** `ble_intent_text()` is
  what the app was asked to do; `ble_status` is the worker's running commentary
  on the attempt. Showing only the second was most of why "nothing seems to
  happen" - a scan that is working looks identical to one that is not.
- **`chase` is what makes the two transports behave the same.** Desktop always
  finds its device by scanning, so chasing only changes its status line. The
  Android worker normally shortcuts a pinned MAC straight to `connectGatt`,
  which is a *bounded* attempt - retried on a fixed cycle it can stay out of
  phase with a 15 s window for a very long time. Chasing makes it scan and
  match the address among the hits instead, exactly as desktop does, so it is
  always listening. The shortcut stays for the normal case, where a continuous
  low-latency scan would cost battery for nothing.

## The Radio config page (`pages.rs` + `radio.rs`)

The Radio page loads the WIO-E5 `RADIO.TOML` (the firmware's own config, not the
app's) and edits it in place. The model lives in `src/radio.rs`; the page in
`radio_page`.

- **Model (`radio.rs`).** `RadioDoc` wraps a `toml_edit::DocumentMut` (the source
  of truth for values) plus an ordered `Vec<RadioField>` of the editable
  settings. `toml_edit` is used precisely so a load/edit/save round-trip keeps
  the file's comments and its `<key>_description` help strings; only the edited
  value's text changes (its surrounding whitespace/decor is preserved).
- **Types.** Each field renders with an input matched to its type
  (`FieldType`): a `DragValue` for int/float, a checkbox for bool, a text field
  for a string, and a dropdown for an `Enum`. The type is inferred from the TOML
  value, but a sibling `<key>_type` string overrides it -
  `"int"`/`"float"`/`"bool"`/`"string"`, or `"enum:a,b,c"` for a dropdown. The
  `<key>_description` and `<key>_type` keys are treated as metadata and never
  shown as their own rows.
- **Per-field edit lock.** State is `RadioEdit` (in `app.rs`): `None ->
  Confirm -> Active`. A field is read-only with a pencil button; pressing it
  opens the floating Edit/Cancel confirm popup (`radio_confirm_popup`);
  confirming unlocks the typed input with a check (set, writes the value into the
  document via `RadioDoc::apply`) and an x (cancel). Only one field is in flight
  at a time - the other pencils are disabled while editing.
- **Generating a default.** With nothing loaded, "Generate default config"
  (`MyApp::default_radio` -> `RadioDoc::default_at`) fills the editor from the
  firmware's own `RADIO.example.toml`, `include_str!`d from the sibling
  esp32c6-gps checkout this crate already builds against - so there is no second
  copy of the schema to keep in step. It starts dirty and writes nothing until
  Save, which backs up any existing file first.
- **Backups.** `Save` copies the previous on-disk file into a `radio-backups`
  directory next to it, under a timestamped name, before overwriting. The
  collapsible "Backups" list shows them newest-first; "Restore" loads one back
  into the editor (unsaved until the next Save). The document tracks a `dirty`
  flag, surfaced as `Save *`.

## Manual position bar (desktop)

With no live GPS source (`gps_rx.is_none()`, i.e. desktop), a bottom-anchored
bar lets a position be typed as "lat, lon". A valid entry feeds the same
`apply_gps_fix` pipeline a real fix would and recenters the map. It is shown on
the Map page only.

## The compass (mobile)

`src/compass.rs` reads the NDK rotation-vector sensor on its own thread. The
handle the app holds (`CompassHandle`) is two halves: the heading channel, and a
`wanted` flag the UI sets.

The sensor is **off by default and only enabled while heading-up is on**. The
rotation vector is fused from the accelerometer, gyroscope and magnetometer, so
leaving it running keeps all three awake for a heading nothing is drawing;
tracking mode turns the map by the bearing to the beacon instead, and the marker
arrow falls back to course over ground. `MyApp::sync_compass_power` runs once per
frame in `MyApp::ui`, pushes `heading_up` into the flag, and clears
`compass_heading` on the way down so a reading that has stopped updating is not
left on screen. The sensor thread polls the flag between event reads and
enables/disables accordingly, dropping any events queued across a disable.

The struct is compiled on every target (the app holds an `Option<CompassHandle>`
everywhere); only `compass::spawn` and the thread are Android-only.

## Platform differences at a glance

Mobile vs desktop is gated on `cfg!(target_os = "android")` and on whether the
live-source channels/insets are present:

- **Zoom**: desktop = wheel + buttons; mobile = pinch (no buttons).
- **GPS**: mobile = live GNSS channel; desktop = manual position bar.
- **Heading-up lock**: mobile locks/centers the view; desktop keeps free pan.
- **Compass**: mobile only, and powered only while heading-up is on.
- **Marker list**: opened by a long press on mobile, a right-click on desktop.
- **Insets**: non-zero on mobile, zero on desktop.
