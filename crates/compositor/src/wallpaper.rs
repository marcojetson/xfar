//! Output-sized wallpaper rendered behind the window space.
//!
//! Rasterization produces a premultiplied RGBA buffer for the shared wallpaper
//! element. Unreadable, invalid, or oversized images fall back to the solid
//! background color.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::Transform;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// How a background image fills the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WallpaperMode {
    /// Scale to exactly the screen size (aspect ratio not preserved).
    Stretch,
    /// Scale to fit inside the screen, preserving aspect: the leftover bands
    /// show the solid background color.
    Fit,
    /// Draw at natural size in the center; oversized parts crop.
    Center,
    /// Repeat the natural-size image across the screen.
    Tile,
}

impl WallpaperMode {
    /// All mode names, in config order (the settings panel's cycle derives
    /// from this so the two can't drift).
    pub fn names() -> [&'static str; 4] {
        ["stretch", "fit", "center", "tile"]
    }

    /// Parse the config string; `None` for anything unrecognized.
    pub fn parse(s: &str) -> Option<WallpaperMode> {
        match s {
            "stretch" => Some(WallpaperMode::Stretch),
            "fit" => Some(WallpaperMode::Fit),
            "center" => Some(WallpaperMode::Center),
            "tile" => Some(WallpaperMode::Tile),
            _ => None,
        }
    }
}

/// Maximum decoded source size in pixels.
const MAX_PIXELS: u64 = 10_000_000;

/// The rasterized wallpaper: a screen-sized buffer, or `None` when no wallpaper
/// is configured or loadable. The buffer always covers the full screen at
/// `(0, 0)`, so no separate origin is needed.
pub struct Wallpaper {
    pub buffer: Option<MemoryRenderBuffer>,
}

impl Wallpaper {
    pub(crate) fn from_config(
        appearance: &crate::config::AppearanceConfig,
        screen: (i32, i32),
    ) -> Wallpaper {
        let (sw, sh) = screen;
        if sw <= 0 || sh <= 0 {
            return Wallpaper::none();
        }
        let Some(path) = &appearance.background_image else {
            return Wallpaper::none();
        };
        let mode = WallpaperMode::parse(&appearance.background_mode).unwrap_or(WallpaperMode::Fit);
        let (sw, sh) = (sw as u32, sh as u32);

        // A leading `~/` expands to `$HOME` (matching the screenshot dir).
        let path = crate::config::expand_home(path);

        // Cache rasterized output by source, mode, and screen size.
        let Some(pixels) = rasterize_or_cached(std::path::Path::new(&path), mode, sw, sh) else {
            return Wallpaper::none();
        };

        tracing::info!("wallpaper: {path} ({mode:?}, {sw}x{sh})");
        Wallpaper {
            buffer: Some(MemoryRenderBuffer::from_slice(
                &swap_rb(&pixels),
                Fourcc::Argb8888,
                (sw as i32, sh as i32),
                1,
                Transform::Normal,
                None,
            )),
        }
    }

    fn none() -> Wallpaper {
        Wallpaper { buffer: None }
    }
}

/// The rasterized screen-sized wallpaper pixels (premultiplied `[R,G,B,A]`),
/// or `None` if the source can't be loaded/rasterized. Hits the disk cache
/// first and fills it on a miss.
fn rasterize_or_cached(path: &Path, mode: WallpaperMode, sw: u32, sh: u32) -> Option<Vec<u8>> {
    let key = source_key(path, mode, sw, sh);
    if let Some(dir) = cache_dir() {
        if let Some(pixels) = cache_read(&dir, key, sw, sh) {
            tracing::info!("wallpaper: cache hit ({key:016x})");
            return Some(pixels);
        }
    }
    let pixels = rasterize_source(path, mode, sw, sh)?;
    if let Some(dir) = cache_dir() {
        cache_write(&dir, key, sw, sh, &pixels);
    }
    Some(pixels)
}

