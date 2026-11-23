use crate::schema::{BBox, Block, BlockKind};

/// Default minimum normalized whitespace gutter width/height for Recursive XY-Cut.
pub const DEFAULT_MIN_GUTTER: f32 = 0.025;

/// Margin thresholds for separating running headers and footers from body flow.
const TOP_HEADER_Y_MAX: f32 = 0.08;
const BOTTOM_FOOTER_Y_MIN: f32 = 0.92;
const MAX_MARGIN_BAND_HEIGHT: f32 = 0.06;

/// Sorts `blocks` in place into logical reading order using Recursive XY-Cut,
/// updating each block's `reading_order` and `column_index`.
pub fn sort_blocks_reading_order(blocks: &mut Vec<Block>, min_gutter: f32) {
    if blocks.is_empty() {
        return;
    }

    let mut top_headers: Vec<usize> = Vec::new();
    let mut bottom_footers: Vec<usize> = Vec::new();
    let mut body: Vec<usize> = Vec::new();

    for (idx, block) in blocks.iter().enumerate() {
        if is_top_header(block) {
            top_headers.push(idx);
        } else if is_bottom_footer(block) {
            bottom_footers.push(idx);
        } else {
            body.push(idx);
        }
    }

    let cmp_top_left = |&a: &usize, &b: &usize| {
        blocks[a]
            .bbox
            .y0
            .total_cmp(&blocks[b].bbox.y0)
            .then_with(|| blocks[a].bbox.x0.total_cmp(&blocks[b].bbox.x0))
    };

    top_headers.sort_by(cmp_top_left);
    bottom_footers.sort_by(cmp_top_left);

    let mut assigned_cols: Vec<Option<u8>> = vec![None; blocks.len()];
    let ordered_body = xy_cut(&body, blocks, min_gutter, &mut assigned_cols);

    let mut final_indices = Vec::with_capacity(blocks.len());
    final_indices.extend(top_headers);
    final_indices.extend(ordered_body);
    final_indices.extend(bottom_footers);

    let old_blocks = std::mem::take(blocks);
    let mut slots: Vec<Option<Block>> = old_blocks.into_iter().map(Some).collect();

    for (order, &orig_idx) in final_indices.iter().enumerate() {
        if let Some(mut blk) = slots[orig_idx].take() {
            blk.reading_order = order;
            blk.column_index = assigned_cols[orig_idx];
            blocks.push(blk);
        }
    }
}

fn is_top_header(block: &Block) -> bool {
    match block.kind {
        BlockKind::HeaderFooter { is_header, .. } => is_header,
        _ => block.bbox.y1 < TOP_HEADER_Y_MAX && block.bbox.height() < MAX_MARGIN_BAND_HEIGHT,
    }
}

fn is_bottom_footer(block: &Block) -> bool {
    match block.kind {
        BlockKind::HeaderFooter { is_header, .. } => !is_header,
        _ => block.
