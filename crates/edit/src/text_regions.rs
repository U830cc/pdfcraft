//! Geometry-based regions and mixed-style rewriting of existing text.

use super::*;

fn same_plane(a: &TextLine, b: &TextLine) -> bool {
    let at = Matrix(a.origin.tm).then(&Matrix(a.origin.ctm));
    let bt = Matrix(b.origin.tm).then(&Matrix(b.origin.ctm));
    a.scope == b.scope
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

/// Scripts written without spaces between words (Thai, Lao, Khmer, Myanmar): a line of them
/// wraps inside a phrase, so the next row continues it directly. Producers drop the space at a
/// wrap, which leaves no trace of the rarer wrap at a phrase space; joining directly keeps
/// every word whole.
fn unspaced(c: char) -> bool {
    matches!(u32::from(c), 0x0E00..=0x0EFF | 0x1000..=0x109F | 0x1780..=0x17FF)
}

fn is_thai(c: char) -> bool {
    matches!(u32::from(c), 0x0E00..=0x0E7F)
}

/// Right-to-left letters and the bidirectional controls: text the Unicode bidirectional
/// algorithm may reorder.
fn reorders(c: char) -> bool {
    matches!(u32::from(c), 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF | 0x200E..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069 | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
}

/// This build has no Thai face to set edited Thai text in.
fn no_thai_font() -> EditError {
    EditError::Invalid(
        "editing Thai text needs PdfCraft's Thai font (Noto Sans Thai), which this build doesn't include \
         (to build it in, set CRAFT_FONTS_DIR to a craft-fonts checkout that has it)"
            .into(),
    )
}

/// Whether two runs share a scope and a baseline direction (an oblique made by shearing the text
/// matrix stands on the same baseline as the upright text around it).
fn same_direction(a: &TextLine, b: &TextLine) -> bool {
    let at = Matrix(a.origin.tm).then(&Matrix(a.origin.ctm));
    let bt = Matrix(b.origin.tm).then(&Matrix(b.origin.ctm));
    let (al, bl) = (at.0[0].hypot(at.0[1]), bt.0[0].hypot(bt.0[1]));
    a.scope == b.scope && al > 0.0 && bl > 0.0 && (at.0[0] / al - bt.0[0] / bl).abs() < 0.001 && (at.0[1] / al - bt.0[1] / bl).abs() < 0.001
}

fn union(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

fn row_rect(row: &[usize], lines: &[TextLine]) -> Option<[f64; 4]> {
    row.iter().filter_map(|i| lines.get(*i)).map(|l| l.rect).reduce(union)
}

pub(super) fn group_blocks(lines: &[TextLine]) -> Vec<TextBlock> {
    group_layout(lines, true, &[])
}

/// Whether a vertical ruling line stands between `a` and the text `b` to its right.
pub(super) fn ruled_apart(rules: &[[f64; 4]], a: [f64; 4], b: [f64; 4]) -> bool {
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

/// Export blocks: one paragraph each, of one size. Runs of a sentence in another weight, slant
/// or colour stay in it ([`TextBlock::style_runs`]); a heading or a line in another size does not.
pub(super) fn group_reading_blocks(lines: &[TextLine], rules: &[[f64; 4]]) -> Vec<TextBlock> {
    group_layout(lines, false, rules)
}

/// Two runs of one sentence in different styles (weight, slant, colour or face) at one size.
/// Export keeps them in one paragraph as separate runs.
fn inline_run(a: &TextLine, b: &TextLine) -> bool {
    (a.size - b.size).abs() < 0.01 && (a.rotation() - b.rotation()).abs() < 0.01
}

fn same_style(a: &TextLine, b: &TextLine) -> bool {
    same_font(a, b.font.as_bytes(), b.face.as_deref())
        && a.base_font == b.base_font
        && a.bold == b.bold
        && a.italic == b.italic
        && a.color == b.color
        && (a.rotation() - b.rotation()).abs() < 0.01
        && (a.size - b.size).abs() < 0.01
}

fn group_layout(lines: &[TextLine], mixed: bool, rules: &[[f64; 4]]) -> Vec<TextBlock> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let joins = rows.last().and_then(|r| r.last()).and_then(|i| lines.get(*i)).is_some_and(|prev| {
            let gap = line.rect[0] - prev.rect[2];
            (mixed || same_style(prev, line) || inline_run(prev, line))
                && (same_plane(prev, line) || !mixed && prev.italic != line.italic && same_direction(prev, line))
                && !ruled_apart(rules, prev.rect, line.rect)
                && (prev.origin.baseline - line.origin.baseline).abs() < prev.size.min(line.size) * 0.2
                // A slanted run's box leans over the start of the upright run after it.
                && gap >= -prev.size.min(line.size) * if !mixed && prev.italic && !line.italic { 0.5 } else { 0.2 }
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
        let Some(rect) = row_rect(&row, lines) else { continue };
        let joins = blocks.last().is_some_and(|block| {
            let Some(prev_row) = block.rows.last() else { return false };
            let Some(prev) = prev_row.first().and_then(|i| lines.get(*i)) else { return false };
            let Some(pr) = row_rect(prev_row, lines) else { return false };
            let gap = prev.origin.baseline - first.origin.baseline;
            // A block's rows share one leading. A clearly wider gap after two or more rows is
            // paragraph spacing: the next paragraph starts its own block.
            let leading = block
                .rows
                .len()
                .checked_sub(2)
                .and_then(|i| block.rows.get(i))
                .and_then(|r| r.first())
                .and_then(|i| lines.get(*i))
                .map(|l| l.origin.baseline - prev.origin.baseline);
            if leading.is_some_and(|l| l > 0.0 && gap > l * 1.25 + 0.5) {
                return false;
            }
            let tol = prev.size.max(first.size).max(1.0);
            let aligned = (pr[0] - rect[0]).abs() < tol || ((pr[0] + pr[2] - rect[0] - rect[2]) / 2.0).abs() < tol || (pr[2] - rect[2]).abs() < tol;
            // A row of several styles (a bold word in a sentence) is body text: it continues the
            // paragraph above, and the paragraph continues under it, at the same size.
            let several = |r: &[usize]| {
                r.first().and_then(|i| lines.get(*i)).is_some_and(|a| r.iter().filter_map(|i| lines.get(*i)).any(|b| !same_style(a, b)))
            };
            let continues = !mixed && (prev.size - first.size).abs() < 0.01 && (several(prev_row) || several(&row));
            (mixed || same_style(prev, first) || continues)
                && (same_plane(prev, first) || continues && same_direction(prev, first))
                && !ruled_below(rules, pr, rect)
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
                marks: Vec::new(),
                rect,
                base_font: first.base_font.clone(),
                size: first.size,
                bold: first.bold,
                italic: first.italic,
                color: first.color,
                rotation: first.rotation(),
                lines: Vec::new(),
                rows: Vec::new(),
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
                && line.rect[0] - prev.rect[2] > line.size * 0.12
            {
                row_text.push(' ');
                block.editor_text.push(' ');
            }
            let start = block.editor_text.len();
            block.editor_text.push_str(&line.text);
            block.spans.push((start..block.editor_text.len(), index));
            row_marks.push((row_text.len(), index));
            row_text.push_str(&line.text);
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

/// Shape in the source or an explicitly registered matching full face. Never use interface fonts.
fn shape_source(
    doc: &Document,
    fonts: &Dict,
    name: &str,
    text: &str,
) -> Result<Option<(Vec<u8>, pdfcraft_fonts::source_shaping::ShapedRun, String)>, EditError> {
    if !pdfcraft_fonts::source_shaping::required(text) {
        return Ok(None);
    }
    let source =
        fonts.get(name.as_bytes()).and_then(|f| doc.dict(f)).ok_or_else(|| EditError::Invalid("the shaped text has no font resource".into()))?;
    let family = Metrics::from_dict(doc, &source).base_font;
    let mut candidates = vec![source];
    candidates.extend(
        fonts
            .iter()
            .filter_map(|(_, f)| doc.dict(f))
            .filter(|f| pdfcraft_fonts::embedding::face_key(&Metrics::from_dict(doc, f).base_font) == pdfcraft_fonts::embedding::face_key(&family)),
    );
    for font in candidates {
        if let Some(bytes) = pdfcraft_fonts::pdf::embedded_sfnt(doc, &font)
            && pdfcraft_fonts::embedding::covers(&bytes, text)
            && let Ok(shaped) = pdfcraft_fonts::source_shaping::shape(&bytes, text)
        {
            return Ok(Some((bytes, shaped, family)));
        }
    }
    Err(EditError::Invalid("complex text needs the complete source TrueType/OpenType face; load that font before editing".into()))
}

/// Rewrite visual rows while keeping the source style on each surviving character. Resource
/// names may differ between Unicode subsets of the same face; encoding searches those subsets.
pub(super) fn rewrite_styled_block(
    doc: &mut Document,
    page: usize,
    block: &TextBlock,
    lines: &[TextLine],
    text: Option<&str>,
    style: &BlockStyle,
) -> Result<LineEdit, EditError> {
    let first = block.lines.first().and_then(|i| lines.get(*i)).ok_or_else(|| EditError::Invalid("the paragraph has no source runs".into()))?;
    let draft = match text {
        Some(t) if t != block.text => t,
        _ => &block.editor_text,
    };
    if draft.len() > 4_000_000 {
        return Err(EditError::Invalid("the replacement text is too large".into()));
    }
    if draft == block.editor_text && *style == BlockStyle::default() {
        return Ok(LineEdit { substituted: None });
    }
    let mut ranges: Vec<_> = style.ranges.iter().collect();
    ranges.sort_by_key(|r| r.range.start);
    if ranges.len() > 1024
        || ranges.iter().any(|r| {
            r.range.start >= r.range.end
                || draft.get(r.range.clone()).is_none()
                || [r.size, r.char_spacing, r.scale].iter().flatten().any(|n| !n.is_finite())
                || r.size.is_some_and(|n| n <= 0.0 || n > 14400.0)
                || r.color.is_some_and(|c| c.iter().any(|n| !n.is_finite() || !(0.0..=1.0).contains(n)))
        })
        || ranges.windows(2).any(|r| r[0].range.end > r[1].range.start)
    {
        return Err(EditError::Invalid("text style ranges must be disjoint UTF-8 boundaries with finite formatting (at most 1024 ranges)".into()));
    }
    let mut styled_lines = lines.to_vec();
    let mut styled_spans = Vec::new();
    let mut underlines = HashMap::new();
    let mut overrides = HashMap::new();
    for (span, source) in block.edited_spans(draft) {
        let mut edges = vec![span.start, span.end];
        for r in &ranges {
            for edge in [r.range.start, r.range.end] {
                if span.contains(&edge) {
                    edges.push(edge);
                }
            }
        }
        edges.sort_unstable();
        edges.dedup();
        for bounds in edges.windows(2) {
            let (Some(start), Some(end)) = (bounds.first().copied(), bounds.get(1).copied()) else { continue };
            let mut index = source;
            let selected = ranges.iter().find(|r| r.range.contains(&start)).copied();
            if selected.is_some() || style.family.is_some() || style.bold.is_some() {
                let default_range = RangeStyle {
                    range: start..end,
                    family: None,
                    resource: None,
                    size: None,
                    color: None,
                    underline: None,
                    char_spacing: None,
                    scale: None,
                };
                let r = selected.unwrap_or(&default_range);
                let mut line = lines.get(source).unwrap_or(first).clone();
                if let Some((family, bold, italic)) =
                    r.family.or(style.family).or_else(|| style.bold.map(|b| (source_family(&line.base_font), b, line.italic)))
                {
                    let base = family.base_font(bold, italic);
                    line.font = format!("PCRange{}", base.replace('-', ""));
                    line.base_font = base.to_owned();
                    line.bold = bold;
                    line.italic = italic;
                }
                if let Some(resource) = &r.resource {
                    line.font = resource.clone();
                }
                if let Some(size) = r.size {
                    line.size = size;
                }
                if let Some(color) = r.color {
                    line.color = color;
                    line.origin.state.fill = vec![Op::new("rg", color.map(Object::Real).to_vec())];
                }
                if let Some(spacing) = r.char_spacing {
                    line.origin.state.char_spacing = spacing / line.origin.k.max(1e-6);
                }
                if let Some(scale) = r.scale {
                    line.origin.state.scale = (scale / 100.0).clamp(0.1, 10.0);
                }
                index = styled_lines.len();
                styled_lines.push(line);
                if let Some(r) = selected {
                    overrides.insert(index, r);
                }
                if let Some(underline) = r.underline {
                    underlines.insert(index, underline);
                }
            }
            styled_spans.push((start..end, index));
        }
    }
    let lines = styled_lines.as_slice();
    let p = page_dict(doc, page)?;
    let mut res = p.dict.get(b"Resources").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
    let mut fonts = res.get(b"Font").and_then(|r| doc.resolve(r).as_dict().cloned()).unwrap_or_default();
    for line in lines.iter().filter(|line| line.font.starts_with("PCRange")) {
        let mut f = Dict::new();
        f.set(b"Type".to_vec(), Object::name("Font"));
        f.set(b"Subtype".to_vec(), Object::name("Type1"));
        f.set(b"BaseFont".to_vec(), Object::name(&line.base_font));
        f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
        fonts.set(line.font.as_bytes().to_vec(), Object::Dict(f));
    }
    let mut metrics = std::collections::BTreeMap::new();
    for (name, object) in fonts.iter().take(256) {
        if let Some(font) = doc.resolve(object).as_dict() {
            metrics.insert(String::from_utf8_lossy(name).into_owned(), Metrics::from_dict(doc, font));
        }
    }
    for index in &block.lines {
        if let Some(line) = lines.get(*index)
            && !metrics.contains_key(&line.font)
            && let Some(font) = fonts.get(line.font.as_bytes()).and_then(|f| doc.resolve(f).as_dict().cloned())
        {
            metrics.insert(line.font.clone(), Metrics::from_dict(doc, &font));
        }
    }
    if [style.size, style.line_spacing, style.char_spacing, style.scale, style.width].iter().flatten().any(|v| !v.is_finite())
        || style.color.is_some_and(|c| c.iter().any(|v| !v.is_finite()))
    {
        return Err(EditError::Invalid("the paragraph's formatting must use finite numbers".into()));
    }
    let [dx, dy] = style.offset.unwrap_or([0.0, 0.0]);
    if !dx.is_finite() || !dy.is_finite() || style.width.is_some_and(|w| !w.is_finite()) {
        return Err(EditError::Invalid("the paragraph's position and width must be finite".into()));
    }
    let width = style.width.unwrap_or(block.rect[2] - block.rect[0]).clamp(1.0, 14_400.0);
    // Each tuple is a character, its source style and an encodable resource name.
    let mut chars = Vec::new();
    let mut substituted = None;
    let mut fallback_names: std::collections::BTreeMap<(String, char), String> = std::collections::BTreeMap::new();
    let mut full_faces: HashMap<String, String> = HashMap::new();
    // Thai needs its marks positioned by the font's own tables. A source subset rarely keeps them
    // (or the letters the edit adds), so Thai it cannot shape is set in the Thai document face,
    // embedded once: every Thai letter of the paragraph then shares one face and stacks correctly.
    let thai_sample: String = draft.chars().filter(|c| is_thai(*c)).collect();
    let mut source_shapes_thai: HashMap<String, bool> = HashMap::new();
    for (range, index) in styled_spans {
        let source = lines.get(index).unwrap_or(first);
        for ch in draft.get(range).unwrap_or_default().chars() {
            if ch == '\n' {
                chars.push((ch, index, source.font.clone()));
                continue;
            }
            let s = ch.to_string();
            let chosen = overrides.get(&index).and_then(|r| r.resource.as_ref()).or(style.resource.as_ref());
            let thai_face = if is_thai(ch) && chosen.is_none() {
                let shapes = *source_shapes_thai.entry(source.font.clone()).or_insert_with(|| {
                    fonts.get(source.font.as_bytes()).and_then(|f| doc.dict(f)).and_then(|f| pdfcraft_fonts::pdf::embedded_sfnt(doc, &f)).is_some_and(
                        |bytes| {
                            pdfcraft_fonts::embedding::covers(&bytes, &thai_sample)
                                && pdfcraft_fonts::source_shaping::shape(&bytes, &thai_sample).is_ok()
                        },
                    )
                });
                if shapes {
                    None
                } else {
                    Some((
                        pdfcraft_fonts::document_thai_font().ok_or_else(no_thai_font)?,
                        pdfcraft_fonts::document_thai_program().ok_or_else(no_thai_font)?,
                    ))
                }
            } else {
                None
            };
            let name = if let Some((face, program)) = thai_face {
                if !pdfcraft_fonts::embedding::covers(program, &s) {
                    return Err(EditError::Invalid(format!("the {} font has no glyph for U+{:04X}", face.family, u32::from(ch))));
                }
                let key = face.name();
                if let Some(name) = full_faces.get(&key) {
                    name.clone()
                } else {
                    let name = pdfcraft_fonts::embedding::embed(doc, &mut fonts, program, face.family).map_err(EditError::Invalid)?;
                    let font = fonts
                        .get(name.as_bytes())
                        .and_then(|f| doc.dict(f))
                        .ok_or_else(|| EditError::Invalid("the Thai font could not be loaded".into()))?;
                    metrics.insert(name.clone(), Metrics::from_dict(doc, &font));
                    full_faces.insert(key, name.clone());
                    if !source.base_font.contains(&face.family.replace(' ', "")) {
                        substituted = Some(face.family.to_string());
                    }
                    name
                }
            } else if let Some(resource) = chosen {
                if !metrics.get(resource).is_some_and(|m| source_encodes(m, &s)) {
                    return Err(EditError::Invalid(format!("the selected font resource {resource} cannot show U+{:04X}", u32::from(ch))));
                }
                resource.clone()
            } else if metrics.get(&source.font).is_some_and(|m| source_encodes(m, &s)) {
                source.font.clone()
            } else if let Some((name, _)) = metrics.iter().find(|(_, m)| {
                pdfcraft_fonts::embedding::face_key(&m.base_font) == pdfcraft_fonts::embedding::face_key(&source.base_font) && source_encodes(m, &s)
            }) {
                name.clone()
            } else if let Some(face) =
                pdfcraft_fonts::embedding::matching_faces(&source.base_font).into_iter().find(|f| pdfcraft_fonts::embedding::covers(f.bytes, &s))
            {
                let key = face.name();
                if let Some(name) = full_faces.get(&key) {
                    name.clone()
                } else {
                    let name = pdfcraft_fonts::embedding::embed(doc, &mut fonts, face.bytes, face.family).map_err(EditError::Invalid)?;
                    let font = fonts
                        .get(name.as_bytes())
                        .and_then(|f| doc.dict(f))
                        .ok_or_else(|| EditError::Invalid("the full source font could not be loaded".into()))?;
                    metrics.insert(name.clone(), Metrics::from_dict(doc, &font));
                    full_faces.insert(key, name.clone());
                    name
                }
            } else {
                let family = source_family(&source.base_font);
                if needs_type3(&s) {
                    let key = (source.base_font.rsplit('+').next().unwrap_or(&source.base_font).to_owned(), ch);
                    if let Some(name) = fallback_names.get(&key) {
                        chars.push((ch, index, name.clone()));
                        continue;
                    }
                    if fallback_names.len() >= MAX_TYPE3_GLYPHS {
                        return Err(EditError::Invalid("the replacement has too many fallback glyphs".into()));
                    }
                    let fallback = type3_font(doc, &mut fonts, &s, family, source.bold, MAX_TYPE3_GLYPHS)?;
                    let name = fallback.names.first().cloned().ok_or_else(no_japanese_font)?;
                    let dict = fonts
                        .get(name.as_bytes())
                        .and_then(|f| doc.resolve(f).as_dict().cloned())
                        .ok_or_else(|| EditError::Invalid("the fallback font could not be loaded".into()))?;
                    metrics.insert(name.clone(), Metrics::from_dict(doc, &dict));
                    substituted = Some(fallback.family.to_string());
                    fallback_names.insert(key, name.clone());
                    name
                } else {
                    let base = family.base_font(source.bold, source.italic);
                    let name = format!("PCEd{}", base.replace('-', ""));
                    let mut f = Dict::new();
                    f.set(b"Type".to_vec(), Object::name("Font"));
                    f.set(b"Subtype".to_vec(), Object::name("Type1"));
                    f.set(b"BaseFont".to_vec(), Object::name(base));
                    f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
                    metrics.entry(name.clone()).or_insert_with(|| Metrics::from_dict(doc, &f));
                    fonts.set(name.clone().into_bytes(), Object::Dict(f));
                    substituted = Some(base.to_string());
                    name
                }
            };
            chars.push((ch, index, name));
        }
    }
    let size_of = |source: &TextLine, index: usize| {
        overrides.get(&index).and_then(|r| r.size).or(style.size).unwrap_or(source.size).max(0.1) / source.origin.k.max(1e-6)
    };
    // A source run may carry TJ kerning/justification that is absent from its text state.
    // Keep its measured advance when rebuilding the run instead of making it wrap on entry.
    let mut calibration = HashMap::new();
    for (index, source) in lines.iter().enumerate() {
        if let Some(m) = metrics.get(&source.font) {
            let nominal = source_width(
                m,
                &source.text,
                source.origin.state.font.as_ref().map_or(1.0, |(_, s)| *s),
                source_space(doc, &fonts, m),
                source.origin.state.char_spacing,
                source.origin.state.word_spacing,
            ) * source.origin.state.scale
                * source.origin.k;
            // Shaped source text (Thai marks, ligatures) doesn't advance by its characters'
            // widths, so its nominal width says nothing about kerning.
            let changed_face = style.resource.is_some()
                || overrides.get(&index).is_some_and(|r| r.family.is_some() || r.resource.is_some())
                || pdfcraft_fonts::source_shaping::required(&source.text);
            let factor = if nominal > 0.0 && !changed_face { ((source.rect[2] - source.rect[0]) / nominal).clamp(0.5, 2.0) } else { 1.0 };
            calibration.insert(index, factor);
        }
    }
    let advance = |item: &(char, usize, String)| -> f64 {
        let source = lines.get(item.1).unwrap_or(first);
        let Some(m) = metrics.get(&item.2) else { return 0.0 };
        source_width(
            m,
            &item.0.to_string(),
            size_of(source, item.1),
            source_space(doc, &fonts, m),
            overrides
                .get(&item.1)
                .and_then(|r| r.char_spacing)
                .or(style.char_spacing)
                .map_or(source.origin.state.char_spacing, |c| c / source.origin.k.max(1e-6)),
            source.origin.state.word_spacing,
        ) * overrides.get(&item.1).and_then(|r| r.scale).or(style.scale).map_or(source.origin.state.scale, |s| (s / 100.0).clamp(0.1, 10.0))
            * source.origin.k
            // The source run's own font only: a substituted face has its own advances.
            * if item.2 == source.font { calibration.get(&item.1).copied().unwrap_or(1.0) } else { 1.0 }
    };
    let measure = |items: &[(char, usize, String)]| -> Result<f64, EditError> {
        let mut width = 0.0;
        let mut at = 0;
        while let Some(item) = items.get(at) {
            let mut end = at + 1;
            while items.get(end).is_some_and(|v| v.1 == item.1 && v.2 == item.2) {
                end += 1;
            }
            let segment = items.get(at..end).unwrap_or_default();
            let string: String = segment.iter().map(|v| v.0).collect();
            if let Some((_, shaped, _)) = shape_source(doc, &fonts, &item.2, &string)? {
                let source = lines.get(item.1).unwrap_or(first);
                let scale =
                    overrides.get(&item.1).and_then(|r| r.scale).or(style.scale).map_or(source.origin.state.scale, |s| (s / 100.0).clamp(0.1, 10.0));
                let spacing = overrides
                    .get(&item.1)
                    .and_then(|r| r.char_spacing)
                    .or(style.char_spacing)
                    .map_or(source.origin.state.char_spacing, |s| s / source.origin.k.max(1e-6));
                width += (shaped.advance * size_of(source, item.1)
                    + spacing * string.chars().count() as f64
                    + source.origin.state.word_spacing * string.matches(' ').count() as f64)
                    * scale
                    * source.origin.k;
            } else {
                width += segment.iter().map(&advance).sum::<f64>();
            }
            at = end;
        }
        Ok(width)
    };
    let mut rows: Vec<Vec<(char, usize, String)>> = Vec::new();
    let mut row = Vec::new();
    let mut row_width = 0.0;
    // Thai has no spaces between words: its wrap points come from the language model. Indexes
    // into `chars` where a Thai word ends and another character follows.
    let mut thai_breaks = std::collections::HashSet::new();
    if chars.iter().any(|c| is_thai(c.0)) {
        let all: String = chars.iter().map(|c| c.0).collect();
        let (mut bytes, mut count) = (0usize, 0usize);
        let mut ends = all.char_indices().map(|(i, c)| i + c.len_utf8());
        for piece in pdfcraft_fonts::thai_break::wrap_pieces(&all) {
            bytes = bytes.saturating_add(piece.len());
            while ends.next().is_some_and(|end| {
                count += 1;
                end < bytes
            }) {}
            if piece.chars().any(is_thai) || chars.get(count).is_some_and(|c| is_thai(c.0)) {
                thai_breaks.insert(count);
            }
        }
    }
    // Wrap at word boundaries, retaining explicit source row breaks and whitespace.
    let mut at = 0;
    while at < chars.len() {
        let Some(item) = chars.get(at) else { break };
        if item.0 == '\n' {
            rows.push(std::mem::take(&mut row));
            row_width = 0.0;
            at += 1;
            continue;
        }
        let mut end = at + 1;
        if !item.0.is_whitespace() {
            while chars.get(end).is_some_and(|c| !c.0.is_whitespace()) && !thai_breaks.contains(&end) {
                end += 1;
            }
        }
        let word = chars.get(at..end).unwrap_or_default();
        let w = measure(word)?;
        if !row.is_empty() && !item.0.is_whitespace() && row_width + w > width + 0.5 {
            while row.last().is_some_and(|c: &(char, usize, String)| c.0.is_whitespace()) {
                row.pop();
            }
            rows.push(std::mem::take(&mut row));
            row_width = 0.0;
        }
        row.extend_from_slice(word);
        row_width += w;
        at = end;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    for row in &rows {
        let text: String = row.iter().map(|v| v.0).collect();
        // Right-to-left text is reordered across the whole row, which separate runs cannot do.
        // Left-to-right shaped text (Thai, combining marks) shapes within each run.
        if text.chars().any(reorders) && row.first().is_some_and(|first| row.iter().any(|v| v.1 != first.1 || v.2 != first.2)) {
            return Err(EditError::Invalid(
                "contextual/bidi text across font or style boundaries needs a uniform full-face row; mixed contextual runs are not yet supported"
                    .into(),
            ));
        }
    }
    let measured: Vec<_> = rows.iter().map(|r| measure(r)).collect::<Result<_, _>>()?;
    let mut shaped_runs = HashMap::new();
    let mut segment_advances = HashMap::new();
    for (row_index, row) in rows.iter().enumerate() {
        let mut at = 0;
        while let Some(item) = row.get(at) {
            let mut end = at + 1;
            while row.get(end).is_some_and(|v| v.1 == item.1 && v.2 == item.2) {
                end += 1;
            }
            segment_advances.insert((row_index, at), row.get(at..end).unwrap_or_default().iter().map(&advance).sum::<f64>());
            let string: String = row.get(at..end).unwrap_or_default().iter().map(|v| v.0).collect();
            if let Some((bytes, shaped, family)) = shape_source(doc, &fonts, &item.2, &string)? {
                shaped_runs.insert((row_index, at), (bytes, shaped, family));
            }
            at = end;
        }
    }
    let mut glyph_resources = HashMap::new();
    for (key, (bytes, shaped, family)) in &shaped_runs {
        let mappings: Vec<_> = shaped.glyphs.iter().map(|g| (g.text.clone(), g.id)).collect();
        let resource = pdfcraft_fonts::embedding::embed_mapping(doc, &mut fonts, bytes, family, &mappings).map_err(EditError::Invalid)?;
        glyph_resources.insert(*key, resource);
    }
    let n = pdfcraft_content::num;
    let mut block_ops = vec![Op::new("q", vec![])];
    let back = Matrix([first.origin.ctm[0], first.origin.ctm[1], first.origin.ctm[2], first.origin.ctm[3], 0.0, 0.0])
        .invert()
        .ok_or_else(|| EditError::Invalid("the paragraph is drawn in a space it cannot be edited in".into()))?;
    let (tx, ty) = back.apply(dx, dy);
    block_ops.push(Op::new("cm", vec![n(1.0), n(0.0), n(0.0), n(1.0), n(tx), n(ty)]));
    let centre = (block.rect[0] + block.rect[2]) / 2.0;
    let lefts: Vec<_> = block.rows.iter().filter_map(|r| row_rect(r, lines).map(|r| r[0])).collect();
    let left_span = lefts.iter().copied().fold(f64::NEG_INFINITY, f64::max) - lefts.iter().copied().fold(f64::INFINITY, f64::min);
    let centered = block.rows.len() > 1
        && left_span > first.size * 0.5
        && block.rows.iter().all(|row| row_rect(row, lines).is_some_and(|r| ((r[0] + r[2]) / 2.0 - centre).abs() < first.size));
    let source_leading = block
        .rows
        .windows(2)
        .filter_map(|rows| {
            let a = rows[0].first().and_then(|i| lines.get(*i))?;
            let b = rows[1].first().and_then(|i| lines.get(*i))?;
            Some((a.origin.baseline - b.origin.baseline).abs())
        })
        .rfind(|v| *v > 0.0)
        .unwrap_or(first.size * 1.5);
    let mut baseline = first.origin.baseline;
    for (row_index, row) in rows.iter().enumerate() {
        let anchor = block.rows.get(row_index).and_then(|r| r.first()).and_then(|i| lines.get(*i));
        if style.line_spacing.is_none()
            && style.size.is_none()
            && let Some(anchor) = anchor
        {
            baseline = anchor.origin.baseline;
        }
        let anchor = anchor.unwrap_or(first);
        let w = measured.get(row_index).copied().unwrap_or(0.0);
        let align = style.align.unwrap_or(if centered { crate::added::Align::Center } else { crate::added::Align::Left });
        let original_rect = block.rows.get(row_index).and_then(|row| row_rect(row, lines)).unwrap_or(block.rect);
        let left = if style.width.is_none() && style.align.is_none() { original_rect[0] } else { block.rect[0] };
        let available = if style.width.is_none() && style.align.is_none() { original_rect[2] - original_rect[0] } else { width };
        let mut x = left
            + match align {
                crate::added::Align::Center => (available - w) / 2.0,
                crate::added::Align::Right => available - w,
                _ => 0.0,
            };
        let mut pos = 0;
        while pos < row.len() {
            let Some(item) = row.get(pos) else { break };
            let source = lines.get(item.1).unwrap_or(first);
            let mut end = pos + 1;
            while row.get(end).is_some_and(|other| other.1 == item.1 && other.2 == item.2) {
                end += 1;
            }
            let segment = row.get(pos..end).unwrap_or_default();
            let string: String = segment.iter().map(|c| c.0).collect();
            let m = metrics.get(&item.2).ok_or_else(|| EditError::Invalid("the paragraph font is missing".into()))?;
            let mut state = source.origin.state.clone();
            state.font = Some((item.2.clone(), size_of(source, item.1)));
            if let Some(c) = overrides.get(&item.1).and_then(|r| r.char_spacing).or(style.char_spacing) {
                state.char_spacing = c / source.origin.k.max(1e-6);
            }
            if let Some(s) = overrides.get(&item.1).and_then(|r| r.scale).or(style.scale) {
                state.scale = (s / 100.0).clamp(0.1, 10.0);
            }
            if !shaped_runs.contains_key(&(row_index, pos)) && item.2 == source.font {
                state.scale *= calibration.get(&item.1).copied().unwrap_or(1.0);
            }
            if let Some([r, g, b]) = overrides.get(&item.1).and_then(|r| r.color).or(style.color) {
                state.fill = vec![Op::new("rg", vec![n(r), n(g), n(b)])];
            }
            let inverse = Matrix(first.origin.ctm).invert().ok_or_else(|| EditError::Invalid("the paragraph has a singular transform".into()))?;
            let mut tm = Matrix(source.origin.tm).then(&Matrix(source.origin.ctm)).then(&inverse).0;
            let (ax, ay) = back.apply(x - source.origin.x, baseline - source.origin.baseline);
            tm[4] += ax;
            tm[5] += ay;
            let mut segment_width = segment_advances.get(&(row_index, pos)).copied().unwrap_or(0.0);
            if let Some((_, shaped, _)) = shaped_runs.get(&(row_index, pos)) {
                let resource = glyph_resources.get(&(row_index, pos)).ok_or_else(|| EditError::Invalid("shaped font is missing".into()))?;
                let mut properties = Dict::new();
                properties.set(b"ActualText".to_vec(), Object::String(PdfString::text(&string)));
                block_ops.push(Op::new("BDC", vec![Object::name("Span"), Object::Dict(properties)]));
                state.font = Some((resource.clone(), size_of(source, item.1)));
                // Absolute glyph matrices preserve marks and contextual positions; font-program
                // codes are separate from Unicode and ToUnicode/ActualText keep logical text.
                let size = size_of(source, item.1);
                let mut extra = 0.0;
                for (i, glyph) in shaped.glyphs.iter().enumerate() {
                    let mut matrix = tm;
                    let x = (glyph.position[0] * size + extra) * state.scale;
                    let y = glyph.position[1] * size;
                    matrix[4] += matrix[0] * x + matrix[2] * y;
                    matrix[5] += matrix[1] * x + matrix[3] * y;
                    let code = u16::try_from(i + 1).map_err(|_| EditError::Invalid("shaped glyph limit".into()))?;
                    block_ops.push(Op::new("BT", vec![]));
                    block_ops.extend(state.ops());
                    block_ops.push(Op::new("Tm", matrix.map(n).to_vec()));
                    block_ops.push(Op::new("Tj", vec![Object::String(PdfString::literal(code.to_be_bytes().to_vec()))]));
                    block_ops.push(Op::new("ET", vec![]));
                    extra += state.char_spacing * glyph.text.chars().count() as f64 + state.word_spacing * glyph.text.matches(' ').count() as f64;
                }
                block_ops.push(Op::new("EMC", vec![]));
                segment_width = (shaped.advance * size + extra) * state.scale * source.origin.k;
            } else {
                block_ops.push(Op::new("BT", vec![]));
                block_ops.extend(state.ops());
                block_ops.push(Op::new("Tm", tm.iter().map(|v| n(*v)).collect()));
                block_ops.extend(source_show(
                    m,
                    &string,
                    size_of(source, item.1),
                    source_space(doc, &fonts, m),
                    state.char_spacing,
                    state.word_spacing,
                )?);
                block_ops.push(Op::new("ET", vec![]));
            }
            if underlines.get(&item.1).copied().or(style.underline) == Some(true) {
                let [r, g, b] = overrides.get(&item.1).and_then(|r| r.color).or(style.color).unwrap_or(source.color);
                let underline_size = size_of(source, item.1) * source.origin.k;
                let (x0, y0) = back.apply(x - first.origin.ctm[4], baseline - first.origin.ctm[5] - underline_size * 0.12);
                let (x1, y1) = back.apply(x + segment_width - first.origin.ctm[4], baseline - first.origin.ctm[5] - underline_size * 0.12);
                block_ops.push(Op::new("RG", vec![n(r), n(g), n(b)]));
                block_ops.push(Op::new("w", vec![n(underline_size * 0.06 / first.origin.ctm[2].hypot(first.origin.ctm[3]).max(1e-6))]));
                block_ops.push(Op::new("m", vec![n(x0), n(y0)]));
                block_ops.push(Op::new("l", vec![n(x1), n(y1)]));
                block_ops.push(Op::new("S", vec![]));
            }
            x += segment_width;
            pos = end;
        }
        baseline -= if style.line_spacing.is_none() && style.size.is_none() {
            source_leading
        } else {
            style.line_spacing.unwrap_or(1.5).clamp(0.5, 5.0) * style.size.unwrap_or(anchor.size)
        };
    }
    block_ops.push(Op::new("Q", vec![]));
    let streams = content_streams(doc, &p.dict);
    let pieces = Pieces::join(&streams.iter().map(|(_, d)| d.as_slice()).collect::<Vec<_>>());
    let ops = pieces.parse().ops;
    let members: Vec<_> = block.lines.iter().filter_map(|i| lines.get(*i)).collect();
    let rects: Vec<_> = members.iter().map(|l| l.rect).collect();
    let mut drop = coincident_ops(lines, &rects);
    drop.extend(members.iter().flat_map(|l| l.ops.iter().copied()));
    let o = &first.origin;
    let start = first.ops.first().copied().unwrap_or(o.bt_op);
    let split = ops
        .get(o.bt_op..start)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .any(|(i, op)| matches!(op.op.as_slice(), b"Tj" | b"TJ" | b"'" | b"\"") && !drop.contains(&(o.bt_op + i)));
    let after = if split { &o.state } else { &o.bt_state };
    block_ops.extend(after.ops());
    if split {
        block_ops.insert(0, Op::new("ET", vec![]));
        block_ops.push(Op::new("BT", vec![]));
        block_ops.push(Op::new("Tm", o.tlm.iter().map(|v| n(*v)).collect()));
    }
    let at = if split { start } else { o.bt_op };
    let data = splice(&pieces, &ops, |i| (if i == at { std::mem::take(&mut block_ops) } else { Vec::new() }, !drop.contains(&i)));
    let contents = rewritten_contents(doc, &streams, data);
    res.set(b"Font".to_vec(), Object::Dict(fonts));
    doc.update_dict(p.obj, |d| {
        d.set(b"Contents".to_vec(), contents);
        d.set(b"Resources".to_vec(), Object::Dict(res));
    })?;
    Ok(LineEdit { substituted })
}

#[cfg(test)]
mod unspaced_tests {
    #[test]
    fn thai_lao_khmer_and_myanmar_rows_join_without_a_space() {
        for c in ['ก', '้', 'ำ', '๙', 'ກ', 'ក', 'က'] {
            assert!(super::unspaced(c), "{c}");
        }
        for c in ['a', '9', ' ', '.', '日', 'ب'] {
            assert!(!super::unspaced(c), "{c}");
        }
    }
}
