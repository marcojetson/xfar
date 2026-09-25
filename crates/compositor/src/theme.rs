//! Resolved color palette and spacing values.

use smithay::backend::renderer::Color32F;

/// Spacing scale, in logical pixels (4px base).
pub const SPACE_3: i32 = 12;

/// The runtime color palette. Built from user configuration (`crate::config`),
/// with any unset color falling back to the built-in default (the values in
/// [`Default`]). Rendering reads colors from here rather than constants so they
/// can be themed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub background: Color32F,
    pub panel: Color32F,
    pub accent: Color32F,
    /// Foreground text color.
    pub text: Color32F,
    /// Dimmed placeholder text color.
    pub placeholder: Color32F,
    /// Active workspace indicator in the panel.
    pub vdesktop_active: Color32F,
    /// Inactive workspace indicator in the panel.
    pub vdesktop: Color32F,
    /// Panel clock text color.
    pub clock: Color32F,
    /// Alt+Tab selection border color.
    pub switch_border: Color32F,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: Color32F::new(0.11, 0.12, 0.15, 1.0),
            panel: Color32F::new(0.15, 0.16, 0.20, 1.0),
            accent: Color32F::new(0.26, 0.50, 0.93, 1.0),
            text: Color32F::new(0.83, 0.85, 0.90, 1.0),
            placeholder: Color32F::new(0.42, 0.45, 0.51, 1.0),
            vdesktop_active: Color32F::new(0.26, 0.50, 0.93, 1.0),
            vdesktop: Color32F::new(0.24, 0.26, 0.32, 1.0),
            clock: Color32F::new(0.83, 0.85, 0.90, 1.0),
            switch_border: Color32F::new(0.26, 0.50, 0.93, 1.0),
        }
    }
}

/// Parse a color written as `#rrggbb` or `#rrggbbaa` into a premultiplied
/// [`Color32F`] (the format the renderer expects). Returns a short reason on
/// failure.
pub fn parse_color(spec: &str) -> Result<Color32F, String> {
    let hex = spec
        .strip_prefix('#')
        .ok_or_else(|| "expected \"#rrggbb\" or \"#rrggbbaa\"".to_string())?;
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid hex color `{spec}` (use #rrggbb or #rrggbbaa)"
        ));
    }
    // Safe: validated as 6/8 ASCII hex digits above.
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f32 / 255.0;
    let (r, g, b, a) = if hex.len() == 6 {
        (byte(0), byte(2), byte(4), 1.0)
    } else {
        (byte(0), byte(2), byte(4), byte(6))
    };
    // Color32F stores premultiplied RGBA; for opaque colors this is a no-op.
    Ok(Color32F::new(r * a, g * a, b * a, a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rrggbb_opaque() {
        let c = parse_color("#ff8000").unwrap();
        assert!((c.r() - 1.0).abs() < 1e-6);
        assert!((c.g() - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(c.b(), 0.0);
        assert_eq!(c.a(), 1.0);
    }

    #[test]
    fn parses_rrggbbaa_premultiplied() {
        // 50% alpha halves the stored (premultiplied) channels.
        let c = parse_color("#ffffff80").unwrap();
        let a = 128.0 / 255.0;
        assert!((c.a() - a).abs() < 1e-6);
        assert!((c.r() - a).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_colors() {
        assert!(parse_color("ff8000").is_err()); // missing '#'
        assert!(parse_color("#fff").is_err()); // wrong length
        assert!(parse_color("#gggggg").is_err()); // non-hex
    }
}
