//! Keybinding primitives: the commands a shortcut can trigger, the modifier+key
//! "chord" that triggers it, and a parser from a human string like
//! `"Super+Shift+1"` into a chord.
//!
//! Parsing and matching are independent of compositor state. `crate::config`
//! builds the user-facing schema, and `crate::input` matches live key events
//! against resolved chords.

use smithay::input::keyboard::{keysyms, xkb};

/// A compositor command that a keybinding can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Open the smart launcher.
    Launcher,
    /// Grow / shrink the focused window's column width (moves the divider).
    GrowWidth,
    ShrinkWidth,
    /// Grow / shrink the focused window's row height within its column.
    GrowHeight,
    ShrinkHeight,
    /// Rotate windows through the grid slots.
    Cycle,
    /// Close the focused window.
    Close,
    /// Quit the compositor.
    Quit,
    /// Create and switch to a new workspace.
    NewWorkspace,
    /// Switch to the next workspace, wrapping around.
    NextWorkspace,
    /// Switch to the previous workspace, wrapping around.
    PrevWorkspace,
    /// Move the focused window one workspace forward, wrapping around.
    MoveToNextWorkspace,
    /// Move the focused window one workspace back, wrapping around.
    MoveToPrevWorkspace,
}

/// A resolved shortcut: a set of modifiers plus a base keysym. The keysym is the
/// layout base symbol (e.g. `1`, not `!`) so bindings are shift-independent, and
/// matching against a live key event compares modifiers exactly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Chord {
    pub logo: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// The raw xkb keysym (lowercased for Latin letters).
    pub keysym: u32,
}

/// Fold an uppercase Latin-letter keysym (`A`..=`Z`) to lowercase so that
/// `"Super+D"` and `"Super+d"` resolve to the same chord, matching the
/// shift-independent base keysym seen at input time.
fn normalize_keysym(raw: u32) -> u32 {
    if (keysyms::KEY_A..=keysyms::KEY_Z).contains(&raw) {
        raw + (keysyms::KEY_a - keysyms::KEY_A)
    } else {
        raw
    }
}

/// Parse a chord string such as `"Super+Shift+1"` or `"Ctrl+Alt+t"`.
///
/// Tokens are split on `+`; every token but the last is a modifier
/// (`super`/`logo`/`meta`/`win`/`mod4`, `shift`, `alt`/`mod1`, `ctrl`/`control`,
/// case-insensitive) and the last is a key name resolved by xkb (also
/// case-insensitive, e.g. `Left`, `d`, `1`, `Tab`, `F5`, `space`). Returns a
/// short human-readable reason on failure.
pub fn parse_chord(spec: &str) -> Result<Chord, String> {
    let parts: Vec<&str> = spec
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let Some((key, mods)) = parts.split_last() else {
        return Err("empty binding".to_string());
    };

    let mut chord = Chord::default();
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "super" | "logo" | "meta" | "win" | "mod4" => chord.logo = true,
            "shift" => chord.shift = true,
            "alt" | "mod1" => chord.alt = true,
            "ctrl" | "control" => chord.ctrl = true,
            other => return Err(format!("unknown modifier `{other}`")),
        }
    }

    // Guard against an interior NUL, which would panic the xkb CString call.
    if key.is_empty() || key.as_bytes().contains(&0) {
        return Err(format!("invalid key `{key}`"));
    }
    let raw = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE).raw();
    if raw == keysyms::KEY_NoSymbol {
        return Err(format!("unknown key `{key}`"));
    }
    chord.keysym = normalize_keysym(raw);
    Ok(chord)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_key() {
        let c = parse_chord("Super+D").unwrap();
        assert!(c.logo && !c.shift && !c.alt && !c.ctrl);
        assert_eq!(c.keysym, keysyms::KEY_d);
    }

    #[test]
    fn key_case_is_normalized() {
        // Upper- and lower-case names resolve to the same base keysym.
        assert_eq!(
            parse_chord("Super+D").unwrap(),
            parse_chord("super+d").unwrap()
        );
    }

    #[test]
    fn parses_multiple_modifiers_and_digits() {
        let c = parse_chord("Super+Shift+1").unwrap();
        assert!(c.logo && c.shift);
        assert_eq!(c.keysym, keysyms::KEY_1);

        let c = parse_chord("Ctrl+Alt+t").unwrap();
        assert!(c.ctrl && c.alt && !c.logo);
        assert_eq!(c.keysym, keysyms::KEY_t);
    }

    #[test]
    fn parses_named_keys() {
        assert_eq!(parse_chord("Super+Left").unwrap().keysym, keysyms::KEY_Left);
        assert_eq!(parse_chord("super+up").unwrap().keysym, keysyms::KEY_Up);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("Super+").is_err()); // no key
        assert!(parse_chord("Hyper+x").is_err()); // unknown modifier
        assert!(parse_chord("Super+Nonsense123").is_err()); // unknown key
    }
}
