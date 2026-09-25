//! Shared output rendering and the nested winit backend.
//!
//! Builds with the `tty` feature use [`crate::backend_drm`] for bare-TTY
//! sessions; this module supplies the shared overlay and scene renderer.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::{
    Error as OutputDamageTrackerError, OutputDamageTracker, RenderOutputResult,
};
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::gles::{GlesError, GlesRenderer, GlesTarget};
use smithay::backend::renderer::{Color32F, ImportAll, ImportMem};
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::desktop::PopupManager;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::render_elements;
use smithay::utils::{Physical, Point, Transform};
use std::cell::RefCell;
use std::time::{Duration, Instant};

use crate::state::State;
use crate::text::TextImage;
use crate::theme;

/// Placeholder refresh rate (millihertz) reported for the output.
const REFRESH_MHZ: i32 = 60_000;

/// Redraw interval (~60 Hz). Placeholder pacing until frame scheduling exists.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Panel fade-in duration (opacity ramp when the launcher or settings panel
/// opens).
const PANEL_FADE: Duration = Duration::from_millis(120);

// Overlay colors are read at render time from `state.theme` (the configurable
// palette); only opacities and geometry are fixed here.

/// Alt+Tab: an accent border drawn around the selected window's tile.
const SWITCH_BORDER: i32 = 2;
const SWITCH_BORDER_ALPHA: f32 = 0.95;

/// Launcher dropdown opacity (geometry lives in `crate::layout`).
const LAUNCHER_PANEL_ALPHA: f32 = 0.98;
use crate::layout::{LAUNCHER_LEFT, LAUNCHER_PAD, LAUNCHER_ROW_H, LAUNCHER_WIDTH};

/// Settings panel geometry (width and inner padding live in `crate::layout`).
const CARET_BLINK: Duration = Duration::from_millis(1000);

/// Panel text height (logical px) and inner horizontal padding.
const BAR_TEXT_PX: f32 = 14.0;
const BAR_PAD: i32 = theme::SPACE_3;

thread_local! {
    /// Caches the rasterized clock glyphs so the panel time is re-rendered
    /// only when the displayed string changes (~once a minute), not every frame.
    static CLOCK_CACHE: RefCell<ClockCache> = const { RefCell::new(ClockCache { key: None, image: None }) };
}

/// The last-rendered clock string and its rasterized pixels.
struct ClockCache {
    key: Option<String>,
    image: Option<TextImage>,
}

// Compositor-drawn overlay elements rendered above the window space: client
// popups, the launcher/switcher, and the cursor. The wallpaper is also an
// `OverlayElement`, appended last (bottom-most) by `render_scene`.
render_elements! {
    pub OverlayElement<R> where R: ImportAll + ImportMem;
    Surface = WaylandSurfaceRenderElement<R>,
    Solid = SolidColorRenderElement,
    Text = MemoryRenderBufferRenderElement<R>,
}

/// Build an overlay element from rasterized text at logical `(x, y)`, drawn at
/// the given `alpha` (1.0 = opaque; used for fade animations).
fn text_element(
    renderer: &mut GlesRenderer,
    image: &TextImage,
    x: i32,
    y: i32,
    alpha: f32,
) -> Option<OverlayElement<GlesRenderer>> {
    let buffer = MemoryRenderBuffer::from_slice(
        &image.data,
        Fourcc::Argb8888,
        (image.width, image.height),
        1,
        Transform::Normal,
        None,
    );
    let element = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (x as f64, y as f64),
        &buffer,
        Some(alpha),
        None,
        None,
        Kind::Unspecified,
    )
    .ok()?;
    Some(OverlayElement::Text(element))
}

