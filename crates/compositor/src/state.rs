//! Compositor state and Wayland protocol handlers.
//!
//! [`State`] is passed through the calloop event loop; [`ClientState`] holds
//! per-client data. Smithay's `delegate_*!` macros connect the handler
//! implementations to the registered globals.

use std::error::Error;
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::space::RenderZindex;
use smithay::desktop::{PopupKind, PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle};
use smithay::utils::{Logical, Point, Rectangle, Serial, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_decoration, delegate_xdg_shell,
};

/// Floating windows render above the grid; fullscreen windows render above
/// floats. Smithay preserves these explicit z-index values across retiles.
const GRID_Z: u8 = RenderZindex::Shell as u8;
const FLOAT_Z: u8 = GRID_Z + 1;
const FULLSCREEN_Z: u8 = GRID_Z + 2;

/// A launched app's grid placeholder is shown once it has been this long
/// without a window (fast apps are already up and show nothing), and dropped
/// after [`LAUNCH_CELL_TIME`] even if the app never maps (it died, or is not
/// a windowed Wayland client). Both timings live here so state (sweep) and the
/// rendering backends (show filter) agree.
pub(crate) const LAUNCH_CELL_AFTER: Duration = Duration::from_millis(350);
pub(crate) const LAUNCH_CELL_TIME: Duration = Duration::from_secs(8);

/// Transient Alt+Tab application-switcher state (present only while cycling).
pub struct Switcher {
    /// Candidate windows, most-recently-used first.
    pub candidates: Vec<Window>,
    /// Index of the currently highlighted candidate.
    pub selected: usize,
}

/// A launch xfar performed whose app has not yet connected to its socket,
/// keyed by the spawned pid when a client connects (see
/// [`State::pending_launch_for_pid`]).
#[derive(Debug, Clone)]
pub enum PendingLaunch {
    /// A session-tracked app launch: recorded on the client so a session save
    /// can restore it.
    App(String),
    /// The `XFAR_STARTUP_CMD` launch, intentionally *not* session-tracked
    /// (restoring it would double-launch with the startup command itself).
    Startup,
}

/// Session-tracked process groups, keyed by process-group ID. Entries remain
/// until the group exits or a matching re-request releases a stale group.
#[derive(Debug, Clone)]
struct SpawnedLaunch {
    /// The command the group was spawned with (re-requested by launcher/restore).
    command: String,
    /// When the group was spawned; used for the stale deadline.
    since: Instant,
    /// True once a window of this group mapped, so a re-request never kills a
    /// live app's process group.
    windowed: bool,
}

/// In-progress session restore. Commands run in parallel and are matched to
/// mapped clients by process ID. [`State::advance_restore`] advances the replay
/// on each frame and when a window maps.
#[derive(Debug)]
struct SessionRestore {
    /// Saved workspaces in slot order.
    desktops: Vec<crate::session::Desktop>,
    /// Workspace slot that was active at save; switched to when the restore
    /// finishes.
    saved_active: usize,
    /// Workspace currently being filled.
    desk: usize,
    /// Commands for `desktops[desk]` that have not opened a window, with an
    /// individual deadline for each command.
    pending: Vec<(String, Instant)>,
    /// Final workspace weights, re-applied after its windows map.
    finish: Option<crate::layout::GridWeights>,
}

/// A launched app that has not yet opened a window: it reserves a grid cell
/// so the user sees where the window is coming before it arrives.
#[derive(Debug)]
pub(crate) struct Launching {
    /// The launched command, shown on the placeholder frame.
    command: String,
    /// When it was spawned (drives the show/take-down timing). Never read more
    /// often than the 60 Hz frame tick.
    since: Instant,
}

/// Drop the first still-unclaimed launch cell for `command`: a window of it
/// just mapped, so its placeholder is replaced in place. One map = one cell,
/// even when a command repeats. Returns whether a cell was dropped.
pub(crate) fn pop_launch_cell(launching: &mut Vec<Launching>, command: &str) -> bool {
    let Some(index) = launching.iter().position(|l| l.command == command) else {
        return false;
    };
    launching.remove(index);
    true
}

/// The launch command attributed to `window`'s client, if any (set at connect
/// via pid resolution; `None` for clients xfar never spawned).
fn window_session_cmd(window: &Window) -> Option<String> {
    // Bind the client clone so its data borrow is held for the call's length.
    let client = window.toplevel().and_then(|t| t.wl_surface().client())?;
    client.get_data::<ClientState>()?.session_cmd.clone()
}

/// Order `items` by `rank`: ranked entries first in ascending rank order (ties
/// keep their input order), then unranked entries in their existing relative
/// order. A stable partition like a stable sort of `Option<usize>`.
fn order_by_rank<T>(rank: &dyn Fn(&T) -> Option<usize>, items: Vec<T>) -> Vec<T> {
    let mut ranked: Vec<(usize, T)> = Vec::new();
    let mut rest: Vec<T> = Vec::new();
    for item in items {
        match rank(&item) {
            Some(r) => ranked.push((r, item)),
            None => rest.push(item),
        }
    }
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, t)| t).chain(rest).collect()
}

/// Re-assert `ordered` as the space's window stack (each kept at its current
/// location); the following retile gives them their cells. Used by window
/// cycling and session-restore ordering, which must re-stack the active space
/// before the grid re-tiles.
fn reassert_order(space: &mut Space<Window>, ordered: Vec<Window>) {
    for window in ordered {
        let loc = space.element_location(&window).unwrap_or_default();
        space.map_element(window, loc, true);
    }
}

/// How long a restored app may take to open its window before it is skipped.
const RESTORE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Compact command and workspace count for session logs.
fn session_note(commands: usize, desktops: usize) -> String {
    format!(
        "{} command{} across {} workspace{}",
        commands,
        if commands == 1 { "" } else { "s" },
        desktops,
        if desktops == 1 { "" } else { "s" }
    )
}

/// Server-wide compositor state, threaded through the event loop.
pub struct State {
    /// Set to `false` to stop the event loop (e.g. the output window closed).
    pub running: bool,
    /// Handle used to create globals and manage clients.
    pub display_handle: DisplayHandle,
    /// `wl_compositor` / `wl_subcompositor` protocol state.
    pub compositor_state: CompositorState,
    /// `wl_shm` protocol state (shared-memory buffers).
    pub shm_state: ShmState,
    /// Seat/input state (advertises `wl_seat`).
    pub seat_state: SeatState<State>,
    /// The single seat, with keyboard and pointer capabilities.
    pub seat: Seat<State>,
    /// Current pointer position in compositor (logical) space.
    pub pointer_location: Point<f64, Logical>,
    /// `xdg_shell` protocol state (application windows/popups).
    pub xdg_shell_state: XdgShellState,
    /// `xdg-decoration` protocol state. xfar always selects server-side mode
    /// and draws no decorations.
    #[allow(dead_code)]
    pub xdg_decoration_state: XdgDecorationState,
    /// The active workspace's window layout.
    pub space: Space<Window>,
    /// Inactive workspaces (the active workspace's slot holds `None`; its space
    /// lives in [`State::space`]). Switching swaps a space in and out.
    pub workspaces: Vec<Option<Space<Window>>>,
    /// Grid weights for each workspace, swapped with [`State::space`]. The
    /// active slot holds the previous workspace's weights while
    /// [`State::grid_weights`] holds the live copy.
    workspace_weights: Vec<crate::layout::GridWeights>,
    /// Index of the active workspace.
    pub active_workspace: usize,
    /// Active Alt+Tab switcher, if the user is currently cycling windows.
    pub switcher: Option<Switcher>,
    /// Tracked xdg popups (menus, tooltips, dropdowns).
    pub popups: PopupManager,
    /// `wl_data_device_manager` state (clipboard / drag-and-drop).
    pub data_device_state: DataDeviceState,
    /// Text renderer for on-screen labels (launcher, etc.), if a font loaded.
    pub text: Option<crate::text::TextRenderer>,
    /// Smart launcher (application search) state.
    pub launcher: crate::launcher::LauncherState,
    /// When the launcher was last opened, used to drive its fade-in animation.
    pub launcher_opened_at: Option<Instant>,
    /// Effective configuration, read by the settings panel.
    pub config: crate::config::Config,
    /// On-screen settings panel (`xfar.settings`).
    pub settings: crate::settings::SettingsPanel,
    /// Resolved keyboard shortcuts (chord → action), built from configuration.
    pub keybindings: Vec<(crate::keybinding::Chord, crate::keybinding::Command)>,
    /// Active color palette, built from configuration; read by rendering.
    pub theme: crate::theme::Palette,
    /// Window behavior settings (focus-new), from configuration.
    pub window: crate::config::WindowConfig,
    /// Tiling settings (minimum cell size), from configuration.
    pub tiling: crate::config::TilingConfig,
    /// Manual-resize weights for the active workspace's grid. Reset to an even
    /// split whenever the window set changes (map/unmap); mutated by the resize
    /// shortcuts to move a column or row divider.
    pub grid_weights: crate::layout::GridWeights,
    /// Whether the compositor draws its own cursor (hardware backends only).
    /// Read only by the `tty` (DRM) backend; the nested winit backend uses the
    /// host cursor, so this field is unused in the default build.
    #[allow(dead_code)]
    pub show_cursor: bool,
    /// Name of our Wayland socket, used to spawn launched apps.
    pub socket_name: String,
    /// Spawned commands not yet claimed by a client, keyed by process ID.
    pub pending_launches: std::collections::HashMap<u32, PendingLaunch>,
    /// Spawned process groups for session-tracked launches (see
    /// [`SpawnedLaunch`]): survives pid attribution and placeholder abandonment
    /// so stale launches can be released on re-request.
    spawned: std::collections::HashMap<u32, SpawnedLaunch>,
    /// Launches that have not yet produced a mapped window. Each reserves a
    /// grid cell (its "opening" frame) until its window maps ([`pop_launch_cell`])
    /// or [`LAUNCH_CELL_TIME`] elapses ([`State::sweep_launching`]). The launcher,
    /// paged text, and session restore all route through `spawn_wayland`, so a
    /// slow app's frame shows from the same path everywhere.
    pub launching: Vec<Launching>,
    /// In-progress session restore, if one was loaded at startup.
    restore: Option<SessionRestore>,
    /// The compositor's output, provided by winit or DRM.
    pub output: Output,
    /// Whether the panel is at the bottom rather than the top.
    pub panel_bottom: bool,
    /// Windows currently in fullscreen (entered via their `fullscreen_request`).
    /// They cover the output and are excluded from the tiling grid until they
    /// leave fullscreen. Owned here so classification is race-free (the acked
    /// `xdg` Fullscreen state would only settle on the client's next commit).
    pub fullscreen: Vec<Window>,
    /// The rasterized background wallpaper, if one is configured and loadable.
    pub wallpaper: crate::wallpaper::Wallpaper,
    /// Directory `xfar.screenshot` saves captures to (already `~`-expanded).
    pub screenshot_dir: String,
    /// How the backend receives capture requests: a channel the DRM backend
    /// drains each loop tick (renders the next composed frame straight to a
    /// PNG). `None` on backends without capture support (e.g. nested winit).
    pub screenshot: Option<std::sync::mpsc::Sender<String>>,
}

