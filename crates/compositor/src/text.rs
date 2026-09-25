//! Minimal on-screen text rendering.
//!
//! Rasterizes single lines of text with [`fontdue`] into premultiplied RGBA
//! bytes. Glyph coverage is multiplied by the caller's [`Color32F`] before the
//! compositor uploads the result as a `MemoryRenderBuffer` (see
//! `crate::backend`).

/// Candidate system font paths in preference order. Monospace faces come first;
/// proportional sans-serif faces provide fallbacks.
const FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
    "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
];

/// Rasterized text pixels plus dimensions.
pub struct TextImage {
    /// Premultiplied RGBA8888 bytes (`width * height * 4`).
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// Loaded font used to rasterize text.
pub struct TextRenderer {
    font: fontdue::Font,
}

impl TextRenderer {
    /// Load the first available system font, or `None` if none are present.
    pub fn new() -> Option<Self> {
        for path in FONT_PATHS {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                {
                    tracing::info!("loaded font {path}");
                    return Some(Self { font });
                }
            }
        }
        tracing::warn!("no system font found; text will not render");
        None
    }

    /// Rasterize a single line of `text` at `px` pixels tall, tinted `color`.
    /// Returns `None` for empty text or if the font lacks line metrics.
    pub fn render_line(
        &self,
        text: &str,
        px: f32,
        color: smithay::backend::renderer::Color32F,
    ) -> Option<TextImage> {
        let metrics = self.font.horizontal_line_metrics(px)?;
        let ascent = metrics.ascent;
        let height = (metrics.ascent - metrics.descent).ceil() as i32;

        // Lay out glyphs left to right.
        let mut glyphs = Vec::new();
        let mut pen = 0.0f32;
        for ch in text.chars() {
            let (m, bitmap) = self.font.rasterize(ch, px);
            glyphs.push((pen, m, bitmap));
            pen += m.advance_width;
        }
        let width = pen.ceil() as i32;
        if width <= 0 || height <= 0 {
            return None;
        }

        let mut data = vec![0u8; (width * height * 4) as usize];
        for (pen_x, m, bitmap) in &glyphs {
            let gx = (pen_x + m.xmin as f32).round() as i32;
            let gy = (ascent - m.ymin as f32 - m.height as f32).round() as i32;
            for row in 0..m.height {
                for col in 0..m.width {
                    let coverage = bitmap[row * m.width + col];
                    if coverage == 0 {
                        continue;
                    }
                    let x = gx + col as i32;
                    let y = gy + row as i32;
                    if x < 0 || y < 0 || x >= width || y >= height {
                        continue;
                    }
                    let idx = ((y * width + x) * 4) as usize;
                    // Premultiplied tinted text: RGB = coverage × color, and
                    // alpha = coverage × color's alpha.
                    data[idx] = (coverage as f32 * color.r()) as u8;
                    data[idx + 1] = (coverage as f32 * color.g()) as u8;
                    data[idx + 2] = (coverage as f32 * color.b()) as u8;
                    data[idx + 3] = (coverage as f32 * color.a()) as u8;
                }
            }
        }
        Some(TextImage {
            data,
            width,
            height,
        })
    }
}
