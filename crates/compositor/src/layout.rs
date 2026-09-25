//! Tiling and overlay geometry.
//!
//! The grid lays out `ceil(sqrt(n))` columns and balances row occupancy within
//! each column. Integer spans cover the base grid exactly; [`tile_grid_gapped`]
//! applies uniform spacing, and [`GridWeights`] stores manual proportions.

use smithay::utils::{Logical, Point, Rectangle};

use serde::{Deserialize, Serialize};

/// Height (logical px) of the persistent panel. The work area excludes this
/// strip on the selected edge.
pub const BAR_HEIGHT: i32 = 32;

/// Width of the panel's search field, matched to the results dropdown.
pub const BAR_SEARCH_W: i32 = 640;
/// Reserved width for the clock at the panel's right edge.
pub const BAR_CLOCK_W: i32 = 160;
/// Gap between the full-height workspace pips.
const BAR_PIP_GAP: i32 = 2;

/// Uniform gap (logical px) between tiled windows and around the work area.
/// Even so the half-gap insets stay exact, keeping the margin identical no
/// matter where a divider sits.
pub const WINDOW_GAP: i32 = 6;

/// Launcher results dropdown geometry (shared by rendering and click hit-test).
pub const LAUNCHER_WIDTH: i32 = 640;
pub const LAUNCHER_ROW_H: i32 = 28;
pub const LAUNCHER_PAD: i32 = 12;
/// Left offset of the dropdown (a 1px inset under the search field).
pub const LAUNCHER_LEFT: i32 = 1;

/// Launcher results geometry, positioned beside the panel according to its
/// configured edge.
pub fn launcher_panel_rect(
    screen_h: i32,
    rows: usize,
    panel_bottom: bool,
) -> Rectangle<i32, Logical> {
    let height = rows as i32 * LAUNCHER_ROW_H + LAUNCHER_PAD * 2;
    let y = if panel_bottom {
        (screen_h - BAR_HEIGHT - height).max(0)
    } else {
        BAR_HEIGHT
    };
    Rectangle::new((LAUNCHER_LEFT, y).into(), (LAUNCHER_WIDTH, height).into())
}

/// The dropdown result-row index at `point` (global coords), given how many
/// results are shown, or `None` if the point is outside the results list.
pub fn launcher_row_at(
    point: Point<i32, Logical>,
    rows: usize,
    screen_h: i32,
    panel_bottom: bool,
) -> Option<usize> {
    if rows == 0 {
        return None;
    }
    let rect = launcher_panel_rect(screen_h, rows, panel_bottom);
    if !rect.contains(point) {
        return None;
    }
    let rel = point.y - rect.loc.y;
    // The panel's inner top padding is not part of the results list.
    if rel < LAUNCHER_PAD {
        return None;
    }
    let idx = (rel - LAUNCHER_PAD) / LAUNCHER_ROW_H;
    (idx < rows as i32).then_some(idx as usize)
}

/// Settings panel width and inner padding (shared by rendering and hit-testing).
pub const SETTINGS_WIDTH: i32 = 380;
pub const SETTINGS_PAD: i32 = 12;

/// Right-anchored settings panel, starting below a top panel or at the top when
/// the panel is at the bottom.
pub fn settings_rect(screen_w: i32, screen_h: i32, panel_bottom: bool) -> Rectangle<i32, Logical> {
    let y = if panel_bottom { 0 } else { BAR_HEIGHT };
    Rectangle::new(
        ((screen_w - SETTINGS_WIDTH).max(0), y).into(),
        (SETTINGS_WIDTH.min(screen_w.max(0)), (screen_h - y).max(0)).into(),
    )
}

/// How many whole settings rows fit in the panel viewport for this output.
pub fn settings_view_rows(screen_h: i32, panel_bottom: bool) -> usize {
    let height = if panel_bottom {
        screen_h
    } else {
        (screen_h - BAR_HEIGHT).max(0)
    };
    (height / crate::settings::SETTINGS_ROW_H).max(0) as usize
}

/// Output geometry excluding the panel on its configured edge.
pub fn work_area(output: Rectangle<i32, Logical>, panel_bottom: bool) -> Rectangle<i32, Logical> {
    let y = if panel_bottom {
        output.loc.y
    } else {
        output.loc.y + BAR_HEIGHT
    };
    Rectangle::new(
        (output.loc.x, y).into(),
        (output.size.w, (output.size.h - BAR_HEIGHT).max(0)).into(),
    )
}

/// Clickable search-field region, relative to [`bar_offset`].
pub fn bar_search_rect(width: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (0, 0).into(),
        (BAR_SEARCH_W.min(width.max(0)), BAR_HEIGHT).into(),
    )
}

