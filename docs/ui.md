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
  - `pages.rs` - the non-map pages: Data, Points, Status, Settings, and the
    desktop manual-position bar.

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
  filled with the panel color, a 16pt margin, with the top safe-area inset
  already skipped. The closure supplies the heading and body. Used by Points,
  Status, Settings.
- `background_area(...)` - like `content_page` but with no margin/top spacing,
  for a page that centers its own content (the Data page).
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

Constants at the top of `mod.rs` (`ICON_SIZE_*`, `CORNER_MARGIN_FRAC`) and the
`OK_GREEN` / `ERR_RED` colors are the tuning knobs for sizing and feedback.

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

## Manual position bar (desktop)

With no live GPS source (`gps_rx.is_none()`, i.e. desktop), a bottom-anchored
bar lets a position be typed as "lat, lon". A valid entry feeds the same
`apply_gps_fix` pipeline a real fix would and recenters the map. It is shown on
the Map and Data pages only.

## Platform differences at a glance

Mobile vs desktop is gated on `cfg!(target_os = "android")` and on whether the
live-source channels/insets are present:

- **Zoom**: desktop = wheel + buttons; mobile = pinch (no buttons).
- **GPS**: mobile = live GNSS channel; desktop = manual position bar.
- **Heading-up lock**: mobile locks/centers the view; desktop keeps free pan.
- **Insets**: non-zero on mobile, zero on desktop.
