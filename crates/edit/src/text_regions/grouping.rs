use super::*;

fn same_plane(a: &TextLine, b: &TextLine) -> bool {
    let at = Matrix(a.origin.tm).then(&Matrix(a.origin.ctm));
    let bt = Matrix(b.origin.tm).then(&Matrix(b.origin.ctm));
    a.scope == b.scope
        && a.paint == b.paint
        && [0, 2].iter().all(|axis| {
            let ax = at.0.get(*axis).copied().unwrap_or(0.0);
            let ay = at.0.get(axis + 1).copied().unwrap_or(0.0);
            let bx = bt.0.get(*axis).copied().unwrap_or(0.0);
            let by = bt.0.get(axis + 1).copied().unwrap_or(0.0);
            let al = ax.hypot(ay);
            let bl = bx.hypot(by);
            al > 0.0 && bl > 0.0 && (ax / al - bx / bl).abs() < 0.001 && (ay / al - by / bl).abs() < 0.001
        })
}

pub(super) fn row_rect(row: &[usize], lines: &[TextLine]) -> Option<[f64; 4]> {
    row.iter().filter_map(|i| lines.get(*i)).map(|l| l.rect).reduce(union)
}

/// [`row_rect`] in its text's own frame ([`TextLine::frame_rect`]), so rows of turned text
/// compare along and across their baselines.
fn frame_row_rect(row: &[usize], lines: &[TextLine]) -> Option<[f64; 4]> {
    row.iter().filter_map(|i| lines.get(*i)).map(TextLine::frame_rect).reduce(union)
}

/// The left edge the rest of the page's text sits on, ignoring the rows of `block`. A single row
/// that does not share it is not merely an indented or full-width left-aligned line, so its own
/// centre is the only remaining explanation for where it sits.
fn neighbour_left(lines: &[TextLine], block: &TextBlock) -> Option<f64> {
    let mut edges: HashMap<i64, usize> = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        if block.lines.contains(&i) {
            continue;
        }
        // Quarter-point buckets: equal edges found through different runs still agree.
        *edges.entry((line.rect[0] * 4.0).round() as i64).or_insert(0) += 1;
    }
    let (best, _) = edges.into_iter().max_by_key(|(_, count)| *count)?;
    Some(best as f64 / 4.0)
}

/// Whether a block reads as centred, and so must keep its centre when it grows or wraps.
///
/// Several ragged rows whose left edges disagree while their centres agree are centred. A one row
/// block cannot say that: all of its left edges are the same, so the ragged test never fires and a
/// centred title would be taken for left-aligned and grow to the right (#2581). Fall back to the
/// page instead — a lone row sitting on the page's own centre, and not on the edge the rest of the
/// text uses, is a centred title. A full-width left-aligned line still lines up with that edge, so
/// it is not mistaken for one.
pub(crate) fn is_centred(block: &TextBlock, lines: &[TextLine], page_centre: f64, size: f64) -> bool {
    let centre = (block.rect[0] + block.rect[2]) / 2.0;
    let lefts: Vec<_> = block.rows.iter().filter_map(|r| row_rect(r, lines).map(|r| r[0])).collect();
    if block.rows.len() > 1 {
        let left_span = lefts.iter().copied().fold(f64::NEG_INFINITY, f64::max) - lefts.iter().copied().fold(f64::INFINITY, f64::min);
        return left_span > size * 0.5
            && block.rows.iter().all(|row| row_rect(row, lines).is_some_and(|r| ((r[0] + r[2]) / 2.0 - centre).abs() < size));
    }
    lefts
        .first()
        .is_some_and(|left| (centre - page_centre).abs() < size && neighbour_left(lines, block).is_none_or(|edge| (*left - edge).abs() > size * 0.5))
}

/// Scripts written without spaces between words (Thai, Lao, Khmer, Myanmar): a line of them
/// wraps inside a phrase, so the next row continues it directly. Producers drop the space at a
/// wrap, which leaves no trace of the rarer wrap at a phrase space; joining directly keeps
/// every word whole.
pub(super) fn unspaced(c: char) -> bool {
    matches!(u32::from(c), 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF)
}