impl State {
    /// Create compositor state and register its Wayland globals. `output` must
    /// already be advertised.
    pub fn new(
        display: &Display<State>,
        output: Output,
        config: &crate::config::Config,
    ) -> Result<Self, Box<dyn Error>> {
        let display_handle = display.handle();
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        tracing::info!(
            repeat_delay_ms = config.keyboard.repeat_delay_ms,
            repeat_rate = config.keyboard.repeat_rate,
            "configuring keyboard"
        );
        seat.add_keyboard(
            XkbConfig::default(),
            config.keyboard.repeat_delay_ms,
            config.keyboard.repeat_rate,
        )?;
        seat.add_pointer();

        // Resolve keyboard shortcuts; fall back to defaults (which are always
        // valid) if the user's config has a malformed binding.
        let keybindings = config.keybindings.resolve().unwrap_or_else(|err| {
            tracing::error!("{err}; using default keybindings");
            crate::config::KeybindingsConfig::default()
                .resolve()
                .expect("default keybindings are valid")
        });

        // Resolve the color palette; fall back to defaults on a bad color.
        let theme = config.appearance.resolve().unwrap_or_else(|err| {
            tracing::error!("{err}; using default appearance");
            crate::theme::Palette::default()
        });

        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);

        // Each workspace has its own space. The output is mapped into every one
        // so rendering knows its geometry regardless of which is active.
        let make_space = || {
            let mut s = Space::default();
            s.map_output(&output, (0, 0));
            s
        };
        let space = make_space();
        // Start with one workspace; restore or `Command::NewWorkspace` can add
        // more.
        let workspaces = vec![None];
        let workspace_weights = vec![crate::layout::GridWeights::default()];

        // Start the pointer in the middle of the output rather than the corner.
        let pointer_location = output
            .current_mode()
            .map(|m| (m.size.w as f64 / 2.0, m.size.h as f64 / 2.0).into())
            .unwrap_or_default();

        // Rasterize the configured wallpaper (if any) to the output's size.
        let wallpaper = crate::wallpaper::Wallpaper::from_config(
            &config.appearance,
            output
                .current_mode()
                .map(|m| (m.size.w, m.size.h))
                .unwrap_or_default(),
        );

