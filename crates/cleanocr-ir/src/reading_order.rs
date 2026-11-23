use crate::schema::{Block, BlockKind};

/// Configuration thresholds for Recursive XY-Cut reading-order sorting.
#[derive(Debug, Clone, Copy)]
pub struct ReadingOrderConfig {
    /// Minimum normalized whitespace valley width required to split along X or Y.
    pub min_gutter: f32,
    /// Blocks ending above this normalized Y coordinate are treated as top headers.
    pub header_max_y: f32,
    /// Blocks starting below this normalized Y coordinate are treated as bottom footers.
    pub footer_min_y: f32,
}

impl Default for ReadingOrderConfig {
    fn default() -> Self {
        Self {
            min_gutter: 0.025,
            header_max_y: 0.08,
            footer_min_y: 0.92,
        }
    }
}

/// Sorts blocks in-place into logical reading order using default Recursive XY-Cut settings,
/// updating `reading_order` and `column_index` on every block.
pub fn sort_reading_order(blocks: &mut Vec<Block>) {
    sort_reading_order_with_config(blocks, &ReadingOrderConfig::default());
}

/// Sorts blocks in-place into logical reading order with custom thresholds.
pub fn sort_reading_order_with_config(blocks: &mut Vec<Block>, config: &ReadingOrderConfig) {
    if blocks.is_empty() {
        return;
    }

    let mut headers: Vec<usize> = Vec::new();
    let mut body: Vec<usize> = Vec::new();
    let mut footers: Vec<usize> = Vec::new();

    for (idx, block) in blocks.iter().enumerate() {
        match &block.kind {
            BlockKind::HeaderFooter {
                is_header: true, ..
            } => headers.push(idx),
            BlockKind::HeaderFooter {
                is_header: false, ..
            } => footers.push(idx),
            _ if block.bbox.y1 < config.header_max_y => headers.push(idx),
            _ if block.bbox.y0 > config.footer_min_y => footers.push(idx),
            _ => body.push(idx),
        }
    }

    sort_top_to_bottom_left_to_right(blocks, &mut headers);
    sort_top_to_bottom_left_to_right(blocks, &mut footers);

    let mut ordered_entries: Vec<(usize, Option<u8>)> = Vec::with_capacity(blocks.len());
    for idx in headers {
        ordered_entries.push((idx, None));
    }

    xy_cut_recursive(blocks, &body, config.min_gutter, None, &mut ordered_entries);

    for idx in footers {
        ordered_entries.push((idx, None));
    }

    // Reorder blocks according to computed indices and stamp reading_order + column_index.
    let mut slots: Vec<Option<Block>> = blocks.drain(..).map(Some).collect();
    for (order, (orig_idx, col_idx)) in ordered_entries.into_iter().enumerate() {
        if let Some(mut block) = slots[orig_idx].take() {
            block.reading_order = order;
            block.column_index = col_idx;
            blocks.push(block);
        }
    }
}

/// Recursively partitions a set of body block indices using projection-profile valleys.
///
/// Strategy:
/// - If a valid vertical gutter (`>= min_gutter`) spans the entire subset (meaning no block
///   bridges across the gutter), cut vertically into columns first so aligned rows across
///   columns are never interleaved.
/// - Otherwise (e.g., when a full-width title, banner, or table bridges across columns),
///   cut horizontally along whitespace valleys to peel off full-width bands first.
/// - If neither axis has a valley wider than `min_gutter`, fall back to deterministic
///   top-to-bottom, left-to-right ordering.
fn xy_cut_recursive(
    blocks: &[Block],
    indices: &[usize],
    min_gutter: f32,
    inherited_col: Option<u8>,
    out: &mut Vec<(usize, Option<u8>)>,
) {
    if indices.is_empty() {
        return;
    }
    if indices.len() == 1 {
        out.push((indices[0], inherited_col));
        return;
    }

    // 1. Check vertical gutters (multi-column split when no block spans the gutter).
    let x_cuts = find_projection_cuts(
        indices
            .iter()
            .map(|&i| (blocks[i].bbox.x0, blocks[i].bbox.x1)),
        min_gutter,
    );
    if !x_cuts.is_empty() {
        let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); x_cuts.len() + 1];
        for &idx in indices {
            let cx = 0.5 * (blocks[idx].bbox.x0 + blocks[idx].bbox.x1);
            let bucket_idx = x_cuts.partition_point(|&cut| cut <= cx);
            buckets[bucket_idx].push(idx);
        }

        let mut col_counter: u8 = 0;
        for bucket in buckets {
            if bucket.is_empty() {
                continue;
            }
            let next_col = inherited_col.or(Some(col_counter));
            col_counter = col_counter.saturating_add(1);
            xy_cut_recursive(blocks, &bucket, min_gutter, next_col, out);
        }
        return;
    }

    // 2. Check horizontal gutters (peels full-width titles/banners/tables from columns).
    let y_cuts = find_projection_cuts(
        indices
            .iter()
            .map(|&i| (blocks[i].bbox.y0, blocks[i].bbox.y1)),
        min_gutter,
    );
    if !y_cuts.is_empty() {
        let mut bands: Vec<Vec<usize>> = vec![Vec::new(); y_cuts.len() + 1];
        for &idx in indices {
            let cy = 0.5 * (blocks[idx].bbox.y0 + blocks[idx].bbox.y1);
            let band_idx = y_cuts.partition_point(|&cut| cut <= cy);
            bands[band_idx].push(idx);
        }

        for band in bands {
            if band.is_empty() {
                continue;
            }
            xy_cut_recursive(blocks, &band, min_gutter, inherited_col, out);
        }
        return;
    }

    // 3. No clean whitespace gutter on either axis: deterministic fallback sort.
    let mut fallback = indices.to_vec();
    sort_top_to_bottom_left_to_right(blocks, &mut fallback);
    for idx in fallback {
        out.push((idx, inherited_col));
    }
}