/// The panel's top-left corner: `(0, 0)` at the top or
/// `(0, max(0, height - BAR_HEIGHT))` at the bottom.
pub fn bar_offset(height: i32, panel_bottom: bool) -> Point<i32, Logical> {
    if panel_bottom {
        (0, (height - BAR_HEIGHT).max(0)).into()
    } else {
        (0, 0).into()
    }
}

/// The clickable workspace pips: `count` full-height squares, right-anchored
/// just left of the reserved clock area. Each click switches to that workspace.
/// With one workspace there is nothing to switch to, so no pips are drawn.
pub fn bar_pip_rects(width: i32, count: usize) -> Vec<Rectangle<i32, Logical>> {
    if count <= 1 {
        return Vec::new();
    }
    let n = count as i32;
    let block = n * BAR_HEIGHT + (n - 1) * BAR_PIP_GAP;
    let right = width - BAR_CLOCK_W;
    let left = (right - block).max(BAR_SEARCH_W);
    (0..count)
        .map(|i| {
            let x = left + i as i32 * (BAR_HEIGHT + BAR_PIP_GAP);
            Rectangle::new((x, 0).into(), (BAR_HEIGHT, BAR_HEIGHT).into())
        })
        .collect()
}

/// Number of columns for `n` tiled windows: `ceil(sqrt(n))`.
fn columns_for(n: usize) -> usize {
    (n as f64).sqrt().ceil() as usize
}

/// The `(column, row)` of window `index` in the column-major order used by
/// [`tile_grid_weighted`] (column 0 top-to-bottom, then column 1, …), or `None`
/// when `index >= n`.
pub fn cell_of(n: usize, index: usize) -> Option<(usize, usize)> {
    let mut acc = 0;
    for (c, rows) in column_counts(n).into_iter().enumerate() {
        if index < acc + rows {
            return Some((c, index - acc));
        }
        acc += rows;
    }
    None
}

/// Window count in each column, left to right. Extra windows go in the
/// rightmost columns (`n = 3` produces `[1, 2]`).
pub fn column_counts(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cols = columns_for(n);
    let base = n / cols;
    let rem = n % cols; // the `rem` rightmost columns get one extra
    (0..cols)
        .map(|c| if c >= cols - rem { base + 1 } else { base })
        .collect()
}

/// Whether `n` windows fit within the configured column and row limits.
pub fn fits_grid(n: usize, max_columns: usize, max_rows: usize) -> bool {
    if n == 0 || max_columns == 0 || max_rows == 0 {
        return n == 0;
    }
    let cols = columns_for(n).min(max_columns);
    n.div_ceil(cols) <= max_rows
}

/// Relative split weights for the grid: `cols` sizes the columns, `rows[c]` the
/// rows of column `c`. Empty or wrong-length entries fall back to equal splits,
/// so the default is a plain even grid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GridWeights {
    pub cols: Vec<f32>,
    pub rows: Vec<Vec<f32>>,
}

impl GridWeights {
    /// Grow (or reset) the weight vectors to match a grid with the given
    /// per-column window counts, filling new entries with an equal `1.0` weight.
    /// Any vector of the wrong length is replaced wholesale, so a stale layout
    /// (from a different window count) falls back to an even split.
    pub fn ensure_shape(&mut self, counts: &[usize]) {
        if self.cols.len() != counts.len() {
            self.cols = vec![1.0; counts.len()];
        }
        if self.rows.len() != counts.len() {
            self.rows = counts.iter().map(|&r| vec![1.0; r]).collect();
        } else {
            for (rows, &count) in self.rows.iter_mut().zip(counts) {
                if rows.len() != count {
                    *rows = vec![1.0; count];
                }
            }
        }
    }
}

/// Partition `total` px (starting at `offset`) into spans proportional to
/// `weights`, snapping to integers and covering the range exactly.
fn weighted_spans(total: i32, offset: i32, weights: &[f32]) -> Vec<(i32, i32)> {
    let sum: f32 = weights.iter().sum();
    let mut spans = Vec::with_capacity(weights.len());
    let mut acc = 0.0f32;
    let mut prev = offset;
    for (i, w) in weights.iter().enumerate() {
        acc += w;
        let next = if i + 1 == weights.len() {
            offset + total // last span absorbs rounding so we cover exactly
        } else {
            offset + (total as f32 * acc / sum).round() as i32
        };
        spans.push((prev, next));
        prev = next;
    }
    spans
}

