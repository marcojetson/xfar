//! Bare-TTY DRM/KMS backend enabled by the `tty` feature.
//!
//! Uses libseat, GBM/EGL/GLES, DRM output management, and libinput. Rendering
//! elements are shared with the nested winit backend. The development VM
//! normally has no `/dev/dri`, so this path requires compatible hardware.

use std::error::Error;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderbuffer, GlesRenderer};
use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
use smithay::backend::session::{libseat::LibSeatSession, Event as SessionEvent, Session};
use smithay::backend::udev;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode as CalloopMode, PostAction};
use smithay::reexports::drm::control::{connector, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Buffer as BufferCoord, DeviceFd, Point, Rectangle, Size, Transform};
use smithay::wayland::socket::ListeningSocketSource;

use crate::backend::{overlay_elements, OverlayElement};
use crate::state::State;

/// The concrete DRM compositor type for our single-GPU, single-output setup.
type Compositor =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

/// Colors the primary plane may use (opaque first).
const COLOR_FORMATS: [smithay::reexports::drm::buffer::DrmFourcc; 2] = [
    smithay::reexports::drm::buffer::DrmFourcc::Argb8888,
    smithay::reexports::drm::buffer::DrmFourcc::Xrgb8888,
];

/// Approximate one-frame delay used to reschedule a repaint when a frame
/// produced no damage (so no vblank will arrive to drive the next one).
const REPAINT_DELAY: Duration = Duration::from_millis(16);

/// Hardware rendering resources, kept alongside [`State`] as the event loop's
/// shared data so every event source can reach them.
struct DrmBackend {
    _session: LibSeatSession,
    /// Kept alive so the DRM device/modeset stays valid for the loop's lifetime.
    _drm: DrmDevice,
    renderer: GlesRenderer,
    compositor: Compositor,
    /// Pre-built mouse cursor image (stable, so idle frames stay damage-free).
    cursor: MemoryRenderBuffer,
    start: Instant,
    /// A frame is queued and awaiting its vblank.
    pending: bool,
}

/// Event-loop data for [`State`] and the DRM backend.
struct TtyData {
    state: State,
    backend: DrmBackend,
    /// Capture requests from the launcher's `xfar.screenshot` command: the
    /// next composed frame is rendered to the given path as a PNG.
    capture_rx: std::sync::mpsc::Receiver<String>,
}

/// Publish `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` to the D-Bus and systemd
/// session. Failure is non-fatal.
fn publish_environment(wayland_display: &str) {
    let cmd = format!(
        "command -v dbus-update-activation-environment >/dev/null 2>&1 && \
         dbus-update-activation-environment --systemd \
           WAYLAND_DISPLAY={wayland_display} XDG_RUNTIME_DIR; \
         command -v systemctl >/dev/null 2>&1 && \
         systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR; \
         true"
    );
    if let Err(err) = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .env("WAYLAND_DISPLAY", wayland_display)
        .spawn()
    {
        tracing::debug!("could not publish environment to the session: {err}");
    }
}