/// Whether two runs share a scope and a baseline direction (an oblique made by shearing the text
/// matrix stands on the same baseline as the upright text around it).
fn same_direction(a: &TextLine, b: &TextLine) -> bool {
    let at = Matrix(a.origin.tm).then(&Matrix(a.origin.ctm));
    let bt = Matrix(b.origin.tm).then(&Matrix(b.origin.ctm));
    let (al, bl) = (at.0[0].hypot(at.0[1]), bt.0[0].hypot(bt.0[1]));
    a.scope == b.scope && al > 0.0 && bl > 0.0 && (at.0[0] / al - bt.0[0] / bl).abs() < 0.001 && (at.0[1] / al - bt.0[1] / bl).abs() < 0.001
}

/// Whether Edit text offers every visual row as a region of its own ([`set_line_regions`]).
static LINE_REGIONS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Choose how Edit text groups a page's text. Off (the default), aligned rows form one visual
/// region: a heading with the lines under it, or neighbouring one-line paragraphs, are edited
/// together. On, every visual row is a region of its own, so one of them can be changed without
/// rewriting the others. The choice applies to every document of the process; block indexes
/// (`text_blocks`, `rewrite_block`, …) differ between the two, so change it only while no
/// paragraph is open for editing.
pub fn set_line_regions(on: bool) {
    LINE_REGIONS.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// The current choice of [`set_line_regions`].
pub fn line_regions() -> bool {
    LINE_REGIONS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn group_for_test(lines: &[TextLine], by_row: bool) -> Vec<TextBlock> {
    group_layout_as(lines, true, &[], by_row)
}

pub(crate) fn group_blocks(lines: &[TextLine]) -> Vec<TextBlock> {
    group_layout_as(lines, true, &[], line_regions())
}

/// Whether a vertical ruling line stands between `a` and the text `b` to its right.
pub(crate) fn ruled_apart(rules: &[[f64; 4]], a: [f64; 4], b: [f64; 4]) -> bool {
    let mid = (a[1].max(b[1]) + a[3].min(b[3])) / 2.0;
    rules.iter().any(|r| {
        let x = (r[0] + r[2]) / 2.0;
        r[3] - r[1] > r[2] - r[0] && x > a[2] - 1.0 && x < b[0] + 1.0 && r[1] <= mid && r[3] >= mid
    })
}

/// Whether a horizontal ruling line as wide as both rows runs between `upper` and `lower`.
fn ruled_below(rules: &[[f64; 4]], upper: [f64; 4], lower: [f64; 4]) -> bool {
    let (x0, x1) = (upper[0].min(lower[0]), upper[2].max(lower[2]));
    let mid_x = (x0 + x1) / 2.0;
    rules.iter().any(|r| {
        let y = (r[1] + r[3]) / 2.0;
        r[2] - r[0] > r[3] - r[1]
            && y < (upper[1] + upper[3]) / 2.0
            && y > (lower[1] + lower[3]) / 2.0
            && r[0] <= mid_x
            && r[2] >= mid_x
            && r[2] - r[0] >= (x1 - x0) * 0.9
    })
}

/// Exporters currently represent a block with one style. Keep their style boundaries until
/// they support rich runs, while edit selection may encompass the complete visual region.
/// Dotted-leader entries stay on one row and use the title run's style.
pub(crate) fn group_reading_blocks(lines: &[TextLine], rules: &[[f64; 4]]) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut offset = 0;
    for row in lines.chunk_by(|a, b| same_plane(a, b) && (a.origin.baseline - b.origin.baseline).abs() < a.size.min(b.size) * 0.2) {
        let mut text = String::new();
        for (i, line) in row.iter().enumerate() {
            if !text.ends_with(char::is_whitespace)
                && !line.text.starts_with(char::is_whitespace)
                && i.checked_sub(1).and_then(|i| row.get(i)).is_some_and(|prev| line.rect[0] - prev.rect[2] > line.size * 0.12)
            {
                text.push(' ');
            }
            text.push_str(&line.text);
        }
        if leader_entry(&text)
            && let Some(first) = row.first()
        {
            // Tab leaders are a single reading row, even when the title, dots and page
            // number are separate text objects far apart. Do not merge neighbouring entries.
            blocks.extend(group_layout(lines.get(start..offset).unwrap_or_default(), false, rules));
            let mut line = first.clone();
            line.text = text;
            line.rect = row.iter().map(|line| line.rect).reduce(union).unwrap_or(first.rect);
            blocks.extend(group_layout(std::slice::from_ref(&line), false, rules));
            start = offset + row.len();
        }
        offset += row.len();
    }
    blocks.extend(group_layout(lines.get(start..).unwrap_or_default(), false, rules));
    blocks
}

/// Dotted leaders end in a page number, with a title before the dots. A dot in an article
/// number, or an ellipsis in ordinary prose, is not enough to make a contents entry.
fn leader_entry(text: &str) -> bool {
    let text = text.trim_end();
    let prefix = text.trim_end_matches(|c: char| {
        c.is_ascii_digit() || matches!(c, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm' | 'I' | 'V' | 'X' | 'L' | 'C' | 'D' | 'M')
    });
    if prefix.len() == text.len() {
        return false;
    }
    let title = prefix.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '…'));
    let dots = prefix
        .get(title.len()..)
        .unwrap_or_default()
        .chars()
        .map(|c| match c {
            '.' => 1,
            '…' => 3,
            _ => 0,
        })
        .sum::<usize>();
    dots >= 3 && title.chars().any(char::is_alphabetic)
}