/// Like [`tile_grid`] but with manual resize applied via `weights` (per-column
/// widths and per-column row heights). Falls back to equal splits where a weight
/// vector is missing or the wrong length.
pub fn tile_grid_weighted(
    area: Rectangle<i32, Logical>,
    n: usize,
    weights: &GridWeights,
) -> Vec<Rectangle<i32, Logical>> {
    let counts = column_counts(n);
    let cols = counts.len();
    if cols == 0 {
        return Vec::new();
    }
    let equal_cols = vec![1.0; cols];
    let col_w = if weights.cols.len() == cols {
        weights.cols.as_slice()
    } else {
        &equal_cols
    };
    let col_spans = weighted_spans(area.size.w, area.loc.x, col_w);

    let mut rects = Vec::with_capacity(n);
    for (c, &rows) in counts.iter().enumerate() {
        let (x0, x1) = col_spans[c];
        let equal_rows = vec![1.0; rows];
        let row_w = match weights.rows.get(c) {
            Some(r) if r.len() == rows => r.as_slice(),
            _ => &equal_rows,
        };
        for (y0, y1) in weighted_spans(area.size.h, area.loc.y, row_w) {
            rects.push(Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()));
        }
    }
    rects
}

/// Like [`tile_grid_weighted`] but insets every cell so there is a uniform
/// `gap` between neighbours and around the work-area edges. The margin stays
/// identical wherever a divider sits (each shared edge loses exactly `gap/2`
/// from both cells), so resizing never makes the gutter uneven or vanish.
pub fn tile_grid_gapped(
    area: Rectangle<i32, Logical>,
    n: usize,
    weights: &GridWeights,
    gap: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if gap <= 0 {
        return tile_grid_weighted(area, n, weights);
    }
    let half = gap / 2;
    let inner = Rectangle::new(
        (area.loc.x + half, area.loc.y + half).into(),
        ((area.size.w - gap).max(0), (area.size.h - gap).max(0)).into(),
    );
    tile_grid_weighted(inner, n, weights)
        .into_iter()
        .map(|r| {
            Rectangle::new(
                (r.loc.x + half, r.loc.y + half).into(),
                ((r.size.w - gap).max(1), (r.size.h - gap).max(1)).into(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (1280, 800).into())
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// The even (unweighted) grid, i.e. the default layout for `n` windows.
    fn grid(area: Rectangle<i32, Logical>, n: usize) -> Vec<Rectangle<i32, Logical>> {
        tile_grid_weighted(area, n, &GridWeights::default())
    }

    #[test]
    fn grid_capacity_respects_columns_and_rows() {
        // Default 3x3: up to nine windows, the tenth overflows.
        assert!(fits_grid(9, 3, 3));
        assert!(!fits_grid(10, 3, 3));
        // The classic quadrant cap: four windows fit, five do not.
        assert!(fits_grid(4, 2, 2));
        assert!(!fits_grid(5, 2, 2));
        // A single column of three rows.
        assert!(fits_grid(3, 1, 3));
        assert!(!fits_grid(4, 1, 3));
        // A single row never turns into columns of stacked cells.
        assert!(fits_grid(2, 4, 1));
        assert!(!fits_grid(3, 4, 1));
        // An empty desktop always fits.
        assert!(fits_grid(0, 3, 3));
    }

    #[test]
    fn tile_grid_matches_agreed_layout() {
        // n=1: full.
        assert_eq!(grid(area(), 1), vec![rect(0, 0, 1280, 800)]);
        // n=2: two full-height halves.
        assert_eq!(
            grid(area(), 2),
            vec![rect(0, 0, 640, 800), rect(640, 0, 640, 800)]
        );
        // n=3: left full-height, right column split into two.
        assert_eq!(
            grid(area(), 3),
            vec![
                rect(0, 0, 640, 800),
                rect(640, 0, 640, 400),
                rect(640, 400, 640, 400),
            ]
        );
        // n=4: quadrants.
        assert_eq!(
            grid(area(), 4),
            vec![
                rect(0, 0, 640, 400),
                rect(0, 400, 640, 400),
                rect(640, 0, 640, 400),
                rect(640, 400, 640, 400),
            ]
        );
    }

    #[test]
    fn tile_grid_covers_area_without_gaps() {
        // For a range of counts, the cells must exactly partition the area.
        for n in 1..=12 {
            let rects = grid(area(), n);
            assert_eq!(rects.len(), n, "n={n} produced {} rects", rects.len());
            let covered: i32 = rects.iter().map(|r| r.size.w * r.size.h).sum();
            assert_eq!(covered, 1280 * 800, "n={n} does not cover the area exactly");
            // No cell escapes the area.
            for r in &rects {
                assert!(r.loc.x >= 0 && r.loc.y >= 0);
                assert!(r.loc.x + r.size.w <= 1280 && r.loc.y + r.size.h <= 800);
            }
        }
    }

    #[test]
    fn cell_of_maps_index_to_column_and_row() {
        // n=3 → columns [1, 2]: index 0 is the sole left tile; 1 and 2 are the
        // top and bottom of the right column.
        assert_eq!(cell_of(3, 0), Some((0, 0)));
        assert_eq!(cell_of(3, 1), Some((1, 0)));
        assert_eq!(cell_of(3, 2), Some((1, 1)));
        assert_eq!(cell_of(3, 3), None);
        assert_eq!(cell_of(0, 0), None);
    }

    #[test]
    fn weighted_columns_shift_the_divider() {
        // Two columns, left weighted 3:1 — the divider sits at 3/4 width.
        let w = GridWeights {
            cols: vec![3.0, 1.0],
            rows: Vec::new(),
        };
        let rects = tile_grid_weighted(area(), 2, &w);
        assert_eq!(rects, vec![rect(0, 0, 960, 800), rect(960, 0, 320, 800)]);
    }

    #[test]
    fn launcher_row_at_matches_render_geometry() {
        let top = BAR_HEIGHT + LAUNCHER_PAD;
        // First row covers [top, top+ROW_H); second row the next slot.
        assert_eq!(
            launcher_row_at((LAUNCHER_LEFT, top).into(), 8, 800, false),
            Some(0)
        );
        assert_eq!(
            launcher_row_at(
                (LAUNCHER_LEFT + 1, top + LAUNCHER_ROW_H).into(),
                8,
                800,
                false
            ),
            Some(1)
        );
        // The 3rd row is beyond 2 results: nothing there.
        assert_eq!(
            launcher_row_at(
                (LAUNCHER_LEFT, top + 2 * LAUNCHER_ROW_H).into(),
                2,
                800,
                false
            ),
            None
        );
        // Outside the results panel or beyond the final row.
        assert_eq!(launcher_row_at((0, top).into(), 8, 800, false), None);
        assert_eq!(
            launcher_row_at((LAUNCHER_LEFT, top - 1).into(), 8, 800, false),
            None
        );
        assert_eq!(
            launcher_row_at(
                (LAUNCHER_LEFT, top + 8 * LAUNCHER_ROW_H).into(),
                8,
                800,
                false
            ),
            None
        );
        assert_eq!(
            launcher_row_at((LAUNCHER_LEFT, 0).into(), 0, 800, false),
            None
        );

        // A bottom panel places the results above it.
        let height = 8 * LAUNCHER_ROW_H + LAUNCHER_PAD * 2;
        let bottom_edge = 800 - BAR_HEIGHT;
        let top = bottom_edge - height + LAUNCHER_PAD;
        assert_eq!(
            launcher_row_at((LAUNCHER_LEFT, top).into(), 8, 800, true),
            Some(0)
        );
        assert_eq!(
            launcher_row_at((LAUNCHER_LEFT, top + LAUNCHER_ROW_H).into(), 8, 800, true),
            Some(1)
        );
        // The gap between the results and bottom panel is not a row.
        assert_eq!(
            launcher_row_at(
                (LAUNCHER_LEFT, bottom_edge - LAUNCHER_PAD + 1).into(),
                8,
                800,
                true
            ),
            None
        );
    }

    #[test]
    fn work_area_clears_the_bar_side() {
        let output = rect(0, 0, 1280, 800);
        // Top panel.
        assert_eq!(
            work_area(output, false),
            rect(0, BAR_HEIGHT, 1280, 800 - BAR_HEIGHT)
        );
        // Bottom panel.
        assert_eq!(work_area(output, true), rect(0, 0, 1280, 800 - BAR_HEIGHT));
    }

    #[test]
    fn bar_offset_moves_bottom_bar_below_the_work_area() {
        assert_eq!(bar_offset(800, false), (0, 0).into());
        assert_eq!(bar_offset(800, true), (0, 800 - BAR_HEIGHT).into());
    }

    #[test]
    fn no_pips_when_single_desktop() {
        // Pips appear only when there is more than one workspace.
        assert!(bar_pip_rects(1280, 1).is_empty());
        assert_eq!(bar_pip_rects(1280, 2).len(), 2);
    }

    #[test]
    fn gapped_grid_keeps_a_uniform_margin() {
        let g = 6;
        let r = tile_grid_gapped(area(), 2, &GridWeights::default(), g);
        let (left, right) = (r[0], r[1]);
        // Edge margins and the interior gutter are all exactly `g`.
        assert_eq!(left.loc.x, g);
        assert_eq!(left.loc.y, g);
        assert_eq!(right.loc.x + right.size.w, 1280 - g);
        assert_eq!(right.loc.x - (left.loc.x + left.size.w), g);
        // The gutter is independent of the divider position.
        let w = GridWeights {
            cols: vec![3.0, 1.0],
            rows: Vec::new(),
        };
        let r = tile_grid_gapped(area(), 2, &w, g);
        assert_eq!(r[1].loc.x - (r[0].loc.x + r[0].size.w), g);
    }
}