/// Run the compositor on real hardware via the DRM/KMS backend.
pub fn run_tty() -> Result<(), Box<dyn Error>> {
    let mut event_loop = EventLoop::<TtyData>::try_new()?;
    let loop_handle = event_loop.handle();

    // --- Session / seat -----------------------------------------------------
    let (mut session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("libseat session active on seat '{seat_name}'");

    // --- Open the primary GPU ----------------------------------------------
    let path = udev::primary_gpu(&seat_name)
        .ok()
        .flatten()
        .unwrap_or_else(|| PathBuf::from("/dev/dri/card1"));
    tracing::info!("using DRM device {}", path.display());
    let fd = session.open(
        &path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (mut drm, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;

    // --- GBM + EGL + GLES ---------------------------------------------------
    let gbm = GbmDevice::new(device_fd.clone())?;
    // SAFETY: Smithay is the only code creating this EGL display, and the GBM
    // device remains alive for the display's lifetime.
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    // SAFETY: this context was created above, is not shared, and is not current
    // on any thread.
    let renderer = unsafe { GlesRenderer::new(egl_context)? };
    let render_formats = renderer.egl_context().dmabuf_render_formats().clone();
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(gbm.clone(), None);

    // --- Pick the connected output + a CRTC + its preferred mode -----------
    let resources = drm.resource_handles()?;
    let connector_info = resources
        .connectors()
        .iter()
        .filter_map(|c| drm.get_connector(*c, false).ok())
        .find(|c| c.state() == connector::State::Connected)
        .ok_or("no connected output")?;
    let connector_name = format!(
        "{:?}-{}",
        connector_info.interface(),
        connector_info.interface_id()
    );
    let drm_mode = *connector_info
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector_info.modes().first())
        .ok_or("output has no modes")?;
    let crtc = connector_info
        .encoders()
        .iter()
        .filter_map(|e| drm.get_encoder(*e).ok())
        .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
        .next()
        .ok_or("no CRTC available for output")?;
    let (mode_w, mode_h) = drm_mode.size();
    tracing::info!("output {connector_name}: {mode_w}x{mode_h} on {crtc:?}");

    // --- Advertise the output ----------------------------------------------
    let display = Display::<State>::new()?;
    let display_handle = display.handle();
    let (phys_w, phys_h) = connector_info.size().unwrap_or((0, 0));
    let output = Output::new(
        connector_name.clone(),
        PhysicalProperties {
            size: (phys_w as i32, phys_h as i32).into(),
            subpixel: Subpixel::Unknown,
            make: "xfar".into(),
            model: connector_name,
        },
    );
    let output_mode = OutputMode {
        size: (mode_w as i32, mode_h as i32).into(),
        refresh: (drm_mode.vrefresh() as i32) * 1000,
    };
    output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);
    output.create_global::<State>(&display_handle);

    // --- DRM surface + compositor ------------------------------------------
    let surface = drm.create_surface(crtc, drm_mode, &[connector_info.handle()])?;
    let compositor: Compositor = DrmCompositor::new(
        &output,
        surface,
        None,
        allocator,
        exporter,
        COLOR_FORMATS,
        render_formats,
        drm.cursor_size(),
        Some(gbm.clone()),
    )?;

    // --- Compositor state ---------------------------------------------------
    let config = crate::config::load().unwrap_or_default();
    let mut state = State::new(&display, output, &config)?;
    tracing::info!("seat '{}' ready (keyboard, pointer)", state.seat.name());

    // DRM capture channel used by `xfar.screenshot`.
    let (capture_tx, capture_rx) = std::sync::mpsc::channel();
    state.screenshot = Some(capture_tx);

    // Wayland socket for clients.
    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    state.socket_name = socket_name.clone();
    publish_environment(&socket_name);
    loop_handle.insert_source(socket, |stream, _, data: &mut TtyData| {
        data.state.register_client(stream);
    })?;
    loop_handle.insert_source(
        Generic::new(display, Interest::READ, CalloopMode::Level),
        |_, display, data: &mut TtyData| {
            // SAFETY: `NoIoDrop` exposes a mutable reference to the wrapped
            // display for this callback. The callback dispatches requests and
            // does not drop or replace the display, so its file descriptor
            // remains valid for the entire call.
            unsafe { display.get_mut() }.dispatch_clients(&mut data.state)?;
            Ok(PostAction::Continue)
        },
    )?;

    // --- Input (libinput) ---------------------------------------------------
    let mut libinput =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|()| "failed to assign libinput seat")?;
    loop_handle.insert_source(
        LibinputInputBackend::new(libinput),
        |event, _, data: &mut TtyData| {
            crate::input::process_input_event::<LibinputInputBackend>(&mut data.state, event);
        },
    )?;

    // --- VBlank: release the presented frame so the next one can be queued --
    loop_handle.insert_source(
        drm_notifier,
        |event, _meta, data: &mut TtyData| match event {
            DrmEvent::VBlank(_crtc) => {
                let _ = data.backend.compositor.frame_submitted();
                data.backend.pending = false;
            }
            DrmEvent::Error(err) => tracing::error!("DRM error: {err}"),
        },
    )?;

    // --- Continuous repaint (~60 Hz), like the winit backend ----------------
    // Rendering is driven by this timer (not just vblank) so newly-mapped
    // windows and animations are always picked up; the `pending` guard plus
    // vblank pacing keep it from outrunning the display.
    loop_handle.insert_source(Timer::immediate(), |_, _, data: &mut TtyData| {
        // Advance any in-progress session restore (spawns the next app once the
        // current one has mapped) and drop stale launch frames before drawing.
        data.state.advance_restore();
        data.state.sweep_launching();
        render(data);
        TimeoutAction::ToDuration(REPAINT_DELAY)
    })?;

    // --- Session activation (VT switch) ------------------------------------
    loop_handle.insert_source(
        session_notifier,
        |event, _, data: &mut TtyData| match event {
            SessionEvent::ActivateSession => {
                tracing::info!("session activated");
                data.backend.pending = false; // resume; the repaint timer renders
            }
            SessionEvent::PauseSession => {
                tracing::info!("session paused");
                data.backend.pending = true; // suppress rendering while inactive
            }
        },
    )?;

    // Claim `XFAR_STARTUP_CMD` before restoring session commands.
    state.spawn_startup_command();
    state.session_restore();

    let mut data = TtyData {
        state,
        backend: DrmBackend {
            _session: session,
            _drm: drm,
            renderer,
            compositor,
            cursor: build_cursor(),
            start: Instant::now(),
            pending: false,
        },
        capture_rx,
    };

    // Kick off the first frame, then let vblanks drive the loop.
    render(&mut data);
    tracing::info!("{} running on '{}'", crate::name(), path.display());

    // Stop cleanly on SIGINT/SIGTERM (or the launcher's Quit action). SIGUSR1
    // writes the current frame to /tmp/xfar-frame.png for remote inspection.
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    let capture_flag = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGUSR1, Arc::clone(&capture_flag))?;
    let signal = event_loop.get_signal();

    event_loop.run(Some(Duration::from_millis(250)), &mut data, move |data| {
        if stop.load(Ordering::Relaxed) || !data.state.running {
            signal.stop();
            return;
        }
        if capture_flag.swap(false, Ordering::Relaxed) {
            capture(data, "/tmp/xfar-frame.png");
        }
        if let Ok(path) = data.capture_rx.try_recv() {
            capture(data, &path);
        }
        let _ = data.state.display_handle.flush_clients();
        data.state.space.refresh();
    })?;

    tracing::info!("shutting down {}", crate::name());
    // Dump the session while windows are still mapped, so the next start
    // relaunches them.
    data.state.session_save();
    Ok(())
}