        Ok(Self {
            running: true,
            display_handle,
            compositor_state,
            shm_state,
            seat_state,
            seat,
            pointer_location,
            xdg_shell_state,
            xdg_decoration_state,
            space,
            workspaces,
            workspace_weights,
            active_workspace: 0,
            switcher: None,
            popups: PopupManager::default(),
            data_device_state,
            text: crate::text::TextRenderer::new(),
            launcher: crate::launcher::LauncherState::new(),
            launcher_opened_at: None,
            config: config.clone(),
            settings: crate::settings::SettingsPanel::new(),
            keybindings,
            theme,
            window: config.window.clone(),
            tiling: config.tiling.clone(),
            grid_weights: crate::layout::GridWeights::default(),
            show_cursor: config.cursor.visible,
            socket_name: String::new(),
            pending_launches: std::collections::HashMap::new(),
            spawned: std::collections::HashMap::new(),
            launching: Vec::new(),
            restore: None,
            output,
            panel_bottom: config.appearance.panel_position == "bottom",
            fullscreen: Vec::new(),
            wallpaper,
            screenshot_dir: crate::config::expand_home(
                config
                    .screenshot
                    .dir
                    .as_deref()
                    .unwrap_or(crate::config::SCREENSHOT_DIR_DEFAULT),
            ),
            screenshot: None,
        })
    }

    /// Open the launcher with an empty query.
    pub fn launcher_open(&mut self) {
        self.settings.close();
        self.launcher.open = true;
        self.launcher.query.clear();
        self.launcher_opened_at = Some(Instant::now());
        self.launcher_refresh();
    }

    /// Append a character to the launcher query and re-rank results.
    pub fn launcher_type(&mut self, ch: char) {
        self.launcher.query.push(ch);
        self.launcher_refresh();
    }

    /// Delete the last launcher query character and re-rank results.
    pub fn launcher_backspace(&mut self) {
        self.launcher.query.pop();
        self.launcher_refresh();
    }

    /// Rebuild the launcher's ranked results from apps, open windows (active
    /// workspace), and system actions.
    fn launcher_refresh(&mut self) {
        use crate::launcher::{Action, SystemAction};

        // With no query typed, show nothing — the search starts empty rather
        // than listing every application.
        if self.launcher.query.is_empty() {
            self.launcher.results.clear();
            self.launcher.selection = 0;
            return;
        }

        let mut actions: Vec<Action> = self
            .launcher
            .apps
            .iter()
            .map(|app| Action::Launch {
                name: app.name.clone(),
                exec: app.exec.clone(),
                terminal: app.terminal,
            })
            .collect();

        for window in self.space.elements() {
            if window.toplevel().is_some() {
                let title = window_title(window).unwrap_or_else(|| "Untitled".to_string());
                actions.push(Action::Focus {
                    name: format!("Window: {title}"),
                    window: window.clone(),
                });
            }
        }

        actions.push(Action::System {
            name: "xfar.quit".to_string(),
            kind: SystemAction::Quit,
        });

        actions.push(Action::System {
            name: "xfar.help".to_string(),
            kind: SystemAction::Help,
        });

        actions.push(Action::System {
            name: "xfar.validate".to_string(),
            kind: SystemAction::Validate,
        });

        // Capture support depends on the active backend.
        actions.push(Action::System {
            name: "xfar.screenshot".to_string(),
            kind: SystemAction::Screenshot,
        });

        // On-screen settings panel.
        actions.push(Action::System {
            name: "xfar.settings".to_string(),
            kind: SystemAction::Settings,
        });

        self.launcher.results = crate::launcher::rank(&self.launcher.query, &actions);
        self.launcher.selection = 0;
    }

    /// Perform the launcher's selected action and close the launcher.
    pub fn launcher_activate(&mut self) {
        use crate::launcher::{Action, SystemAction};

        let action = self.launcher.selected().cloned();
        self.launcher.close();
        match action {
            Some(Action::Launch {
                name,
                exec,
                terminal,
            }) => {
                // Terminal (TUI) apps have no window of their own; run them
                // inside a terminal emulator (configurable via XFAR_TERMINAL).
                let command = if terminal {
                    let term =
                        std::env::var("XFAR_TERMINAL").unwrap_or_else(|_| "foot".to_string());
                    format!("{term} {exec}")
                } else {
                    exec.clone()
                };
                self.spawn_wayland(&command);
                tracing::info!("launched '{name}': {command}");
            }
            Some(Action::Focus { window, .. }) => {
                if self.space.elements().any(|w| w == &window) {
                    let serial = SERIAL_COUNTER.next_serial();
                    self.focus_window(&window, serial);
                }
            }
            Some(Action::System {
                kind: SystemAction::Quit,
                ..
            }) => {
                tracing::info!("quit requested from launcher");
                self.running = false;
            }
            Some(Action::System {
                kind: SystemAction::Help,
                ..
            }) => self.launcher_show_help(),
            Some(Action::System {
                kind: SystemAction::Validate,
                ..
            }) => self.launcher_show_validation(),
            Some(Action::System {
                kind: SystemAction::Screenshot,
                ..
            }) => self.launcher_screenshot(),
            Some(Action::System {
                kind: SystemAction::Settings,
                ..
            }) => self.settings_open(),
            None => {}
        }
    }

    /// Open the on-screen settings panel with the effective configuration.
    pub fn settings_open(&mut self) {
        self.settings.open(&self.config);
    }

    /// Close the settings panel (if open).
    pub fn settings_close(&mut self) {
        self.settings.close();
    }

    /// Launch `command` through `sh -c` with this compositor's
    /// `WAYLAND_DISPLAY`. Tracked launches reserve a grid cell and retain their
    /// command for session restore and client attribution.
    fn spawn(&mut self, command: &str, track: bool) {
        if track {
            // A re-request is made against a fresh instance; a previous launch
            // of the same app that never opened a window is wedged (say, on a
            // cold xdg-desktop-portal) and would swallow the new process via
            // the app's own single-instance handoff. Release it first.
            self.kill_stale(command);
        }
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("WAYLAND_DISPLAY", &self.socket_name)
            .process_group(0)
            .spawn()
        {
            Ok(child) => {
                self.pending_launches.insert(
                    child.id(),
                    if track {
                        PendingLaunch::App(command.to_string())
                    } else {
                        PendingLaunch::Startup
                    },
                );
                if track {
                    self.spawned.insert(
                        child.id(),
                        SpawnedLaunch {
                            command: command.to_string(),
                            since: Instant::now(),
                            windowed: false,
                        },
                    );
                }
                tracing::info!("launched (pid {}): {command}", child.id());
                if track {
                    // Reserve a grid cell and retile before the window maps.
                    self.launching.push(Launching {
                        command: command.to_string(),
                        since: Instant::now(),
                    });
                    self.retile();
                }
            }
            Err(err) => tracing::warn!("failed to launch '{command}': {err}"),
        }
    }

    /// Launch `command` as a session-tracked client of this compositor.
    pub fn spawn_wayland(&mut self, command: &str) {
        self.spawn(command, true);
    }

    /// TERM every still-alive process group xfar spawned for `command` that has
    /// not opened a window within [`LAUNCH_CELL_TIME`]. The group release (not
    /// the wrapper pid) matters: a GTK app is forked below `sh -c`, so killing
    /// the shell alone would leave the app alive and holding its DBus
    /// single-instance name, silently eating every later retry.
    fn kill_stale(&mut self, command: &str) {
        let now = Instant::now();
        let stale: Vec<u32> = self
            .spawned
            .iter()
            .filter(|(_, s)| {
                s.command == command && !s.windowed && s.since + LAUNCH_CELL_TIME <= now
            })
            .map(|(&pgid, _)| pgid)
            .filter(|pgid| group_alive(*pgid))
            .collect();
        for pgid in stale {
            tracing::info!("killed stale '{command}' launch (pgid {pgid})");
            self.spawned.remove(&pgid);
            self.pending_launches.remove(&pgid);
            // SAFETY: `pgid` is a process group xfar created for its own spawn;
            // TERM is best-effort (a group that raced to exit reports ESRCH).
            unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGTERM) };
        }
    }

    /// Resolve a client pid to the tracked spawn group that launched it, by
    /// walking its ancestor chain up to xfar's `sh -c <command>` wrapper (or
    /// the app itself — a GTK single-instance *reopener* may connect in its
    /// stead). `None` when the client was never spawned by xfar.
    fn resolve_group(&self, pid: u32) -> Option<u32> {
        let mut cur = pid;
        // A deep ancestor chain only exists for weird nesting; 8 is generous.
        for _ in 0..8 {
            if self.spawned.contains_key(&cur) {
                return Some(cur);
            }
            cur = parent_pid(cur)?;
        }
        None
    }

    /// Mark the launch that produced `window`'s client as windowed when its
    /// process ancestry resolves. A matching re-request then cannot TERM the
    /// live process group.
    fn mark_group_windowed(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let Some(client) = toplevel.wl_surface().client() else {
            return;
        };
        let Some(pid) = client.get_data::<ClientState>().and_then(|c| c.client_pid) else {
            return;
        };
        if let Some(pgid) = self.resolve_group(pid) {
            if let Some(launch) = self.spawned.get_mut(&pgid) {
                launch.windowed = true;
            }
        }
    }

    /// Launch the `XFAR_STARTUP_CMD` app (if set) as a client of this compositor.
    /// Not session-tracked — restoring it next boot would double-launch it.
    pub fn spawn_startup_command(&mut self) {
        let command = match std::env::var("XFAR_STARTUP_CMD") {
            Ok(cmd) if !cmd.is_empty() => cmd,
            _ => return,
        };
        self.spawn(&command, false);
    }

    /// A window of `command` just mapped: replace its placeholder frame in
    /// place (one map = one cell, even when a command repeats) and, during a
    /// restore, count that command's wait as satisfied. Returns whether a
    /// restore wait was consumed (the caller then advances the restore).
    fn consume_launch(&mut self, command: &str) -> bool {
        pop_launch_cell(&mut self.launching, command);
        self.restore.as_mut().is_some_and(|restore| {
            let Some(index) = restore.pending.iter().position(|(c, _)| c == command) else {
                return false;
            };
            restore.pending.remove(index);
            tracing::debug!("restore: '{command}' opened a window");
            true
        })
    }

    /// Forget a launch that will never open a window — its process died, or it
    /// stayed windowless past its deadlines: free its placeholder frame, drop
    /// the restore wait on it, and remove its unclaimed pid entries (a recycled
    /// pid must never misattribute a later client).
    fn abandon_launch(&mut self, command: &str) {
        self.pending_launches
            .retain(|_, l| !matches!(l, PendingLaunch::App(c) if c == command));
        pop_launch_cell(&mut self.launching, command);
        if let Some(restore) = self.restore.as_mut() {
            let Some(index) = restore.pending.iter().position(|(c, _)| c == command) else {
                return;
            };
            restore.pending.remove(index);
            tracing::info!("restore: '{command}' opened no window; skipping");
        }
    }

    /// Resolve which launch a newly-connected client came from, by the genuine
    /// pid of the connecting process (SO_PEERCRED on the accepted socket). The
    /// pid-keyed entry is consumed exactly once. Launches go through
    /// `sh -c`, which execs the app directly for simple commands but forks it
    /// for compound ones, so the client pid (or one of its ancestors) is the
    /// recorded launch pid. Called exactly once per new client.
    pub fn pending_launch_for_pid(&mut self, pid: u32) -> Option<PendingLaunch> {
        let mut current = Some(pid);
        // A deep ancestor chain only exists for weird nesting; 8 is generous.
        for _ in 0..8 {
            let Some(pid) = current else { break };
            if let Some(entry) = self.pending_launches.remove(&pid) {
                return Some(entry);
            }
            current = parent_pid(pid);
        }
        None
    }

    /// Register a freshly-connected Wayland client, attributing it to the
    /// launch it actually came from (matched by the connecting process's pid,
    /// not FIFO order) so a session save can restore it (see
    /// [`pending_launch_for_pid`]). The startup command and clients xfar never
    /// spawned claim nothing. Called by both backends' socket handlers.
    pub fn register_client(&mut self, stream: std::os::unix::net::UnixStream) {
        let pid = socket_peer_pid(&stream);
        let client = Arc::new(ClientState {
            session_cmd: match pid.and_then(|pid| self.pending_launch_for_pid(pid)) {
                Some(PendingLaunch::App(command)) => Some(command),
                _ => None, // the startup command, or a client xfar never spawned
            },
            client_pid: pid,
            ..Default::default()
        });
        if let Err(err) = self.display_handle.insert_client(stream, client) {
            tracing::warn!("failed to register wayland client: {err}");
        }
    }

    /// Forget launches whose process exited or whose window did not appear
    /// within [`LAUNCH_CELL_TIME`].
    /// ponytail: On Linux this performs a nonblocking `waitpid` per launch and
    /// a process-group liveness probe per tracked group per frame; batch the
    /// checks if launch counts grow.
    pub fn sweep_launching(&mut self) {
        let now = Instant::now();
        let before = self.launching.len();

        // Free launches whose process already exited.
        let dead: Vec<(u32, Option<String>)> = self
            .pending_launches
            .iter()
            .filter_map(|(&pid, launch)| match launch {
                PendingLaunch::App(command) if pid_dead(pid) => Some((pid, Some(command.clone()))),
                PendingLaunch::Startup if pid_dead(pid) => Some((pid, None)),
                _ => None,
            })
            .collect();
        for (pid, _) in &dead {
            self.pending_launches.remove(pid);
        }
        for (_, command) in dead {
            if let Some(command) = command {
                // A restore still waiting on this command can advance immediately.
                self.abandon_launch(&command);
            }
        }

        // Launches still running but never windowed: drop their placeholder now
        // (a windowless daemon can't be restored, and its pid shouldn't stay
        // claimable all boot).
        let timed_out: Vec<String> = self
            .launching
            .iter()
            .filter(|l| l.since + LAUNCH_CELL_TIME <= now)
            .map(|l| l.command.clone())
            .collect();
        for command in &timed_out {
            tracing::info!("launch '{command}' never opened a window; dropping its placeholder");
            self.abandon_launch(command);
        }

        // A cell was freed (crash or timeout): let the windows grow back into
        // it now, not on the next map.
        if self.launching.len() != before {
            self.retile();
        }

        // Release spawn groups whose app is fully gone: a launch Consumed at
        // connect is otherwise never released because nothing waitpids its
        // `sh` again. Reap the leader first (a zombie counts as a live member
        // for `kill(-pgid, 0)`), then probe whether any real member survives —
        // orphaned children outliving a reaped leader keep the entry.
        self.spawned.retain(|pgid, _| {
            pid_dead(*pgid);
            group_alive(*pgid)
        });
    }

    /// The active workspace's placeholder cells, in launch order: each
    /// not-yet-mapped app's would-be grid rectangle (via [`tile_grid_rects`], so
    /// the frame and the window that replaces it always line up). Empty when
    /// nothing is launching.
    pub(crate) fn launching_cells(&self) -> Vec<(String, Instant, Rectangle<i32, Logical>)> {
        if self.launching.is_empty() {
            return Vec::new();
        }
        let Some((windows, rects)) = tile_grid_rects(
            &self.space,
            &self.output,
            &self.grid_weights,
            self.panel_bottom,
            &self.fullscreen,
            self.launching.len(),
        ) else {
            return Vec::new();
        };
        self.launching
            .iter()
            .enumerate()
            .filter_map(|(i, launch)| {
                rects
                    .get(windows.len() + i)
                    .map(|rect| (launch.command.clone(), launch.since, *rect))
            })
            .collect()
    }

    /// Relaunch saved commands workspace by workspace after the socket opens.
    pub fn session_restore(&mut self) {
        if !self.config.session.keep_state {
            tracing::info!("session: keeping state disabled; skipping restore");
            return;
        }
        let Some(path) = crate::session::session_path() else {
            return;
        };
        if !path.is_file() {
            return; // nothing was saved; nothing to restore
        }
        let session = match crate::session::load_from(&path) {
            Ok(session) if !session.desktops.is_empty() => session,
            Ok(_) => return,
            Err(err) => {
                tracing::warn!("session: {err}; nothing restored");
                return;
            }
        };

        // Recreate saved workspace slots, including an empty active workspace.
        let needed = session.desktops.len().max(session.active + 1);
        while self.workspaces.len() < needed {
            let mut space = Space::<Window>::default();
            space.map_output(&self.output, (0, 0));
            self.workspaces.push(Some(space));
            self.workspace_weights
                .push(crate::layout::GridWeights::default());
        }

        let windows: usize = session.desktops.iter().map(|d| d.commands.len()).sum();
        tracing::info!(
            "restoring session ({})",
            session_note(windows, session.desktops.len())
        );
        // Start the first workspace; `advance_restore` advances after its
        // commands map or time out.
        let mut restore = SessionRestore {
            desktops: session.desktops,
            saved_active: session.active,
            desk: 0,
            pending: Vec::new(),
            finish: None,
        };
        self.restore_launch_desktop(&mut restore);
        self.restore = Some(restore);
        self.advance_restore();
    }

    /// Spawn all commands for one restored workspace, each with its own
    /// deadline.
    fn restore_launch_desktop(&mut self, restore: &mut SessionRestore) {
        let now = Instant::now();
        restore.pending = restore.desktops[restore.desk]
            .commands
            .iter()
            .map(|command| {
                tracing::info!(workspace = restore.desk + 1, "restoring '{command}'");
                // `spawn_wayland` records the launch under its pid; the first
                // connecting window of *that* app claims it.
                let pair = (command.clone(), now + RESTORE_WAIT);
                self.spawn_wayland(command);
                pair
            })
            .collect();
    }

    /// Advance session restore on each frame, including after window-map events
    /// and command timeouts. Completes each workspace in saved order, then
    /// restores its grid weights.
    pub fn advance_restore(&mut self) {
        let Some(mut restore) = self.restore.take() else {
            return;
        };

        // Re-assert the final saved weights after initial map commits.
        if let Some(weights) = restore.finish.take() {
            self.grid_weights = weights;
            self.retile();
            self.restore = None;
            tracing::info!("session restored");
            return;
        }

        // Expire commands that did not open a window in time.
        if !restore.pending.is_empty() {
            let before = self.launching.len();
            restore.pending.retain(|(command, deadline)| {
                let keep = *deadline > Instant::now();
                if !keep {
                    tracing::warn!("session restore: '{command}' opened no window; skipping");
                    // Drop the pid entry and placeholder frame too; the retain
                    // below removes the dead wait entry itself.
                    self.abandon_launch(command);
                }
                keep
            });
            // Reflow if an expired command released its reserved cell.
            if self.launching.len() != before {
                self.retile();
            }
        }

        // Advance until another workspace is waiting on commands.
        while restore.pending.is_empty() {
            let Some(weights) = restore
                .desktops
                .get(restore.desk)
                .map(|d| d.weights.clone())
            else {
                self.restore = None;
                tracing::info!("session restored");
                return;
            };
            // Restore saved window order and split weights before advancing.
            self.grid_weights = weights.clone();
            self.reorder_desktop_windows(&restore.desktops[restore.desk].commands);
            self.retile();
            if restore.desk + 1 >= restore.desktops.len() {
                // Restore the saved active workspace.
                if restore.saved_active != restore.desk {
                    self.switch_workspace(restore.saved_active);
                    self.restore = None;
                    tracing::info!("session restored");
                    return;
                }
                restore.finish = Some(weights);
                break;
            }
            restore.desk += 1;
            self.switch_workspace(restore.desk);
            self.restore_launch_desktop(&mut restore);
        }

        self.restore = Some(restore);
    }

    /// Restore the active workspace's saved order for tiled windows. Unmatched
    /// tiled windows keep their relative order after matched windows.
    fn reorder_desktop_windows(&mut self, commands: &[String]) {
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| is_tiled_window(w, &self.fullscreen))
            .cloned()
            .collect();
        if windows.is_empty() {
            return;
        }
        let ordered = order_by_rank(
            &|window: &Window| {
                window_session_cmd(window)
                    .and_then(|command| commands.iter().position(|c| *c == command))
            },
            windows,
        );
        // Re-assert stacking order to match; cell locations come from the
        // retile that follows (same pattern as `cycle_windows`).
        reassert_order(&mut self.space, ordered);
    }

    /// Dump the current layout to the session file so the next start can restore
    /// it. Called on shutdown, while windows are still mapped. An empty write
    /// replaces any stale session.
    pub fn session_save(&mut self) {
        if !self.config.session.keep_state {
            tracing::info!("session: keeping state disabled; skipping save");
            return;
        }
        let session = self.current_session();
        let windows: usize = session.desktops.iter().map(|d| d.commands.len()).sum();
        match crate::session::session_path() {
            Some(path) => match crate::session::save_to(&session, &path) {
                Ok(()) => tracing::info!(
                    "saved session ({})",
                    session_note(windows, session.desktops.len())
                ),
                Err(err) => tracing::warn!("session: {err}"),
            },
            None => tracing::warn!("session: no state directory; nothing saved"),
        }
    }

    /// Snapshot each workspace's commands and grid weights. Each client is
    /// recorded once per workspace; clients not launched by xfar are omitted.
    fn current_session(&self) -> crate::session::Session {
        let total = self.workspaces.len();
        let mut desktops = Vec::with_capacity(total);
        for i in 0..total {
            let space = if i == self.active_workspace {
                &self.space
            } else {
                self.workspaces[i]
                    .as_ref()
                    .expect("inactive workspace has a space")
            };
            let mut commands = Vec::new();
            let mut seen: Vec<ClientId> = Vec::new();
            for window in space.elements() {
                let Some(client) = window.toplevel().and_then(|t| t.wl_surface().client()) else {
                    continue;
                };
                if seen.contains(&client.id()) {
                    continue;
                }
                seen.push(client.id());
                if let Some(command) = window_session_cmd(window) {
                    commands.push(command);
                }
            }
            let weights = if i == self.active_workspace {
                self.grid_weights.clone()
            } else {
                self.workspace_weights[i].clone()
            };
            desktops.push(crate::session::Desktop { commands, weights });
        }
        crate::session::Session {
            active: self.active_workspace,
            desktops,
        }
    }

    /// Build, validate, save, and apply the settings panel's pending patches.
    /// A patch error keeps the panel open; invalid numeric and enumerated values
    /// are reset before the configuration is written.
    pub fn settings_save(&mut self) {
        let patches = self.settings.patches();
        if patches.is_empty() {
            self.settings.close();
            return;
        }
        let mut config = match crate::config::patched(&self.config, &patches) {
            Ok(mut config) => {
                for problem in config.validate() {
                    tracing::warn!("settings: {problem}");
                }
                config
            }
            Err(err) => {
                tracing::warn!("settings: {err}");
                return;
            }
        };
        let Some(path) = crate::config::config_path() else {
            tracing::warn!("settings: no config path; nothing saved");
            return;
        };
        if let Err(err) = crate::config::save_to(&config, &path) {
            tracing::warn!("settings: {err}");
            return;
        }
        tracing::info!("saved configuration to {}", path.display());
        std::mem::swap(&mut config, &mut self.config);
        self.apply_config_state();
        self.settings.close();
    }

    /// Apply a saved [`Config`] to live compositor state.
    fn apply_config_state(&mut self) {
        let config = &self.config;
        self.keybindings = config.keybindings.resolve().unwrap_or_else(|err| {
            tracing::error!("{err}; using default keybindings");
            crate::config::KeybindingsConfig::default()
                .resolve()
                .expect("default keybindings are valid")
        });
        self.theme = config.appearance.resolve().unwrap_or_else(|err| {
            tracing::error!("{err}; using default appearance");
            crate::theme::Palette::default()
        });
        self.window = config.window.clone();
        self.tiling = config.tiling.clone();
        self.panel_bottom = config.appearance.panel_position == "bottom";
        self.show_cursor = config.cursor.visible;
        self.screenshot_dir = crate::config::expand_home(
            config
                .screenshot
                .dir
                .as_deref()
                .unwrap_or(crate::config::SCREENSHOT_DIR_DEFAULT),
        );
        self.wallpaper = crate::wallpaper::Wallpaper::from_config(
            &config.appearance,
            self.output
                .current_mode()
                .map(|m| (m.size.w, m.size.h))
                .unwrap_or_default(),
        );
        if let Some(kbd) = self.seat.get_keyboard() {
            kbd.change_repeat_info(config.keyboard.repeat_rate, config.keyboard.repeat_delay_ms);
        }

        self.reflow();
        self.focus_topmost();
    }

    /// Queue a PNG capture request for the DRM/KMS backend. Unsupported backends
    /// leave the capture channel unset.
    fn launcher_screenshot(&mut self) {
        let Some(tx) = &self.screenshot else {
            tracing::warn!("screenshots are not supported on this backend");
            return;
        };
        if let Err(err) = std::fs::create_dir_all(&self.screenshot_dir) {
            tracing::warn!(
                "screenshot: could not create {}: {err}",
                self.screenshot_dir
            );
            return;
        }
        let path = format!("{}/xfar-{}.png", self.screenshot_dir, stamp());
        if let Err(err) = tx.send(path) {
            tracing::warn!("screenshot: capture channel closed: {err}");
        }
    }

    /// Page text through the configured terminal (`XFAR_TERMINAL`): write it to
    /// a temp file and page with `less` (falling back to `more` then `cat`).
    fn page_text(&mut self, name: &str, text: &str) {
        let path = std::env::temp_dir().join(name);
        if let Err(err) = std::fs::write(&path, text) {
            tracing::warn!("failed to write {path:?}: {err}");
            return;
        }
        self.open_file_in_terminal(&path);
    }

    /// Page `path` through the configured terminal (`XFAR_TERMINAL`, default
    /// `foot`) as a Wayland client, like any launched app (so the terminal is
    /// restored with the session). `less` with a `more`/`cat` fallback.
    fn open_file_in_terminal(&mut self, path: &std::path::Path) {
        let term = std::env::var("XFAR_TERMINAL").unwrap_or_else(|_| "foot".to_string());
        let command = format!(
            "{term} sh -c 'less {p} || more {p} || cat {p}'",
            p = path.display()
        );
        self.spawn_wayland(&command);
    }

    /// Show the built-in help (shortcuts).
    fn launcher_show_help(&mut self) {
        self.page_text("xfar-help.txt", crate::help::HELP_TEXT);
    }

    /// Run `xfar validate` on the running binary and show its report.
    fn launcher_show_validation(&mut self) {
        let Ok(exe) = std::env::current_exe() else {
            tracing::warn!("cannot locate the running binary");
            return;
        };
        let output = match std::process::Command::new(&exe).arg("validate").output() {
            Ok(output) => output,
            Err(err) => {
                tracing::warn!("failed to run '{} validate': {err}", exe.display());
                return;
            }
        };
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        self.page_text("xfar-validate.txt", &report);
    }

    /// Ask the focused window (or the topmost one) to close.
    pub fn close_focused_window(&mut self) {
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus());
        let window = focused
            .and_then(|surface| self.window_for_surface(&surface))
            .or_else(|| self.space.elements().next_back().cloned());
        if let Some(toplevel) = window.as_ref().and_then(|w| w.toplevel()) {
            tracing::info!("closing focused window");
            toplevel.send_close();
        }
    }

    /// Switch to workspace `index`, swapping its space in as the active one and
    /// moving keyboard focus to its topmost window.
    pub fn switch_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() || index == self.active_workspace {
            return;
        }
        // Stash the outgoing workspace's grid split weights in its slot, then swap
        // the space; the incoming workspace brings its own weights back.
        self.workspace_weights[self.active_workspace] = self.grid_weights.clone();
        let incoming = self.workspaces[index]
            .take()
            .expect("inactive workspace has a space");
        let outgoing = std::mem::replace(&mut self.space, incoming);
        self.workspaces[self.active_workspace] = Some(outgoing);
        self.active_workspace = index;
        self.grid_weights = self.workspace_weights[index].clone();

        // A float/fullscreen window moved here (or went fullscreen) while this
        // workspace was inactive never got the commit-driven placement, so
        // re-place off-grid windows now.
        for window in self
            .space
            .elements()
            .filter(|w| !is_tiled_window(w, &self.fullscreen))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.classify_mapped(&window);
        }

        tracing::info!(workspace = index + 1, "switched workspace");
        self.prune_empty_workspaces();
        self.focus_topmost();
    }

    /// Create and switch to a new workspace.
    pub fn add_workspace(&mut self) {
        let mut space = Space::<Window>::default();
        space.map_output(&self.output, (0, 0));
        self.workspaces.push(Some(space));
        self.workspace_weights
            .push(crate::layout::GridWeights::default());
        let index = self.workspaces.len() - 1;
        tracing::info!(workspace = index + 1, "created workspace");
        self.switch_workspace(index);
    }

    /// Switch to the workspace `delta` steps away, wrapping around the ends.
    pub fn cycle_workspace(&mut self, delta: isize) {
        if self.workspaces.is_empty() {
            return;
        }
        let next = ((self.active_workspace as isize + delta)
            .rem_euclid(self.workspaces.len() as isize)) as usize;
        self.switch_workspace(next);
    }

    /// Move the focused window `delta` workspaces away, wrapping around the
    /// ends like [`cycle_workspace`].
    pub fn move_focused_to_workspace_rel(&mut self, delta: isize) {
        if self.workspaces.is_empty() {
            return;
        }
        let next = ((self.active_workspace as isize + delta)
            .rem_euclid(self.workspaces.len() as isize)) as usize;
        if next != self.active_workspace {
            self.move_focused_to_workspace(next);
        }
    }

    /// Remove empty inactive workspaces except slot zero. Restore keeps its
    /// slots stable until replay finishes.
    fn prune_empty_workspaces(&mut self) {
        if self.restore.is_some() {
            return;
        }
        let mut i = 1;
        while i < self.workspaces.len() {
            if i == self.active_workspace {
                i += 1;
                continue;
            }
            let empty = self.workspaces[i]
                .as_ref()
                .is_none_or(|s| s.elements().next().is_none());
            if empty {
                self.workspaces.remove(i);
                self.workspace_weights.remove(i);
                if i < self.active_workspace {
                    self.active_workspace -= 1;
                }
            } else {
                i += 1;
            }
        }
    }

    /// Move the focused window to workspace `index`. Both the source (which the
    /// window leaves) and the target (which it joins) are re-tiled so each grid
    /// reflects its new window count — the window takes a fresh cell on the
    /// target rather than keeping a stale position.
    pub fn move_focused_to_workspace(&mut self, index: usize) {
        if index >= self.workspaces.len() || index == self.active_workspace {
            return;
        }
        let Some(window) = self
            .focused_window()
            .or_else(|| self.space.elements().next_back().cloned())
        else {
            return;
        };
        self.space.unmap_elem(&window);
        // The source workspace reflows (resetting its weights) without the window.
        self.reflow();
        // The target gets the window and is tiled fresh with an even split.
        if let Some(target) = self.workspaces[index].as_mut() {
            target.map_element(window, (0, 0), true);
            tile_space(
                target,
                &self.output,
                &crate::layout::GridWeights::default(),
                self.panel_bottom,
                &self.fullscreen,
                0, // no reserved cells on an inactive workspace
            );
        }
        tracing::info!(workspace = index + 1, "moved window to workspace");
        self.focus_topmost();
    }

    /// Focus the topmost window of the active workspace, or clear focus.
    fn focus_topmost(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        let top = self.space.elements().next_back().cloned();
        if let Some(window) = top {
            self.focus_window(&window, serial);
        } else if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, serial);
        }
    }

    /// Advance the Alt+Tab switcher (starting it on the first press). Candidates
    /// are the active workspace's windows in tiling order (left→right,
    /// top→bottom); cycling moves forward from the currently focused window.
    pub fn switcher_cycle(&mut self) {
        if let Some(switcher) = &mut self.switcher {
            if !switcher.candidates.is_empty() {
                switcher.selected = (switcher.selected + 1) % switcher.candidates.len();
            }
            return;
        }
        let candidates = self.tiled_windows();
        if candidates.is_empty() {
            return;
        }
        // Start on the window after the focused one, so the first Tab moves
        // forward to the next tile.
        let current = self
            .focused_window()
            .and_then(|f| candidates.iter().position(|w| w == &f))
            .unwrap_or(0);
        let selected = (current + 1) % candidates.len();
        self.switcher = Some(Switcher {
            candidates,
            selected,
        });
    }

    /// Commit the Alt+Tab selection: focus the chosen window and hide the UI.
    pub fn switcher_finish(&mut self) {
        let Some(switcher) = self.switcher.take() else {
            return;
        };
        let Some(window) = switcher.candidates.get(switcher.selected).cloned() else {
            return;
        };
        // The window may have closed while cycling; only focus it if still mapped.
        if self.space.elements().any(|w| w == &window) {
            let serial = SERIAL_COUNTER.next_serial();
            self.focus_window(&window, serial);
        }
    }

    /// The client surface under `pos` (global coordinates), if any, together
    /// with that surface's origin in global space (used for pointer focus).
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, location) = self.space.element_under(pos)?;
        window
            .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
            .map(|(surface, surface_offset)| (surface, (location + surface_offset).to_f64()))
    }

    /// Mark `window` activated (deactivating the others) and give it keyboard
    /// focus. In the tiling model windows never overlap, so focus does *not*
    /// raise: the stacking order stays stable and drives the tile assignment.
    pub fn focus_window(&mut self, window: &Window, serial: Serial) {
        for w in self.space.elements() {
            if let Some(toplevel) = w.toplevel() {
                let active = w == window;
                toplevel.with_pending_state(|state| {
                    if active {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                let _ = toplevel.send_pending_configure();
            }
        }

        let surface = window.toplevel().map(|t| t.wl_surface().clone());
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, surface, serial);
        }
    }

    /// The mapped grid windows of the active workspace, in stable tiling order
    /// (stacking order, which we no longer disturb on focus). Floats (dialogs,
    /// modal, fixed-size) and fullscreen windows are excluded.
    fn tiled_windows(&self) -> Vec<Window> {
        self.space
            .elements()
            .filter(|w| is_tiled_window(w, &self.fullscreen))
            .cloned()
            .collect()
    }

    /// The active workspace's mapped window carrying `surface`'s wl_surface,
    /// if any.
    fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// The window that currently holds keyboard focus, if it is a mapped
    /// toplevel on the active workspace.
    fn focused_window(&self) -> Option<Window> {
        let surface = self.seat.get_keyboard()?.current_focus()?;
        self.window_for_surface(&surface)
    }

    /// Output area excluding the persistent panel, or `None` before the output
    /// has geometry.
    fn work_area(&self) -> Option<Rectangle<i32, Logical>> {
        Some(crate::layout::work_area(
            self.space.output_geometry(&self.output)?,
            self.panel_bottom,
        ))
    }

    /// Re-tile all mapped grid windows on the active workspace into the automatic
    /// gapped grid, applying the current manual resize weights. Grid-window
    /// position and size come entirely from the grid.
    pub fn retile(&mut self) {
        tile_space(
            &mut self.space,
            &self.output,
            &self.grid_weights,
            self.panel_bottom,
            &self.fullscreen,
            self.launching.len(),
        );
    }

    /// Reset the manual resize weights to an even split and re-tile. Called when
    /// the window set changes so a new/closed window gives everyone a fair cell.
    pub fn reflow(&mut self) {
        self.grid_weights = crate::layout::GridWeights::default();
        self.retile();
    }

    /// Grow or shrink the focused window's cell along one axis by ~10%, moving
    /// the shared column (`horizontal`) or row divider. No-op when the axis has
    /// no divider to move, or when the change would push any cell below the
    /// configured minimum size (that is the cap; the grid never shrinks past it).
    pub fn resize_focused(&mut self, horizontal: bool, grow: bool) {
        let windows = self.tiled_windows();
        let n = windows.len();
        let Some(focused) = self.focused_window() else {
            return;
        };
        let Some(index) = windows.iter().position(|w| w == &focused) else {
            return;
        };
        let Some((col, row)) = crate::layout::cell_of(n, index) else {
            return;
        };
        let counts = crate::layout::column_counts(n);
        let mut weights = self.grid_weights.clone();
        weights.ensure_shape(&counts);

        const STEP: f32 = 1.10; // ±10% per press
        let factor = if grow { STEP } else { 1.0 / STEP };
        if horizontal {
            if counts.len() < 2 {
                return; // single column: full width, no divider to move
            }
            weights.cols[col] *= factor;
        } else {
            if counts[col] < 2 {
                return; // single row in this column: full height
            }
            weights.rows[col][row] *= factor;
        }

        // Only commit if every resulting cell honors the minimum size.
        if let Some((_, rects)) = tile_grid_rects(
            &self.space,
            &self.output,
            &weights,
            self.panel_bottom,
            &self.fullscreen,
            0,
        ) {
            let ok = rects
                .iter()
                .all(|r| r.size.w >= self.tiling.min_width && r.size.h >= self.tiling.min_height);
            if !ok {
                return;
            }
        }
        self.grid_weights = weights;
        self.retile();
    }

    /// Rotate every window one slot forward through the grid cells, keeping
    /// keyboard focus on the same window (which lands in the next cell).
    pub fn cycle_windows(&mut self) {
        let mut windows = self.tiled_windows();
        if windows.len() < 2 {
            return;
        }
        windows.rotate_left(1);
        // Re-assert stacking order to match the rotated order, then re-tile.
        reassert_order(&mut self.space, windows);
        self.retile();
    }

    /// Assign `window` its layout role from its committed state. Called on every
    /// commit of an active-workspace window (and on workspace switch), so it
    /// must be a no-op for windows already in their role: a tiled window that
    /// already carries a grid cell is left alone; floating and fullscreen
    /// windows are re-placed (centered / covering) whenever they changed.
    fn classify_mapped(&mut self, window: &Window) {
        if self.fullscreen.contains(window) {
            self.place_fullscreen(window);
            return;
        }
        if is_tiled_window(window, &self.fullscreen) {
            // Already holds a grid cell → nothing to do. Fresh windows (and the
            // overflow check) are sized + placed once, from their first commit.
            let has_cell = window
                .toplevel()
                .map(|t| t.current_state().size.is_some())
                .unwrap_or(true);
            if has_cell {
                return;
            }
            self.tile_new_window(window);
        } else {
            let had_cell = window
                .toplevel()
                .map(|t| t.current_state().size.is_some())
                .unwrap_or(false);
            self.place_float(window);
            // A window that was tiled (grid-sized) before its dialog nature was
            // known frees its cell for the remaining windows.
            if had_cell {
                self.reflow();
            }
        }
    }

    /// Place a newly committed tiled window, moving it to the next existing
    /// workspace if the active grid is full.
    fn tile_new_window(&mut self, window: &Window) {
        let count = self.tiled_windows().len() + 1;
        let fits = crate::layout::fits_grid(
            count,
            self.tiling.max_columns as usize,
            self.tiling.max_rows as usize,
        );
        let next = self.active_workspace + 1;
        if !fits && next < self.workspaces.len() {
            self.space.unmap_elem(window);
            let target = self.workspaces[next]
                .as_mut()
                .expect("inactive workspace has a space");
            target.map_element(window.clone(), (0, 0), true);
            tile_space(
                target,
                &self.output,
                &crate::layout::GridWeights::default(),
                self.panel_bottom,
                &self.fullscreen,
                0, // overflow target has no reserved cells
            );
            self.switch_workspace(next);
            tracing::info!(
                workspace = next + 1,
                "mapped new toplevel window (workspace full, overflow)"
            );
            return;
        }
        self.reflow();
        tracing::info!("mapped new toplevel window");
    }

    /// Keep a floating window (dialog / modal / fixed-size) centered in the work
    /// area at its own size. Floats are never sized by the grid: we send only
    /// the work-area bounds and no size, so the client picks its natural size,
    /// and the position is re-clamped whenever the client resizes. The size
    /// configure is dropped by `send_pending_configure` when nothing changed.
    fn place_float(&mut self, window: &Window) {
        let Some(area) = self.work_area() else {
            return;
        };
        window.override_z_index(FLOAT_Z);
        let geom = window.geometry().size;
        if geom.w > 0 && geom.h > 0 {
            let w = geom.w.min(area.size.w);
            let h = geom.h.min(area.size.h);
            let loc = Point::from((
                area.loc.x + (area.size.w - w) / 2,
                area.loc.y + (area.size.h - h) / 2,
            ));
            if self.space.element_location(window) != Some(loc) {
                self.space.map_element(window.clone(), loc, false);
            }
        }
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = None;
                state.bounds = Some(area.size);
            });
            let _ = toplevel.send_pending_configure();
        }
    }

    /// Place a fullscreen window above the grid and floats. The panel is hidden
    /// while the active workspace has a mapped fullscreen window.
    fn place_fullscreen(&mut self, window: &Window) {
        let Some(geom) = self.space.output_geometry(&self.output) else {
            return;
        };
        window.override_z_index(FULLSCREEN_Z);
        if self.space.element_location(window) != Some(geom.loc) {
            self.space.map_element(window.clone(), geom.loc, false);
        }
    }

    /// Whether the active workspace has a mapped fullscreen window.
    pub fn fullscreen_active(&self) -> bool {
        self.space.elements().any(|w| self.fullscreen.contains(w))
    }
}

