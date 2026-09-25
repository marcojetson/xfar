//! User configuration.
//!
//! xfar reads an optional TOML file at `$XDG_CONFIG_HOME/xfar/config.toml`,
//! falling back to `~/.config/xfar/config.toml`. Unset fields use their
//! defaults. Parse failures include the file path and parser location; callers
//! can report the error and continue with defaults.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use smithay::backend::renderer::Color32F;

use crate::keybinding::{parse_chord, Chord, Command};
use crate::theme::{parse_color, Palette};

/// Omit values that match their type's default when serializing.
fn is_default<T: PartialEq + Default>(value: &T) -> bool {
    *value == T::default()
}

/// Per-key serialization helpers that compare against each config type's real
/// defaults.
macro_rules! skip_default {
    ($config:ty, $ty:ty, $($field:ident),+) => {
        $(
            pub(crate) fn $field(v: &$ty) -> bool {
                v == &<$config>::default().$field
            }
        )+
    };
}

skip_default!(KeyboardConfig, i32, repeat_delay_ms, repeat_rate);
skip_default!(
    TilingConfig,
    i32,
    min_width,
    min_height,
    max_columns,
    max_rows
);
skip_default!(WindowConfig, bool, focus_new);
skip_default!(CursorConfig, bool, visible);
skip_default!(SessionConfig, bool, keep_state);
skip_default!(AppearanceConfig, String, background_mode, panel_position);
skip_default!(
    KeybindingsConfig,
    String,
    launcher,
    grow_width,
    shrink_width,
    grow_height,
    shrink_height,
    cycle,
    close,
    quit,
    new_workspace,
    next_workspace,
    prev_workspace,
    move_to_next_workspace,
    move_to_prev_workspace
);

/// Top-level user configuration. Every section and field is optional; anything
/// unset uses xfar's built-in default. Unknown keys are rejected so typos
/// surface as errors rather than being silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Keyboard behavior (key repeat).
    #[serde(skip_serializing_if = "is_default")]
    pub keyboard: KeyboardConfig,
    /// Keyboard shortcuts.
    #[serde(skip_serializing_if = "is_default")]
    pub keybindings: KeybindingsConfig,
    /// Colors and other visual appearance.
    #[serde(skip_serializing_if = "is_default")]
    pub appearance: AppearanceConfig,
    /// Window behavior (placement, focus).
    #[serde(skip_serializing_if = "is_default")]
    pub window: WindowConfig,
    /// Tiling behavior (minimum cell size).
    #[serde(skip_serializing_if = "is_default")]
    pub tiling: TilingConfig,
    /// Cursor visibility.
    #[serde(skip_serializing_if = "is_default")]
    pub cursor: CursorConfig,
    /// Screen capture (`xfar.screenshot`) destination.
    #[serde(skip_serializing_if = "is_default")]
    pub screenshot: ScreenshotConfig,
    /// Session save/restore behavior.
    #[serde(skip_serializing_if = "is_default")]
    pub session: SessionConfig,
}

impl Config {
    /// Check numeric fields for sane ranges. Any out-of-range value is reset to
    /// its default and described in the returned list (empty when everything is
    /// valid), so a nonsensical number produces a clear message rather than odd
    /// behavior. String-typed sections (keybindings, appearance) are validated
    /// separately when they are resolved.
    pub fn validate(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let d = Config::default();

        check_min(
            "keyboard.repeat_delay_ms",
            &mut self.keyboard.repeat_delay_ms,
            0,
            d.keyboard.repeat_delay_ms,
            &mut problems,
        );
        check_min(
            "keyboard.repeat_rate",
            &mut self.keyboard.repeat_rate,
            0,
            d.keyboard.repeat_rate,
            &mut problems,
        );
        check_min(
            "tiling.min_width",
            &mut self.tiling.min_width,
            1,
            d.tiling.min_width,
            &mut problems,
        );
        check_min(
            "tiling.min_height",
            &mut self.tiling.min_height,
            1,
            d.tiling.min_height,
            &mut problems,
        );
        check_min(
            "tiling.max_columns",
            &mut self.tiling.max_columns,
            1,
            d.tiling.max_columns,
            &mut problems,
        );
        check_min(
            "tiling.max_rows",
            &mut self.tiling.max_rows,
            1,
            d.tiling.max_rows,
            &mut problems,
        );
        if crate::wallpaper::WallpaperMode::parse(&self.appearance.background_mode).is_none() {
            problems.push(format!(
                "appearance.background_mode must be one of \"stretch\", \"fit\", \"center\", \
                 or \"tile\" (was {:?}); using \"{}\"",
                self.appearance.background_mode, d.appearance.background_mode
            ));
            self.appearance.background_mode = d.appearance.background_mode;
        }
        if self.appearance.panel_position != "top" && self.appearance.panel_position != "bottom" {
            problems.push(format!(
                "appearance.panel_position must be one of \"top\" or \"bottom\" \
                 (was {:?}); using \"{}\"",
                self.appearance.panel_position, d.appearance.panel_position
            ));
            self.appearance.panel_position = d.appearance.panel_position;
        }
        problems
    }
}