/// Text-space units → user space along the baseline. It is the vertical scale ([`Origin::k`])
/// unless the text matrix stretches the glyphs, as an OCR layer fitting each word to the scan does.
pub(super) fn across(line: &TextLine) -> f64 {
    let m = Matrix(line.origin.tm).then(&Matrix(line.origin.ctm)).0;
    let h = m[0].hypot(m[1]);
    if h.is_finite() && h > 1e-9 { h } else { line.origin.k }
}

/// Invisible text (rendering mode 3), as in an OCR layer over a scanned image.
fn invisible(line: &TextLine) -> bool {
    line.origin.state.render_mode == 3
}

fn same_style(a: &TextLine, b: &TextLine) -> bool {
    same_font(a, b.font.as_bytes(), &b.base_font, b.face.as_deref())
        // With no `/BaseFont` (or only a subset tag) there is no name to compare: the resource or
        // fallback face `same_font` matched is the whole identity, so keep the lines together.
        && (same_base_font(&a.base_font, &b.base_font)
            || (strip_subset_tag(&a.base_font).is_empty() && strip_subset_tag(&b.base_font).is_empty()))
        && a.bold == b.bold
        && a.italic == b.italic
        && a.color == b.color
        && (a.rotation() - b.rotation()).abs() < 0.01
        && (a.size - b.size).abs() < 0.01
}

