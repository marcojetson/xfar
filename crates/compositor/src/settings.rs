//! Editable settings panel for the effective [`Config`].
//!
//! [`crate::backend`] renders the rows and [`crate::input`] handles edits.
//! [`crate::state::State::settings_save`] builds, validates, saves, and applies
//! the patched configuration.

use std::time::Instant;

use smithay::backend::renderer::Color32F;

use crate::config::Config;
use crate::theme;

/// Height (logical px) of every settings row.
pub const SETTINGS_ROW_H: i32 = 30;

/// How a field's value is edited. This drives both key handling and validation.
#[derive(Clone)]
pub enum FieldKind {
    /// Yes/no switch; Space (or Return) flips it.
    Toggle,
    /// One of a fixed set of options; arrows (or Return) walk it.
    Cycle(Vec<String>),
    /// An integer; Return starts typing. Only digits can be entered.
    Number,
    /// A `#rrggbb` color; Return starts typing. Only hex digits and `#` fit.
    Color,
    /// A keybinding chord ("Super+D"). Any text is typed, but only valid chords
    /// (see `crate::keybinding::parse_chord`) can be committed.
    Binding,
    /// Free text (paths, shortcuts, …); Return starts typing.
    Text,
}

/// A single setting row: a human label plus its effective value, plus the
/// config key it edits so saves stay schema-correct.
#[derive(Clone)]
pub struct FieldRow {
    pub label: String,
    /// Effective value, formatted for display ("200 ms", "fit", "#427fed", …).
    pub value: String,
    pub kind: FieldKind,
    /// Number-unit suffix ("ms", "/s", "px", ""), empty for non-numbers.
    pub suffix: &'static str,
    /// TOML section the edited key lives in (`keyboard`, `appearance`, …).
    pub section: &'static str,
    /// TOML key within [`Self::section`].
    pub key: &'static str,
}

/// A display row in the settings panel.
pub enum Row {
    /// An xfar config section (`[keyboard]`, `[appearance]`, …) header.
    Section { label: String },
    /// A single setting: a human label plus its effective value.
    Field(FieldRow),
}

/// State of the settings panel overlay.
pub struct SettingsPanel {
    pub open: bool,
    /// Flat rows: section headers interleaved with fields, top to bottom.
    rows: Vec<Row>,
    /// Per-row pending edit (the raw text to write for that key), or `None`.
    patches: Vec<Option<String>>,
    /// Index of the first visible row.
    pub top: usize,
    /// Index of the selected row.
    pub selected: usize,
    /// When the panel was last opened (drives its fade-in).
    pub opened_at: Option<Instant>,
    /// The row currently being typed into, if any.
    editing: Option<usize>,
    /// The in-progress raw text of the ongoing edit.
    draft: String,
    /// When the draft (or the edit as a whole) last changed, for caret blink.
    pub edited_at: Option<Instant>,
}

impl SettingsPanel {
    pub fn new() -> Self {
        Self {
            open: false,
            rows: Vec::new(),
            patches: Vec::new(),
            top: 0,
            selected: 0,
            opened_at: None,
            editing: None,
            draft: String::new(),
            edited_at: None,
        }
    }

    /// How many rows the panel holds.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// The row at index `i`, or `None` out of range.
    pub fn row(&self, i: usize) -> Option<&Row> {
        self.rows.get(i)
    }

    /// The row being typed into, if any.
    pub fn editing_row(&self) -> Option<usize> {
        self.editing
    }

    /// The in-progress text of the ongoing edit (what the editing row displays).
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Open the panel showing the effective `config`, resetting the selection.
    pub fn open(&mut self, config: &Config) {
        let rows = rows_from_config(config);
        self.patches = vec![None; rows.len()];
        self.rows = rows;
        self.top = 0;
        self.selected = 0;
        self.editing = None;
        self.draft.clear();
        self.edited_at = None;
        self.open = true;
        self.opened_at = Some(Instant::now());
        self.snap_selection();
    }

    pub fn close(&mut self) {
        self.editing = None;
        self.open = false;
    }

