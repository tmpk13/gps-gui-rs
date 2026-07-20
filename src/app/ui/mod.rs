//! The view layer: page-rendering `impl MyApp` blocks plus the shared egui
//! scaffolding they build on.
//!
//! [`crate::app`] owns the app state; these modules own how it is drawn. The
//! repeated `Area`/`Frame` setup and the color feedback lines live here as
//! functions so each page reads as its own content rather than boilerplate.

mod map;
mod pages;

use crate::app::{MyApp, Page};

/// Icon side length as a fraction of the smaller screen dimension, clamped to
/// this point range. Keeps the toolbar proportional across phone and desktop.
const ICON_SIZE_FRAC: f32 = 0.05;
const ICON_SIZE_MIN: f32 = 40.0;
const ICON_SIZE_MAX: f32 = 70.0;

/// Padding around an icon inside its button, as a fraction of the icon side.
/// The horizontal one is what makes a toolbar button wider than its glyph, so
/// it has to be part of any width budget (see [`icon_size_for_row`]).
const BUTTON_PAD_X_FRAC: f32 = 0.7;
const BUTTON_PAD_Y_FRAC: f32 = 0.45;

/// Inset of the floating corner toggle from the screen edge, as a fraction of
/// the smaller screen dimension.
const CORNER_MARGIN_FRAC: f32 = 0.03;

/// Margin between a content page's body and the screen edge, as a fraction of
/// the smaller screen dimension.
const PAGE_MARGIN_FRAC: f32 = 0.025;

/// The vertical rhythm of the pages, in body-text heights: a hair, a tight
/// gap, the gap between controls, between blocks, and between sections.
///
/// Spacing written in text units follows the font, so a page keeps its
/// proportions on a phone and on a desktop; a fixed point count reads as
/// cramped on one and loose on the other.
const GAP_HAIR: f32 = 0.25;
const GAP_TIGHT: f32 = 0.4;
const GAP_ITEM: f32 = 0.5;
const GAP_BLOCK: f32 = 0.75;
const GAP_SECTION: f32 = 1.0;

/// A text input is a fraction of the screen width, held between these two
/// widths in text units: wide enough to type in on a phone, and not sprawling
/// across a desktop window.
const FIELD_MIN_EM: f32 = 8.0;
const FIELD_MAX_EM: f32 = 22.0;

/// Green "ok" and red "error" used for the feedback lines across the pages.
const OK_GREEN: egui::Color32 = egui::Color32::from_rgb(60, 180, 75);
const ERR_RED: egui::Color32 = egui::Color32::from_rgb(220, 80, 60);

/// Square icon side length in points for the current screen size.
///
/// The clamp is the one measure in the UI that stays absolute: it is a touch
/// target, and a fingertip is the same size whatever the screen is.
fn icon_size_for(screen: egui::Rect) -> f32 {
    (screen.size().min_elem() * ICON_SIZE_FRAC).clamp(ICON_SIZE_MIN, ICON_SIZE_MAX)
}

/// The body text height: the unit the page measures are written in.
fn em(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body)
}

/// Vertical space of `ems` body-text heights.
fn gap(ui: &mut egui::Ui, ems: f32) {
    let space = em(ui) * ems;
    ui.add_space(space);
}

/// Margin between a page's body and the screen edge, in points.
fn page_margin(screen: egui::Rect) -> f32 {
    screen.size().min_elem() * PAGE_MARGIN_FRAC
}

/// Width for a text input: `frac` of the screen width, kept readable.
fn field_width(ui: &egui::Ui, screen: egui::Rect, frac: f32) -> f32 {
    let em = em(ui);
    (screen.width() * frac).clamp(em * FIELD_MIN_EM, em * FIELD_MAX_EM)
}

/// Largest icon side that keeps a row of `count` icon buttons within `avail`
/// points, so no button may claim more than its equal share of the width.
///
/// A button is wider than its icon by [`BUTTON_PAD_X_FRAC`] on each side, and
/// the buttons are separated by `spacing`; `avail` is expected to already have
/// the enclosing frame's margin taken off. The result is capped at the usual
/// [`icon_size_for`] size, so the row only shrinks below it on a screen too
/// narrow to hold the whole set - the share of the width is a ceiling, not a
/// target. [`ICON_SIZE_MIN`] does not apply here: a button that overflows the
/// screen is worse than a small one.
fn icon_size_for_row(screen: egui::Rect, avail: f32, spacing: f32, count: usize) -> f32 {
    let base = icon_size_for(screen);
    if count == 0 {
        return base;
    }
    let count = count as f32;
    let per_button = (avail - (count - 1.0) * spacing) / count;
    let fit = per_button / (1.0 + 2.0 * BUTTON_PAD_X_FRAC);
    base.min(fit.max(1.0))
}