/// Reset `value` to `default` (and record a problem) when it is below `min`.
fn check_min<T>(field: &str, value: &mut T, min: T, default: T, problems: &mut Vec<String>)
where
    T: Copy + Ord + std::fmt::Display,
{
    if *value < min {
        problems.push(format!(
            "{field} must be >= {min} (was {value}); using {default}"
        ));
        *value = default;
    }
}

/// Keyboard behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeyboardConfig {
    /// Delay before a held key begins repeating, in milliseconds.
    #[serde(skip_serializing_if = "repeat_delay_ms")]
    pub repeat_delay_ms: i32,
    /// Key repeats per second once repeating starts.
    #[serde(skip_serializing_if = "repeat_rate")]
    pub repeat_rate: i32,
}

impl Default for KeyboardConfig {
    fn default() -> Self {
        Self {
            repeat_delay_ms: 200,
            repeat_rate: 25,
        }
    }
}

/// Keyboard shortcuts. Values use the chord syntax accepted by
/// [`crate::keybinding::parse_chord`]. Workspace switching and movement use
/// next/previous commands because workspace count is dynamic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeybindingsConfig {
    #[serde(skip_serializing_if = "launcher")]
    pub launcher: String,
    #[serde(skip_serializing_if = "grow_width")]
    pub grow_width: String,
    #[serde(skip_serializing_if = "shrink_width")]
    pub shrink_width: String,
    #[serde(skip_serializing_if = "grow_height")]
    pub grow_height: String,
    #[serde(skip_serializing_if = "shrink_height")]
    pub shrink_height: String,
    #[serde(skip_serializing_if = "cycle")]
    pub cycle: String,
    #[serde(skip_serializing_if = "close")]
    pub close: String,
    #[serde(skip_serializing_if = "quit")]
    pub quit: String,
    /// Create and switch to a new workspace.
    #[serde(skip_serializing_if = "new_workspace")]
    pub new_workspace: String,
    /// Switch to the next workspace, wrapping around.
    #[serde(skip_serializing_if = "next_workspace")]
    pub next_workspace: String,
    /// Switch to the previous workspace, wrapping around.
    #[serde(skip_serializing_if = "prev_workspace")]
    pub prev_workspace: String,
    /// Move the focused window one workspace forward, wrapping around.
    #[serde(skip_serializing_if = "move_to_next_workspace")]
    pub move_to_next_workspace: String,
    /// Move the focused window one workspace back, wrapping around.
    #[serde(skip_serializing_if = "move_to_prev_workspace")]
    pub move_to_prev_workspace: String,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            launcher: "Super+D".into(),
            grow_width: "Super+Right".into(),
            shrink_width: "Super+Left".into(),
            grow_height: "Super+Up".into(),
            shrink_height: "Super+Down".into(),
            cycle: "Super+space".into(),
            close: "Super+Q".into(),
            quit: "Super+Shift+Q".into(),
            new_workspace: "Super+N".into(),
            next_workspace: "Super+Tab".into(),
            prev_workspace: "Super+Shift+Tab".into(),
            move_to_next_workspace: "Super+Shift+Right".into(),
            move_to_prev_workspace: "Super+Shift+Left".into(),
        }
    }
}