    /// Move the selection `delta` rows (negative = up), skipping section
    /// headers, clamping at the ends and keeping the selected row visible.
    pub fn move_selection(&mut self, delta: i32, view_rows: usize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let mut sel = self.selected as isize;
        for _ in 0..delta.abs() {
            let dir = if delta > 0 { 1 } else { -1 };
            let mut next = sel + dir;
            while (0..len).contains(&next)
                && matches!(self.rows[next as usize], Row::Section { .. })
            {
                next += dir;
            }
            if !(0..len).contains(&next) {
                break;
            }
            sel = next;
        }
        self.selected = sel as usize;
        let view = view_rows.max(1) as i32;
        let sel = sel as i32;
        let top = self.top as i32;
        if sel < top {
            self.top = sel as usize;
        } else if sel >= (top + view) {
            self.top = (sel - view + 1).max(0) as usize;
        }
    }

    /// Jump the selection to the first row.
    pub fn goto_start(&mut self) {
        self.selected = 0;
        self.top = 0;
        self.snap_selection();
    }

    /// Jump the selection to a specific row, keeping it visible.
    pub fn goto(&mut self, row: usize, view_rows: usize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let sel = row.min(last);
        self.selected = sel;
        self.snap_selection();
        let sel = self.selected;
        let top = (sel as i32 - view_rows.max(1) as i32 + 1).clamp(0, sel as i32) as usize;
        self.top = top;
    }

    /// Jump the selection to the last row, keeping it visible.
    pub fn goto_end(&mut self, view_rows: usize) {
        self.goto(self.rows.len() - 1, view_rows);
    }

    /// Point the selection at the absolute row `i` (e.g. a mouse click);
    /// section headers are skipped by landing on the nearest field.
    pub fn select(&mut self, i: usize) {
        if i < self.rows.len() {
            self.selected = i;
            self.snap_selection();
        }
    }

    /// The selection may never sit on a section header; nearest field wins.
    /// (First field forward from the wished row; if none, last field behind.)
    fn snap_selection(&mut self) {
        for i in self.selected..self.rows.len() {
            if matches!(self.rows[i], Row::Field(_)) {
                self.selected = i;
                return;
            }
        }
        if let Some(i) = (0..self.selected)
            .rev()
            .find(|&i| matches!(self.rows[i], Row::Field(_)))
        {
            self.selected = i;
        }
    }

