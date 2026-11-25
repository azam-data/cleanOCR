use std::collections::VecDeque;

use cleanocr_ir::BBox;
use image::RgbImage;

/// A detected horizontal or vertical ruling line segment in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSegment {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl LineSegment {
    pub fn center_x(&self) -> u32 {
        (self.x0 + self.x1) / 2
    }

    pub fn center_y(&self) -> u32 {
        (self.y0 + self.y1) / 2
    }
}

/// A single cell inside a detected bordered (lattice) table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectedCell {
    pub row: usize,
    pub col: usize,
    pub row_span: usize,
    pub col_span: usize,
    pub bbox: BBox,
}

/// A bordered table grid reconstructed from intersecting horizontal and vertical rulings.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectedTableGrid {
    pub bbox: BBox,
    pub num_rows: usize,
    pub num_cols: usize,
    pub cells: Vec<DetectedCell>,
}

/// Result of morphological lattice table detection on a single page image.
#[derive(Debug, Clone, PartialEq)]
pub struct LatticeDetectionResult {
    pub horizontal_lines: Vec<LineSegment>,
    pub vertical_lines: Vec<LineSegment>,
    /// 1 byte per pixel (`1` = table ruling pixel, `0` = non-ruling), length `width * height`.
    pub table_grid_mask: Vec<u8>,
    pub tables: Vec<DetectedTableGrid>,
}

/// Detects bordered (lattice) tables via fast 1D morphological opening on the page luminance.
pub fn detect_lattice_tables(img: &RgbImage) -> LatticeDetectionResult {
    let width = img.width();
    let height = img.height();
    let w = width as usize;
    let h = height as usize;

    if w < 16 || h < 16 {
        return LatticeDetectionResult {
            horizontal_lines: Vec::new(),
            vertical_lines: Vec::new(),
            table_grid_mask: vec![0u8; w * h],
            tables: Vec::new(),
        };
    }

    // 1. Binarize dark pixels (lum < 190).
    let raw = img.as_raw();
    let mut bin = vec![0u8; w * h];
    for (i, rgb) in raw.chunks_exact(3).enumerate() {
        let lum = 0.299 * rgb[0] as f32 + 0.587 * rgb[1] as f32 + 0.114 * rgb[2] as f32;
        if lum < 190.0 {
            bin[i] = 1;
        }
    }

    // 2. Fast O(W*H) 1D morphological opening:
    //    Horizontal kernel ~ 2.5% of page width; vertical kernel ~ 2.0% of page height.
    let min_h_len = ((w as f32 * 0.025).round() as usize).max(12).min(w);
    let min_v_len = ((h as f32 * 0.020).round() as usize).max(12).min(h);

    let mut h_mask = vec![0u8; w * h];
    for y in 0..h {
        let row_off = y * w;
        let mut x = 0;
        while x < w {
            if bin[row_off + x] == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && bin[row_off + x] == 1 {
                x += 1;
            }
            if x - start >= min_h_len {
                for px in start..x {
                    h_mask[row_off + px] = 1;
                }
            }
        }
    }

    let mut v_mask = vec![0u8; w * h];
    for x in 0..w {
        let mut y = 0;
        while y < h {
            if bin[y * w + x] == 0 {
                y += 1;
                continue;
            }
            let start = y;
            while y < h && bin[y * w + x] == 1 {
                y += 1;
            }
            if y - start >= min_v_len {
                for py in start..y {
                    v_mask[py * w + x] = 1;
                }
            }
        }
    }

    let mut table_grid_mask = vec![0u8; w * h];
    for i in 0..(w * h) {
        if h_mask[i] == 1 || v_mask[i] == 1 {
            table_grid_mask[i] = 1;
        }
    }

    // 3. Extract merged horizontal and vertical line segments.
    let horizontal_lines = extract_horizontal_segments(&h_mask, w, h);
    let vertical_lines = extract_vertical_segments(&v_mask, w, h);

    // 4. Cluster intersecting H/V segments into rectangular table grids with merged-cell support.
    let tables = build_table_grids(&horizontal_lines, &vertical_lines, width, height);

    LatticeDetectionResult {
        horizontal_lines,
        vertical_lines,
        table_grid_mask,
        tables,
    }
}

