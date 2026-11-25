use cleanocr_ir::{BBox, Block, BlockKind, TableCell, TableIr, TextLine, TextSpan};

/// Detects borderless (stream) tables from flat OCR/native paragraph blocks using
/// financial token splitting, Y-overlap row clustering, and 1D X-axis whitespace gutters.
pub fn detect_stream_tables(blocks: Vec<Block>, page_index: usize) -> Vec<Block> {
    if blocks.is_empty() {
        return blocks;
    }

    let mut preserved: Vec<Block> = Vec::new();
    let mut candidate_spans: Vec<(usize, TextSpan)> = Vec::new();
    let mut original_paras: Vec<(usize, Block)> = Vec::new();

    for (idx, block) in blocks.into_iter().enumerate() {
        match &block.kind {
            BlockKind::Paragraph { lines } => {
                let mut extracted = Vec::new();
                for line in lines {
                    for span in &line.spans {
                        extracted.extend(split_financial_span(span));
                    }
                }
                if extracted.is_empty() {
                    preserved.push(block);
                } else {
                    for sp in extracted {
                        candidate_spans.push((idx, sp));
                    }
                    original_paras.push((idx, block));
                }
            }
            _ => preserved.push(block),
        }
    }

    if candidate_spans.len() < 6 {
        for (_, b) in original_paras {
            preserved.push(b);
        }
        return preserved;
    }

    // Sort spans top-to-bottom, then left-to-right.
    candidate_spans.sort_by(|a, b| {
        a.1.bbox
            .y0
            .total_cmp(&b.1.bbox.y0)
            .then_with(|| a.1.bbox.x0.total_cmp(&b.1.bbox.x0))
    });

    // Cluster into horizontal visual rows using >= 50% Y-axis overlap of the shorter box.
    let mut rows: Vec<Vec<(usize, TextSpan)>> = Vec::new();
    for item in candidate_spans {
        let placed = if let Some(last_row) = rows.last_mut() {
            let (ry0, ry1) = row_y_bounds(last_row);
            let rh = (ry1 - ry0).max(1e-5);
            let sh = item.1.bbox.height().max(1e-5);
            let overlap = (ry1.min(item.1.bbox.y1) - ry0.max(item.1.bbox.y0)).max(0.0);
            if overlap >= 0.5 * rh.min(sh) {
                last_row.push(item.clone());
                true
            } else {
                false
            }
        } else {
            false
        };

        if !placed {
            rows.push(vec![item]);
        }
    }

    for row in &mut rows {
        row.sort_by(|a, b| a.1.bbox.x0.total_cmp(&b.1.bbox.x0));
    }

    // Identify contiguous runs of >= 3 multi-span rows (>= 2 spans per row).
    let mut consumed_block_ids = vec![false; original_paras.len() + preserved.len() + 1024];
    let mut emitted_tables: Vec<Block> = Vec::new();
    let mut r_idx = 0usize;
    let mut table_counter = 0usize;

    while r_idx < rows.len() {
        if rows[r_idx].len() < 2 {
            r_idx += 1;
            continue;
        }

        let run_start = r_idx;
        while r_idx < rows.len() && rows[r_idx].len() >= 2 {
            r_idx += 1;
        }
        let run = &rows[run_start..r_idx];
        if run.len() < 3 {
            continue;
        }

        if let Some(table_block) =
            try_build_stream_table(run, page_index, table_counter, &mut consumed_block_ids)
        {
            table_counter += 1;
            emitted_tables.push(table_block);
        }
    }

    if emitted_tables.is_empty() {
        for (_, b) in original_paras {
            preserved.push(b);
        }
        return preserved;
    }

    // Keep unconsumed paragraphs (if a paragraph was partially split, keep remaining unconsumed spans).
    for (orig_idx, block) in original_paras {
        if !consumed_block_ids.get(orig_idx).copied().unwrap_or(false) {
            preserved.push(block);
        }
    }

    // Also recover any unconsumed spans that belonged to a multi-span paragraph whose other spans
    // went into a table? Handled above: whole paragraphs in `run` are marked consumed;
    // paragraphs outside `run` remain intact.
    preserved.extend(emitted_tables);
    preserved
}