/// Decode and rasterize the configured image, or `None` on any failure.
fn rasterize_source(path: &Path, mode: WallpaperMode, sw: u32, sh: u32) -> Option<Vec<u8>> {
    // `open` and `decode` report file and format errors separately.
    let img = match image::ImageReader::open(path) {
        Ok(reader) => match reader.decode() {
            Ok(img) => img,
            Err(err) => {
                tracing::warn!(
                    "wallpaper: could not decode {} (using solid background): {err}",
                    path.display()
                );
                return None;
            }
        },
        Err(err) => {
            tracing::warn!(
                "wallpaper: could not open {} (using solid background): {err}",
                path.display()
            );
            return None;
        }
    };
    if u64::from(img.width()) * u64::from(img.height()) > MAX_PIXELS {
        tracing::warn!(
            "wallpaper: {} is too large ({}x{}); using solid background",
            path.display(),
            img.width(),
            img.height()
        );
        return None;
    }
    let src = img.to_rgba8();
    let (iw, ih) = (src.width(), src.height());
    if iw == 0 || ih == 0 {
        return None;
    }
    Some(rasterize(mode, src.as_raw(), iw, ih, sw, sh))
}

/// `$XDG_CACHE_HOME/xfar` falling back to `~/.cache/xfar`; `None` without HOME.
fn cache_dir() -> Option<PathBuf> {
    crate::config::xdg_dir("XDG_CACHE_HOME", ".cache", "")
}

/// Cache key for the source path, size, modification time, mode, and output.
fn source_key(path: &Path, mode: WallpaperMode, sw: u32, sh: u32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    mode.hash(&mut hasher);
    sw.hash(&mut hasher);
    sh.hash(&mut hasher);
    path.hash(&mut hasher);
    if let Ok(meta) = fs::metadata(path) {
        meta.len().hash(&mut hasher);
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// Cache header: magic followed by little-endian `sw` and `sh` values.
const CACHE_MAGIC: &[u8; 8] = b"xfarw1\0\0";

/// Read a cached rasterization for `sw` x `sh`, or `None` on any mismatch.
fn cache_read(dir: &Path, key: u64, sw: u32, sh: u32) -> Option<Vec<u8>> {
    let data = fs::read(dir.join(format!("wallpaper-{key:016x}.bin"))).ok()?;
    if data.len() < 16 || &data[..8] != CACHE_MAGIC {
        return None;
    }
    let got = (
        u32::from(data[8])
            | u32::from(data[9]) << 8
            | u32::from(data[10]) << 16
            | u32::from(data[11]) << 24,
        u32::from(data[12])
            | u32::from(data[13]) << 8
            | u32::from(data[14]) << 16
            | u32::from(data[15]) << 24,
    );
    if got != (sw, sh) {
        return None;
    }
    let pixels = &data[16..];
    if pixels.len() as u64 != u64::from(sw) * u64::from(sh) * 4 {
        return None;
    }
    Some(pixels.to_vec())
}

/// Best-effort cache write; filesystem failures do not affect rendering.
fn cache_write(dir: &Path, key: u64, sw: u32, sh: u32, pixels: &[u8]) {
    let _ = fs::create_dir_all(dir);
    let mut data = Vec::with_capacity(16 + pixels.len());
    data.extend_from_slice(CACHE_MAGIC);
    data.extend_from_slice(&sw.to_le_bytes());
    data.extend_from_slice(&sh.to_le_bytes());
    data.extend_from_slice(pixels);
    let path = dir.join(format!("wallpaper-{key:016x}.bin"));
    match fs::write(&path, &data) {
        Ok(()) => {}
        Err(err) => tracing::warn!("wallpaper: could not write cache {path:?}: {err}"),
    }
}

/// Largest `fit` target preserving the image's aspect within `sw` x `sh`;
/// never smaller than 1px on either side.
fn fit_size(iw: u32, ih: u32, sw: u32, sh: u32) -> (u32, u32) {
    let scale = (sw as f32 / iw as f32).min(sh as f32 / ih as f32);
    (
        (iw as f32 * scale).round().max(1.0) as u32,
        (ih as f32 * scale).round().max(1.0) as u32,
    )
}

/// Rasterize `rgba` (straight-alpha, `iw` x `ih`) across a `sw` x `sh` screen
/// for `mode`, returning premultiplied screen pixels (stride `sw`).
fn rasterize(mode: WallpaperMode, rgba: &[u8], iw: u32, ih: u32, sw: u32, sh: u32) -> Vec<u8> {
    match mode {
        WallpaperMode::Tile => tile(&premultiply(rgba), iw, ih, sw, sh),
        WallpaperMode::Stretch => premultiply(&scale_to(rgba, iw, ih, sw, sh)),
        _ => {
            let (ow, oh) = match mode {
                WallpaperMode::Fit => fit_size(iw, ih, sw, sh),
                _ => (iw, ih),
            };
            let scaled = if (ow, oh) == (iw, ih) {
                rgba.to_vec()
            } else {
                scale_to(rgba, iw, ih, ow, oh)
            };
            let scaled = premultiply(&scaled);
            let x = (sw as i64 - ow as i64).max(0) / 2;
            let y = (sh as i64 - oh as i64).max(0) / 2;
            let mut screen = vec![0u8; (sw * sh * 4) as usize];
            for row in 0..oh {
                let dst = ((y + i64::from(row)) * i64::from(sw) + x) as usize * 4;
                let src = row as usize * ow as usize * 4;
                screen[dst..dst + (ow * 4) as usize]
                    .copy_from_slice(&scaled[src..src + (ow * 4) as usize]);
            }
            screen
        }
    }
}

/// Scale RGBA `iw x ih` to `ow x oh`. Returns straight-alpha RGBA pixels.
fn scale_to(rgba: &[u8], iw: u32, ih: u32, ow: u32, oh: u32) -> Vec<u8> {
    let img = image::imageops::resize(
        &image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(iw, ih, rgba.to_vec()).unwrap(),
        ),
        ow,
        oh,
        image::imageops::FilterType::Triangle,
    );
    img.into_raw()
}

/// Convert straight-alpha RGBA (`[R,G,B,A]`) into premultiplied RGBA, keeping
/// the byte order the text renderer uses (`crate::text`).
fn premultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        let a = u16::from(px[3]);
        px[0] = (u16::from(px[0]) * a / 255) as u8;
        px[1] = (u16::from(px[1]) * a / 255) as u8;
        px[2] = (u16::from(px[2]) * a / 255) as u8;
    }
    out
}