impl KeybindingsConfig {
    /// Resolve every binding string into a `(chord, command)` table, or return the
    /// first invalid binding with the field name that caused it.
    pub fn resolve(&self) -> Result<Vec<(Chord, Command)>, KeybindingError> {
        let entries: [(&str, &str, Command); 13] = [
            ("launcher", self.launcher.as_str(), Command::Launcher),
            ("grow_width", self.grow_width.as_str(), Command::GrowWidth),
            (
                "shrink_width",
                self.shrink_width.as_str(),
                Command::ShrinkWidth,
            ),
            (
                "grow_height",
                self.grow_height.as_str(),
                Command::GrowHeight,
            ),
            (
                "shrink_height",
                self.shrink_height.as_str(),
                Command::ShrinkHeight,
            ),
            ("cycle", self.cycle.as_str(), Command::Cycle),
            ("close", self.close.as_str(), Command::Close),
            ("quit", self.quit.as_str(), Command::Quit),
            (
                "new_workspace",
                self.new_workspace.as_str(),
                Command::NewWorkspace,
            ),
            (
                "next_workspace",
                self.next_workspace.as_str(),
                Command::NextWorkspace,
            ),
            (
                "prev_workspace",
                self.prev_workspace.as_str(),
                Command::PrevWorkspace,
            ),
            (
                "move_to_next_workspace",
                self.move_to_next_workspace.as_str(),
                Command::MoveToNextWorkspace,
            ),
            (
                "move_to_prev_workspace",
                self.move_to_prev_workspace.as_str(),
                Command::MoveToPrevWorkspace,
            ),
        ];
        let mut table = Vec::with_capacity(entries.len());
        for (field, spec, action) in entries {
            let chord = parse_chord(spec).map_err(|reason| KeybindingError {
                field: field.to_string(),
                value: spec.to_string(),
                reason,
            })?;
            table.push((chord, action));
        }
        Ok(table)
    }
}

/// Window behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    /// Whether a newly-mapped window receives keyboard focus.
    #[serde(skip_serializing_if = "focus_new")]
    pub focus_new: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self { focus_new: true }
    }
}

/// Tiling behavior. `min_width` and `min_height` bound manual resizing.
/// `max_columns` and `max_rows` cap each workspace's grid. A new window that
/// exceeds the capacity moves to the next existing workspace; otherwise the
/// current grid grows past the configured limit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TilingConfig {
    /// Minimum width of a tiled cell, in logical pixels (resize clamp).
    #[serde(skip_serializing_if = "min_width")]
    pub min_width: i32,
    /// Minimum height of a tiled cell, in logical pixels (resize clamp).
    #[serde(skip_serializing_if = "min_height")]
    pub min_height: i32,
    /// Maximum grid columns per workspace before overflow.
    #[serde(skip_serializing_if = "max_columns")]
    pub max_columns: i32,
    /// Maximum rows per grid column before overflow.
    #[serde(skip_serializing_if = "max_rows")]
    pub max_rows: i32,
}

impl Default for TilingConfig {
    fn default() -> Self {
        Self {
            min_width: 320,
            min_height: 240,
            max_columns: 3,
            max_rows: 3,
        }
    }
}

/// Cursor configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    /// Whether the compositor draws its own cursor (hardware backends only —
    /// the nested winit backend uses the host cursor).
    #[serde(skip_serializing_if = "visible")]
    pub visible: bool,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self { visible: true }
    }
}

/// Default screenshot directory (a leading `~/` expands to `$HOME`).
pub const SCREENSHOT_DIR_DEFAULT: &str = "~/screenshots";

/// Where screenshots taken via `xfar.screenshot` are saved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenshotConfig {
    /// Screenshot directory; a leading `~/` expands to `$HOME`. `None` (the
    /// default) uses [`SCREENSHOT_DIR_DEFAULT`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Whether xfar remembers the open apps across restarts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// When false, the session is neither saved on quit nor restored on start.
    #[serde(skip_serializing_if = "keep_state")]
    pub keep_state: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { keep_state: true }
    }
}