/// Initialize the nested winit backend, advertise its window as a `wl_output`,
/// and drive rendering from the event loop. Returns the [`Output`] so it can be
/// stored in [`State`].
///
/// Errors if no host display is available: the compositor must currently run
/// nested inside an X11 or Wayland session.
pub fn init_winit(
    handle: &LoopHandle<'static, State>,
    display_handle: &DisplayHandle,
) -> Result<Output, Box<dyn std::error::Error>> {
    let (mut backend, mut winit) = winit::init::<GlesRenderer>()?;
    let size = backend.window_size();

    // Advertise the winit window to clients as an output.
    let output = Output::new(
        "winit-0".to_string(),
        PhysicalProperties {
            size: (0, 0).into(), // physical size is unknown for a nested window
            subpixel: Subpixel::Unknown,
            make: "xfar".to_string(),
            model: "winit".to_string(),
        },
    );
    let mode = Mode {
        size,
        refresh: REFRESH_MHZ,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    output.create_global::<State>(display_handle);

    tracing::info!(
        "winit output '{}' ready: {}x{} @ {}Hz",
        output.name(),
        size.w,
        size.h,
        REFRESH_MHZ / 1000
    );

    // The winit EGL surface has a bottom-left origin, so the composited output
    // must be flipped vertically. We do this on the render side (Flipped180)
    // rather than on the `wl_output` transform, so clients still see a normal
    // output. Recreated on resize (below).
    let mut damage_tracker = OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Flipped180);
    let start = Instant::now();

    handle.insert_source(Timer::immediate(), move |_, _, state: &mut State| {
        // Pump host window-system events.
        winit.dispatch_new_events(|event| match event {
            WinitEvent::CloseRequested => {
                tracing::info!("output window closed; stopping");
                state.running = false;
            }
            WinitEvent::Resized { size, scale_factor } => {
                let mode = Mode {
                    size,
                    refresh: REFRESH_MHZ,
                };
                let scale = Scale::Integer(scale_factor.round().max(1.0) as i32);
                state
                    .output
                    .change_current_state(Some(mode), None, Some(scale), None);
                state.output.set_preferred(mode);
                damage_tracker =
                    OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Flipped180);
                tracing::debug!("output resized to {}x{}", size.w, size.h);
            }
            WinitEvent::Input(event) => {
                crate::input::process_input_event(state, event);
            }
            _ => {}
        });

        // Keep the space's bookkeeping current, advance any in-progress session
        // restore, drop stale launch frames, then composite it to the window.
        state.advance_restore();
        state.sweep_launching();
        state.space.refresh();
        if let Err(err) = render(&mut backend, &mut damage_tracker, state) {
            tracing::error!("render failed: {err}");
        }

        // Tell clients they may draw their next frame (drives toolkit redraws).
        let now = start.elapsed();
        for window in state.space.elements() {
            window.send_frame(&state.output, now, None, |_surface, _states| {
                Some(state.output.clone())
            });
        }

        TimeoutAction::ToDuration(FRAME_INTERVAL)
    })?;

    Ok(output)
}

/// Build overlay elements above the window space, ordered front-to-back:
/// settings panel, persistent panel, launcher results, Alt+Tab border, launch
/// placeholders, and client popups. Shared by the winit and DRM backends.
pub(crate) fn overlay_elements(
    renderer: &mut GlesRenderer,
    state: &State,
) -> Vec<OverlayElement<GlesRenderer>> {
    // Popups sit just above their windows; the Alt+Tab highlight goes above
    // those; the launcher dropdown and panel are prepended so the panel
    // remains above them.
    let mut overlay: Vec<OverlayElement<GlesRenderer>> = collect_popup_elements(renderer, state)
        .into_iter()
        .map(OverlayElement::Surface)
        .collect();
    overlay.extend(launching_elements(renderer, state));
    if let Some(switcher) = &state.switcher {
        overlay.splice(0..0, switcher_highlight(state, switcher));
    }
    // Position launcher results beside the panel. Fullscreen hides the panel so
    // the window covers the output.
    if state.launcher.open {
        let mut dropdown = launcher_dropdown(renderer, state);
        dropdown.append(&mut overlay);
        overlay = dropdown;
    }
    if !state.fullscreen_active() {
        let mut bar = top_bar(renderer, state);
        bar.append(&mut overlay);
        overlay = bar;
    }
    // The settings panel is the front-most overlay while it is open.
    if state.settings.open {
        let mut panel = settings_panel(renderer, state);
        panel.append(&mut overlay);
        overlay = panel;
    }
    overlay
}