/// A square icon button. The icons are white SVGs tinted to the current text
/// color so they follow the theme.
fn icon_button(ui: &mut egui::Ui, size: f32, source: egui::ImageSource<'_>) -> egui::Response {
    icon_button_pulse(ui, size, source, false)
}

/// Same as [`icon_button`], but when `pulse` is set the button background
/// oscillates red to flag that the action currently has no target (used by the
/// center button when there is no marker to center on).
fn icon_button_pulse(
    ui: &mut egui::Ui,
    size: f32,
    source: egui::ImageSource<'_>,
    pulse: bool,
) -> egui::Response {
    let tint = ui.visuals().text_color();
    let mut button = egui::Button::image(
        egui::Image::new(source)
            .fit_to_exact_size(egui::vec2(size, size))
            .tint(tint),
    );
    if pulse {
        // 0..1 oscillation, one cycle every ~1.6s.
        let t = ui.input(|i| i.time);
        let wave = 0.5 + 0.5 * (t * std::f64::consts::PI * 1.25).sin() as f32;
        let alpha = (60.0 + wave * 150.0) as u8;
        button = button.fill(egui::Color32::from_rgba_unmultiplied(200, 40, 40, alpha));
        // Keep the animation running even when nothing else asks for a repaint.
        ui.ctx().request_repaint();
    }
    ui.add(button)
}

/// A full-screen page: a Background `Area` filled with the panel color, a
/// [`page_margin`] margin, sized to the screen, with the top safe-area inset
/// already skipped. The closure supplies the page's heading and body (and its
/// own `ScrollArea` where one is used).
fn content_page(
    ctx: &egui::Context,
    id: &str,
    screen: egui::Rect,
    top: f32,
    add: impl FnOnce(&mut egui::Ui),
) {
    // `Margin` counts in whole points, so the fraction is rounded once here and
    // the layout inside uses that same rounded value.
    let margin = page_margin(screen) as i8;
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Background)
        .fixed_pos(egui::Pos2::ZERO)
        .movable(false)
        .constrain(false)
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(ui.visuals().panel_fill)
                .inner_margin(egui::Margin::same(margin))
                .show(ui, |ui| {
                    // An Area sizes itself to whatever it held last frame, so
                    // its Ui has no width to wrap against until something pins
                    // one: without this a long label lays out as one endless
                    // line and widens the page instead of wrapping. `set_width`
                    // pins both bounds, which is also what makes the frame
                    // (content plus its two margins) exactly screen-wide.
                    let margin = f32::from(margin);
                    ui.set_width(screen.width() - 2.0 * margin);
                    ui.set_min_height(screen.height() - 2.0 * margin);
                    ui.add_space(top);
                    gap(ui, GAP_ITEM);
                    add(ui);
                });
        });
}

/// A floating popup `Frame` in its own `Area`, used for the transient overlays
/// (selection hint, download confirm/progress, marker info bubble, manual
/// position bar).
fn floating(
    ctx: &egui::Context,
    id: &str,
    order: egui::Order,
    pos: egui::Pos2,
    pivot: egui::Align2,
    constrain: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    egui::Area::new(egui::Id::new(id))
        .order(order)
        .fixed_pos(pos)
        .pivot(pivot)
        .movable(false)
        .constrain(constrain)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| add(ui));
        });
}

/// Show a stored result as a colored line: green on `Ok`, red on `Err`, and
/// nothing on `None`. Used for the config-load and BLE-ack feedback.
fn feedback_label(ui: &mut egui::Ui, feedback: &Option<Result<String, String>>) {
    match feedback {
        Some(Ok(msg)) => {
            ui.colored_label(OK_GREEN, msg);
        }
        Some(Err(msg)) => {
            ui.colored_label(ERR_RED, msg);
        }
        None => {}
    }
}

/// A labeled boolean status row: the label followed by a green "yes" or a red
/// "no", for the Status page's health indicators.
fn status_bool(ui: &mut egui::Ui, label: &str, ok: bool) {
    ui.horizontal(|ui| {
        ui.label(format!("{label}:"));
        let (text, color) = if ok { ("yes", OK_GREEN) } else { ("no", ERR_RED) };
        ui.colored_label(color, text);
    });
}