fn extract_horizontal_segments(h_mask: &[u8], w: usize, h: usize) -> Vec<LineSegment> {
    let mut raw_runs: Vec<LineSegment> = Vec::new();
    for y in 0..h {
        let row_off = y * w;
        let mut x = 0;
        while x < w {
            if h_mask[row_off + x] == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && h_mask[row_off + x] == 1 {
                x += 1;
            }
            raw_runs.push(LineSegment {
                x0: start as u32,
                y0: y as u32,
                x1: (x - 1) as u32,
                y1: y as u32,
            });
        }
    }

    // Merge vertically adjacent runs (thickness up to 4 px) with overlapping X spans.
    let mut merged: Vec<LineSegment> = Vec::new();
    for run in raw_runs {
        if let Some(prev) = merged.iter_mut().rev().find(|s| {
            run.y0 <= s.y1.saturating_add(2)
                && run.x0 <= s.x1.saturating_add(3)
                && s.x0 <= run.x1.saturating_add(3)
        }) {
            prev.x0 = prev.x0.min(run.x0);
            prev.x1 = prev.x1.max(run.x1);
            prev.y0 = prev.y0.min(run.y0);
            prev.y1 = prev.y1.max(run.y1);
        } else {
            merged.push(run);
        }
    }
    merged
}

fn extract_vertical_segments(v_mask: &[u8], w: usize, h: usize) -> Vec<LineSegment> {
    let mut raw_runs: Vec<LineSegment> = Vec::new();
    for x in 0..w {
        let mut y = 0;
        while y < h {
            if v_mask[y * w + x] == 0 {
                y += 1;
                continue;
            }
            let start = y;
            while y < h && v_mask[y * w + x] == 1 {
                y += 1;
            }
            raw_runs.push(LineSegment {
                x0: x as u32,
                y0: start as u32,
                x1: x as u32,
                y1: (y - 1) as u32,
            });
        }
    }

    // Merge horizontally adjacent runs (thickness up to 4 px) with overlapping Y spans.
    let mut merged: Vec<LineSegment> = Vec::new();
    for run in raw_runs {
        if let Some(prev) = merged.iter_mut().rev().find(|s| {
            run.x0 <= s.x1.saturating_add(2)
                && run.y0 <= s.y1.saturating_add(3)
                && s.y0 <= run.y1.saturating_add(3)
        }) {
            prev.x0 = prev.x0.min(run.x0);
            prev.x1 = prev.x1.max(run.x1);
            prev.y0 = prev.y0.min(run.y0);
            prev.y1 = prev.y1.max(run.y1);
        } else {
            merged.push(run);
        }
    }
    merged
}

fn segments_intersect(h: &LineSegment, v: &LineSegment, tol: u32) -> bool {
    h.x0 <= v.x1.saturating_add(tol)
        && v.x0 <= h.x1.saturating_add(tol)
        && v.y0 <= h.y1.saturating_add(tol)
        && h.y0 <= v.y1.saturating_add(tol)
}