/// The tiled windows of `space` and their grid rectangles, laid out for
/// `windows + extra` cells (`extra` = reserved not-yet-mapped cells). Shared by
/// layout (`tile_space`), the placeholder frames (`launching_cells`), and the
/// resize cap check, so every consumer places a window (or its frame) in the
/// same cell. `None` when the output has no geometry yet.
type TileLayout = Option<(Vec<Window>, Vec<Rectangle<i32, Logical>>)>;

fn tile_grid_rects(
    space: &Space<Window>,
    output: &Output,
    weights: &crate::layout::GridWeights,
    panel_bottom: bool,
    fullscreen: &[Window],
    extra: usize,
) -> TileLayout {
    let area = crate::layout::work_area(space.output_geometry(output)?, panel_bottom);
    let windows: Vec<Window> = space
        .elements()
        .filter(|w| is_tiled_window(w, fullscreen))
        .cloned()
        .collect();
    let rects = crate::layout::tile_grid_gapped(
        area,
        windows.len() + extra,
        weights,
        crate::layout::WINDOW_GAP,
    );
    Some((windows, rects))
}

/// Tile every grid window in `space` into its work area (excluding the panel)
/// using `weights`. Shared by the active-workspace retile and by moving a
/// window to another (inactive) workspace, which must re-tile that space too.
/// `extra` counts reserved but not-yet-mapped cells (see
/// [`State::launching`]): the grid is laid out for `windows + extra`, so a
/// future window's frame and the window itself always occupy the same cell.
/// Floating/fullscreen windows are left where they are (their z-index keeps
/// them above the grid).
fn tile_space(
    space: &mut Space<Window>,
    output: &Output,
    weights: &crate::layout::GridWeights,
    panel_bottom: bool,
    fullscreen: &[Window],
    extra: usize,
) {
    let Some((windows, rects)) =
        tile_grid_rects(space, output, weights, panel_bottom, fullscreen, extra)
    else {
        return;
    };
    for (window, rect) in windows.into_iter().zip(rects) {
        // Grid windows sit below floats/fullscreen (see the z-index constants).
        window.override_z_index(GRID_Z);
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| state.size = Some(rect.size));
            let _ = toplevel.send_pending_configure();
        }
        space.map_element(window, rect.loc, false);
    }
}