/// Build the persistent panel. Elements are front-to-back: text and indicators,
/// then the panel background.
fn top_bar(renderer: &mut GlesRenderer, state: &State) -> Vec<OverlayElement<GlesRenderer>> {
    let width = state
        .output
        .current_mode()
        .map(|mode| mode.size.w)
        .unwrap_or(0);
    let height = state
        .output
        .current_mode()
        .map(|mode| mode.size.h)
        .unwrap_or(0);
    let bar_h = crate::layout::BAR_HEIGHT;
    let origin = crate::layout::bar_offset(height, state.panel_bottom);
    let bar_y = origin.y;
    let mut elements = Vec::new();

    if let Some(text) = &state.text {
        // Left: the search field. When idle it shows a dimmed "Search"
        // placeholder; when focused the placeholder is gone and a blinking
        // cursor follows the (possibly empty) query. The region is clickable.
        if state.launcher.open {
            let mut cursor_x = BAR_PAD;
            if !state.launcher.query.is_empty() {
                if let Some(image) =
                    text.render_line(&state.launcher.query, BAR_TEXT_PX, state.theme.text)
                {
                    let y = bar_y + (bar_h - image.height) / 2;
                    if let Some(element) = text_element(renderer, &image, BAR_PAD, y, 1.0) {
                        elements.push(element);
                    }
                    cursor_x = BAR_PAD + image.width + 2;
                }
            }
            if cursor_visible(state) {
                let cursor_h = BAR_TEXT_PX as i32 + 2;
                let y = bar_y + (bar_h - cursor_h) / 2;
                elements.push(solid_element(
                    2,
                    cursor_h,
                    cursor_x,
                    y,
                    state.theme.accent,
                    1.0,
                ));
            }
        } else if let Some(image) = text.render_line("Search", BAR_TEXT_PX, state.theme.placeholder)
        {
            let y = bar_y + (bar_h - image.height) / 2;
            if let Some(element) = text_element(renderer, &image, BAR_PAD, y, 1.0) {
                elements.push(element);
            }
        }

        // Right: the clock, rasterized only when its minute changes, right-
        // aligned within its reserved area.
        let clock = clock_string();
        let clock_element = CLOCK_CACHE.with(|cell| {
            let mut cache = cell.borrow_mut();
            if cache.key.as_deref() != Some(clock.as_str()) {
                cache.image = text.render_line(&clock, BAR_TEXT_PX, state.theme.clock);
                cache.key = Some(clock);
            }
            let image = cache.image.as_ref()?;
            let x = width - image.width - BAR_PAD;
            let y = bar_y + (bar_h - image.height) / 2;
            text_element(renderer, image, x, y, 1.0)
        });
        if let Some(element) = clock_element {
            elements.push(element);
        }
    }

    // Workspace pips: full-height squares, the active one in the accent color.
    // Geometry is shared with click hit-testing (`layout`). A single workspace has
    // nothing to switch to and draws no pips.
    if state.workspaces.len() > 1 {
        for (i, rect) in crate::layout::bar_pip_rects(width, state.workspaces.len())
            .iter()
            .enumerate()
        {
            let color = if i == state.active_workspace {
                state.theme.vdesktop_active
            } else {
                state.theme.vdesktop
            };
            elements.push(solid_element(
                rect.size.w,
                rect.size.h,
                rect.loc.x,
                bar_y + rect.loc.y,
                color,
                1.0,
            ));
        }
    }

    // Panel background; the adjacent window gap provides separation.
    elements.push(solid_element(
        width,
        bar_h,
        0,
        bar_y,
        state.theme.panel,
        1.0,
    ));
    elements
}

/// Whether the search cursor is in its visible blink phase (~500ms on/off),
/// timed from when the launcher last opened so it starts visible.
fn cursor_visible(state: &State) -> bool {
    state
        .launcher_opened_at
        .map(|t| (t.elapsed().as_millis() / 500) % 2 == 0)
        .unwrap_or(true)
}