/// Every page in menu order, each with its label and icon. Drives the page
/// dropdown menu.
fn page_items() -> [(Page, &'static str, egui::ImageSource<'static>); 6] {
    [
        (
            Page::Map,
            "Map",
            egui::include_image!("../../../assets/icons/map.svg"),
        ),
        (
            Page::Points,
            "Points",
            egui::include_image!("../../../assets/icons/points.svg"),
        ),
        (
            Page::Status,
            "Status",
            egui::include_image!("../../../assets/icons/status.svg"),
        ),
        (
            Page::Beacon,
            "Beacon",
            egui::include_image!("../../../assets/icons/beacon.svg"),
        ),
        (
            Page::Settings,
            "Settings",
            egui::include_image!("../../../assets/icons/settings.svg"),
        ),
        (
            Page::Radio,
            "Radio",
            egui::include_image!("../../../assets/icons/radio.svg"),
        ),
    ]
}

impl MyApp {
    /// Dropdown menu to jump straight to any page. The current page is marked.
    /// Rendered inline in the map controls bar and in the floating corner
    /// toggle on other pages. The trigger glyph crossfades from the hamburger
    /// to an X while the menu is open.
    fn page_menu(&mut self, ui: &mut egui::Ui, icon: f32) {
        let text = ui.visuals().text_color();
        // Transparent base image: it reserves the icon-sized hit area and owns
        // the click/menu behavior; the visible glyph is painted on top so it
        // can crossfade between the hamburger and the X.
        let base = egui::Image::new(egui::include_image!("../../../assets/icons/menu.svg"))
            .fit_to_exact_size(egui::vec2(icon, icon))
            .tint(egui::Color32::TRANSPARENT);
        let resp = ui.menu_image_button(base, |ui| {
            // Every measure in the popup is a fraction of the trigger icon,
            // which is itself a fraction of the screen (see `icon_size_for`).
            // A fixed row height reads as a sliver beside a 70pt toolbar on a
            // desktop and crowds the touch targets on a phone.
            let text_size = icon * 0.35;
            // `Button::image_and_text` caps the image at the row height of the
            // button font, so scaling that font is also what lets the row
            // glyphs grow with the screen.
            ui.style_mut().text_styles.insert(
                egui::TextStyle::Button,
                egui::FontId::proportional(text_size),
            );
            ui.spacing_mut().button_padding = egui::vec2(icon * 0.25, icon * 0.2);
            ui.spacing_mut().item_spacing.y = icon * 0.12;
            // Wide enough that the longest label never wraps the rows to
            // different widths.
            ui.set_min_width(icon * 4.0);
            for (page, label, src) in page_items() {
                let image = egui::Image::new(src)
                    .fit_to_exact_size(egui::vec2(text_size, text_size))
                    .tint(ui.visuals().text_color());
                let selected = self.page == page;
                if ui
                    .add(egui::Button::image_and_text(image, label).selected(selected))
                    .clicked()
                {
                    self.page = page;
                    ui.close();
                }
            }
        });

        // `inner` is `Some` only while the menu popup is shown, so it drives the
        // open/close crossfade. `animate_bool_with_time` eases it and keeps
        // requesting repaints until it settles.
        let open = resp.inner.is_some();
        let rect =
            egui::Rect::from_center_size(resp.response.rect.center(), egui::vec2(icon, icon));
        let t = ui
            .ctx()
            .animate_bool_with_time(egui::Id::new("page_menu_icon_anim"), open, 0.15);
        egui::Image::new(egui::include_image!("../../../assets/icons/menu.svg"))
            .tint(text.gamma_multiply(1.0 - t))
            .paint_at(ui, rect);
        egui::Image::new(egui::include_image!("../../../assets/icons/close.svg"))
            .tint(text.gamma_multiply(t))
            .paint_at(ui, rect);

        resp.response.on_hover_text("Pages");
    }

    /// Floating page menu in the top-right corner. Used on every page but the
    /// map, where the menu lives at the right end of the controls bar instead.
    pub(crate) fn page_toggle(&mut self, ctx: &egui::Context, screen: egui::Rect) {
        let size = icon_size_for(screen);
        let top = self.top_inset(ctx);
        // Corner inset as a fraction of the screen, so the button stays clear
        // of the edge on any size (a fixed few points crowds a dense screen).
        let margin = screen.size().min_elem() * CORNER_MARGIN_FRAC;
        egui::Area::new(egui::Id::new("page_toggle"))
            // Float above the (Background) page content it sits over.
            .order(egui::Order::Tooltip)
            .fixed_pos(egui::Pos2::new(screen.right() - margin, top + margin))
            .pivot(egui::Align2::RIGHT_TOP)
            .movable(false)
            .constrain(false)
            .show(ctx, |ui| {
                ui.spacing_mut().button_padding =
                    egui::vec2(size * BUTTON_PAD_X_FRAC, size * BUTTON_PAD_Y_FRAC);
                self.page_menu(ui, size);
            });
    }
}