/// Computes sorted midpoint coordinates of all whitespace valleys `>= min_gutter`
/// in a 1D interval projection.
fn find_projection_cuts(intervals: impl Iterator<Item = (f32, f32)>, min_gutter: f32) -> Vec<f32> {
    let mut sorted: Vec<(f32, f32)> = intervals
        .map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
        .collect();
    if sorted.len() < 2 {
        return Vec::new();
    }

    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

    let mut cuts = Vec::new();
    let mut cur_end = sorted[0].1;

    for (start, end) in sorted.into_iter().skip(1) {
        let gap = start - cur_end;
        if gap >= min_gutter {
            cuts.push(0.5 * (cur_end + start));
            cur_end = end;
        } else if end > cur_end {
            cur_end = end;
        }
    }

    cuts
}

fn sort_top_to_bottom_left_to_right(blocks: &[Block], indices: &mut [usize]) {
    indices.sort_by(|&a, &b| {
        blocks[a]
            .bbox
            .y0
            .total_cmp(&blocks[b].bbox.y0)
            .then_with(|| blocks[a].bbox.x0.total_cmp(&blocks[b].bbox.x0))
            .then_with(|| blocks[a].id.cmp(&blocks[b].id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BBox, TextLine, TextSource, TextSpan};

    fn make_para(id: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> Block {
        let bbox = BBox::new(x0, y0, x1, y1);
        Block {
            id: id.to_string(),
            bbox,
            reading_order: 0,
            column_index: None,
            confidence: 0.99,
            kind: BlockKind::Paragraph {
                lines: vec![TextLine {
                    bbox,
                    spans: vec![TextSpan {
                        text: id.to_string(),
                        bbox,
                        confidence: 0.99,
                        is_bold: false,
                        is_italic: false,
                        inline_math_latex: None,
                        source: TextSource::Ocr,
                    }],
                }],
            },
        }
    }

    fn make_header_footer(id: &str, is_header: bool, y0: f32, y1: f32) -> Block {
        Block {
            id: id.to_string(),
            bbox: BBox::new(0.1, y0, 0.9, y1),
            reading_order: 0,
            column_index: None,
            confidence: 0.99,
            kind: BlockKind::HeaderFooter {
                is_header,
                text: id.to_string(),
            },
        }
    }

    #[test]
    fn two_column_page_with_title_header_and_footer_does_not_interleave() {
        // Deliberately shuffled input; left and right columns share identical Y bands
        // to ensure columns are never interleaved horizontally.
        let mut blocks = vec![
            make_header_footer("footer", false, 0.94, 0.97),
            make_para("right_2", 0.54, 0.46, 0.90, 0.70),
            make_para("left_2", 0.10, 0.46, 0.46, 0.70),
            make_para("title", 0.10, 0.10, 0.90, 0.16),
            make_para("right_1", 0.54, 0.21, 0.90, 0.41),
            make_header_footer("header", true, 0.02, 0.05),
            make_para("left_1", 0.10, 0.21, 0.46, 0.41),
        ];

        sort_reading_order(&mut blocks);

        let ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "header", "title", "left_1", "left_2", "right_1", "right_2", "footer"
            ]
        );

        let orders: Vec<usize> = blocks.iter().map(|b| b.reading_order).collect();
        assert_eq!(orders, vec![0, 1, 2, 3, 4, 5, 6]);

        let cols: Vec<Option<u8>> = blocks.iter().map(|b| b.column_index).collect();
        assert_eq!(
            cols,
            vec![None, None, Some(0), Some(0), Some(1), Some(1), None]
        );
    }

    #[test]
    fn three_column_layout_with_full_width_bottom_band() {
        let mut blocks = vec![
            make_para("full_bottom", 0.08, 0.76, 0.92, 0.88),
            make_para("col2_a", 0.68, 0.20, 0.92, 0.65),
            make_para("col1_a", 0.38, 0.20, 0.62, 0.65),
            make_para("col0_a", 0.08, 0.20, 0.32, 0.65),
            make_para("banner", 0.08, 0.09, 0.92, 0.15),
        ];

        sort_reading_order(&mut blocks);

        let ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["banner", "col0_a", "col1_a", "col2_a", "full_bottom"]
        );

        let cols: Vec<Option<u8>> = blocks.iter().map(|b| b.column_index).collect();
        assert_eq!(cols, vec![None, Some(0), Some(1), Some(2), None]);
    }

    #[test]
    fn overlapping_blocks_sort_deterministically() {
        let mut blocks = vec![
            make_para("b", 0.20, 0.25, 0.60, 0.45),
            make_para("a", 0.10, 0.20, 0.50, 0.40),
        ];

        sort_reading_order(&mut blocks);

        let ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn empty_and_single_block_do_not_panic() {
        let mut empty: Vec<Block> = Vec::new();
        sort_reading_order(&mut empty);
        assert!(empty.is_empty());

        let mut single = vec![make_para("only", 0.1, 0.2, 0.9, 0.3)];
        sort_reading_order(&mut single);
        assert_eq!(single[0].reading_order, 0);
        assert_eq!(single[0].column_index, None);
    }
}