    /// The `(section, key)` → raw-text patches to apply on save.
    pub fn patches(&self) -> Vec<(String, String, String)> {
        self.rows
            .iter()
            .zip(&self.patches)
            .filter_map(|(row, patch)| match (row, patch) {
                (Row::Field(field), Some(text)) => Some((
                    field.section.to_string(),
                    field.key.to_string(),
                    text.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    /// The selected row's edit kind (the owned copy avoids borrowing `self.rows`
    /// while a mutating method runs).
    fn selected_kind(&self) -> Option<FieldKind> {
        match self.rows.get(self.selected) {
            Some(Row::Field(field)) => Some(field.kind.clone()),
            _ => None,
        }
    }

    /// Act on the selected row with the "primary" key (Return / Space):
    /// toggle or cycle where the kind allows it, otherwise start typing.
    pub fn activate_selected(&mut self) {
        match self.selected_kind() {
            Some(FieldKind::Toggle) => self.toggle_selected(),
            Some(FieldKind::Cycle(options)) => self.cycle_selected(1, &options),
            Some(_) => self.edit_start(self.selected),
            None => {}
        }
    }

    /// Step a cycling row through its options by `dir` (+1/−1).
    pub fn apply_counter(&mut self, dir: i32) {
        if let Some(FieldKind::Cycle(options)) = self.selected_kind() {
            self.cycle_selected(dir, &options);
        }
    }

    fn toggle_selected(&mut self) {
        let Some(Row::Field(_)) = self.rows.get(self.selected) else {
            return;
        };
        let flip = |value: &str| {
            Some(if value == "yes" {
                "no".to_string()
            } else {
                "yes".to_string()
            })
        };
        self.set_selected_value(&flip);
    }

    fn cycle_selected(&mut self, dir: i32, options: &[String]) {
        let shift = |value: &str| -> Option<String> {
            let current = options.iter().position(|o| o == value)?;
            let next = (current as i32 + dir).rem_euclid(options.len() as i32) as usize;
            Some(options[next].clone())
        };
        self.set_selected_value(&|value: &str| shift(value));
    }

    /// Replace the selected field's display value with `f(prev)`, recording the
    /// new text as a pending patch (used by toggles and cyclers).
    fn set_selected_value(&mut self, f: &dyn Fn(&str) -> Option<String>) {
        let Some(Row::Field(field)) = self.rows.get(self.selected) else {
            return;
        };
        let Some(next) = f(&field.value) else {
            return;
        };
        let row = self.selected;
        let mut field = field.clone();
        field.value = next.clone();
        self.rows[row] = Row::Field(field);
        self.patches[row] = Some(next);
        self.edited_at = Some(Instant::now());
    }

    /// Start typing into the field at row `i`: the draft begins as the field's
    /// raw value (digit part for numbers).
    pub fn edit_start(&mut self, i: usize) {
        let Some(Row::Field(field)) = self.rows.get(i) else {
            return;
        };
        let raw = if field.kind.is_number() {
            if field.suffix.is_empty() {
                field.value.trim().to_string()
            } else {
                field
                    .value
                    .trim_end_matches(&field.suffix)
                    .trim()
                    .to_string()
            }
        } else {
            field.value.clone()
        };
        self.editing = Some(i);
        self.draft = raw;
        self.edited_at = Some(Instant::now());
    }

    /// Insert `ch` into the ongoing draft. Chars the field kind cannot contain are
    /// ignored, so invalid input never reaches the draft in the first place.
    pub fn edit_char(&mut self, ch: char) {
        let Some(Row::Field(field)) = self.rows.get(self.editing.unwrap_or(self.selected)) else {
            return;
        };
        if !field.kind.accepts(ch) {
            return;
        }
        self.editing = Some(self.editing.unwrap_or(self.selected));
        self.draft.push(ch);
        self.edited_at = Some(Instant::now());
    }

    /// Delete the last character of the ongoing draft.
    pub fn edit_backspace(&mut self) {
        self.draft.pop();
        self.edited_at = Some(Instant::now());
    }

    /// Discard the ongoing edit.
    pub fn edit_cancel(&mut self) {
        self.editing = None;
        self.draft.clear();
        self.edited_at = Some(Instant::now());
    }

    /// Validate and accept the ongoing draft into the row (and its patch).
    /// Invalid input is left in place so the user can fix it.
    pub fn edit_commit(&mut self) {
        let Some(row) = self.editing else {
            return;
        };
        let Some(Row::Field(field)) = self.rows.get(row) else {
            return;
        };
        if !field.kind.valid(&self.draft) {
            return;
        }
        let value = if field.kind.is_number() {
            number_display(self.draft.trim(), field.suffix)
        } else {
            self.draft.trim().to_string()
        };
        let mut field = field.clone();
        field.value = value;
        self.rows[row] = Row::Field(field);
        self.patches[row] = Some(self.draft.trim().to_string());
        self.editing = None;
        self.draft.clear();
        self.edited_at = Some(Instant::now());
    }
}

impl FieldKind {
    fn is_number(&self) -> bool {
        matches!(self, FieldKind::Number)
    }

    /// Whether `ch` may be inserted into a draft for this kind. Fields that are
    /// chosen, not typed (toggles, cyclers) accept everything — they never get
    /// typed into; values that must look like one thing reject anything that
    /// cannot be part of it, giving immediate feedback while typing.
    fn accepts(&self, ch: char) -> bool {
        match self {
            FieldKind::Number => ch.is_ascii_digit(),
            FieldKind::Color => ch == '#' || ch.is_ascii_hexdigit(),
            FieldKind::Toggle | FieldKind::Cycle(_) | FieldKind::Binding | FieldKind::Text => {
                !ch.is_control()
            }
        }
    }

    /// Whether `text` is a valid value for this kind (checked on commit so
    /// typos cannot poison the saved config).
    fn valid(&self, text: &str) -> bool {
        match self {
            FieldKind::Number => text.trim().parse::<i64>().is_ok(),
            FieldKind::Color => theme::parse_color(text.trim()).is_ok(),
            FieldKind::Cycle(options) => options.iter().any(|o| o == text.trim()),
            FieldKind::Toggle => matches!(text.trim(), "yes" | "no"),
            FieldKind::Binding => crate::keybinding::parse_chord(text.trim()).is_ok(),
            FieldKind::Text => !text.trim().is_empty(),
        }
    }
}

/// The effective (resolved) hex string for a color: the configured value when
/// set, otherwise the built-in default the theme falls back to.
fn color_or_default(configured: &Option<String>, fallback: Color32F) -> String {
    configured.clone().unwrap_or_else(|| hex(fallback))
}

/// Format a palette color as `#rrggbb` (or `#rrggbbaa` when it has alpha).
fn hex(color: Color32F) -> String {
    // smithay's `Color32F` stores premultiplied floats in 0..=1.
    let component = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let alpha = component(color.a());
    if alpha == 255 {
        format!(
            "#{:02x}{:02x}{:02x}",
            component(color.r()),
            component(color.g()),
            component(color.b())
        )
    } else {
        format!(
            "#{:02x}{:02x}{:02x}{alpha:02x}",
            component(color.r()),
            component(color.g()),
            component(color.b())
        )
    }
}

/// Format a numeric value for display: "200 ms", "320 px", "25/s" (no space
/// before a suffix that begins with '/'), or "4" when the suffix is empty.
fn number_display(value: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        value.to_string()
    } else if suffix.starts_with('/') {
        format!("{value}{suffix}")
    } else {
        format!("{value} {suffix}")
    }
}

fn yes_no(b: bool) -> String {
    if b {
        "yes".into()
    } else {
        "no".into()
    }
}

/// Build settings rows for an effective `config`, following the schema in
/// `config.rs`.
pub fn rows_from_config(config: &Config) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::with_capacity(46);

    rows.push(Row::Section {
        label: "keyboard".into(),
    });
    rows.push(number_row(
        "Key repeat delay",
        config.keyboard.repeat_delay_ms,
        "ms",
        "keyboard",
        "repeat_delay_ms",
    ));
    rows.push(number_row(
        "Key repeat rate",
        config.keyboard.repeat_rate,
        "/s",
        "keyboard",
        "repeat_rate",
    ));

    rows.push(Row::Section {
        label: "keybindings".into(),
    });
    let bindings: [(&str, &str, &str); 13] = [
        ("Open launcher", &config.keybindings.launcher, "launcher"),
        ("Grow width", &config.keybindings.grow_width, "grow_width"),
        (
            "Shrink width",
            &config.keybindings.shrink_width,
            "shrink_width",
        ),
        (
            "Grow height",
            &config.keybindings.grow_height,
            "grow_height",
        ),
        (
            "Shrink height",
            &config.keybindings.shrink_height,
            "shrink_height",
        ),
        ("Cycle windows", &config.keybindings.cycle, "cycle"),
        ("Close window", &config.keybindings.close, "close"),
        ("Quit", &config.keybindings.quit, "quit"),
        (
            "New workspace",
            &config.keybindings.new_workspace,
            "new_workspace",
        ),
        (
            "Move window to next workspace",
            &config.keybindings.move_to_next_workspace,
            "move_to_next_workspace",
        ),
        (
            "Move window to previous workspace",
            &config.keybindings.move_to_prev_workspace,
            "move_to_prev_workspace",
        ),
        (
            "Next workspace",
            &config.keybindings.next_workspace,
            "next_workspace",
        ),
        (
            "Previous workspace",
            &config.keybindings.prev_workspace,
            "prev_workspace",
        ),
    ];
    for (label, value, key) in bindings {
        rows.push(Row::Field(FieldRow {
            label: label.into(),
            value: (*value).into(),
            kind: FieldKind::Binding,
            suffix: "",
            section: "keybindings",
            key,
        }));
    }

    rows.push(Row::Section {
        label: "appearance".into(),
    });
    let defaults = theme::Palette::default();
    let colors: [(&str, &Option<String>, Color32F, &str); 9] = [
        (
            "Background",
            &config.appearance.background,
            defaults.background,
            "background",
        ),
        ("Panel", &config.appearance.panel, defaults.panel, "panel"),
        (
            "Accent",
            &config.appearance.accent,
            defaults.accent,
            "accent",
        ),
        ("Text", &config.appearance.text, defaults.text, "text"),
        (
            "Placeholder",
            &config.appearance.placeholder,
            defaults.placeholder,
            "placeholder",
        ),
        (
            "Active workspace pip",
            &config.appearance.vdesktop_active,
            defaults.vdesktop_active,
            "vdesktop_active",
        ),
        (
            "Workspace pip",
            &config.appearance.vdesktop,
            defaults.vdesktop,
            "vdesktop",
        ),
        ("Clock", &config.appearance.clock, defaults.clock, "clock"),
        (
            "Switcher border",
            &config.appearance.switch_border,
            defaults.switch_border,
            "switch_border",
        ),
    ];
    for (label, configured, fallback, key) in colors {
        rows.push(Row::Field(FieldRow {
            label: label.into(),
            value: color_or_default(configured, fallback),
            kind: FieldKind::Color,
            suffix: "",
            section: "appearance",
            key,
        }));
    }
    rows.push(Row::Field(FieldRow {
        label: "Background image".into(),
        value: config
            .appearance
            .background_image
            .clone()
            .unwrap_or_else(|| "none".into()),
        kind: FieldKind::Text,
        suffix: "",
        section: "appearance",
        key: "background_image",
    }));
    rows.push(Row::Field(FieldRow {
        label: "Background mode".into(),
        value: config.appearance.background_mode.clone(),
        kind: FieldKind::Cycle(
            crate::wallpaper::WallpaperMode::names()
                .into_iter()
                .map(String::from)
                .collect(),
        ),
        suffix: "",
        section: "appearance",
        key: "background_mode",
    }));
    rows.push(Row::Field(FieldRow {
        label: "Panel position".into(),
        value: config.appearance.panel_position.clone(),
        kind: FieldKind::Cycle(vec!["top".into(), "bottom".into()]),
        suffix: "",
        section: "appearance",
        key: "panel_position",
    }));

    rows.push(Row::Section {
        label: "window".into(),
    });
    rows.push(Row::Field(FieldRow {
        label: "Focus new windows".into(),
        value: yes_no(config.window.focus_new),
        kind: FieldKind::Toggle,
        suffix: "",
        section: "window",
        key: "focus_new",
    }));

    rows.push(Row::Section {
        label: "tiling".into(),
    });
    let tiling: [(&str, i32, &str, &str); 4] = [
        (
            "Min tiled cell width",
            config.tiling.min_width,
            "px",
            "min_width",
        ),
        (
            "Min tiled cell height",
            config.tiling.min_height,
            "px",
            "min_height",
        ),
        (
            "Grid columns per desktop",
            config.tiling.max_columns,
            "",
            "max_columns",
        ),
        (
            "Grid rows per desktop",
            config.tiling.max_rows,
            "",
            "max_rows",
        ),
    ];
    for (label, value, suffix, key) in tiling {
        rows.push(number_row(label, value, suffix, "tiling", key));
    }

    rows.push(Row::Section {
        label: "cursor".into(),
    });
    rows.push(Row::Field(FieldRow {
        label: "Show cursor".into(),
        value: yes_no(config.cursor.visible),
        kind: FieldKind::Toggle,
        suffix: "",
        section: "cursor",
        key: "visible",
    }));

    rows.push(Row::Section {
        label: "screenshot".into(),
    });
    rows.push(Row::Field(FieldRow {
        label: "Screenshot directory".into(),
        value: config
            .screenshot
            .dir
            .clone()
            .unwrap_or_else(|| format!("{} (default)", crate::config::SCREENSHOT_DIR_DEFAULT)),
        kind: FieldKind::Text,
        suffix: "",
        section: "screenshot",
        key: "dir",
    }));

    rows.push(Row::Section {
        label: "session".into(),
    });
    rows.push(Row::Field(FieldRow {
        label: "Keep windows across restart".into(),
        value: yes_no(config.session.keep_state),
        kind: FieldKind::Toggle,
        suffix: "",
        section: "session",
        key: "keep_state",
    }));

    rows
}

/// An integer setting row (displayed with its `suffix`, edited as digits).
fn number_row<S: Into<String>>(
    label: S,
    value: i32,
    suffix: &'static str,
    section: &'static str,
    key: &'static str,
) -> Row {
    Row::Field(FieldRow {
        label: label.into(),
        value: number_display(&value.to_string(), suffix),
        kind: FieldKind::Number,
        suffix,
        section,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(rows: &'a [Row], label: &str) -> &'a FieldRow {
        rows.iter()
            .find_map(|r| match r {
                Row::Field(f) if f.label == label => Some(f),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no row labeled {label}"))
    }

    #[test]
    fn default_config_covers_every_section() {
        let rows = rows_from_config(&Config::default());
        assert_eq!(rows.len(), 43); // 8 sections + 35 fields
                                    // Section headers come first, in schema order.
        assert!(matches!(rows[0], Row::Section { .. }));
        // Spot-check defaults.
        assert_eq!(value(row(&rows, "Open launcher")), "Super+D");
        assert_eq!(value(row(&rows, "Quit")), "Super+Shift+Q");
        assert_eq!(value(row(&rows, "Background")), "#1c1f26");
        assert_eq!(value(row(&rows, "Background mode")), "fit");
        assert_eq!(value(row(&rows, "Panel position")), "top");
        assert_eq!(value(row(&rows, "Focus new windows")), "yes");
        assert_eq!(value(row(&rows, "Min tiled cell width")), "320 px");
        assert_eq!(value(row(&rows, "Key repeat delay")), "200 ms");
        assert_eq!(
            value(row(&rows, "Screenshot directory")),
            "~/screenshots (default)"
        );
        assert_eq!(value(row(&rows, "Keep windows across restart")), "yes");
        assert_eq!(value(row(&rows, "Next workspace")), "Super+Tab");
        assert_eq!(value(row(&rows, "Previous workspace")), "Super+Shift+Tab");
    }

    fn value(row: &FieldRow) -> &str {
        &row.value
    }

    #[test]
    fn reflects_configured_values() {
        let mut config = Config::default();
        config.window.focus_new = false;
        config.appearance.panel_position = "bottom".into();
        config.appearance.background = Some("#000000".into());
        let rows = rows_from_config(&config);
        assert_eq!(value(row(&rows, "Focus new windows")), "no");
        assert_eq!(value(row(&rows, "Panel position")), "bottom");
        assert_eq!(value(row(&rows, "Background")), "#000000");
    }

    #[test]
    fn unset_colors_show_builtin_defaults() {
        let config = Config::default();
        let rows = rows_from_config(&config);
        // Configured colors were tested above; unset ones fall back to hex.
        assert!(!value(row(&rows, "Panel")).is_empty());
        assert!(value(row(&rows, "Accent")).starts_with('#'));
    }

    #[test]
    fn selection_scrolls_and_clamps() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let view = 10;
        let rows = rows_from_config(&Config::default());
        let first = rows
            .iter()
            .position(|r| matches!(r, Row::Field(_)))
            .unwrap();
        // Moving down past the end clamps at the last row.
        panel.goto_end(view);
        assert_eq!(panel.selected, panel.len() - 1);
        // And up clamps at the first field (row 0 is a section header).
        panel.move_selection(-(panel.len() as i32), view);
        assert_eq!(panel.selected, first);
        assert_eq!(panel.top, first);
        // Stepping down several rows never lands on a header and stays visible.
        panel.move_selection(view as i32, view);
        let kept = panel.selected;
        assert!(matches!(rows[kept], Row::Field(_)));
        assert!(panel.top <= kept && kept < panel.top + view);
    }

    #[test]
    fn selection_never_lands_on_section_headers() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let rows = rows_from_config(&Config::default());
        let first = rows
            .iter()
            .position(|r| matches!(r, Row::Field(_)))
            .unwrap();
        // The first header *after* a field ("keyboard", etc.).
        let section = rows
            .iter()
            .enumerate()
            .skip(first)
            .find(|(_, r)| matches!(r, Row::Section { .. }))
            .map(|(i, _)| i)
            .unwrap();
        // Jumping onto a section header lands on the following field.
        panel.select(section);
        assert_eq!(panel.selected, section + 1);
        // Row 0 is a header: selecting it lands on the first field.
        panel.select(0);
        assert_eq!(panel.selected, first);
        // Crossing a header downward lands on the field after it.
        panel.move_selection((section - first) as i32, 10);
        assert_eq!(panel.selected, section + 1);
        // Crossing a header upward lands on the field before it.
        panel.move_selection(-1, 10);
        assert_eq!(panel.selected, section - 1);
    }

    #[test]
    fn empty_panel_handles_navigation() {
        let mut panel = SettingsPanel::new();
        assert_eq!(panel.len(), 0);
        panel.move_selection(3, 10); // no panic on an empty list
        assert_eq!(panel.selected, 0);
    }

    #[test]
    fn toggle_produces_a_patch() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let focus = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Focus new windows" => Some(i),
                _ => None,
            })
            .unwrap();
        panel.select(focus);
        panel.activate_selected(); // yes → no
        assert_eq!(value(row_at(&panel, focus)), "no");
        let patches = panel.patches();
        assert_eq!(
            patches,
            vec![("window".into(), "focus_new".into(), "no".into())]
        );
    }

    #[test]
    fn cycle_walks_options_and_wraps() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let mode = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Background mode" => Some(i),
                _ => None,
            })
            .unwrap();
        panel.select(mode);
        panel.apply_counter(1); // fit → center
        assert_eq!(value(row_at(&panel, mode)), "center");
        panel.apply_counter(1); // center → tile
        assert_eq!(value(row_at(&panel, mode)), "tile");
        panel.apply_counter(1); // tile → stretch (wraps)
        assert_eq!(value(row_at(&panel, mode)), "stretch");
        panel.apply_counter(-1); // stretch → tile (wraps backwards)
        assert_eq!(value(row_at(&panel, mode)), "tile");
    }