/// Splits a wide span containing multiple values separated by >= 2 spaces or tabs
/// into proportional sub-spans so financial rate columns are never glued together.
pub fn split_financial_span(span: &TextSpan) -> Vec<TextSpan> {
    let text = span.text.as_str();
    let chars: Vec<char> = text.chars().collect();
    let total_chars = chars.len();
    if total_chars < 3 {
        return vec![span.clone()];
    }

    // Find segments separated by >= 2 consecutive spaces or any tab.
    let mut segments: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0usize;
    let mut seg_start: Option<usize> = None;

    while i < total_chars {
        let is_tab = chars[i] == '\t';
        let is_multi_space =
            chars[i] == ' ' && i + 1 < total_chars && (chars[i + 1] == ' ' || chars[i + 1] == '\t');

        if is_tab || is_multi_space {
            if let Some(start) = seg_start.take() {
                let s: String = chars[start..i].iter().collect();
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    segments.push((start, i, trimmed.to_string()));
                }
            }
            while i < total_chars && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
        } else {
            if seg_start.is_none() && chars[i] != ' ' {
                seg_start = Some(i);
            }
            i += 1;
        }
    }
    if let Some(start) = seg_start {
        let s: String = chars[start..total_chars].iter().collect();
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            segments.push((start, total_chars, trimmed.to_string()));
        }
    }

    if segments.len() <= 1 {
        return vec![span.clone()];
    }

    let w = span.bbox.width();
    segments
        .into_iter()
        .map(|(s, e, sub)| {
            let x0 = span.bbox.x0 + (s as f32 / total_chars as f32) * w;
            let x1 = span.bbox.x0 + (e as f32 / total_chars as f32) * w;
            TextSpan {
                text: sub,
                bbox: BBox::new(x0, span.bbox.y0, x1.max(x0 + 1e-4), span.bbox.y1),
                confidence: span.confidence,
                is_bold: span.is_bold,
                is_italic: span.is_italic,
                inline_math_latex: span.inline_math_latex.clone(),
                source: span.source,
            }
        })
        .collect()
}

fn row_y_bounds(row: &[(usize, TextSpan)]) -> (f32, f32) {
    let mut y0 = f32::MAX;
    let mut y1 = f32::MIN;
    for (_, sp) in row {
        y0 = y0.min(sp.bbox.y0);
        y1 = y1.max(sp.bbox.y1);
    }
    (y0, y1)
}

fn try_build_stream_table(
    run: &[Vec<(usize, TextSpan)>],
    page_index: usize,
    table_idx: usize,
    consumed_block_ids: &mut Vec<bool>,
) -> Option<Block> {
    const BINS: usize = 200;

    let mut min_x0 = f32::MAX;
    let mut max_x1 = f32::MIN;
    let mut min_y0 = f32::MAX;
    let mut max_y1 = f32::MIN;
    for row in run {
        for (_, sp) in row {
            min_x0 = min_x0.min(sp.bbox.x0);
            max_x1 = max_x1.max(sp.bbox.x1);
            min_y0 = min_y0.min(sp.bbox.y0);
            max_y1 = max_y1.max(sp.bbox.y1);
        }
    }

    let min_bin = ((min_x0 * BINS as f32).floor() as usize).min(BINS - 1);
    let max_bin = ((max_x1 * BINS as f32).ceil() as usize).min(BINS);
    if max_bin <= min_bin + 2 {
        return None;
    }

    // 1D X-axis occupancy per row.
    let mut empty_counts = vec![0usize; BINS];
    for row in run {
        let mut occupied = [false; BINS];
        for (_, sp) in row {
            let b0 = ((sp.bbox.x0 * BINS as f32).floor() as usize).min(BINS - 1);
            let b1 = ((sp.bbox.x1 * BINS as f32).ceil() as usize).clamp(b0 + 1, BINS);
            for occ in occupied.iter_mut().take(b1).skip(b0) {
                *occ = true;
            }
        }
        for b in min_bin..max_bin {
            if !occupied[b] {
                empty_counts[b] += 1;
            }
        }
    }

    // Gutters shared by >= 75% of rows inside [min_bin, max_bin).
    let mut col_cuts: Vec<f32> = Vec::new();
    let mut b = min_bin;
    while b < max_bin {
        if empty_counts[b] * 4 >= run.len() * 3 {
            let g_start = b;
            while b < max_bin && empty_counts[b] * 4 >= run.len() * 3 {
                b += 1;
            }
            // Ignore leading/trailing outer margins.
            if g_start > min_bin && b < max_bin {
                let mid = 0.5 * ((g_start as f32 + b as f32) / BINS as f32);
                col_cuts.push(mid);
            }
        } else {
            b += 1;
        }
    }

    let num_cols = col_cuts.len() + 1;
    if num_cols < 2 {
        return None;
    }

    let num_rows = run.len();
    let mut grid_text: Vec<Vec<Vec<TextSpan>>> = vec![vec![Vec::new(); num_cols]; num_rows];
    let mut conf_sum = 0.0f32;
    let mut conf_count = 0usize;

    for (r, row) in run.iter().enumerate() {
        for (orig_idx, sp) in row {
            if *orig_idx >= consumed_block_ids.len() {
                consumed_block_ids.resize(*orig_idx + 1, false);
            }
            consumed_block_ids[*orig_idx] = true;

            let cx = 0.5 * (sp.bbox.x0 + sp.bbox.x1);
            let col = col_cuts.partition_point(|&cut| cut <= cx);
            grid_text[r][col].push(sp.clone());
            conf_sum += sp.confidence;
            conf_count += 1;
        }
    }

    let mut cells = Vec::with_capacity(num_rows * num_cols);
    for (r, row_cols) in grid_text.into_iter().enumerate() {
        let (ry0, ry1) = row_y_bounds(&run[r]);
        for (c, spans) in row_cols.into_iter().enumerate() {
            let col_x0 = if c == 0 { min_x0 } else { col_cuts[c - 1] };
            let col_x1 = if c + 1 == num_cols {
                max_x1
            } else {
                col_cuts[c]
            };

            let (text, bbox) = if spans.is_empty() {
                (String::new(), BBox::new(col_x0, ry0, col_x1, ry1))
            } else {
                let mut bx0 = f32::MAX;
                let mut by0 = f32::MAX;
                let mut bx1 = f32::MIN;
                let mut by1 = f32::MIN;
                let mut parts = Vec::with_capacity(spans.len());
                for sp in spans {
                    bx0 = bx0.min(sp.bbox.x0);
                    by0 = by0.min(sp.bbox.y0);
                    bx1 = bx1.max(sp.bbox.x1);
                    by1 = by1.max(sp.bbox.y1);
                    parts.push(sp.text);
                }
                (parts.join(" "), BBox::new(bx0, by0, bx1, by1))
            };

            cells.push(TableCell {
                row: r,
                col: c,
                row_span: 1,
                col_span: 1,
                is_header: r == 0,
                bbox,
                text,
            });
        }
    }

    let table_conf = if conf_count == 0 {
        1.0
    } else {
        conf_sum / conf_count as f32
    };

    // Keep unused TextLine import out by not needing it; only TableIr is emitted.
    let _ = std::marker::PhantomData::<TextLine>;

    Some(Block {
        id: format!("p{page_index}_stbl{table_idx:02}"),
        bbox: BBox::new(min_x0, min_y0, max_x1, max_y1),
        reading_order: 0,
        column_index: None,
        confidence: table_conf,
        kind: BlockKind::Table(TableIr {
            num_rows,
            num_cols,
            cells,
            caption: None,
        }),
    })
}