/// The panel clock string in local time, e.g. `"Sun 21 Sep 14:05"`.
fn clock_string() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let Some(tm) = crate::state::local_tm() else {
        return String::new();
    };
    let day = DAYS.get(tm.tm_wday.rem_euclid(7) as usize).unwrap_or(&"");
    let month = MONTHS.get(tm.tm_mon.clamp(0, 11) as usize).unwrap_or(&"");
    format!(
        "{day} {:02} {month} {:02}:{:02}",
        tm.tm_mday, tm.tm_hour, tm.tm_min
    )
}

/// Composite the space's windows (over the background) to the output and present.
fn render(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: &mut OutputDamageTracker,
    state: &State,
) -> Result<(), Box<dyn std::error::Error>> {
    let age = backend.buffer_age().unwrap_or(0);
    let damage;
    {
        let (renderer, mut framebuffer) = backend.bind()?;
        let overlay = overlay_elements(renderer, state);
        let result = render_scene(
            renderer,
            &mut framebuffer,
            state,
            age,
            overlay,
            damage_tracker,
            state.theme.background,
        )?;
        // Copy the damage so the borrow of `damage_tracker` ends before submit.
        damage = result.damage.cloned();
    }
    // Only present when something actually changed.
    if let Some(damage) = damage {
        backend.submit(Some(&damage))?;
    }
    Ok(())
}

/// Composite overlays, the window space, and the wallpaper (bottom-most) into
/// `framebuffer`. `smithay::desktop::space::render_output` cannot express a
/// wallpaper, because custom elements always render *above* the space — so like
/// the DRM backend we build a flat front-to-back element list (overlays, then
/// window surfaces top-to-bottom, then the wallpaper) and hand it to the damage
/// tracker, which renders it back-to-front.
pub(crate) fn render_scene<'d>(
    renderer: &mut GlesRenderer,
    framebuffer: &mut GlesTarget<'_>,
    state: &State,
    age: usize,
    overlay: Vec<OverlayElement<GlesRenderer>>,
    damage: &'d mut OutputDamageTracker,
    clear: Color32F,
) -> Result<RenderOutputResult<'d>, OutputDamageTrackerError<GlesError>> {
    let mut elements: Vec<OverlayElement<GlesRenderer>> = Vec::with_capacity(overlay.len() + 1);
    elements.extend(overlay);
    add_window_elements(renderer, state, &mut elements);
    if let Some(wall) = wallpaper_element(renderer, state) {
        elements.push(wall);
    }
    damage.render_output(renderer, framebuffer, age, &elements, clear)
}

/// The wallpaper as a bottom-most render element (or `None` when none is
/// configured or was loaded — the solid background shows instead). The buffer
/// covers the full screen, so its position is always the origin.
pub(crate) fn wallpaper_element(
    renderer: &mut GlesRenderer,
    state: &State,
) -> Option<OverlayElement<GlesRenderer>> {
    let buffer = state.wallpaper.buffer.as_ref()?;
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (0.0, 0.0),
        buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    )
    .ok()
    .map(OverlayElement::Text)
}

/// Append each open window's render elements (front-to-back) to `elements`. The
/// window's buffer top-left sits at its position minus its geometry origin
/// (client-side decorations extend above/left of the geometry).
pub(crate) fn add_window_elements(
    renderer: &mut GlesRenderer,
    state: &State,
    elements: &mut Vec<OverlayElement<GlesRenderer>>,
) {
    for window in state.space.elements().rev() {
        let loc = state.space.element_location(window).unwrap_or_default();
        let render_loc = loc - window.geometry().loc;
        let phys: Point<i32, Physical> = (render_loc.x, render_loc.y).into();
        let surfaces: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            window.render_elements(renderer, phys, smithay::utils::Scale::from(1.0), 1.0);
        elements.extend(surfaces.into_iter().map(OverlayElement::Surface));
    }
}