/// The classic left-pointing arrow cursor as a pixel mask: `X` = black outline,
/// `.` = white fill, space = transparent. Each row is [`CURSOR_W`] chars.
const CURSOR_ART: [&str; 17] = [
    "X          ",
    "XX         ",
    "X.X        ",
    "X..X       ",
    "X...X      ",
    "X....X     ",
    "X.....X    ",
    "X......X   ",
    "X.......X  ",
    "X........X ",
    "X.....XXXXX",
    "X..X..X    ",
    "X.X X..X   ",
    "XX  X..X   ",
    "X    X..X  ",
    "     X..X  ",
    "      XX   ",
];
const CURSOR_W: usize = 11;

/// The DRM backend must draw its own cursor (unlike the nested winit backend,
/// where the host draws it). Build the arrow once into a [`MemoryRenderBuffer`]
/// (premultiplied ARGB8888) so it has a stable identity across frames.
fn build_cursor() -> MemoryRenderBuffer {
    let h = CURSOR_ART.len();
    let mut data = vec![0u8; CURSOR_W * h * 4];
    for (y, row) in CURSOR_ART.iter().enumerate() {
        for (x, ch) in row.chars().enumerate() {
            // Argb8888 little-endian byte order is [B, G, R, A]; opaque here.
            let px = match ch {
                'X' => [0, 0, 0, 255],       // black outline
                '.' => [255, 255, 255, 255], // white fill
                _ => [0, 0, 0, 0],           // transparent
            };
            let i = (y * CURSOR_W + x) * 4;
            data[i..i + 4].copy_from_slice(&px);
        }
    }
    MemoryRenderBuffer::from_slice(
        &data,
        Fourcc::Argb8888,
        (CURSOR_W as i32, h as i32),
        1,
        Transform::Normal,
        None,
    )
}

/// The cursor render element at the current pointer location (topmost).
fn cursor_element(
    renderer: &mut GlesRenderer,
    cursor: &MemoryRenderBuffer,
    state: &State,
) -> Option<OverlayElement<GlesRenderer>> {
    let x = state.pointer_location.x.round();
    let y = state.pointer_location.y.round();
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        (x, y),
        cursor,
        None,
        None,
        None,
        Kind::Cursor,
    )
    .ok()
    .map(OverlayElement::Text)
}