/// Does a row open with a list marker? A bullet or a dash, or an ordered marker — digits (or a
/// single letter) followed by `.` or `)` and then a space or the row's end, so prose such as
/// `1.5` or `Dr. Smith` is left alone.
fn opens_with_marker(text: &str) -> bool {
    let Some(first) = text.chars().next() else { return false };
    if matches!(first, '•' | '◦' | '▪' | '‣' | '⁃' | '*' | '-' | '–' | '—') {
        return true;
    }
    if !first.is_ascii_digit() && !first.is_ascii_alphabetic() {
        return false;
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let letters = text.chars().take_while(char::is_ascii_alphabetic).count();
    // Both runs are ASCII characters, so their length in characters is their length in bytes:
    // the slice below starts on a character boundary.
    let rest = if digits > 0 {
        &text[digits..]
    } else if letters == 1 {
        &text[1..]
    } else {
        return false;
    };
    let mut after = rest.chars();
    matches!(after.next(), Some('.' | ')')) && after.next().is_none_or(char::is_whitespace)
}

/// A row that opens with a list marker opens its own item: joining it to the row above is what
/// let an edit of one paragraph delete every item after it (#2564). The other error, splitting
/// an ordinary paragraph, costs one more paragraph to click and never any text — so err this way.
fn starts_list_item(row: &[usize], lines: &[TextLine]) -> bool {
    row.iter().filter_map(|i| lines.get(*i)).map(|l| l.text.trim_start()).find(|t| !t.is_empty()).is_some_and(opens_with_marker)
}

fn group_layout(lines: &[TextLine], mixed: bool, rules: &[[f64; 4]]) -> Vec<TextBlock> {
    group_layout_as(lines, mixed, rules, false)
}

/// [`group_layout`]; with `by_row`, no row joins the block above it.
fn group_layout_as(lines: &[TextLine], mixed: bool, rules: &[[f64; 4]], by_row: bool) -> Vec<TextBlock> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let joins = rows.last().and_then(|r| r.last()).and_then(|i| lines.get(*i)).is_some_and(|prev| {
            let gap = line.frame_rect()[0] - prev.frame_rect()[2];
            // OCR words (invisible text over a scan) join only across a word space: a wider gap
            // is a table cell or a tab stop, which a rewrite would close up.
            let scanned = !mixed || (invisible(prev) == invisible(line) && (!invisible(line) || gap < prev.size.max(line.size) * 0.6));
            scanned
                && (mixed || same_style(prev, line))
                // An oblique made by shearing stands in the sentence of the upright (or font-slanted)
                // text beside it.
                && (same_plane(prev, line) || (prev.italic || line.italic) && same_direction(prev, line))
                && !ruled_apart(rules, prev.rect, line.rect)
                // A superscript or subscript run (a footnote marker, say) sits on the same visual
                // row as the text around it, raised or lowered by `Ts` from the body baseline
                // (#3252). That shift is typically up to half the body size, not the marker's own
                // smaller size — but the wider tolerance only applies between a run and a
                // distinctly smaller or larger one, with no horizontal overlap (`gap >= 0.0`):
                // two equal-size runs can land within it too, at a baseline shift tight columns
                // or compressed text (`Tz`) can produce between genuinely separate rows, and the
                // original 20% tolerance is what keeps those apart.
                && ((prev.frame_origin().1 - line.frame_origin().1).abs() < prev.size.min(line.size) * 0.2
                    || (gap >= 0.0
                        && prev.size.min(line.size) < prev.size.max(line.size) * 0.9
                        && (prev.frame_origin().1 - line.frame_origin().1).abs() < prev.size.max(line.size) * 0.5))
                // A slanted run's box leans over the start of the upright run after it.
                && gap >= -prev.size.min(line.size) * if prev.italic && !same_plane(prev, line) { 0.5 } else { 0.2 }
                && gap < prev.size.max(line.size) * 1.5
                // Separate same-style text objects that merely touch or overlap, with no space
                // between them, are separate labels (for example a paragraph dragged against
                // another, #594). Style runs and words separated by a space or gap still join.
                && (prev.origin.bt_op == line.origin.bt_op
                    || gap > prev.size.min(line.size) * 0.01
                    || !same_style(prev, line)
                    || prev.text.ends_with(char::is_whitespace)
                    || line.text.starts_with(char::is_whitespace))
        });
        if joins && let Some(row) = rows.last_mut() {
            row.push(i);
        } else {
            rows.push(vec![i]);
        }
    }
    let mut blocks: Vec<TextBlock> = Vec::new();
    // Whether the previous row's text ended in whitespace before trimming.
    let mut ends_spaced = false;
    for row in rows {
        let Some(first) = row.first().and_then(|i| lines.get(*i)) else { continue };
        let (Some(rect), Some(frame)) = (row_rect(&row, lines), frame_row_rect(&row, lines)) else { continue };
        let joins = !by_row
            && blocks.last().is_some_and(|block| {
                // A bullet of its own opens a list item (#2564): such a row is a new paragraph, and
                // letting it join is what made an edit of the row above delete this item and the ones
                // after it.
                if starts_list_item(&row, lines) {
                    return false;
                }
                let Some(prev_row) = block.rows.last() else { return false };
                let Some(prev) = prev_row.first().and_then(|i| lines.get(*i)) else { return false };
                let (Some(pr), Some(prev_rect)) = (frame_row_rect(prev_row, lines), row_rect(prev_row, lines)) else { return false };
                let gap = prev.frame_origin().1 - first.frame_origin().1;
                // A block's rows share one leading. A clearly wider gap after two or more rows of
                // one size is paragraph spacing: the next paragraph starts its own block. (Rows of
                // growing sizes, as in a title card, are spaced by their sizes instead.)
                let same_size = |l: &TextLine| (l.size - prev.size).abs() < 0.01 && (l.size - first.size).abs() < 0.01;
                let leading = block
                    .rows
                    .len()
                    .checked_sub(2)
                    .and_then(|i| block.rows.get(i))
                    .and_then(|r| r.first())
                    .and_then(|i| lines.get(*i))
                    .filter(|l| same_size(l))
                    .map(|l| l.frame_origin().1 - prev.frame_origin().1);
                if leading.is_some_and(|l| l > 0.0 && gap > l * 1.25 + 0.5) {
                    return false;
                }
                let tol = prev.size.max(first.size).max(1.0);
                // The block opens with a list marker and this row sits further right: the item's text
                // is at a hanging indent, so the row it wrapped onto continues it (#2564). A row at
                // the block's own left edge is already covered by the match on `pr[0]`. Both boxes are
                // in the frame, as the rest of this comparison is, so turned text matches too.
                let hanging = frame[0] > pr[0]
                    && frame[0] - pr[0] <= tol * 3.0
                    && block.lines.first().and_then(|&i| lines.get(i)).is_some_and(|l| opens_with_marker(l.text.trim_start()));
                let aligned = (pr[0] - frame[0]).abs() < tol
                    || ((pr[0] + pr[2] - frame[0] - frame[2]) / 2.0).abs() < tol
                    || (pr[2] - frame[2]).abs() < tol
                    || hanging;
                // OCR lines stay apart: the scan fixes where each one is, so they never rewrap together.
                (!mixed || !(invisible(prev) || invisible(first)))
                && (mixed || same_style(prev, first))
                && (same_plane(prev, first) || mixed && (prev.italic || first.italic) && same_direction(prev, first))
                && !ruled_below(rules, prev_rect, rect)
                // The rewrite sets every row in the first one's horizontal scale.
                && (prev.origin.state.scale - first.origin.state.scale).abs() < 1e-6
                && aligned
                && first.size.min(prev.size) >= first.size.max(prev.size) * 0.4
                && gap > first.size.min(prev.size) * 0.8
                && gap < first.size.max(prev.size) * 2.5
            });
        if !joins {
            blocks.push(TextBlock {
                text: String::new(),
                editor_text: String::new(),
                spans: Vec::new(),
                rect,
                base_font: first.base_font.clone(),
                size: first.size,
                bold: first.bold,
                italic: first.italic,
                color: first.color,
                rotation: first.rotation(),
                lines: Vec::new(),
                rows: Vec::new(),
                marks: Vec::new(),
                invisible: true,
            });
        }
        let Some(block) = blocks.last_mut() else { continue };
        let mut row_text = String::new();
        // Where each line of this row starts in `row_text`.
        let mut row_marks: Vec<(usize, usize)> = Vec::new();
        if !block.rows.is_empty() {
            block.editor_text.push('\n');
        }
        for &index in &row {
            let Some(line) = lines.get(index) else { continue };
            // Some producers position separate words without encoding a space.
            if let Some(prev) = block.lines.last().filter(|i| row.contains(i)).and_then(|i| lines.get(*i))
                && !row_text.ends_with(char::is_whitespace)
                && !line.text.starts_with(char::is_whitespace)
                && line.frame_rect()[0] - prev.frame_rect()[2] > line.size * 0.12
            {
                row_text.push(' ');
                block.editor_text.push(' ');
            }
            let start = block.editor_text.len();
            block.editor_text.push_str(&line.text);
            block.spans.push((start..block.editor_text.len(), index));
            row_marks.push((row_text.len(), index));
            row_text.push_str(&line.text);
            block.invisible &= line.origin.state.render_mode == 3;
            block.lines.push(index);
        }
        // A producer that keeps the space it wrapped at (Word does; Chrome drops it) says so here.
        let spaced = row_text.starts_with(paragraph_separator) || std::mem::replace(&mut ends_spaced, row_text.ends_with(paragraph_separator));
        let lead = row_text.len() - row_text.trim_start_matches(paragraph_separator).len();
        let row_text = row_text.trim_matches(paragraph_separator);
        if !block.text.is_empty() {
            if block.text.ends_with('-') {
                block.text.pop();
            } else if spaced || !(block.text.ends_with(unspaced) && row_text.starts_with(unspaced)) {
                block.text.push(' ');
            }
        }
        let base = block.text.len();
        block.marks.extend(row_marks.into_iter().map(|(at, index)| (base + at.saturating_sub(lead).min(row_text.len()), index)));
        block.text.push_str(row_text);
        block.rect = union(block.rect, rect);
        block.rows.push(row);
    }
    blocks
}