fn build_table_grids(
    h_lines: &[LineSegment],
    v_lines: &[LineSegment],
    img_w: u32,
    img_h: u32,
) -> Vec<DetectedTableGrid> {
    let tol = 4u32;
    let mut h_visited = vec![false; h_lines.len()];
    let mut v_visited = vec![false; v_lines.len()];
    let mut tables = Vec::new();

    for start_h in 0..h_lines.len() {
        if h_visited[start_h] {
            continue;
        }

        let mut comp_h: Vec<usize> = Vec::new();
        let mut comp_v: Vec<usize> = Vec::new();
        let mut queue: VecDeque<(bool, usize)> = VecDeque::new();

        h_visited[start_h] = true;
        queue.push_back((true, start_h));

        while let Some((is_h, idx)) = queue.pop_front() {
            if is_h {
                comp_h.push(idx);
                let h_seg = &h_lines[idx];
                for (v_idx, v_seg) in v_lines.iter().enumerate() {
                    if !v_visited[v_idx] && segments_intersect(h_seg, v_seg, tol) {
                        v_visited[v_idx] = true;
                        queue.push_back((false, v_idx));
                    }
                }
            } else {
                comp_v.push(idx);
                let v_seg = &v_lines[idx];
                for (h_idx, h_seg) in h_lines.iter().enumerate() {
                    if !h_visited[h_idx] && segments_intersect(h_seg, v_seg, tol) {
                        h_visited[h_idx] = true;
                        queue.push_back((true, h_idx));
                    }
                }
            }
        }

        if comp_h.len() < 2 || comp_v.len() < 2 {
            continue;
        }

        let y_coords = cluster_coords(comp_h.iter().map(|&i| h_lines[i].center_y()).collect(), tol);
        let x_coords = cluster_coords(comp_v.iter().map(|&i| v_lines[i].center_x()).collect(), tol);

        if y_coords.len() < 2 || x_coords.len() < 2 {
            continue;
        }

        let num_rows = y_coords.len() - 1;
        let num_cols = x_coords.len() - 1;

        let has_h_wall = |row_line_idx: usize, col_idx: usize| -> bool {
            let target_y = y_coords[row_line_idx];
            let cx0 = x_coords[col_idx];
            let cx1 = x_coords[col_idx + 1];
            let cell_w = cx1.saturating_sub(cx0).max(1);

            comp_h.iter().any(|&hi| {
                let seg = &h_lines[hi];
                if seg.center_y().abs_diff(target_y) > tol {
                    return false;
                }
                let ox0 = seg.x0.max(cx0);
                let ox1 = seg.x1.min(cx1);
                ox1 > ox0 && (ox1 - ox0) * 10 >= cell_w * 6
            })
        };

        let has_v_wall = |row_idx: usize, col_line_idx: usize| -> bool {
            let target_x = x_coords[col_line_idx];
            let ry0 = y_coords[row_idx];
            let ry1 = y_coords[row_idx + 1];
            let cell_h = ry1.saturating_sub(ry0).max(1);

            comp_v.iter().any(|&vi| {
                let seg = &v_lines[vi];
                if seg.center_x().abs_diff(target_x) > tol {
                    return false;
                }
                let oy0 = seg.y0.max(ry0);
                let oy1 = seg.y1.min(ry1);
                oy1 > oy0 && (oy1 - oy0) * 10 >= cell_h * 6
            })
        };

        let mut occupied = vec![vec![false; num_cols]; num_rows];
        let mut cells = Vec::new();

        for r in 0..num_rows {
            for c in 0..num_cols {
                if occupied[r][c] {
                    continue;
                }

                let mut col_span = 1usize;
                while c + col_span < num_cols
                    && !occupied[r][c + col_span]
                    && !has_v_wall(r, c + col_span)
                {
                    col_span += 1;
                }

                let mut row_span = 1usize;
                'row_expand: while r + row_span < num_rows {
                    for dc in 0..col_span {
                        if occupied[r + row_span][c + dc] || has_h_wall(r + row_span, c + dc) {
                            break 'row_expand;
                        }
                    }
                    row_span += 1;
                }

                for row_occ in occupied.iter_mut().skip(r).take(row_span) {
                    for cell_occ in row_occ.iter_mut().skip(c).take(col_span) {
                        *cell_occ = true;
                    }
                }

                let px0 = x_coords[c] as f32 / img_w as f32;
                let py0 = y_coords[r] as f32 / img_h as f32;
                let px1 = x_coords[c + col_span] as f32 / img_w as f32;
                let py1 = y_coords[r + row_span] as f32 / img_h as f32;

                cells.push(DetectedCell {
                    row: r,
                    col: c,
                    row_span,
                    col_span,
                    bbox: BBox::new(px0, py0, px1, py1),
                });
            }
        }

        let tx0 = *x_coords.first().expect("non-empty x_coords") as f32 / img_w as f32;
        let ty0 = *y_coords.first().expect("non-empty y_coords") as f32 / img_h as f32;
        let tx1 = *x_coords.last().expect("non-empty x_coords") as f32 / img_w as f32;
        let ty1 = *y_coords.last().expect("non-empty y_coords") as f32 / img_h as f32;

        tables.push(DetectedTableGrid {
            bbox: BBox::new(tx0, ty0, tx1, ty1),
            num_rows,
            num_cols,
            cells,
        });
    }

    tables.sort_by(|a, b| a.bbox.y0.total_cmp(&b.bbox.y0));
    tables
}