/// Colors and other visual appearance. Each color is a string `"#rrggbb"` or
/// `"#rrggbbaa"`; an unset color keeps xfar's built-in default. Field names match
/// [`crate::theme::Palette`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vdesktop_active: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vdesktop: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_border: Option<String>,
    /// Path to an image drawn behind the windows (see `crate::wallpaper`).
    /// Relative paths are resolved from the working directory. `None` (the
    /// default) keeps the solid `background` color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_image: Option<String>,
    /// How the background image fills the screen: `stretch`, `fit`, `center`,
    /// or `tile` (default `fit`).
    #[serde(skip_serializing_if = "background_mode")]
    pub background_mode: String,
    /// Persistent panel position: `"top"` or `"bottom"` (default `"top"`).
    #[serde(skip_serializing_if = "panel_position")]
    pub panel_position: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            background: None,
            panel: None,
            accent: None,
            text: None,
            placeholder: None,
            vdesktop_active: None,
            vdesktop: None,
            clock: None,
            switch_border: None,
            background_image: None,
            background_mode: "fit".into(),
            panel_position: "top".into(),
        }
    }
}

impl AppearanceConfig {
    /// Resolve into a full [`Palette`], overlaying any provided colors onto the
    /// defaults, or return the first invalid color with the field that caused it.
    pub fn resolve(&self) -> Result<Palette, AppearanceError> {
        let mut palette = Palette::default();
        apply_color("background", &self.background, &mut palette.background)?;
        apply_color("panel", &self.panel, &mut palette.panel)?;
        apply_color("accent", &self.accent, &mut palette.accent)?;
        apply_color("text", &self.text, &mut palette.text)?;
        apply_color("placeholder", &self.placeholder, &mut palette.placeholder)?;
        apply_color(
            "vdesktop_active",
            &self.vdesktop_active,
            &mut palette.vdesktop_active,
        )?;
        apply_color("vdesktop", &self.vdesktop, &mut palette.vdesktop)?;
        apply_color("clock", &self.clock, &mut palette.clock)?;
        apply_color(
            "switch_border",
            &self.switch_border,
            &mut palette.switch_border,
        )?;
        Ok(palette)
    }
}

/// Parse `value` (if set) into `slot`, tagging any error with the field name.
fn apply_color(
    field: &str,
    value: &Option<String>,
    slot: &mut Color32F,
) -> Result<(), AppearanceError> {
    if let Some(spec) = value {
        *slot = parse_color(spec).map_err(|reason| AppearanceError {
            field: field.to_string(),
            value: spec.clone(),
            reason,
        })?;
    }
    Ok(())
}

/// An error loading the configuration file. A missing file is *not* an error
/// (the defaults are used); these represent a file that exists but is unusable.
#[derive(Debug)]
pub enum ConfigError {
    /// The file exists but could not be read.
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file was read but is not valid configuration.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "could not read config file {}: {source}", path.display())
            }
            ConfigError::Parse { path, source } => {
                // toml's error Display includes the line/column and a message.
                write!(f, "invalid config file {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// An invalid keybinding string in the configuration.
#[derive(Debug)]
pub struct KeybindingError {
    /// The `[keybindings]` field that was invalid (e.g. `tile_left`).
    pub field: String,
    /// The offending value as written in the file.
    pub value: String,
    /// A short reason (e.g. `unknown key`).
    pub reason: String,
}

impl fmt::Display for KeybindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid keybinding for `{}` (\"{}\"): {}",
            self.field, self.value, self.reason
        )
    }
}

impl std::error::Error for KeybindingError {}

/// An invalid appearance color in the configuration.
#[derive(Debug)]
pub struct AppearanceError {
    /// The `[appearance]` field that was invalid (e.g. `accent`).
    pub field: String,
    /// The offending value as written in the file.
    pub value: String,
    /// A short reason (e.g. `invalid hex color`).
    pub reason: String,
}

impl fmt::Display for AppearanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid appearance color for `{}` (\"{}\"): {}",
            self.field, self.value, self.reason
        )
    }
}

impl std::error::Error for AppearanceError {}

/// `$XDG_<var>/xfar/<sub>` when `XDG_<var>` is set (non-empty), else
/// `$HOME/<fallback>/xfar/<sub>`. `None` only when the variable and `HOME` are
/// both unset. Shared by the config, session, and wallpaper paths.
pub(crate) fn xdg_dir(var: &str, fallback: &str, sub: &str) -> Option<PathBuf> {
    let root = match std::env::var(var) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(fallback),
    };
    let dir = root.join("xfar");
    Some(if sub.is_empty() { dir } else { dir.join(sub) })
}