/// A window takes a tiling cell only if it is none of: fullscreen (covers the
/// output), a dialog/modal window (has an `xdg_toplevel` parent or the modal
/// hint), or a fixed-size window (`min_size == max_size`, e.g. utility
/// windows that size themselves). Fullscreen membership is tracked on the
/// compositor side; the last two are read from the client's committed state.
fn is_tiled_window(window: &Window, fullscreen: &[Window]) -> bool {
    if fullscreen.contains(window) {
        return false;
    }
    let Some(toplevel) = window.toplevel() else {
        return false;
    };
    let dialog = with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .map(|attrs| attrs.parent.is_some() || attrs.modal)
            .unwrap_or(false)
    });
    if dialog {
        return false;
    }
    let (min, max) = with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        (current.min_size, current.max_size)
    });
    !((min.w > 0 || min.h > 0) && min == max)
}

/// Current broken-down local time (`localtime_r` honors the system TZ), or
/// `None` if `localtime_r` fails. Shared by the panel clock and screenshot
/// stamps so the `libc` time handling lives in one place.
pub(crate) fn local_tm() -> Option<libc::tm> {
    // SAFETY: `time` accepts a null pointer; it returns epoch seconds or `-1`
    // on error, and the returned value is passed by value to `localtime_r`.
    let secs = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: an all-zero bit pattern is a valid initialized `libc::tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `&secs` and `&mut tm` are valid, distinct caller-owned storage;
    // `localtime_r` writes only `tm` on success.
    if unsafe { libc::localtime_r(&secs, &mut tm).is_null() } {
        None
    } else {
        Some(tm)
    }
}

/// The pid of the process connected to an accepted wayland socket
/// (SO_PEERCRED), used to attribute a client to the launch that spawned it.
/// `None` on non-Linux backends, which never run clients here anyway.
fn socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `getsockopt` fills a caller-owned `ucred` on a valid fd; the
        // socket is owned by the caller and stays open for the call.
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut cred as *mut libc::ucred as *mut libc::c_void,
                &mut len,
            )
        };
        (rc == 0).then_some(cred.pid as u32)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        None
    }
}

/// Parent pid of `pid` from `/proc/<pid>/stat` (field 4, after the parenthesized
/// comm which may itself contain spaces). Linux-only, like `pending_launch_for_pid`.
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// Whether the process `pid` no longer exists — and reap it if it just
/// finished. Spawned launches are `Command::spawn` children of xfar; nobody
/// `waitpid`s them, so an exited one would linger as a zombie whose pid stays
/// claimable (and reserve its grid frame) until xfar exits. A non-blocking
/// `waitpid` both detects the exit and reaps it.
#[cfg(target_os = "linux")]
fn pid_dead(pid: u32) -> bool {
    let mut status = 0;
    // SAFETY: `waitpid` on one of our own child pids with WNOHANG; reaps a
    // finished child and is a no-op while it still runs.
    let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    match rc {
        0 => false,                           // still running
        p if p == pid as libc::pid_t => true, // was finished; reaped now
        _ => true,                            // ECHILD: no longer ours (already gone)
    }
}

#[cfg(not(target_os = "linux"))]
fn pid_dead(pid: u32) -> bool {
    let _ = pid;
    false
}

/// Whether any process in the process group `pgid` is still alive (a `kill(0`
/// probe delivers no signal). Used instead of `pid_dead` for spawn release:
/// a group can outlive its `sh` leader (which is a direct child of xfar and
/// gets reaped by `pid_dead`), so only the group membership answers whether
/// the app it started still holds its DBus name.
fn group_alive(pgid: u32) -> bool {
    // SAFETY: callers pass valid non-zero process-group IDs (production groups
    // are created by xfar); signal 0 probes membership and delivers no signal.
    let rc = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
    rc == 0
}

/// Compact local-time stamp for screenshot filenames (`YYYYMMDD-HHMMSS`),
/// matching the format of the manual capture script.
fn stamp() -> String {
    let Some(tm) = local_tm() else {
        return "0".to_string();
    };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// The xdg-toplevel title of a window, if the client set one.
fn window_title(window: &Window) -> Option<String> {
    let surface = window.toplevel()?.wl_surface();
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .and_then(|attrs| attrs.title.clone())
    })
}

/// Per-client state attached to each connected Wayland client.
#[derive(Default)]
pub struct ClientState {
    /// Per-client compositor bookkeeping required by Smithay.
    pub compositor_state: CompositorClientState,
    /// The launch command this client's app was spawned with, when xfar spawned
    /// it (`State::spawn_wayland`); lets session save/restore attribute open
    /// windows to commands. `None` for clients xfar did not launch — those apps
    /// cannot be restored.
    pub session_cmd: Option<String>,
    /// The client's pid, when connectable from the socket; lets a mapped
    /// window resolve back to the spawn that produced it (see
    /// [`State::resolve_group`]).
    pub client_pid: Option<u32>,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Import the newly-attached buffer so the renderer can sample it.
        on_commit_buffer_handler::<Self>(surface);

        // Update the owning window's cached state, then assign its layout role
        // (grid cell / float / fullscreen) and send the initial configure. The
        // role only becomes known once the client's committed state is in
        // (xdg parent set, fixed min/max, fullscreen request), so every commit
        // re-runs classification — which is a no-op once the window settled.
        let window = self.window_for_surface(surface);
        if let Some(window) = window {
            window.on_commit();
            self.classify_mapped(&window);
            if let Some(toplevel) = window.toplevel() {
                if !toplevel.is_initial_configure_sent() {
                    toplevel.send_configure();
                }
            }
        }

        // Popups: keep the manager current and send the initial configure.
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                let _ = popup.send_configure();
            }
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for State {}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Track the new window on the active workspace. Its layout role (grid
        // cell, floating dialog, fullscreen) is not known yet — the xdg parent
        // (and fixed min/max) arrive with the client's own requests — so the
        // first commit classifies and places/sizes it in `classify_mapped`.
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);

        // During restore, the active workspace is the one being filled. Consume
        // the matching command and advance after its window maps.
        if let Some(command) = window_session_cmd(&window) {
            if self.consume_launch(&command) {
                self.advance_restore();
            }
        }

        // A mapped client marks its matching tracked group as windowed when the
        // process group can be resolved.
        self.mark_group_windowed(&window);

        // Apply the configured focus-on-map behavior.
        if self.window.focus_new {
            let serial = SERIAL_COUNTER.next_serial();
            self.focus_window(&window, serial);
        }
    }

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        // The xdg parent is set synchronously (not double-buffered) and can
        // arrive any time after `new_toplevel`, so re-classify: a window that
        // just gained (or lost) a parent now floats / tiles again.
        let window = self.window_for_surface(surface.wl_surface());
        if let Some(window) = window {
            self.classify_mapped(&window);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        // Fullscreen covers the output and hides the panel.
        let Some(geom) = self.space.output_geometry(&self.output) else {
            return;
        };
        let window = self.window_for_surface(surface.wl_surface());
        if let Some(window) = window {
            self.fullscreen.push(window.clone());
            surface.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(geom.size);
            });
            let _ = surface.send_pending_configure();
            // The remaining grid windows re-tile without the fullscreen one.
            self.reflow();
            self.place_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Leaving fullscreen: drop the track, clear the state, and let the
        // window back into the grid where it belongs.
        let window = self.window_for_surface(surface.wl_surface());
        if let Some(window) = window {
            self.fullscreen.retain(|w| w != &window);
            surface.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
            });
            let _ = surface.send_pending_configure();
            self.reflow();
        }
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: WlSeat, _serial: Serial) {
        // Tiling model: windows are placed by the grid, so client-initiated
        // interactive moves are ignored.
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // Tiling model: sizes come from the grid (and the resize shortcuts),
        // never from client-initiated interactive resize.
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // Position the popup per its positioner (relative to the parent), then
        // track it so it is configured and rendered.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
        });
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        tracing::info!("toplevel closed");
        let window = self
            .space
            .elements()
            .chain(self.workspaces.iter().flatten().flat_map(|s| s.elements()))
            .find(|w| {
                w.toplevel()
                    .is_some_and(|t| t.wl_surface() == surface.wl_surface())
            })
            .cloned();
        if let Some(window) = window {
            if self.space.elements().any(|w| w == &window) {
                self.space.unmap_elem(&window);
            } else if let Some(slot) = self
                .workspaces
                .iter_mut()
                .flatten()
                .find(|s| s.elements().any(|w| w == &window))
            {
                slot.unmap_elem(&window);
            }
            // Removing a fullscreen window updates whether the panel is hidden.
            self.fullscreen.retain(|w| w != &window);
        }
        // Reflow the remaining windows into the grid (resetting resize weights).
        self.reflow();

        // Move focus to the next remaining window (topmost), or clear it.
        self.focus_topmost();
        // A closed window may have emptied an inactive workspace.
        self.prune_empty_workspaces();
    }
}

/// Handle `xdg-decoration` by always selecting server-side mode. xfar draws no
/// decoration, so clients that honor the protocol omit their own.
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        enforce_no_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // The client's preference is ignored; the policy is always server-side.
        enforce_no_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        enforce_no_decoration(&toplevel);
    }
}

/// Configure a toplevel for server-side decorations (which xfar draws as none).
fn enforce_no_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    toplevel.send_configure();
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_output!(State);
delegate_seat!(State);
delegate_xdg_shell!(State);
delegate_xdg_decoration!(State);
delegate_data_device!(State);

#[cfg(test)]
mod tests {
    use super::*;

    fn launching(commands: &[&str]) -> Vec<Launching> {
        commands
            .iter()
            .map(|c| Launching {
                command: (*c).to_string(),
                since: Instant::now(),
            })
            .collect()
    }

    #[test]
    fn pop_launch_cell_removes_first_match_only() {
        // Duplicate commands each map their own cell: popping once leaves the
        // second for the second window of the same command.
        let mut list = launching(&["foot", "false", "foot"]);
        assert!(pop_launch_cell(&mut list, "foot"));
        assert_eq!(
            list.iter().map(|l| l.command.as_str()).collect::<Vec<_>>(),
            vec!["false", "foot"]
        );
        assert!(pop_launch_cell(&mut list, "foot"));
        assert_eq!(
            list.iter().map(|l| l.command.as_str()).collect::<Vec<_>>(),
            vec!["false"]
        );
        assert!(!pop_launch_cell(&mut list, "never-launched"));
        assert!(!pop_launch_cell(&mut list, "foot"));
    }

    #[test]
    fn order_by_rank_matches_saved_commands_with_unranked_after() {
        // Rank commands by saved workspace order (c2 first).
        let commands = ["c2", "c1", "c3", "c2"];
        let items = vec!["c3", "c2", "c1", "other", "c2"];
        let result = order_by_rank(
            &|item: &&str| commands.iter().position(|c| c == item),
            items,
        );
        // Ranked ascending (both c2s at rank 0 keep input order, then c1, c3);
        // the unmatched "other" trails in its relative order.
        assert_eq!(result, vec!["c2", "c2", "c1", "c3", "other"]);
    }

    #[test]
    fn process_group_terms_whole_spawn_and_liveness_tracks_it() {
        // The stale-reclaim mechanism TERMs `-pgid` of xfar's own `sh -c`
        // spawn and decides liveness by `kill(-pgid, 0)`. Verify both against a
        // real process group, including a *forked* member (the GTK wedge case:
        // the shell's child, not the shell itself, holds the DBus name): with
        // `&`, `sh` does not exec and only the group signal reaches the sleeper.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30 & sleep 30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id();
        // Its own new group: xfar's group TERM cannot touch the test runner.
        assert_ne!(pgid as libc::pid_t, std::process::id() as libc::pid_t);
        assert!(
            group_alive(pgid),
            "fresh spawn reports alive through its group"
        );

        // SAFETY: `pgid` is the group created above for this child process.
        unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGTERM) };

        // The group dies as a unit; reap the leader so `pid_dead` on the sweep
        // path sees a gone pid, not a zombie.
        let deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(pgid) {
            if Instant::now() >= deadline {
                panic!("process group survived SIGTERM for 5s");
            }
            pid_dead(pgid);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!group_alive(pgid), "group dies with its members");
        let _ = child.wait();
    }
}