/// Build the "opening …" frames: a dim panel with the launched command label
/// at each reserved grid cell of a not-yet-mapped app. The placeholder appears
/// only after [`crate::state::LAUNCH_CELL_AFTER`] (fast apps are already
/// mapped) and the real window replaces it the moment it maps, because its
/// grid cell is identical.
fn launching_elements(
    renderer: &mut GlesRenderer,
    state: &State,
) -> Vec<OverlayElement<GlesRenderer>> {
    let Some(text) = &state.text else {
        return Vec::new();
    };
    let now = Instant::now();
    let mut elements = Vec::new();
    for (command, since, rect) in state.launching_cells() {
        let age = now.duration_since(since);
        if age < crate::state::LAUNCH_CELL_AFTER || age >= crate::state::LAUNCH_CELL_TIME {
            continue;
        }
        elements.push(solid_element(
            rect.size.w,
            rect.size.h,
            rect.loc.x,
            rect.loc.y,
            state.theme.panel,
            0.7,
        ));
        // The command, centered and truncated so it stays inside the cell.
        let label = truncate_to_fit(&command, text, state.theme.placeholder, rect.size.w);
        if let Some(image) = text.render_line(&label, BAR_TEXT_PX, state.theme.placeholder) {
            if let Some(element) = text_element(
                renderer,
                &image,
                rect.loc.x + (rect.size.w - image.width) / 2,
                rect.loc.y + (rect.size.h - image.height) / 2,
                1.0,
            ) {
                elements.push(element);
            }
        }
    }
    elements
}

/// Build the Alt+Tab highlight: an accent border drawn around the currently
/// selected candidate window's tile, so cycling shows *which* window will be
/// focused in place, rather than a separate row of items to decode.
fn switcher_highlight(
    state: &State,
    switcher: &crate::state::Switcher,
) -> Vec<OverlayElement<GlesRenderer>> {
    let Some(window) = switcher.candidates.get(switcher.selected) else {
        return Vec::new();
    };
    let Some(geo) = state.space.element_geometry(window) else {
        return Vec::new();
    };
    let (x, y, w, h) = (geo.loc.x, geo.loc.y, geo.size.w, geo.size.h);
    let t = SWITCH_BORDER.min(w).min(h);
    let accent = state.theme.switch_border;
    // Four edge strips forming a ring inside the window rectangle.
    vec![
        solid_element(w, t, x, y, accent, SWITCH_BORDER_ALPHA), // top
        solid_element(w, t, x, y + h - t, accent, SWITCH_BORDER_ALPHA), // bottom
        solid_element(t, h, x, y, accent, SWITCH_BORDER_ALPHA), // left
        solid_element(t, h, x + w - t, y, accent, SWITCH_BORDER_ALPHA), // right
    ]
}

/// Build a solid-color overlay rectangle.
fn solid_element(
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    color: Color32F,
    alpha: f32,
) -> OverlayElement<GlesRenderer> {
    let buffer = SolidColorBuffer::new((w, h), color);
    OverlayElement::Solid(SolidColorRenderElement::from_buffer(
        &buffer,
        (x, y),
        1.0,
        alpha,
        Kind::Unspecified,
    ))
}

/// Build the ranked launcher results beside the panel's search field.
fn launcher_dropdown(
    renderer: &mut GlesRenderer,
    state: &State,
) -> Vec<OverlayElement<GlesRenderer>> {
    let launcher = &state.launcher;
    let Some(text) = &state.text else {
        return Vec::new();
    };
    let rows = launcher.results.len() as i32;
    if rows == 0 {
        return Vec::new();
    }
    // Align results with the search field.
    let height = state
        .output
        .current_mode()
        .map(|mode| mode.size.h)
        .unwrap_or(0);
    let px = LAUNCHER_LEFT;
    let panel = crate::layout::launcher_panel_rect(height, rows as usize, state.panel_bottom);
    let (py, panel_h) = (panel.loc.y, panel.size.h);

    // Fade the panel in when it opens (a calm ease-out over PANEL_FADE).
    let fade = state
        .launcher_opened_at
        .map(|t| crate::anim::ease_out_cubic(crate::anim::progress(t.elapsed(), PANEL_FADE)))
        .unwrap_or(1.0);

    // Elements are ordered front-to-back (index 0 is topmost): text first,
    // selection highlight next, panel background last.
    let mut elements = Vec::new();

    for (i, entry) in launcher.results.iter().enumerate() {
        let row_y = py + LAUNCHER_PAD + i as i32 * LAUNCHER_ROW_H;
        // Match the panel's font size and center each row vertically.
        if let Some(image) = text.render_line(entry.label(), BAR_TEXT_PX, state.theme.text) {
            let ty = row_y + (LAUNCHER_ROW_H - image.height) / 2;
            if let Some(element) = text_element(renderer, &image, px + LAUNCHER_PAD, ty, fade) {
                elements.push(element);
            }
        }
        if i == launcher.selection {
            elements.push(solid_element(
                LAUNCHER_WIDTH - LAUNCHER_PAD * 2,
                LAUNCHER_ROW_H,
                px + LAUNCHER_PAD,
                row_y,
                state.theme.accent,
                0.95 * fade,
            ));
        }
    }

    // Panel background (no border — keeps the dropdown flush and borderless).
    elements.push(solid_element(
        LAUNCHER_WIDTH,
        panel_h,
        px,
        py,
        state.theme.panel,
        LAUNCHER_PANEL_ALPHA * fade,
    ));

    elements
}