/// Expand a leading `~/` to `$HOME` (config paths are user-friendly this way —
/// the shell isn't involved in spawning the capture command).
pub(crate) fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

/// The path xfar reads configuration from: `$XDG_CONFIG_HOME/xfar/config.toml`,
/// or `~/.config/xfar/config.toml` when `XDG_CONFIG_HOME` is unset or empty.
/// `None` only when neither `XDG_CONFIG_HOME` nor `HOME` is set.
pub fn config_path() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config", "config.toml")
}

/// Load configuration from [`config_path`]. Returns the defaults when the file
/// is absent (or no config path can be determined); returns an error only when a
/// file exists but cannot be read or parsed.
pub fn load() -> Result<Config, ConfigError> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => parse(&contents).map_err(|source| ConfigError::Parse { path, source }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

/// Parse configuration from a TOML string, applying defaults for anything unset.
pub fn parse(contents: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(contents)
}

/// Write `self` to `path` as TOML, creating parent directories, emitting only
/// the sections whose contents differ from the defaults (see [`is_default`]).
pub fn save_to(config: &Config, path: &std::path::Path) -> Result<(), String> {
    let text = toml::to_string_pretty(config)
        .map_err(|e| format!("could not serialize configuration: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Keys that hold integer values in the configuration schema.
const NUMERIC_KEYS: &[(&str, &str)] = &[
    ("keyboard", "repeat_delay_ms"),
    ("keyboard", "repeat_rate"),
    ("tiling", "min_width"),
    ("tiling", "min_height"),
    ("tiling", "max_columns"),
    ("tiling", "max_rows"),
];

/// Keys that hold boolean values in the configuration schema.
const BOOLEAN_KEYS: &[(&str, &str)] = &[
    ("window", "focus_new"),
    ("cursor", "visible"),
    ("session", "keep_state"),
];

/// Convert a settings-panel edit (a `(section, key)` → text value) into a typed
/// [`toml::Value`], so saves stay schema-correct without special-casing save.
pub fn patch_value(section: &str, key: &str, text: &str) -> Result<toml::Value, String> {
    if NUMERIC_KEYS.contains(&(section, key)) {
        text.trim()
            .parse::<i64>()
            .map(toml::Value::Integer)
            .map_err(|_| format!("{section}.{key} must be a whole number (was \"{text}\")"))
    } else if BOOLEAN_KEYS.contains(&(section, key)) {
        match text.trim().to_ascii_lowercase().as_str() {
            "yes" | "true" | "on" | "1" => Ok(toml::Value::Boolean(true)),
            "no" | "false" | "off" | "0" => Ok(toml::Value::Boolean(false)),
            _ => Err(format!(
                "{section}.{key} must be true or false \
                 (yes/no, on/off, and 1/0 are also accepted; was \"{text}\")"
            )),
        }
    } else {
        Ok(toml::Value::String(text.to_string()))
    }
}

/// Apply settings-panel edits (a `(section, key)` → text value list) onto a copy
/// of `base`, producing the candidate configuration to save and apply.
pub fn patched(base: &Config, patches: &[(String, String, String)]) -> Result<Config, String> {
    let mut root = match toml::Value::try_from(base) {
        Ok(toml::Value::Table(table)) => table,
        Ok(_) => return Err("configuration is not a TOML table".into()),
        Err(e) => return Err(format!("could not read configuration: {e}")),
    };
    for (section, key, text) in patches {
        let value = patch_value(section, key, text)?;
        if let Some(table) = root.get_mut(section).and_then(toml::Value::as_table_mut) {
            table.insert(key.clone(), value);
        } else {
            let mut table = toml::map::Map::new();
            table.insert(key.clone(), value);
            root.insert(section.clone(), toml::Value::Table(table));
        }
    }
    toml::Value::Table(root)
        .try_into()
        .map_err(|e| format!("patched configuration is invalid: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_builtin_values() {
        let c = Config::default();
        assert_eq!(c.keyboard.repeat_delay_ms, 200);
        assert_eq!(c.keyboard.repeat_rate, 25);
    }

    #[test]
    fn empty_document_is_all_defaults() {
        assert_eq!(parse("").unwrap(), Config::default());
    }

    #[test]
    fn full_document_parses() {
        let c = parse("[keyboard]\nrepeat_delay_ms = 300\nrepeat_rate = 40\n").unwrap();
        assert_eq!(c.keyboard.repeat_delay_ms, 300);
        assert_eq!(c.keyboard.repeat_rate, 40);
    }

    #[test]
    fn partial_document_keeps_other_defaults() {
        // Only one field set; the other keeps its default.
        let c = parse("[keyboard]\nrepeat_rate = 40\n").unwrap();
        assert_eq!(c.keyboard.repeat_rate, 40);
        assert_eq!(c.keyboard.repeat_delay_ms, 200);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // A typo'd key surfaces as an error instead of being ignored.
        assert!(parse("[keyboard]\nrepeat_raet = 40\n").is_err());
    }

    #[test]
    fn unknown_section_is_rejected() {
        assert!(parse("[keybord]\nrepeat_rate = 40\n").is_err());
    }

    #[test]
    fn wrong_type_is_rejected() {
        assert!(parse("[keyboard]\nrepeat_rate = \"fast\"\n").is_err());
    }

    #[test]
    fn wallpaper_fields_default_and_override() {
        let c = parse(
            "[appearance]\nbackground_image = \"/tmp/wall.png\"\nbackground_mode = \"tile\"\n",
        )
        .unwrap();
        assert_eq!(
            c.appearance.background_image.as_deref(),
            Some("/tmp/wall.png")
        );
        assert_eq!(c.appearance.background_mode, "tile");
    }

    #[test]
    fn invalid_background_mode_is_reset() {
        let mut c = parse("[appearance]\nbackground_mode = \"cover\"\n").unwrap();
        let problems = c.validate();
        assert!(problems.iter().any(|p| p.contains("background_mode")));
        assert_eq!(c.appearance.background_mode, "fit");
    }

    #[test]
    fn default_keybindings_resolve() {
        // The shipped defaults must always parse into a full table.
        let table = KeybindingsConfig::default().resolve().unwrap();
        assert_eq!(table.len(), 13);
        // Sanity: the launcher default is Super+D.
        let launcher = table
            .iter()
            .find(|(_, a)| *a == Command::Launcher)
            .map(|(c, _)| *c)
            .unwrap();
        assert!(launcher.logo && !launcher.shift);
    }

    #[test]
    fn custom_keybinding_overrides_default() {
        let c = parse("[keybindings]\nlauncher = \"Ctrl+space\"\n").unwrap();
        // Overridden field changes; others keep their defaults.
        assert_eq!(c.keybindings.launcher, "Ctrl+space");
        assert_eq!(c.keybindings.shrink_width, "Super+Left");
        assert!(c.keybindings.resolve().is_ok());
    }

    #[test]
    fn invalid_keybinding_reports_field() {
        let c = parse("[keybindings]\ngrow_width = \"Super+Nope\"\n").unwrap();
        let err = c.keybindings.resolve().unwrap_err();
        assert_eq!(err.field, "grow_width");
        assert!(err.to_string().contains("grow_width"));
    }

    #[test]
    fn default_appearance_is_default_palette() {
        assert_eq!(
            AppearanceConfig::default().resolve().unwrap(),
            Palette::default()
        );
        // The wallpaper keys default to off / fit.
        assert_eq!(AppearanceConfig::default().background_image, None);
        assert_eq!(AppearanceConfig::default().background_mode, "fit");
    }

    #[test]
    fn custom_color_overrides_one_field() {
        let c = parse("[appearance]\naccent = \"#ff0000\"\n").unwrap();
        let palette = c.appearance.resolve().unwrap();
        assert_eq!(palette.accent, parse_color("#ff0000").unwrap());
        // Untouched colors keep their defaults.
        assert_eq!(palette.background, Palette::default().background);
    }

    #[test]
    fn invalid_color_reports_field() {
        let c = parse("[appearance]\naccent = \"blurple\"\n").unwrap();
        let err = c.appearance.resolve().unwrap_err();
        assert_eq!(err.field, "accent");
        assert!(err.to_string().contains("accent"));
    }

    #[test]
    fn new_appearance_fields_override_their_slots() {
        // The per-element colors (pips, clock, Alt+Tab border) resolve into their
        // own palette slots, keeping the shared accents untouched.
        let c = parse(
            "[appearance]\nvdesktop_active = \"#112233\"\nvdesktop = \"#445566\"\n\
             clock = \"#000000\"\nswitch_border = \"#ff00ff\"\n",
        )
        .unwrap();
        let palette = c.appearance.resolve().unwrap();
        assert_eq!(palette.vdesktop_active, parse_color("#112233").unwrap());
        assert_eq!(palette.vdesktop, parse_color("#445566").unwrap());
        assert_eq!(palette.clock, parse_color("#000000").unwrap());
        assert_eq!(palette.switch_border, parse_color("#ff00ff").unwrap());
        // Untouched colors keep their defaults.
        assert_eq!(palette.accent, Palette::default().accent);
        assert_eq!(palette.background, Palette::default().background);
    }

    #[test]
    fn panel_position_defaults_top_and_validates() {
        assert_eq!(AppearanceConfig::default().panel_position, "top");
        assert_eq!(
            parse("[appearance]\npanel_position = \"bottom\"\n")
                .unwrap()
                .appearance
                .panel_position,
            "bottom"
        );

        let mut c = parse("[appearance]\npanel_position = \"middle\"\n").unwrap();
        let problems = c.validate();
        assert!(problems.iter().any(|p| p.contains("panel_position")));
        assert_eq!(c.appearance.panel_position, "top");
    }

    #[test]
    fn window_defaults_and_overrides() {
        assert!(WindowConfig::default().focus_new);

        let c = parse("[window]\nfocus_new = false\n").unwrap();
        assert!(!c.window.focus_new);
    }

    #[test]
    fn window_partial_keeps_defaults() {
        let c = parse("[window]\nfocus_new = false\n").unwrap();
        assert!(!c.window.focus_new);
        assert!(WindowConfig::default().focus_new);
    }

    #[test]
    fn tiling_default_and_override() {
        assert_eq!(TilingConfig::default().min_width, 320);
        assert_eq!(TilingConfig::default().min_height, 240);
        let c = parse("[tiling]\nmin_width = 500\nmin_height = 400\n").unwrap();
        assert_eq!(c.tiling.min_width, 500);
        assert_eq!(c.tiling.min_height, 400);
    }

    #[test]
    fn tiling_capacity_default_and_override() {
        assert_eq!(TilingConfig::default().max_columns, 3);
        assert_eq!(TilingConfig::default().max_rows, 3);
        let c = parse("[tiling]\nmax_columns = 2\nmax_rows = 4\n").unwrap();
        assert_eq!(c.tiling.max_columns, 2);
        assert_eq!(c.tiling.max_rows, 4);
    }

    #[test]
    fn new_workspace_binding_default_and_override() {
        assert_eq!(KeybindingsConfig::default().new_workspace, "Super+N");
        let c = parse("[keybindings]\nnew_workspace = \"Super+T\"\n").unwrap();
        assert_eq!(c.keybindings.new_workspace, "Super+T");
    }

    #[test]
    fn workspace_navigation_bindings_default_and_override() {
        let d = KeybindingsConfig::default();
        assert_eq!(d.next_workspace, "Super+Tab");
        assert_eq!(d.prev_workspace, "Super+Shift+Tab");
        assert_eq!(d.move_to_next_workspace, "Super+Shift+Right");
        assert_eq!(d.move_to_prev_workspace, "Super+Shift+Left");
        let c = parse(
            "[keybindings]\nnext_workspace = \"Ctrl+Right\"\n\
             move_to_prev_workspace = \"Super+Ctrl+Left\"\n",
        )
        .unwrap();
        assert_eq!(c.keybindings.next_workspace, "Ctrl+Right");
        assert_eq!(c.keybindings.move_to_prev_workspace, "Super+Ctrl+Left");
        assert!(c.keybindings.resolve().is_ok());
    }

    #[test]
    fn cursor_default_and_override() {
        assert!(CursorConfig::default().visible);
        let c = parse("[cursor]\nvisible = false\n").unwrap();
        assert!(!c.cursor.visible);
    }

    #[test]
    fn validate_accepts_a_valid_config() {
        assert!(Config::default().validate().is_empty());
    }

    #[test]
    fn validate_resets_out_of_range_values() {
        let mut c =
            parse("[keyboard]\nrepeat_rate = -5\n\n[tiling]\nmin_width = 0\nmax_columns = 0\n")
                .unwrap();
        let problems = c.validate();
        assert_eq!(problems.len(), 3);
        assert!(problems.iter().any(|p| p.contains("keyboard.repeat_rate")));
        assert!(problems.iter().any(|p| p.contains("tiling.min_width")));
        assert!(problems.iter().any(|p| p.contains("tiling.max_columns")));
        // The bad values are reset to their defaults.
        assert_eq!(c.keyboard.repeat_rate, 25);
        assert_eq!(c.tiling.min_width, 320);
        assert_eq!(c.tiling.max_columns, 3);
    }

    #[test]
    fn all_sections_together_parse() {
        // A representative full config parses and unknown keys are still caught.
        let toml = "[keyboard]\nrepeat_rate = 30\n\n[window]\nfocus_new = false\n\n\
                    [tiling]\nmin_width = 400\n";
        let c = parse(toml).unwrap();
        assert_eq!(c.keyboard.repeat_rate, 30);
        assert!(!c.window.focus_new);
        assert_eq!(c.tiling.min_width, 400);
    }

    #[test]
    fn saving_only_writes_changed_keys() {
        // Editing one key must not drag sibling defaults along: only the changed
        // key (and its section) appear in the file.
        let c = patched(
            &Config::default(),
            &[("keyboard".into(), "repeat_delay_ms".into(), "201".into())],
        )
        .unwrap();
        let dir = std::env::temp_dir().join("xfar-config-test-one-key");
        let path = dir.join("config.toml");
        save_to(&c, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("repeat_delay_ms = 201"));
        assert!(!text.contains("repeat_rate"));
        // Reverting the edit back to the default removes the whole section.
        let reverted = patched(
            &c,
            &[("keyboard".into(), "repeat_delay_ms".into(), "200".into())],
        )
        .unwrap();
        save_to(&reverted, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("keyboard"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_round_trips_into_an_equal_config() {
        let c = parse(
            "[keyboard]\nrepeat_rate = 40\n\n[appearance]\naccent = \"#ff0000\"\n\n\
             [tiling]\nmax_columns = 2\n",
        )
        .unwrap();
        let dir = std::env::temp_dir().join("xfar-config-test-roundtrip");
        let path = dir.join("config.toml");
        save_to(&c, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse(&text).unwrap(), c);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_omits_default_sections() {
        let c = Config::default();
        let dir = std::env::temp_dir().join("xfar-config-test-omits");
        let path = dir.join("config.toml");
        save_to(&c, &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Nothing differs from the defaults, so no section is written.
        assert!(text.trim().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn patched_applies_typed_edits() {
        let base = Config::default();
        let patches = [
            ("keyboard".into(), "repeat_rate".into(), "40".into()),
            ("window".into(), "focus_new".into(), "no".into()),
            ("appearance".into(), "accent".into(), "#123456".into()),
            (
                "keybindings".into(),
                "new_workspace".into(),
                "Super+T".into(),
            ),
            ("session".into(), "keep_state".into(), "no".into()),
        ];
        let c = patched(&base, &patches).unwrap();
        assert_eq!(c.keyboard.repeat_rate, 40);
        assert!(!c.window.focus_new);
        assert_eq!(c.appearance.accent.as_deref(), Some("#123456"));
        assert_eq!(c.keybindings.new_workspace, "Super+T");
        assert!(!c.session.keep_state);
        // Untouched fields keep their defaults.
        assert_eq!(c.keyboard.repeat_delay_ms, 200);
        assert_eq!(c.tiling.min_width, 320);
    }

    #[test]
    fn patched_rejects_bad_numbers() {
        let err = patched(
            &Config::default(),
            &[("tiling".into(), "min_width".into(), "wide".into())],
        )
        .unwrap_err();
        assert!(err.contains("tiling.min_width"));
    }
}