/// Build the full front-to-back element list (overlays above window surfaces)
/// and render + queue a frame. On an empty (no-damage) frame, reschedule a
/// repaint via a short timer since no vblank will arrive to drive the next one.
fn render(data: &mut TtyData) {
    if data.backend.pending {
        return;
    }

    // Cursor first (topmost), then overlays (launcher/switcher/indicator/
    // preview/popups), then window surfaces top-to-bottom, then the wallpaper
    // last (bottom-most) — front-to-back.
    let mut elements = Vec::new();
    if data.state.show_cursor {
        if let Some(cursor) = cursor_element(
            &mut data.backend.renderer,
            &data.backend.cursor,
            &data.state,
        ) {
            elements.push(cursor);
        }
    }
    elements.extend(overlay_elements(&mut data.backend.renderer, &data.state));
    crate::backend::add_window_elements(&mut data.backend.renderer, &data.state, &mut elements);
    if let Some(wall) = crate::backend::wallpaper_element(&mut data.backend.renderer, &data.state) {
        elements.push(wall);
    }

    let clear = data.state.theme.background;
    let result = data.backend.compositor.render_frame(
        &mut data.backend.renderer,
        &elements,
        clear,
        FrameFlags::DEFAULT,
    );

    match result {
        Ok(frame) => {
            // Tell clients they may draw their next frame.
            let now = data.backend.start.elapsed();
            for window in data.state.space.elements() {
                window.send_frame(&data.state.output, now, Some(Duration::ZERO), |_, _| {
                    Some(data.state.output.clone())
                });
            }
            if !frame.is_empty {
                // Present it; the vblank event will release it (frame_submitted).
                // An empty (no-damage) frame is simply skipped — the continuous
                // repaint timer will try again next tick.
                match data.backend.compositor.queue_frame(()) {
                    Ok(()) => data.backend.pending = true,
                    Err(err) => tracing::error!("queue_frame failed: {err}"),
                }
            }
        }
        Err(err) => tracing::error!("render_frame failed: {err}"),
    }
}

/// Render the current scene to an offscreen buffer and write it as a PNG. Used
/// by `SIGUSR1` and `xfar.screenshot`.
fn capture(data: &mut TtyData, path: &str) {
    let Some(size) = data.state.output.current_mode().map(|m| m.size) else {
        return;
    };
    if size.w <= 0 || size.h <= 0 {
        return;
    }

    // Cursor + overlays as custom elements; windows are composited from the
    // space by `render_output` (which handles their geometry offset correctly).
    let mut custom = Vec::new();
    if data.state.show_cursor {
        if let Some(c) = cursor_element(
            &mut data.backend.renderer,
            &data.backend.cursor,
            &data.state,
        ) {
            custom.push(c);
        }
    }
    custom.extend(overlay_elements(&mut data.backend.renderer, &data.state));

    let renderer = &mut data.backend.renderer;
    let buffer_size: Size<i32, BufferCoord> = (size.w, size.h).into();
    let mut target: GlesRenderbuffer = match renderer.create_buffer(Fourcc::Abgr8888, buffer_size) {
        Ok(t) => t,
        Err(err) => return tracing::error!("capture: create_buffer: {err}"),
    };
    let mut framebuffer = match renderer.bind(&mut target) {
        Ok(fb) => fb,
        Err(err) => return tracing::error!("capture: bind: {err}"),
    };
    let mut damage = OutputDamageTracker::new((size.w, size.h), 1.0, Transform::Normal);
    if let Err(err) = crate::backend::render_scene(
        renderer,
        &mut framebuffer,
        &data.state,
        0,
        custom,
        &mut damage,
        data.state.theme.background,
    ) {
        return tracing::error!("capture: render_scene: {err}");
    }

    let region = Rectangle::new(Point::from((0, 0)), buffer_size);
    let mapping = match renderer.copy_framebuffer(&framebuffer, region, Fourcc::Abgr8888) {
        Ok(m) => m,
        Err(err) => return tracing::error!("capture: copy_framebuffer: {err}"),
    };
    let bytes = match renderer.map_texture(&mapping) {
        Ok(b) => b,
        Err(err) => return tracing::error!("capture: map_texture: {err}"),
    };
    // Abgr8888 little-endian byte order is [R, G, B, A] — exactly PNG's RGBA.
    match write_png(path, size.w as u32, size.h as u32, bytes) {
        Ok(()) => tracing::info!("capture: wrote {path}"),
        Err(err) => tracing::error!("capture: png: {err}"),
    }
}

/// Encode tightly packed RGBA8 pixels as a PNG.
fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> Result<(), image::ImageError> {
    let img = image::DynamicImage::ImageRgba8(
        // r/w/h come from the framebuffer we just mapped, so the sizes match.
        image::RgbaImage::from_raw(w, h, rgba.to_vec())
            .expect("capture: framebuffer size and mapped pixels disagree"),
    );
    img.save_with_format(path, image::ImageFormat::Png)
}