/// Repeat a `iw x ih` premultiplied image across a `sw x sh` buffer.
fn tile(premul: &[u8], iw: u32, ih: u32, sw: u32, sh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let src = ((y % ih) * iw + (x % iw)) * 4;
            let dst = (y * sw + x) * 4;
            out[dst as usize..(dst + 4) as usize]
                .copy_from_slice(&premul[src as usize..(src + 4) as usize]);
        }
    }
    out
}

/// Swap each pixel's red and blue channels, converting premultiplied
/// `[R,G,B,A]` stream order into the `[B,G,R,A]` memory order of
/// `Fourcc::Argb8888` on little-endian hardware (see the DRM cursor buffer).
fn swap_rb(rgba: &[u8]) -> Vec<u8> {
    let mut out = rgba.to_vec();
    for px in out.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const YELLOW: [u8; 4] = [255, 255, 0, 255];
    const CLEAR: [u8; 4] = [0, 0, 0, 0];

    fn pixel(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn stretch_fills_the_screen() {
        // 2x2 opaque red image stretched over a 3x4 screen: every pixel red.
        let src = [RED, RED, RED, RED].concat();
        let buf = rasterize(WallpaperMode::Stretch, &src, 2, 2, 3, 4);
        assert_eq!(buf.len(), 3 * 4 * 4);
        for y in 0..4 {
            for x in 0..3 {
                assert_eq!(pixel(&buf, 3, x, y), RED, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn center_keeps_natural_size_and_position() {
        // 2x2 red centered on a 4x4 screen: red at (1,1), clear at the corners.
        let src = [RED, RED, RED, RED].concat();
        let buf = rasterize(WallpaperMode::Center, &src, 2, 2, 4, 4);
        assert_eq!(buf.len(), 4 * 4 * 4);
        assert_eq!(pixel(&buf, 4, 1, 1), RED);
        for (x, y) in [(0, 0), (3, 0), (0, 3), (3, 3)] {
            assert_eq!(pixel(&buf, 4, x, y), CLEAR, "pixel ({x},{y})");
        }
    }

    #[test]
    fn fit_scales_inside_and_centers() {
        // 2x2 red on a 6x8 screen: scaled to 6x6 (fits the width) and centered
        // vertically (y=1). The uniform color avoids scale-interpolation noise.
        let src = [RED, RED, RED, RED].concat();
        let buf = rasterize(WallpaperMode::Fit, &src, 2, 2, 6, 8);
        assert_eq!(buf.len(), 6 * 8 * 4);
        for (x, y) in [(0, 0), (5, 0), (0, 7), (5, 7)] {
            assert_eq!(pixel(&buf, 6, x, y), CLEAR, "letterbox pixel ({x},{y})");
        }
        assert_eq!(pixel(&buf, 6, 2, 3), RED);
        assert_eq!(pixel(&buf, 6, 5, 6), RED);
    }

    #[test]
    fn tile_repeats_the_image() {
        // 2x2 four-color image tiled across a 5x3 screen.
        let src = [RED, GREEN, BLUE, YELLOW].concat();
        let buf = rasterize(WallpaperMode::Tile, &src, 2, 2, 5, 3);
        assert_eq!(buf.len(), 5 * 3 * 4);
        assert_eq!(pixel(&buf, 5, 0, 0), RED);
        assert_eq!(pixel(&buf, 5, 1, 1), YELLOW);
        assert_eq!(pixel(&buf, 5, 4, 0), RED); // x wraps (4 % 2 = 0)
        assert_eq!(pixel(&buf, 5, 3, 2), GREEN); // x wraps odd, y wraps
    }

    #[test]
    fn premultiply_straight_alpha() {
        // Half-alpha red: channels scale by 0.5 (127/255), alpha is kept.
        let straight = [255, 0, 0, 127];
        let out = premultiply(&straight);
        assert_eq!(out, [127, 0, 0, 127]);
        // Fully transparent pixels become zero in every channel.
        assert_eq!(premultiply(&[255, 255, 255, 0]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn swap_rb_converts_to_argb8888_memory_order() {
        // Red [255,0,0,...] becomes blue-byte-first [0,0,255,...] (B,G,R,A).
        assert_eq!(
            swap_rb(&[255, 0, 0, 255, 0, 255, 0, 128]),
            [0, 0, 255, 255, 0, 255, 0, 128]
        );
    }

    #[test]
    fn mode_names_round_trip() {
        for (mode, name) in [
            (WallpaperMode::Stretch, "stretch"),
            (WallpaperMode::Fit, "fit"),
            (WallpaperMode::Center, "center"),
            (WallpaperMode::Tile, "tile"),
        ] {
            assert_eq!(WallpaperMode::parse(name), Some(mode));
        }
        assert_eq!(WallpaperMode::parse("cover"), None);
        assert_eq!(WallpaperMode::parse(""), None);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xfar-wp-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn cache_round_trip_stores_and_loads() {
        let dir = temp_dir("roundtrip");
        let pixels = vec![7u8; 3 * 3 * 4];
        cache_write(&dir, 42, 3, 3, &pixels);
        assert_eq!(cache_read(&dir, 42, 3, 3), Some(pixels));
        // A different size never matches the same key.
        assert_eq!(cache_read(&dir, 42, 3, 4), None);
    }

    #[test]
    fn cache_rejects_bad_magic_or_truncated_payload() {
        let dir = temp_dir("badmagic");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wallpaper-0000000000000001.bin");
        fs::write(&path, b"nonsense-nonsense-nonsense").unwrap();
        assert_eq!(cache_read(&dir, 1, 2, 2), None);
        // Correct magic but the payload length doesn't match 2x2x4.
        let mut data = CACHE_MAGIC.to_vec();
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(&2u32.to_le_bytes());
        data.push(1);
        fs::write(&path, &data).unwrap();
        assert_eq!(cache_read(&dir, 1, 2, 2), None);
    }

    #[test]
    fn source_key_tracks_mode_and_screen() {
        let src = temp_dir("key");
        fs::create_dir_all(&src).unwrap();
        let path = src.join("img.png");
        fs::write(&path, b"fake image").unwrap();
        let a = source_key(&path, WallpaperMode::Fit, 800, 600);
        assert_eq!(a, source_key(&path, WallpaperMode::Fit, 800, 600));
        assert_ne!(a, source_key(&path, WallpaperMode::Stretch, 800, 600));
        assert_ne!(a, source_key(&path, WallpaperMode::Fit, 801, 600));
    }
}