/// Build the settings panel (`xfar.settings`): a right-anchored, scrollable
/// inventory of the effective configuration. Section headers use the accent
/// color, labels the text color, values the placeholder color; the selected
/// row gets a thin accent underline. Front-to-back: text, highlight, background.
fn settings_panel(renderer: &mut GlesRenderer, state: &State) -> Vec<OverlayElement<GlesRenderer>> {
    let Some(text) = &state.text else {
        return Vec::new();
    };
    let (width, height) = state
        .output
        .current_mode()
        .map(|m| (m.size.w, m.size.h))
        .unwrap_or((0, 0));
    let rect = crate::layout::settings_rect(width, height, state.panel_bottom);
    let view_rows = crate::layout::settings_view_rows(height, state.panel_bottom);
    let panel = &state.settings;
    let last_visible = (panel.top + view_rows).min(panel.len());

    // Fade the panel in when it opens (same ease as the launcher dropdown).
    let fade = panel
        .opened_at
        .map(|t| crate::anim::ease_out_cubic(crate::anim::progress(t.elapsed(), PANEL_FADE)))
        .unwrap_or(1.0);

    let mut elements = Vec::new();

    // The row being edited, if any; its value is shown in the accent color and
    // its caret blinks (driven by `edited_at`, reset on every keystroke).
    let editing = panel.editing_row();
    let caret_on = editing.is_some()
        && panel
            .edited_at
            .map(|t| {
                (t.elapsed().as_millis() % CARET_BLINK.as_millis()) < CARET_BLINK.as_millis() / 2
            })
            .unwrap_or(true);

    for view_i in 0..view_rows {
        let i = panel.top + view_i;
        if i >= last_visible {
            break;
        }
        let y = rect.loc.y + view_i as i32 * crate::settings::SETTINGS_ROW_H;
        let x = rect.loc.x;
        let is_editing_this = editing == Some(i);
        match panel.row(i) {
            Some(crate::settings::Row::Section { label }) => {
                if let Some(image) = text.render_line(label, BAR_TEXT_PX, state.theme.accent) {
                    let ty = y + (crate::settings::SETTINGS_ROW_H - image.height) / 2;
                    if let Some(element) =
                        text_element(renderer, &image, x + crate::layout::SETTINGS_PAD, ty, fade)
                    {
                        elements.push(element);
                    }
                }
            }
            Some(crate::settings::Row::Field(field)) => {
                let mut label_w = 0;
                if let Some(image) = text.render_line(&field.label, BAR_TEXT_PX, state.theme.text) {
                    label_w = image.width;
                    let ty = y + (crate::settings::SETTINGS_ROW_H - image.height) / 2;
                    if let Some(element) =
                        text_element(renderer, &image, x + crate::layout::SETTINGS_PAD, ty, fade)
                    {
                        elements.push(element);
                    }
                }
                // Value: right-aligned, truncated with an ellipsis so a long
                // value (e.g. a wallpaper path) never collides with the label.
                let max_value_w = rect.size.w
                    - crate::layout::SETTINGS_PAD * 2
                    - label_w
                    - crate::layout::SETTINGS_PAD;
                let value_color = if is_editing_this {
                    state.theme.accent
                } else {
                    state.theme.placeholder
                };
                // While editing, the row shows the *draft* (what has been typed),
                // not the last committed value; the caret follows it.
                let shown_value = if is_editing_this {
                    panel.draft()
                } else {
                    &field.value
                };
                let shown = truncate_to_fit(shown_value, text, value_color, max_value_w);
                if let Some(image) = text.render_line(&shown, BAR_TEXT_PX, value_color) {
                    let ty = y + (crate::settings::SETTINGS_ROW_H - image.height) / 2;
                    let vx = x + rect.size.w - crate::layout::SETTINGS_PAD - image.width;
                    // Editing caret: a thin accent bar just after the value. Drawn
                    // in front of the value even when it is absent (empty draft).
                    if is_editing_this && caret_on {
                        let caret_h = crate::settings::SETTINGS_ROW_H - 12;
                        elements.push(solid_element(
                            2,
                            caret_h,
                            vx + image.width + 1,
                            y + (crate::settings::SETTINGS_ROW_H - caret_h) / 2,
                            state.theme.accent,
                            fade,
                        ));
                    }
                    if let Some(element) = text_element(renderer, &image, vx, ty, fade) {
                        elements.push(element);
                    }
                }
            }
            None => {}
        }
        // Selected-row highlight: a thin accent strip under the row.
        if i == panel.selected {
            elements.push(solid_element(
                rect.size.w - crate::layout::SETTINGS_PAD * 2,
                2,
                x + crate::layout::SETTINGS_PAD,
                y + crate::settings::SETTINGS_ROW_H - 2,
                state.theme.accent,
                fade,
            ));
        }
    }

    // Panel background (borderless, like the launcher dropdown).
    elements.push(solid_element(
        rect.size.w,
        rect.size.h,
        rect.loc.x,
        rect.loc.y,
        state.theme.panel,
        LAUNCHER_PANEL_ALPHA * fade,
    ));

    elements
}

