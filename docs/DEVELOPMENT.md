# Development Setup

Develop xfar on macOS and build or run it in a disposable Ubuntu VM. The
repository stays on the host and is mounted at `/home/ubuntu/xfar` in the VM.

## Host setup

Install [Homebrew](https://brew.sh) and multipass:

```bash
brew install --cask multipass
```

Rust is optional on the host. Install it with [rustup](https://rustup.rs) if
you want to run `cargo fmt --check`; build, check, test, and compositor runs
belong in the Linux VM.

## VM lifecycle

Create and provision the VM:

```bash
./scripts/vm.sh create
```

Provisioning installs the C/C++ build tools, Rust toolchain, Wayland
development libraries, and packages for nested and headless testing. Cargo
artifacts are stored under `~/.cache/xfar/target` in the VM rather than on the
host mount.

| Command | Effect |
|---|---|
| `./scripts/vm.sh create` | Create, mount, and provision the VM |
| `./scripts/vm.sh provision` | Re-run provisioning |
| `./scripts/vm.sh shell` | Open a shell in the VM |
| `./scripts/vm.sh status` | Show VM status |
| `./scripts/vm.sh destroy` | Delete and purge the VM |

The VM name, image, CPU count, memory, and disk size can be changed with
`XFAR_VM_NAME`, `XFAR_VM_IMAGE`, `XFAR_VM_CPUS`, `XFAR_VM_MEM`, and
`XFAR_VM_DISK`. Keep project state in git; the VM is disposable and can be
recreated at any time.

## Build and test

Run these commands inside the VM:

| Task | Command |
|---|---|
| Debug build | `./scripts/build.sh` |
| Release build | `./scripts/build.sh --release` |
| Format check and tests | `./scripts/test.sh` |
| Development session | `./scripts/dev.sh` |
| User install | `./scripts/install.sh` |

Arguments to `build.sh` and `test.sh` are forwarded to Cargo. `install.sh`
runs `cargo install` and places `xfar` under `$XFAR_PREFIX/bin`, which defaults
to `~/.local/bin`.

`rust-toolchain.toml` pins the toolchain. `RUST_LOG` controls logging and
defaults to `info`:

```bash
RUST_LOG=debug ./scripts/dev.sh
```

### Nested session

The default build uses Smithay's winit backend. `dev.sh` uses an existing
Wayland or X11 display and starts Xvfb with software GL when the VM is
headless. To inspect that display, run this in the VM:

```bash
x11vnc -display :99 -localhost
```

Forward port 5900 from the VM to the host, then connect the VNC client to
`localhost:5900`. `XFAR_STARTUP_CMD` can launch a Wayland client with the
compositor's socket when the session starts:

```bash
XFAR_STARTUP_CMD=weston-terminal ./scripts/dev.sh
```

### Bare-TTY session

Build with the `tty` feature to use DRM/KMS, libinput, and libseat on real
hardware:

```bash
cargo build --release --features tty --bin xfar
```

With that feature, xfar selects DRM when neither `WAYLAND_DISPLAY` nor
`DISPLAY` is set; otherwise it uses winit. The VM normally has no `/dev/dri`
device, so exercise this path on compatible hardware. Screenshots through
`xfar.screenshot` and `SIGUSR1` are also available only on the DRM backend.

## Compositor controls

xfar uses an automatic grid with `ceil(sqrt(n))` columns. Map and unmap events
retile the active workspace evenly. Dialogs and fixed-size windows float;
fullscreen windows cover the output and hide the panel.

- Click a window to focus it. `window.focus_new` controls focus on map.
- `Super+Left` and `Super+Right` resize the focused column.
- `Super+Up` and `Super+Down` resize the focused row.
- `Super+Space` rotates windows through the grid.
- `Alt+Tab` cycles tiled windows and outlines the pending selection.
- `Super+Q` closes the focused window; `Super+Shift+Q` quits xfar.
- `Super+N` creates a workspace. `Super+Tab` and `Super+Shift+Tab` switch in
  either direction; the `Super+Shift+Left` and `Super+Shift+Right` bindings
  move the focused window between workspaces.
- Empty inactive workspaces are retired. A window that exceeds the configured
  grid capacity moves to the next workspace only when it already exists;
  otherwise the current grid grows past the configured limit.

The persistent panel contains the launcher field, workspace indicators, and a
local-time clock. It can be placed at the top or bottom of the output.
Indicators are clickable when more than one workspace exists. The launcher
searches applications, open windows, and built-in actions. `xfar.help` and
`xfar.validate` open their reports in `XFAR_TERMINAL` (default `foot`);
`xfar.settings` opens the in-compositor settings panel.

## Configuration

xfar reads `$XDG_CONFIG_HOME/xfar/config.toml`, falling back to
`~/.config/xfar/config.toml`. The file and every field are optional. Unknown
keys are rejected. On the nested backend, a malformed file is reported with its
path and parser location, then xfar continues with defaults. Invalid ranges and
enumerated values are also reset to defaults with a diagnostic. The DRM backend
currently uses defaults when loading fails and does not run this normalization.

The current schema is:

```toml
[keyboard]
repeat_delay_ms = 200
repeat_rate = 25

[keybindings]
# Modifiers are Super, Shift, Alt, and Ctrl; key names are case-insensitive.
launcher = "Super+D"
grow_width = "Super+Right"
shrink_width = "Super+Left"
grow_height = "Super+Up"
shrink_height = "Super+Down"
cycle = "Super+Space"
close = "Super+Q"
quit = "Super+Shift+Q"
new_workspace = "Super+N"
next_workspace = "Super+Tab"
prev_workspace = "Super+Shift+Tab"
move_to_next_workspace = "Super+Shift+Right"
move_to_prev_workspace = "Super+Shift+Left"

[appearance]
# Colors accept #rrggbb or #rrggbbaa.
background = "#1c1f26"
panel = "#262933"
accent = "#4280ed"
text = "#d4d9e6"
placeholder = "#6b7382"
vdesktop_active = "#4280ed"
vdesktop = "#3d4252"
clock = "#d4d9e6"
switch_border = "#4280ed"
background_image = "wallpaper.png" # Relative paths use the working directory; ~/ expands.
background_mode = "fit"           # "stretch", "fit", "center", or "tile".
panel_position = "top"            # "top" or "bottom".

[window]
focus_new = true

[tiling]
min_width = 320
min_height = 240
max_columns = 3
max_rows = 3

[cursor]
visible = true # The nested backend uses the host cursor.

[screenshot]
dir = "~/screenshots"

[session]
keep_state = true
```

Validate without starting the compositor:

```bash
xfar validate
```

With `session.keep_state` enabled, xfar saves tracked launch commands, workspace
placement, and grid weights on a normal shutdown. The next start relaunches
them. Clients that were not launched by xfar cannot be restored. Session state
is stored at `$XDG_STATE_HOME/xfar/session.toml`, or
`~/.local/state/xfar/session.toml` when `XDG_STATE_HOME` is unset or empty.

## Releases

Tag a commit and push the tag; the `release` workflow builds the binary with
`--features tty` and attaches `xfar-<version>-x86_64-unknown-linux-gnu.tar.gz`
to a GitHub release:

```bash
git tag v0.1.0 && git push origin v0.1.0
```

Run the same workflow manually from the Actions tab to get the tarball as a
build artifact without creating a release.