fn cluster_coords(mut coords: Vec<u32>, tol: u32) -> Vec<u32> {
    if coords.is_empty() {
        return coords;
    }
    coords.sort_unstable();
    let mut clusters: Vec<u32> = Vec::new();
    let mut sum: u64 = coords[0] as u64;
    let mut count: u64 = 1;
    let mut last = coords[0];

    for c in coords.into_iter().skip(1) {
        if c.saturating_sub(last) <= tol {
            sum += c as u64;
            count += 1;
            last = c;
        } else {
            clusters.push((sum / count) as u32);
            sum = c as u64;
            count = 1;
            last = c;
        }
    }
    clusters.push((sum / count) as u32);
    clusters
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    #[test]
    fn detects_4x5_lattice_table_with_merged_section_header_row() {
        let w = 800u32;
        let h = 600u32;
        let mut img = RgbImage::from_pixel(w, h, Rgb([255, 255, 255]));
        let black = Rgb([15, 15, 15]);

        let xs = [100u32, 250, 400, 550, 700];
        let ys = [80u32, 140, 200, 260, 320, 380];

        // Draw 6 horizontal lines (5 rows) with 2 px thickness.
        for &y in &ys {
            for dy in 0..2 {
                for x in xs[0]..=xs[4] {
                    img.put_pixel(x, y + dy, black);
                }
            }
        }

        // Draw outer vertical borders (col 0 and col 4) across all 5 rows.
        for &x in &[xs[0], xs[4]] {
            for dx in 0..2 {
                for y in ys[0]..=ys[5] {
                    img.put_pixel(x + dx, y, black);
                }
            }
        }

        // Draw internal vertical lines (cols 1, 2, 3), skipping row 1 (ys[1]..ys[2])
        // so row 1 forms a full-width merged section header (col_span == 4).
        for &x in &[xs[1], xs[2], xs[3]] {
            for dx in 0..2 {
                for y in ys[0]..=ys[1] {
                    img.put_pixel(x + dx, y, black);
                }
                for y in ys[2]..=ys[5] {
                    img.put_pixel(x + dx, y, black);
                }
            }
        }

        let res = detect_lattice_tables(&img);
        assert!(
            res.table_grid_mask.contains(&1),
            "table_grid_mask must be non-empty"
        );
        assert_eq!(res.tables.len(), 1);

        let table = &res.tables[0];
        assert_eq!(table.num_rows, 5);
        assert_eq!(table.num_cols, 4);
        // 4 cells (row 0) + 1 merged cell (row 1) + 4*3 cells (rows 2..4) = 17 cells.
        assert_eq!(table.cells.len(), 17);

        let merged_cell = table
            .cells
            .iter()
            .find(|c| c.row == 1 && c.col == 0)
            .expect("merged section header cell at row 1, col 0");
        assert_eq!(merged_cell.col_span, 4);
        assert_eq!(merged_cell.row_span, 1);
    }
}
