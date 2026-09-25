//! xfar's Wayland compositor and desktop-environment logic.
//!
//! The binary entry point stays thin so compositor logic can be covered by
//! deterministic unit tests.

mod anim;
mod backend;
#[cfg(feature = "tty")]
mod backend_drm;
mod config;
mod help;
mod input;
mod keybinding;
mod launcher;
mod layout;
mod session;
mod settings;
mod state;
mod text;
mod theme;
mod wallpaper;

use std::error::Error;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use crate::state::State;

/// The application name used in logs and diagnostics.
pub fn name() -> &'static str {
    "xfar"
}

/// The compositor version, taken from the crate's Cargo metadata.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Validate the user's configuration file without starting the compositor,
/// reporting every problem found. Returns the process exit code: 0 when the
/// file is valid (or absent — defaults then apply), 1 when it is unusable or
/// contains invalid ranges, bindings, or colors.
pub fn validate_config() -> i32 {
    use config::Config;

    let Some(path) = config::config_path() else {
        println!(
            "xfar: no configuration found (neither XDG_CONFIG_HOME nor HOME set); using defaults"
        );
        return 0;
    };
    if !path.exists() {
        println!("xfar: no config file at {}; using defaults", path.display());
        return 0;
    }

    let mut config: Config = match config::load() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("xfar: {err}");
            return 1;
        }
    };

    let mut failed = 0;
    for problem in config.validate() {
        eprintln!("xfar: config: {problem}");
        failed += 1;
    }
    if let Err(err) = config.keybindings.resolve() {
        eprintln!("xfar: {err}");
        failed += 1;
    }
    if let Err(err) = config.appearance.resolve() {
        eprintln!("xfar: {err}");
        failed += 1;
    }

    if failed == 0 {
        println!("xfar: configuration at {} is valid", path.display());
        0
    } else {
        eprintln!(
            "xfar: configuration at {} has {} problem{}",
            path.display(),
            failed,
            if failed == 1 { "" } else { "s" }
        );
        1
    }
}

/// Initialize structured logging.
///
/// Honors `RUST_LOG` when set. The default keeps xfar and Smithay at `info` but
/// silences Smithay's very verbose EGL/GLES initialization dumps (whole
/// extension lists), so bring-up and shutdown stay legible. Safe to call more
/// than once (subsequent calls are ignored).
fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info")
            .add_directive("smithay::backend::egl=warn".parse().unwrap())
            .add_directive("smithay::backend::renderer::gles=warn".parse().unwrap())
    });
    let _ = fmt().with_env_filter(filter).try_init();
}

/// Start DRM/KMS when the `tty` feature is enabled and neither `WAYLAND_DISPLAY`
/// nor `DISPLAY` is set; otherwise start the nested winit backend.
pub fn run() -> Result<(), Box<dyn Error>> {
    init_logging();
    tracing::info!("starting {} {}", name(), version());

    // On real hardware there is no session to nest inside. When built with the
    // `tty` feature and no X11/Wayland session is present, drive the display
    // directly via the DRM/KMS backend; otherwise run nested via winit (the
    // development path used in the VM).
    #[cfg(feature = "tty")]
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return backend_drm::run_tty();
    }

    run_winit()
}

/// Run nested inside an existing X11/Wayland session using the winit backend
/// (the development path; see `docs/DEVELOPMENT.md`).
fn run_winit() -> Result<(), Box<dyn Error>> {
    // Load user configuration. A missing file is fine (defaults are used); a
    // malformed file is reported clearly and we fall back to defaults so the
    // desktop still starts.
    let mut config = config::load().unwrap_or_else(|err| {
        tracing::error!("{err}; using default configuration");
        config::Config::default()
    });
    // Range-check numeric values; out-of-range ones are reset to defaults and
    // reported (string sections are validated when resolved in `State::new`).
    for problem in config.validate() {
        tracing::error!("config: {problem}");
    }

    let mut event_loop = EventLoop::<State>::try_new()?;
    let display = Display::<State>::new()?;
    let handle = event_loop.handle();

    // Bring up the nested output/renderer backend and advertise its output.
    let output = backend::init_winit(&handle, &display.handle())?;

    let mut state = State::new(&display, output, &config)?;
    tracing::info!("seat '{}' ready (keyboard, pointer)", state.seat.name());

    // Accept client connections on an auto-selected Wayland socket.
    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();
    state.socket_name = socket_name.clone();
    handle.insert_source(socket, |stream, _, state: &mut State| {
        state.register_client(stream);
    })?;

    // Dispatch client requests whenever the display's fd becomes readable.
    handle.insert_source(
        Generic::new(display, Interest::READ, Mode::Level),
        |_, display, state: &mut State| {
            // SAFETY: calloop's `NoIoDrop` only exposes a `&mut` to the wrapped
            // `Display` through this unsafe accessor, so that the polled fd is
            // never dropped from within the callback. We only dispatch client
            // requests and never drop the `Display`, so the fd stays valid.
            unsafe { display.get_mut() }.dispatch_clients(state)?;
            Ok(PostAction::Continue)
        },
    )?;

    // Stop the loop on SIGINT/SIGTERM so the compositor shuts down cleanly.
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;

    tracing::info!(
        "{} ready; clients can connect on wayland socket '{socket_name}'",
        name()
    );
    state.spawn_startup_command();
    state.session_restore();

    while state.running && !stop.load(Ordering::Relaxed) {
        // The render timer wakes the loop ~60x/s, so signals are handled promptly.
        event_loop.dispatch(Some(Duration::from_millis(250)), &mut state)?;
        state.display_handle.flush_clients()?;
    }

    let reason = if stop.load(Ordering::Relaxed) {
        "termination signal"
    } else {
        "output window closed"
    };
    tracing::info!("shutting down {} ({reason})", name());

    // Flush any last events to clients, then return. Teardown then happens by
    // drop, in this order: `state` is dropped first (declared after
    // `event_loop`), removing the globals and seat while the display is still
    // alive; then `event_loop` drops its sources — the listening socket (which
    // removes the socket file), the display (disconnecting remaining clients),
    // and the winit backend/renderer.
    // The session is dumped first while windows are still mapped, so the next
    // start relaunches them.
    state.session_save();
    let _ = state.display_handle.flush_clients();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_crate_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
        assert!(!version().is_empty());
    }

    #[test]
    fn reports_application_name() {
        assert_eq!(name(), "xfar");
    }
}