    #[test]
    fn typing_edits_and_commits_a_number() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let rate = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Key repeat rate" => Some(i),
                _ => None,
            })
            .unwrap();
        panel.select(rate);
        panel.activate_selected(); // starts editing
        assert_eq!(panel.editing_row(), Some(rate));
        // The draft is pre-filled with the current value ("25"); clear it by
        // backspacing, then type a fresh number.
        panel.edit_backspace();
        panel.edit_backspace();
        panel.edit_char('3');
        panel.edit_char('5');
        panel.edit_commit();
        assert_eq!(panel.editing_row(), None);
        assert_eq!(value(row_at(&panel, rate)), "35/s");
        // The raw patch text (no suffix) is what gets written to TOML.
        let patches = panel.patches();
        assert_eq!(
            patches,
            vec![("keyboard".into(), "repeat_rate".into(), "35".into())]
        );
    }

    #[test]
    fn number_typing_accepts_only_digits() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let rate = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Key repeat rate" => Some(i),
                _ => None,
            })
            .unwrap();
        let accent = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Accent" => Some(i),
                _ => None,
            })
            .unwrap();
        // Non-digits never reach a number draft.
        panel.select(rate);
        panel.activate_selected();
        panel.edit_char('x');
        panel.edit_char('-');
        assert_eq!(panel.draft(), "25");
        panel.edit_backspace();
        panel.edit_backspace();
        panel.edit_char('4');
        panel.edit_commit();
        assert_eq!(value(row_at(&panel, rate)), "4/s");
        // Non-hex characters never reach a color draft.
        panel.select(accent);
        panel.activate_selected();
        let before = value(row_at(&panel, accent)).to_string();
        panel.edit_char('g');
        assert_eq!(panel.draft(), before);
        panel.edit_cancel();
    }

    #[test]
    fn invalid_color_is_not_committed() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let accent = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Accent" => Some(i),
                _ => None,
            })
            .unwrap();
        panel.select(accent);
        panel.activate_selected();
        panel.edit_char('#');
        panel.edit_char('1');
        panel.edit_commit(); // "#1" is not a valid color
        assert_eq!(panel.editing_row(), Some(accent));
        panel.edit_cancel();
        // The draft was cleared; retype a full color (keeping the leading '#').
        panel.edit_char('#');
        panel.edit_char('1');
        panel.edit_char('2');
        panel.edit_char('3');
        panel.edit_char('4');
        panel.edit_char('5');
        panel.edit_char('6');
        panel.edit_commit(); // "#123456" is valid
        assert_eq!(value(row_at(&panel, accent)), "#123456");
    }

    #[test]
    fn binding_commits_only_valid_chords() {
        let mut panel = SettingsPanel::new();
        panel.open(&Config::default());
        let launcher = rows_from_config(&Config::default())
            .iter()
            .enumerate()
            .find_map(|(i, r)| match r {
                Row::Field(f) if f.label == "Open launcher" => Some(i),
                _ => None,
            })
            .unwrap();
        // A malformed chord cannot be committed.
        panel.select(launcher);
        panel.activate_selected();
        for _ in 0..panel.draft().len() {
            panel.edit_backspace();
        }
        for ch in "Super+Nope".chars() {
            panel.edit_char(ch);
        }
        panel.edit_commit();
        assert_eq!(panel.editing_row(), Some(launcher));
        assert!(panel.patches().is_empty());
        // A valid chord commits and patches as expected.
        panel.edit_cancel();
        for ch in "Ctrl+space".chars() {
            panel.edit_char(ch);
        }
        panel.edit_commit();
        assert_eq!(value(row_at(&panel, launcher)), "Ctrl+space");
        let patches = panel.patches();
        assert_eq!(
            patches,
            vec![("keybindings".into(), "launcher".into(), "Ctrl+space".into())]
        );
    }

    fn row_at(panel: &SettingsPanel, i: usize) -> &FieldRow {
        match panel.row(i) {
            Some(Row::Field(field)) => field,
            _ => panic!("row {i} is not a field"),
        }
    }
}