#[cfg(test)]
mod tests {
    use cleanocr_ir::TextSource;

    use super::*;

    fn span_block(id: &str, text: &str, x0: f32, y0: f32, x1: f32, y1: f32) -> Block {
        let bbox = BBox::new(x0, y0, x1, y1);
        Block {
            id: id.to_string(),
            bbox,
            reading_order: 0,
            column_index: None,
            confidence: 0.97,
            kind: BlockKind::Paragraph {
                lines: vec![TextLine {
                    bbox,
                    spans: vec![TextSpan {
                        text: text.to_string(),
                        bbox,
                        confidence: 0.97,
                        is_bold: false,
                        is_italic: false,
                        inline_math_latex: None,
                        source: TextSource::Ocr,
                    }],
                }],
            },
        }
    }

    #[test]
    fn detects_borderless_4col_financial_rate_table_and_splits_glued_rates() {
        let blocks = vec![
            span_block("h", "Summary of Deposit Rates", 0.10, 0.08, 0.55, 0.11),
            // Row 0 (header)
            span_block("r0c0", "Tenure", 0.10, 0.20, 0.22, 0.23),
            span_block("r0c1", "Individual", 0.30, 0.20, 0.44, 0.23),
            span_block("r0c23", "Corporate    Senior", 0.52, 0.20, 0.88, 0.23),
            // Row 1
            span_block("r1c0", "3 Months", 0.10, 0.25, 0.22, 0.28),
            span_block("r1c1", "8.50%", 0.30, 0.25, 0.40, 0.28),
            span_block("r1c23", "9.00%    9.25%", 0.52, 0.25, 0.88, 0.28),
            // Row 2
            span_block("r2c0", "6 Months", 0.10, 0.30, 0.22, 0.33),
            span_block("r2c1", "9.00%", 0.30, 0.30, 0.40, 0.33),
            span_block("r2c23", "9.50%    9.75%", 0.52, 0.30, 0.88, 0.33),
            // Row 3
            span_block("r3c0", "1 Year", 0.10, 0.35, 0.22, 0.38),
            span_block("r3c1", "9.50%", 0.30, 0.35, 0.40, 0.38),
            span_block("r3c23", "10.00%    10.25%", 0.52, 0.35, 0.88, 0.38),
        ];

        let out = detect_stream_tables(blocks, 0);
        assert_eq!(out.len(), 2, "expected 1 title paragraph + 1 stream table");

        let tbl = out
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("stream table block");

        assert_eq!(tbl.num_rows, 4);
        assert_eq!(tbl.num_cols, 4);
        assert_eq!(tbl.cells.len(), 16);

        let last_row: Vec<&str> = tbl
            .cells
            .iter()
            .filter(|c| c.row == 3)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(last_row, vec!["1 Year", "9.50%", "10.00%", "10.25%"]);
    }
}