/// Shorten `text` so it fits in `max_w` logical px, appending an ellipsis when
/// it would overflow (returning the original when it already fits). The first
/// candidate length is estimated from the full string's measured width — the
/// mono font is near-uniform, so this lands within a character; the loop then
/// trims until the prefix + ellipsis measurably fits.
fn truncate_to_fit(
    text: &str,
    renderer: &crate::text::TextRenderer,
    color: Color32F,
    max_w: i32,
) -> String {
    let ellipsis = '…';
    let width_of = |s: &str| {
        renderer
            .render_line(s, BAR_TEXT_PX, color)
            .map(|i| i.width)
            .unwrap_or(0)
    };
    if text.is_empty() || max_w <= 0 {
        return String::new();
    }
    if width_of(text) <= max_w {
        return text.to_string();
    }
    let full = width_of(text);
    let n = text.chars().count().max(1);
    let mut count = (max_w as f64 / full.max(1) as f64 * n as f64).floor() as usize;
    loop {
        let prefix: String = text.chars().take(count).collect();
        let w = width_of(&prefix) + width_of(&ellipsis.to_string());
        if w <= max_w || count == 0 {
            break;
        }
        count -= 1;
    }
    let prefix: String = text.chars().take(count).collect();
    if prefix.is_empty() {
        ellipsis.to_string()
    } else {
        format!("{prefix}{ellipsis}")
    }
}

/// Build render elements for all popups, positioned over their parent windows.
fn collect_popup_elements(
    renderer: &mut GlesRenderer,
    state: &State,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    let mut elements = Vec::new();
    for window in state.space.elements() {
        let Some(toplevel) = window.toplevel() else {
            continue;
        };
        let window_loc = state.space.element_location(window).unwrap_or_default();
        for (popup, popup_offset) in PopupManager::popups_for_surface(toplevel.wl_surface()) {
            // Popup position = window origin + popup offset within the window,
            // adjusted for the popup surface's own geometry origin.
            let loc = window_loc + window.geometry().loc + popup_offset - popup.geometry().loc;
            elements.extend(render_elements_from_surface_tree(
                renderer,
                popup.wl_surface(),
                (loc.x, loc.y),
                1.0,
                1.0,
                Kind::Unspecified,
            ));
        }
    }
    elements
}
