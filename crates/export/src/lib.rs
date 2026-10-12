//! pdfcraft-export — Export a PDF ▸ Word, HTML, RTF (L4).
//!
//! The engine reduces each page to [`Page`]: paragraphs (text, box, size, bold/italic) and
//! images (encoded bytes and box), in reading order. The writers turn that into a flowing
//! document: headings are the paragraphs set larger than the body text. Word keeps pages with
//! rotated text, or text overlapped by artwork or watermark-sized text, as positioned, editable
//! shapes to avoid reflowing overlays; other pages flow with images inline.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod pptx;
mod zip;

pub use pptx::pptx;
pub use zip::Zip;

/// A paragraph of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub text: String,
    /// [x0, y0, x1, y1] on the displayed page in points (y up).
    pub rect: [f64; 4],
    /// Font size in points.
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// Text fill colour as sRGB components in 0..=1 (black when unknown).
    pub color: [f64; 3],
    /// Counterclockwise baseline angle in user space (degrees).
    pub rotation: f64,
}

/// An image of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// "png" or "jpg".
    pub ext: &'static str,
    pub bytes: Vec<u8>,
    pub rect: [f64; 4],
    /// Width and height before rotation, in points.
    pub size: [f64; 2],
    /// Counterclockwise rotation in user space (degrees).
    pub rotation: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Page {
    pub width: f64,
    pub height: f64,
    pub blocks: Vec<Block>,
    pub images: Vec<Image>,
    /// Ruling lines as rectangles `[x0, y0, x1, y1]` on the displayed page (points, y up).
    /// A grid of them is a table whatever its text alignment.
    pub rules: Vec<[f64; 4]>,
}

/// One cell of a detected table. `span` is how many grid columns the cell covers (a header
/// row that stretches over sub-columns).
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub text: String,
    pub size: f64,
    pub bold: bool,
    pub italic: bool,
    /// Text fill colour as sRGB components in 0..=1.
    pub color: [f64; 3],
    pub span: usize,
}

/// A table detected from the page's text layout: rows of cells, and the left edge of each
/// grid column (user space, y up) so the writers can size the columns.
#[derive(Clone, Debug, PartialEq)]
pub struct Table {
    pub rect: [f64; 4],
    pub cols: Vec<f64>,
    pub rows: Vec<Vec<Cell>>,
    /// Drawn with ruling lines on the page: the writers give it borders.
    pub ruled: bool,
}

/// What goes into the output, in order.
enum Item<'a> {
    Para(&'a Block, u8),
    /// Owned: tables are detected per page inside [`items`], so they cannot borrow.
    Table(Table),
    Img(&'a Image),
    PageBreak,
}

/// The body text size: the size most of the document's characters are set in.
fn body_size(pages: &[Page]) -> f64 {
    let mut by: Vec<(i64, usize)> = Vec::new();
    for b in pages.iter().flat_map(|p| &p.blocks) {
        let k = (b.size * 2.0).round() as i64;
        match by.iter_mut().find(|e| e.0 == k) {
            Some(e) => e.1 += b.text.len(),
            None => by.push((k, b.text.len())),
        }
    }
    by.into_iter().max_by_key(|e| e.1).map_or(11.0, |e| e.0 as f64 / 2.0)
}

/// Heading level for a block: 1 for ≥ 1.6× the body size, 2 for ≥ 1.25×, 0 for body text.
fn level(b: &Block, body: f64) -> u8 {
    let short = b.text.len() < 200;
    if short && b.size >= body * 1.6 {
        1
    } else if short && (b.size >= body * 1.25 || (b.bold && b.size >= body && b.text.len() < 80 && !b.text.ends_with('.'))) {
        2
    } else {
        0
    }
}

/// How far two cell left edges may drift (points) and still be the same grid column.
const COL_TOL: f64 = 4.0;
/// More grid columns than this isn't a table. It is Word's limit: a .docx whose table has 64 or
/// more columns doesn't open. It also bounds a table's size: every row is materialized to all
/// its columns, so a hostile page laid out as a staircase of blocks would otherwise make
/// (blocks / 2)² cells.
const MAX_COLS: usize = 63;
/// A cell is short: taller blocks are body text (or a multi-column layout), not table cells.
fn is_cell_like(b: &Block) -> bool {
    (b.rect[3] - b.rect[1]) <= b.size * 5.0 && !b.text.trim().is_empty()
}

/// Detect tables in a page's blocks from their text layout: columns are left edges shared by
/// several short blocks, rows are blocks whose vertical spans overlap. Returns the tables and,
/// per block, whether a table consumed it (so `items` can drop it from the paragraph flow).
pub(crate) fn tables(blocks: &[Block]) -> (Vec<Table>, Vec<bool>) {
    let mut consumed = vec![false; blocks.len()];
    // Candidate columns: cluster the left edges of cell-like blocks.
    let mut xs: Vec<(f64, usize)> = blocks.iter().enumerate().filter(|(_, b)| is_cell_like(b)).map(|(i, b)| (b.rect[0], i)).collect();
    xs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut cols: Vec<(f64, Vec<usize>)> = Vec::new(); // (left edge, member block indices)
    for (x, i) in xs {
        match cols.last_mut() {
            Some((e, m)) if (x - *e).abs() <= COL_TOL => {
                *e = (*e * m.len() as f64 + x) / (m.len() + 1) as f64;
                m.push(i);
            }
            _ => cols.push((x, vec![i])),
        }
    }
    let cols: Vec<(f64, Vec<usize>)> = cols.into_iter().filter(|(_, m)| m.len() >= 2).collect();
    if cols.len() < 2 {
        return (Vec::new(), consumed);
    }
    // Each block's column (an index into `cols`), so lookups stay linear on pages with many blocks.
    let mut col_of: Vec<Option<usize>> = vec![None; blocks.len()];
    for (ci, (_, members)) in cols.iter().enumerate() {
        for &i in members {
            if let Some(c) = col_of.get_mut(i) {
                *c = Some(ci);
            }
        }
    }
    // The column whose edge is within tolerance of `x` (edges are in ascending order).
    let column_at = |x: f64| -> Option<usize> {
        let k = cols.partition_point(|(e, _)| *e < x - COL_TOL);
        cols.get(k).filter(|(e, _)| (*e - x).abs() <= COL_TOL).map(|_| k)
    };
    // Assign every cell-like block to its nearest column (within tolerance).
    let mut rows: Vec<Vec<(usize, usize)>> = Vec::new(); // (column index, block index), top to bottom
    let mut order: Vec<usize> = (0..blocks.len()).collect();
    order.sort_by(|a, b| blocks[*b].rect[3].total_cmp(&blocks[*a].rect[3]).then(blocks[*a].rect[0].total_cmp(&blocks[*b].rect[0])));
    for &bi in &order {
        let b = &blocks[bi];
        if !is_cell_like(b) {
            continue;
        }
        let Some(ci) = column_at(b.rect[0]) else { continue };
        let joins = rows
            .last()
            .and_then(|row| row.first())
            .map(|(_, fi)| {
                let rb = &blocks[*fi];
                let overlap = rb.rect[3].min(b.rect[3]) - rb.rect[1].max(b.rect[1]);
                overlap > 0.5 * (rb.rect[3] - rb.rect[1]).min(b.rect[3] - b.rect[1])
            })
            .unwrap_or(false);
        if joins {
            if let Some(row) = rows.last_mut() {
                row.push((ci, bi));
            }
        } else {
            rows.push(vec![(ci, bi)]);
        }
    }
    // Runs of consecutive rows that have two or more distinct columns are tables.
    let mut out = Vec::new();
    let flush = |run: &mut Vec<Vec<(usize, usize)>>, out: &mut Vec<Table>, consumed: &mut Vec<bool>| {
        if run.len() < 2 {
            run.clear();
            return;
        }
        let mut used: Vec<usize> = run.iter().flatten().map(|(_, i)| *i).collect();
        used.sort_unstable();
        used.dedup();
        let mut edges: Vec<f64> = used.iter().filter_map(|&i| col_of.get(i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        edges.sort_by(|a, b| a.total_cmp(b));
        edges.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        let ncols = edges.len();
        let ok = (ncols >= 3 || (ncols == 2 && run.len() >= 3)) && ncols <= MAX_COLS;
        if !ok {
            run.clear();
            return;
        }
        let right = used.iter().map(|&i| blocks[i].rect[2]).fold(f64::MIN, f64::max) + COL_TOL;
        let mut rows_out = Vec::new();
        let mut slots: Vec<Option<Cell>> = vec![None; ncols];
        for row in run.drain(..) {
            for slot in slots.iter_mut() {
                *slot = None;
            }
            for (_ci, bi) in row {
                let b = &blocks[bi];
                let Some(slot) = edges.iter().position(|e| (*e - b.rect[0]).abs() <= COL_TOL) else { continue };
                let span = edges[slot + 1..].iter().filter(|e| **e > b.rect[0] + COL_TOL && **e < b.rect[2] - COL_TOL).count() + 1;
                let cell = Cell { text: b.text.trim().to_string(), size: b.size, bold: b.bold, italic: b.italic, color: b.color, span };
                match &mut slots[slot] {
                    Some(c) => {
                        c.text.push(' ');
                        c.text.push_str(&cell.text);
                    }
                    None => slots[slot] = Some(cell),
                }
                consumed[bi] = true;
            }
            let mut materialized = Vec::with_capacity(ncols);
            let mut column = 0;
            while column < ncols {
                match slots[column].take() {
                    Some(mut cell) => {
                        cell.span = cell.span.min(ncols - column).max(1);
                        column += cell.span;
                        materialized.push(cell);
                    }
                    None => {
                        column += 1;
                        materialized.push(Cell { text: String::new(), size: 11.0, bold: false, italic: false, color: [0.0; 3], span: 1 });
                    }
                }
            }
            while materialized.last().is_some_and(|cell| cell.text.is_empty()) && materialized.iter().any(|cell| cell.span > 1) {
                materialized.pop();
            }
            rows_out.push(materialized);
        }
        let rect = [
            edges[0],
            used.iter().map(|&i| blocks[i].rect[1]).fold(f64::MAX, f64::min),
            right,
            used.iter().map(|&i| blocks[i].rect[3]).fold(f64::MIN, f64::max),
        ];
        out.push(Table { rect, cols: edges, rows: rows_out, ruled: false });
    };
    // Runs of consecutive multi-column rows are tables. Rows on either side of a run that
    // hold a single cell wide enough to span two columns (a header band, a totals row) join
    // the table; other single-column rows (captions, following text) stay paragraphs.
    let spans_run = |edges: &[f64], bi: usize| -> bool {
        let b = &blocks[bi];
        edges.iter().any(|e| (*e - b.rect[0]).abs() <= COL_TOL) && edges.iter().any(|e| *e > b.rect[0] + COL_TOL && *e < b.rect[2] - COL_TOL)
    };
    let run_edges = |run: &[Vec<(usize, usize)>]| -> Vec<f64> {
        let mut es: Vec<f64> =
            run.iter().flatten().filter_map(|(_, i)| col_of.get(*i).copied().flatten()).filter_map(|ci| cols.get(ci)).map(|c| c.0).collect();
        es.sort_by(|a, b| a.total_cmp(b));
        es.dedup_by(|a, b| (*a - *b).abs() <= COL_TOL);
        es
    };
    let qual: Vec<bool> = rows
        .iter()
        .map(|row| {
            let mut cs: Vec<usize> = row.iter().map(|(c, _)| *c).collect();
            cs.sort_unstable();
            cs.dedup();
            cs.len() >= 2
        })
        .collect();
    let mut i = 0;
    while i < rows.len() {
        if !qual[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < rows.len() && qual[i] {
            i += 1;
        }
        let end = i; // run = rows[start..end]
        let mut pieces: Vec<Vec<(usize, usize)>> = Vec::new();
        if start > 0 && rows[start - 1].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[start - 1][0].1) {
            pieces.push(rows[start - 1].clone());
        }
        pieces.extend(rows[start..end].iter().cloned());
        if end < rows.len() && rows[end].len() == 1 && spans_run(&run_edges(&rows[start..end]), rows[end][0].1) {
            pieces.push(rows[end].clone());
        }
        flush(&mut pieces, &mut out, &mut consumed);
    }
    (out, consumed)
}

/// How far two rule positions may differ (points) and still be the same grid line.
const RULE_TOL: f64 = 2.0;
/// More ruling lines than this are not searched for grids (the search is quadratic).
const MAX_GRID_RULES: usize = 2000;
/// More rows than this isn't materialized as one table.
const MAX_ROWS: usize = 2000;

/// Positions clustered within [`RULE_TOL`], ascending.
fn grid_lines(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.total_cmp(b));
    let mut out: Vec<f64> = Vec::new();
    for x in v {
        match out.last() {
            Some(last) if x - *last <= RULE_TOL => {}
            _ => out.push(x),
        }
    }
    out
}

/// Tables drawn with ruling lines: each connected set of crossing horizontal and vertical rules
/// is a grid, and the blocks inside each of its boxes are a cell. A box whose side has no rule
/// is merged with its neighbour to the right (a spanning cell). Returns the tables and, per
/// block, whether a table took it.
fn ruled_tables(blocks: &[Block], rules: &[[f64; 4]]) -> (Vec<Table>, Vec<bool>) {
    let mut consumed = vec![false; blocks.len()];
    let mut out = Vec::new();
    if rules.len() < 4 || rules.len() > MAX_GRID_RULES {
        return (out, consumed);
    }
    let vertical = |r: &[f64; 4]| r[3] - r[1] > r[2] - r[0];
    let touch = |a: &[f64; 4], b: &[f64; 4]| a[0] <= b[2] + RULE_TOL && b[0] <= a[2] + RULE_TOL && a[1] <= b[3] + RULE_TOL && b[1] <= a[3] + RULE_TOL;
    // Connected components (rules that touch), by flood fill.
    let mut group: Vec<Option<usize>> = vec![None; rules.len()];
    let mut groups = 0usize;
    for seed in 0..rules.len() {
        if group.get(seed).copied().flatten().is_some() {
            continue;
        }
        let mut stack = vec![seed];
        while let Some(i) = stack.pop() {
            let Some(slot) = group.get_mut(i) else { continue };
            if slot.is_some() {
                continue;
            }
            *slot = Some(groups);
            let Some(a) = rules.get(i) else { continue };
            for (j, b) in rules.iter().enumerate() {
                if group.get(j).copied().flatten().is_none() && touch(a, b) {
                    stack.push(j);
                }
            }
        }
        groups += 1;
    }
    for g in 0..groups {
        let members: Vec<&[f64; 4]> = rules.iter().zip(&group).filter(|(_, m)| **m == Some(g)).map(|(r, _)| r).collect();
        let xs = grid_lines(members.iter().filter(|r| vertical(r)).map(|r| (r[0] + r[2]) / 2.0).collect());
        // Top to bottom.
        let mut ys = grid_lines(members.iter().filter(|r| !vertical(r)).map(|r| (r[1] + r[3]) / 2.0).collect());
        ys.reverse();
        let (ncols, nrows) = (xs.len().saturating_sub(1), ys.len().saturating_sub(1));
        // One box is a frame around text, not a table.
        if ncols == 0 || nrows == 0 || ncols * nrows < 2 || ncols > MAX_COLS || nrows > MAX_ROWS {
            continue;
        }
        let (Some(&left), Some(&right), Some(&top), Some(&bottom)) = (xs.first(), xs.last(), ys.first(), ys.last()) else { continue };
        // Each block goes to the box holding its centre.
        let mut cells: Vec<Vec<usize>> = vec![Vec::new(); ncols * nrows];
        let mut inside = Vec::new();
        for (bi, b) in blocks.iter().enumerate() {
            let (cx, cy) = ((b.rect[0] + b.rect[2]) / 2.0, (b.rect[1] + b.rect[3]) / 2.0);
            if b.rotation.abs() > 0.5 || cx <= left || cx >= right || cy >= top || cy <= bottom {
                continue;
            }
            let col = xs.partition_point(|x| *x <= cx).saturating_sub(1).min(ncols - 1);
            let row = ys.partition_point(|y| *y >= cy).saturating_sub(1).min(nrows - 1);
            if let Some(cell) = cells.get_mut(row * ncols + col) {
                cell.push(bi);
                inside.push(bi);
            }
        }
        if inside.is_empty() {
            continue;
        }
        let mut rows = Vec::with_capacity(nrows);
        for row in 0..nrows {
            let (Some(&y_top), Some(&y_bottom)) = (ys.get(row), ys.get(row + 1)) else { continue };
            let mid = (y_top + y_bottom) / 2.0;
            // Whether a vertical rule separates column `col` from the next one in this row.
            let divided = |col: usize| {
                xs.get(col + 1)
                    .is_some_and(|x| members.iter().any(|r| vertical(r) && ((r[0] + r[2]) / 2.0 - x).abs() <= RULE_TOL && r[1] <= mid && r[3] >= mid))
            };
            let mut out_row: Vec<Cell> = Vec::new();
            let mut col = 0;
            while col < ncols {
                let mut span = 1;
                while col + span < ncols && !divided(col + span - 1) {
                    span += 1;
                }
                let mut members: Vec<usize> = (col..col + span).filter_map(|c| cells.get(row * ncols + c)).flatten().copied().collect();
                members.sort_by(|a, b| match (blocks.get(*a), blocks.get(*b)) {
                    (Some(a), Some(b)) => b.rect[3].total_cmp(&a.rect[3]).then(a.rect[0].total_cmp(&b.rect[0])),
                    _ => std::cmp::Ordering::Equal,
                });
                let first = members.first().and_then(|i| blocks.get(*i));
                let text = members.iter().filter_map(|i| blocks.get(*i)).map(|b| b.text.trim()).collect::<Vec<_>>().join(" ");
                out_row.push(Cell {
                    text,
                    size: first.map_or(11.0, |b| b.size),
                    bold: first.is_some_and(|b| b.bold),
                    italic: first.is_some_and(|b| b.italic),
                    color: first.map_or([0.0; 3], |b| b.color),
                    span,
                });
                col += span;
            }
            rows.push(out_row);
        }
        for bi in inside {
            if let Some(c) = consumed.get_mut(bi) {
                *c = true;
            }
        }
        out.push(Table { rect: [left, bottom, right, top], cols: xs.iter().take(ncols).copied().collect(), rows, ruled: true });
    }
    (out, consumed)
}

/// A page's tables: the ruled ones first, then tables found from the alignment of the text
/// that is left.
fn page_tables(page: &Page) -> (Vec<Table>, Vec<bool>) {
    let (mut found, mut consumed) = ruled_tables(&page.blocks, &page.rules);
    if found.is_empty() {
        return tables(&page.blocks);
    }
    let rest: Vec<usize> = (0..page.blocks.len()).filter(|i| !consumed.get(*i).copied().unwrap_or(false)).collect();
    let free: Vec<Block> = rest.iter().filter_map(|i| page.blocks.get(*i)).cloned().collect();
    let (aligned, taken) = tables(&free);
    for (i, _) in taken.iter().enumerate().filter(|(_, t)| **t) {
        if let Some(c) = rest.get(i).and_then(|bi| consumed.get_mut(*bi)) {
            *c = true;
        }
    }
    found.extend(aligned);
    (found, consumed)
}

fn items(pages: &[Page]) -> Vec<Item<'_>> {
    let body = body_size(pages);
    let mut out = Vec::new();
    for (i, p) in pages.iter().enumerate() {
        if i > 0 {
            out.push(Item::PageBreak);
        }
        let (tables, consumed) = page_tables(p);
        // Blocks and images by their top edge, top to bottom.
        let mut parts: Vec<(f64, f64, Item)> = p
            .blocks
            .iter()
            .enumerate()
            .filter(|(bi, _)| !consumed.get(*bi).copied().unwrap_or(false))
            .map(|(_, b)| (b.rect[3], b.rect[0], Item::Para(b, level(b, body))))
            .collect();
        parts.extend(tables.into_iter().map(|t| (t.rect[3], t.rect[0], Item::Table(t))));
        parts.extend(p.images.iter().map(|im| (im.rect[3], im.rect[0], Item::Img(im))));
        parts.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.total_cmp(&b.1)));
        out.extend(parts.into_iter().map(|x| x.2));
    }
    out
}

/// Thai letters, vowels, tone marks and digits. Office treats them as complex script: Word sizes
/// and weights them from the `cs` properties, not the Latin ones, and breaks lines by language.
pub(crate) fn has_thai(s: &str) -> bool {
    s.chars().any(|c| matches!(u32::from(c), 0x0E00..=0x0E7F))
}

/// A face Word, PowerPoint and LibreOffice have for Thai on Windows and macOS. Naming it keeps
/// the reader from falling back to a face whose tone marks collide with upper vowels.
pub(crate) const THAI_FACE: &str = "Tahoma";

/// Whether a character may appear in the output. XML 1.0 forbids the C0 controls other than tab,
/// line feed and carriage return, and U+FFFE/U+FFFF. Text extracted from PDFs often holds them
/// (fonts without a usable `/ToUnicode`), and Word refuses a whole document over one (#72).
fn allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || !(c.is_ascii_control() || c == '\u{FFFE}' || c == '\u{FFFF}')
}

pub(crate) fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c => o.push(c),
        }
    }
    o
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(A[(n >> 18) as usize & 63] as char);
        s.push(A[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { A[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { A[n as usize & 63] as char } else { '=' });
    }
    s
}

/// One HTML file: headings and paragraphs, tables as real `<table>` elements, images inline, a
/// rule between pages.
pub fn html(pages: &[Page], title: &str) -> String {
    // Thai needs its language (browsers break its lines with a dictionary only when they know
    // it), a face with Thai glyphs ahead of the generic one, and room for stacked marks.
    let thai = pages.iter().flat_map(|p| &p.blocks).any(|b| has_thai(&b.text));
    let (lang, face, leading) =
        if thai { (" lang=\"th\"", "\"Sarabun\",\"Noto Sans Thai\",\"Leelawadee UI\",Thonburi,Tahoma,", "1.7") } else { ("", "", "1.45") };
    let mut s = format!(
        "<!DOCTYPE html>\n<html{lang}>\n<head>\n<meta charset=\"utf-8\">\n<title>{}</title>\n<style>body{{max-width:46em;margin:2em auto;font-family:{face}sans-serif;line-height:{leading}}}\
img{{max-width:100%}}hr{{border:0;border-top:1px solid #ccc;margin:2em 0}}\
table{{border-collapse:collapse;margin:1em 0}}td,th{{border:1px solid #999;padding:2px 6px;vertical-align:top}}</style>\n</head>\n<body>\n",
        esc(title)
    );
    for it in items(pages) {
        match it {
            Item::Para(b, lvl) => {
                let mut t = esc(&b.text);
                if b.italic {
                    t = format!("<em>{t}</em>");
                }
                if b.bold && lvl == 0 {
                    t = format!("<strong>{t}</strong>");
                }
                let style = hex(b.color).map_or(String::new(), |h| format!(" style=\"color:#{h}\""));
                match lvl {
                    0 => s.push_str(&format!("<p{style}>{t}</p>\n")),
                    l => s.push_str(&format!("<h{l}{style}>{t}</h{l}>\n")),
                }
            }
            Item::Table(t) => {
                s.push_str("<table>\n");
                for row in &t.rows {
                    s.push_str("<tr>");
                    for c in row {
                        let mut body = esc(&c.text);
                        if c.italic {
                            body = format!("<em>{body}</em>");
                        }
                        if c.bold {
                            body = format!("<strong>{body}</strong>");
                        }
                        let span = if c.span > 1 { format!(" colspan=\"{}\"", c.span) } else { String::new() };
                        let style = hex(c.color).map_or(String::new(), |h| format!(" style=\"color:#{h}\""));
                        s.push_str(&format!("<td{span}{style}>{body}</td>"));
                    }
                    s.push_str("</tr>\n");
                }
                s.push_str("</table>\n");
            }
            Item::Img(im) => {
                let mime = if im.ext == "jpg" { "image/jpeg" } else { "image/png" };
                s.push_str(&format!("<p><img alt=\"\" src=\"data:{mime};base64,{}\"></p>\n", base64(&im.bytes)));
            }
            Item::PageBreak => s.push_str("<hr>\n"),
        }
    }
    s.push_str("</body>\n</html>\n");
    s
}

/// A colour's 8-bit sRGB components (NaN and out-of-range values clamp; NaN reads as 0).
pub(crate) fn rgb8(c: [f64; 3]) -> [u8; 3] {
    // The value is clamped to 0..=255 before the cast, so it cannot truncate.
    c.map(|v| if v.is_finite() { (v.clamp(0.0, 1.0) * 255.0).round() as u8 } else { 0 })
}

/// The colour as `RRGGBB`, or `None` for black (the default every format already uses).
pub(crate) fn hex(c: [f64; 3]) -> Option<String> {
    let [r, g, b] = rgb8(c);
    (r | g | b != 0).then(|| format!("{r:02X}{g:02X}{b:02X}"))
}

/// A formatted text run (shared by paragraphs and table cells).
fn run_xml(text: &str, size: f64, bold: bool, italic: bool, color: [f64; 3]) -> String {
    let thai = has_thai(text);
    // CT_RPr is a sequence: rFonts, b, bCs, i, iCs, color, sz, szCs, lang.
    let mut rpr = String::new();
    if thai {
        rpr.push_str(&format!("<w:rFonts w:cs=\"{THAI_FACE}\"/>"));
    }
    // Complex-script text (Thai, Arabic, Hebrew) takes its weight, slant and size from the
    // `Cs` twins; without them Word shows it regular at the default size.
    if bold {
        rpr.push_str("<w:b/><w:bCs/>");
    }
    if italic {
        rpr.push_str("<w:i/><w:iCs/>");
    }
    if let Some(h) = hex(color) {
        rpr.push_str(&format!("<w:color w:val=\"{h}\"/>"));
    }
    let half_points = if size.is_finite() { (size * 2.0).round().clamp(2.0, 3276.0) as i64 } else { 24 };
    rpr.push_str(&format!("<w:sz w:val=\"{half_points}\"/><w:szCs w:val=\"{half_points}\"/>"));
    if thai {
        rpr.push_str("<w:lang w:bidi=\"th-TH\"/>");
    }
    format!("<w:r><w:rPr>{rpr}</w:rPr><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc(text))
}

/// One Word table: explicit absent borders (alignment alone cannot prove PDF rules), a grid sized
/// from the detected column edges, `gridSpan` for spanning cells, empty cells for holes.
fn docx_table(t: &Table) -> String {
    let mut edges = t.cols.clone();
    edges.push(t.rect[2]);
    let widths: Vec<i64> = edges.windows(2).map(|w| ((w[1] - w[0]).max(20.0) * 20.0).round().clamp(60.0, 31680.0) as i64).collect();
    let mut s = String::from("<w:tbl><w:tblPr><w:tblW w:w=\"0\" w:type=\"auto\"/><w:tblBorders>");
    // A table ruled on the page keeps its lines; alignment alone doesn't prove any.
    let border = if t.ruled { "w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"" } else { "w:val=\"nil\"" };
    for edge in ["top", "left", "bottom", "right", "insideH", "insideV"] {
        s.push_str(&format!("<w:{edge} {border}/>"));
    }
    s.push_str("</w:tblBorders></w:tblPr><w:tblGrid>");
    for w in &widths {
        s.push_str(&format!("<w:gridCol w:w=\"{w}\"/>"));
    }
    s.push_str("</w:tblGrid>");
    for row in &t.rows {
        s.push_str("<w:tr>");
        for c in row {
            let span = if c.span > 1 { format!("<w:gridSpan w:val=\"{}\"/>", c.span) } else { String::new() };
            let w: i64 = widths.iter().take(c.span.min(widths.len())).sum();
            s.push_str(&format!(
                "<w:tc><w:tcPr><w:tcW w:w=\"{w}\" w:type=\"dxa\"/>{span}</w:tcPr><w:p>{}</w:p></w:tc>",
                run_xml(&c.text, c.size, c.bold, c.italic, c.color)
            ));
        }
        // Pad the grid so every row covers all columns (Word rejects short rows).
        let used: usize = row.iter().map(|c| c.span).sum();
        for _ in used..t.cols.len() {
            s.push_str("<w:tc><w:tcPr><w:tcW w:w=\"0\" w:type=\"auto\"/></w:tcPr><w:p/></w:tc>");
        }
        s.push_str("</w:tr>");
    }
    s.push_str("</w:tbl>");
    s
}

/// Pages with rotated text, or text that actually overlaps an image or an oversized
/// (watermark-like) text block, cannot be put in paragraph flow without changing their layout.
/// Keep those pages as editable, absolutely positioned text boxes. Every other page, including
/// one with a logo or a big title that sits clear of the text, flows as paragraphs and tables.
fn positioned(page: &Page) -> bool {
    if page.blocks.iter().any(|b| b.rotation.abs() > 0.1) {
        return true;
    }
    let body = body_size(std::slice::from_ref(page)).max(1.0);
    let overlays = page
        .images
        .iter()
        .map(|im| (im.rect, None))
        .chain(page.blocks.iter().enumerate().filter(|(_, b)| b.size >= 2.0 * body).map(|(i, b)| (b.rect, Some(i))));
    // Bound the pairwise test on hostile pages (many overlays times many blocks): past the
    // budget, keep the default flowing layout.
    let mut budget: usize = 1_000_000;
    for (r, own) in overlays {
        for (i, b) in page.blocks.iter().enumerate() {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            if Some(i) != own && overlaps(r, b.rect) {
                return true;
            }
        }
    }
    false
}

/// Whether two [x0, y0, x1, y1] boxes share more than a hairline (0.5 pt each way); boxes that
/// only touch, or hostile non-finite coordinates, do not count.
fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    let w = a[2].min(b[2]) - a[0].max(b[0]);
    let h = a[3].min(b[3]) - a[1].max(b[1]);
    w.is_finite() && h.is_finite() && w > 0.5 && h > 0.5
}

/// Bound every value written into a shape's style, including hostile PDF coordinates.
fn layout_number(n: f64) -> f64 {
    if n.is_finite() { n.clamp(-1584.0, 1584.0) } else { 0.0 }
}

fn positioned_page<'a>(page: &'a Page, media: &mut Vec<(String, &'a Image)>, shape_id: &mut usize) -> String {
    // One 1 pt paragraph anchors the whole page. Shapes never reserve body space, so even a
    // page-sized watermark cannot push the invoice footer onto another page.
    let mut s = String::from("<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/></w:pPr>");
    let body = body_size(std::slice::from_ref(page)).max(1.0);
    for b in &page.blocks {
        *shape_id = shape_id.saturating_add(1);
        let rotation = layout_number(-b.rotation).rem_euclid(360.0);
        let (sin, cos) = rotation.to_radians().sin_cos();
        let (bw, bh) = (layout_number(b.rect[2] - b.rect[0]).max(1.0), layout_number(b.rect[3] - b.rect[1]).max(1.0));
        let h = if b.rotation.abs() > 0.1 { layout_number(b.size).max(1.0) } else { bh };
        // Recover the unrotated width from the axis-aligned box. Use the larger axis so
        // quarter turns and 45-degree watermarks do not divide by (nearly) zero.
        let w = if b.rotation.abs() <= 0.1 {
            bw
        } else if cos.abs() >= sin.abs() {
            (bw - h * sin.abs()) / cos.abs()
        } else {
            (bh - h * cos.abs()) / sin.abs()
        };
        let w = layout_number(w).max(1.0);
        let (x, y) = (layout_number(b.rect[0] + (bw - w) / 2.0), layout_number(page.height - b.rect[3] + (bh - h) / 2.0));
        let z = if b.size >= 2.0 * body { -2 } else { 1 };
        let style = format!(
            "position:absolute;margin-left:{x:.3}pt;margin-top:{y:.3}pt;width:{w:.3}pt;height:{h:.3}pt;rotation:{rotation:.3};z-index:{z};mso-position-horizontal-relative:page;mso-position-vertical-relative:page"
        );
        s.push_str(&format!(
            "<w:r><w:pict><v:rect id=\"Text{}\" style=\"{style}\" filled=\"f\" stroked=\"f\"><v:textbox inset=\"0,0,0,0\" style=\"mso-fit-shape-to-text:t\"><w:txbxContent><w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\"/></w:pPr>{}</w:p></w:txbxContent></v:textbox><w10:wrap type=\"none\"/></v:rect></w:pict></w:r>",
            shape_id, run_xml(&b.text, b.size, b.bold, b.italic, b.color)
        ));
    }
    for im in &page.images {
        *shape_id = shape_id.saturating_add(1);
        let n = media.len().saturating_add(1);
        let (w, h) = (layout_number(im.size[0]).max(1.0), layout_number(im.size[1]).max(1.0));
        let (x, y) = (layout_number((im.rect[0] + im.rect[2] - w) / 2.0), layout_number(page.height - (im.rect[1] + im.rect[3] + h) / 2.0));
        let rotation = layout_number(-im.rotation).rem_euclid(360.0);
        s.push_str(&format!("<w:r><w:pict><v:rect id=\"Image{shape_id}\" style=\"position:absolute;margin-left:{x:.3}pt;margin-top:{y:.3}pt;width:{w:.3}pt;height:{h:.3}pt;rotation:{rotation:.3};z-index:-1;mso-position-horizontal-relative:page;mso-position-vertical-relative:page\" stroked=\"f\"><v:imagedata r:id=\"rIdImg{n}\"/><w10:wrap type=\"none\"/></v:rect></w:pict></w:r>"));
        media.push((format!("image{n}.{}", im.ext), im));
    }
    s.push_str("</w:p>");
    s
}

fn word_page_size(page: &Page) -> (i64, i64) {
    let twips = |pt: f64| if pt.is_finite() { (pt * 20.0).round().clamp(144.0, 31680.0) as i64 } else { 12240 };
    (twips(page.width), twips(page.height))
}

fn section(page: &Page) -> String {
    let (pw, ph) = word_page_size(page);
    let (mx, my) = ((pw / 8).min(1440), (ph / 8).min(1440));
    format!(
        "<w:sectPr><w:type w:val=\"nextPage\"/><w:pgSz w:w=\"{pw}\" w:h=\"{ph}\"/><w:pgMar w:top=\"{my}\" w:right=\"{mx}\" w:bottom=\"{my}\" w:left=\"{mx}\" w:header=\"{}\" w:footer=\"{}\" w:gutter=\"0\"/></w:sectPr>",
        my / 2,
        my / 2
    )
}

/// A Word document (.docx, Office Open XML). Ordinary pages flow as paragraphs and tables;
/// pages with rotated text, or text that overlaps artwork, use floating shapes to retain their layout.
pub fn docx(pages: &[Page], title: &str) -> Vec<u8> {
    let mut body = String::new();
    let mut media: Vec<(String, &Image)> = Vec::new();
    // Word needs a paragraph between adjacent tables and after the last one in the body.
    let mut after_table = false;
    let run = |b: &Block| run_xml(&b.text, b.size, b.bold, b.italic, b.color);
    let mut shape_id = 0usize;
    for (page_index, page) in pages.iter().enumerate() {
        let (pw, _) = word_page_size(page);
        let text_w = (pw - 2 * (pw / 8).min(1440)) as f64 / 20.0;
        if positioned(page) {
            body.push_str(&positioned_page(page, &mut media, &mut shape_id));
            after_table = false;
        } else {
            for it in items(std::slice::from_ref(page)) {
                match &it {
                    Item::Para(b, lvl) => {
                        let style = match lvl {
                            1 => "<w:pPr><w:pStyle w:val=\"Heading1\"/></w:pPr>",
                            2 => "<w:pPr><w:pStyle w:val=\"Heading2\"/></w:pPr>",
                            _ => "",
                        };
                        body.push_str(&format!("<w:p>{style}{}</w:p>", run(b)));
                    }
                    Item::Table(t) => {
                        if after_table {
                            body.push_str("<w:p/>");
                        }
                        body.push_str(&docx_table(t));
                    }
                    Item::Img(im) => {
                        let n = media.len() + 1;
                        let name = format!("image{n}.{}", im.ext);
                        // Size on the page, in EMU (12700 per point), at most the text width.
                        let (w, h) = ((im.rect[2] - im.rect[0]).max(1.0), (im.rect[3] - im.rect[1]).max(1.0));
                        let k = (text_w / w).min(1.0);
                        let (cx, cy) = ((w * k * 12700.0) as i64, (h * k * 12700.0) as i64);
                        body.push_str(&format!(
                    "<w:p><w:r><w:drawing><wp:inline><wp:extent cx=\"{cx}\" cy=\"{cy}\"/><wp:docPr id=\"{n}\" name=\"Picture {n}\"/>\
<a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"rIdImg{n}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>\
</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>"
                ));
                        media.push((name, im));
                    }
                    Item::PageBreak => body.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>"),
                }
                after_table = matches!(it, Item::Table(_));
            }
        }
        // Section breaks carry this source page's size and create exactly one next page.
        // The last section must be a direct child of the body (OOXML 17.6.17).
        if page_index + 1 < pages.len() {
            body.push_str(&format!(
                "<w:p><w:pPr><w:spacing w:before=\"0\" w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/>{}</w:pPr></w:p>",
                section(page)
            ));
        } else {
            if after_table || page.blocks.is_empty() && page.images.is_empty() {
                body.push_str("<w:p><w:pPr><w:spacing w:after=\"0\" w:line=\"20\" w:lineRule=\"exact\"/></w:pPr></w:p>");
            }
            body.push_str(&section(page));
        }
        after_table = false;
    }
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" xmlns:v=\"urn:schemas-microsoft-com:vml\" xmlns:w10=\"urn:schemas-microsoft-com:office:word\"><w:body>{body}\
</w:body></w:document>"
    );
    let mut types = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/><Default Extension=\"jpg\" ContentType=\"image/jpeg\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
<Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>",
    );
    types.push_str("</Types>");
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/></Relationships>";
    let mut doc_rels = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
    );
    for (i, (name, _)) in media.iter().enumerate() {
        doc_rels.push_str(&format!(
            "<Relationship Id=\"rIdImg{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{name}\"/>",
            i + 1
        ));
    }
    doc_rels.push_str("</Relationships>");
    let styles = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:styles xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:pPr><w:spacing w:after=\"120\"/></w:pPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading1\"><w:name w:val=\"heading 1\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"240\"/><w:outlineLvl w:val=\"0\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
<w:style w:type=\"paragraph\" w:styleId=\"Heading2\"><w:name w:val=\"heading 2\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:pPr><w:keepNext/><w:spacing w:before=\"200\"/><w:outlineLvl w:val=\"1\"/></w:pPr><w:rPr><w:b/></w:rPr></w:style>\
</w:styles>";
    let core = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{}</dc:title></cp:coreProperties>",
        esc(title)
    );
    let mut z = Zip::default();
    z.add("[Content_Types].xml", types.as_bytes(), true);
    z.add("_rels/.rels", rels.as_bytes(), true);
    z.add("docProps/core.xml", core.as_bytes(), true);
    z.add("word/document.xml", doc.as_bytes(), true);
    z.add("word/styles.xml", styles.as_bytes(), true);
    z.add("word/_rels/document.xml.rels", doc_rels.as_bytes(), true);
    for (name, im) in &media {
        z.add(&format!("word/media/{name}"), &im.bytes, false);
    }
    z.finish()
}

fn rtf_text(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars().filter(|c| allowed(*c)) {
        match c {
            '\\' | '{' | '}' => {
                o.push('\\');
                o.push(c);
            }
            c if (c as u32) < 128 => o.push(c),
            c => {
                // \uN with a ? fallback; UTF-16 units as signed 16-bit numbers.
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    o.push_str(&format!("\\u{}?", *u as i16));
                }
            }
        }
    }
    o
}

/// Rich Text Format: paragraphs with their sizes and bold/italic, real table rows, page breaks
/// between pages (images are left out).
pub fn rtf(pages: &[Page]) -> String {
    let items = items(pages);
    // The colour table: entry 0 is the default (auto) colour, then each non-black colour once.
    let mut colors: Vec<[u8; 3]> = Vec::new();
    let mut note = |c: [f64; 3]| {
        let c = rgb8(c);
        if c != [0, 0, 0] && !colors.contains(&c) {
            colors.push(c);
        }
    };
    for it in &items {
        match it {
            Item::Para(b, _) => note(b.color),
            Item::Table(t) => t.rows.iter().flatten().for_each(|c| note(c.color)),
            Item::Img(_) | Item::PageBreak => {}
        }
    }
    // `\cfN` for a colour (nothing for black, which keeps the default).
    let cf = |c: [f64; 3]| {
        let c = rgb8(c);
        colors.iter().position(|e| *e == c).map_or(String::new(), |i| format!("\\cf{}", i + 1))
    };
    // \af, \afs, \ab and \ai are the complex-script twins of \f, \fs, \b and \i: Word draws
    // Thai with them. The Thai face is only declared when the text needs it.
    let thai_doc = items.iter().any(|it| match it {
        Item::Para(b, _) => has_thai(&b.text),
        Item::Table(t) => t.rows.iter().flatten().any(|c| has_thai(&c.text)),
        Item::Img(_) | Item::PageBreak => false,
    });
    let complex = |text: &str, size: f64, bold: bool, italic: bool| {
        if !has_thai(text) {
            return String::new();
        }
        format!("\\af1\\afs{}{}{}\\alang1054", (size * 2.0).round() as i64, if bold { "\\ab" } else { "" }, if italic { "\\ai" } else { "" })
    };
    let mut s = if thai_doc {
        format!("{{\\rtf1\\ansi\\deff0{{\\fonttbl{{\\f0 Helvetica;}}{{\\f1\\fcharset222 {THAI_FACE};}}}}\n")
    } else {
        String::from("{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Helvetica;}}\n")
    };
    if !colors.is_empty() {
        s.push_str("{\\colortbl;");
        for [r, g, b] in &colors {
            s.push_str(&format!("\\red{r}\\green{g}\\blue{b};"));
        }
        s.push_str("}\n");
    }
    for it in items {
        match it {
            Item::Para(b, _) => {
                let mut fmt = format!("\\fs{}", (b.size * 2.0).round() as i64);
                if b.bold {
                    fmt.push_str("\\b");
                }
                if b.italic {
                    fmt.push_str("\\i");
                }
                fmt.push_str(&cf(b.color));
                fmt.push_str(&complex(&b.text, b.size, b.bold, b.italic));
                s.push_str(&format!("{{\\pard{fmt} {}\\par}}\n", rtf_text(&b.text)));
            }
            Item::Table(t) => {
                let mut edges = t.cols.clone();
                edges.push(t.rect[2]);
                let cellx: Vec<i64> = edges.iter().skip(1).map(|e| (e * 20.0).round() as i64).collect();
                for row in &t.rows {
                    s.push_str("\\trowd\\trgaph108");
                    // The row's cell boundaries come first: each cell ends at the right edge of
                    // the last grid column it spans.
                    let mut column = 0;
                    for c in row {
                        column += c.span.max(1);
                        if let Some(x) = cellx.get(column.min(cellx.len()).saturating_sub(1)) {
                            s.push_str(&format!("\\cellx{x}"));
                        }
                    }
                    for c in row {
                        let mut fmt = format!("\\intbl\\fs{}", (c.size * 2.0).round() as i64);
                        if c.bold {
                            fmt.push_str("\\b");
                        }
                        if c.italic {
                            fmt.push_str("\\i");
                        }
                        fmt.push_str(&cf(c.color));
                        fmt.push_str(&complex(&c.text, c.size, c.bold, c.italic));
                        s.push_str(&format!("{{{fmt} {}}}\\cell", rtf_text(&c.text)));
                    }
                    s.push_str("\\row\n");
                }
            }
            Item::Img(_) => {}
            Item::PageBreak => s.push_str("\\page\n"),
        }
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Page {
        let b = |t: &str, y: f64, size: f64, bold: bool| Block {
            text: t.into(),
            rect: [72.0, y, 500.0, y + size],
            size,
            bold,
            italic: false,
            color: [0.0; 3],
            rotation: 0.0,
        };
        Page {
            rules: Vec::new(),
            width: 612.0,
            height: 792.0,
            blocks: vec![
                b("Body text after the picture, which is long enough to be the main text size of the page.", 400.0, 11.0, false),
                b("Annual Report", 700.0, 24.0, true),
                b("Overview", 650.0, 14.0, true),
                b("Some <body> text & more of it so that eleven points is the body size here.", 620.0, 11.0, false),
            ],
            images: vec![Image {
                ext: "png",
                bytes: b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x03".to_vec(),
                rect: [72.0, 450.0, 272.0, 600.0],
                size: [200.0, 150.0],
                rotation: 0.0,
            }],
        }
    }

    #[test]
    fn html_has_headings_paragraphs_and_images_in_order() {
        let h = html(&[page(), page()], "Report");
        let order: Vec<usize> = ["<h1>Annual Report</h1>", "<h2>Overview</h2>", "<p>Some &lt;body&gt; text &amp;", "<img", "<p>Body text after"]
            .iter()
            .map(|x| h.find(x).unwrap_or_else(|| panic!("{x} missing in {h}")))
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        assert_eq!(h.matches("<hr>").count(), 1);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
    }

    #[test]
    fn docx_is_a_word_package() {
        let d = docx(&[page()], "Report");
        assert!(d.starts_with(b"PK"));
        let names: Vec<&str> =
            ["[Content_Types].xml", "word/document.xml", "word/styles.xml", "word/_rels/document.xml.rels", "word/media/image1.png", "_rels/.rels"]
                .to_vec();
        for n in names {
            assert!(d.windows(n.len()).any(|w| w == n.as_bytes()), "{n}");
        }
    }

    #[test]
    fn word_does_not_invent_borders_for_aligned_invoice_fields() {
        let xml = part(&docx(&[table_page()], "Invoice"), "word/document.xml");
        assert!(xml.contains("<w:tbl>"));
        assert!(!xml.contains("w:val=\"single\""), "text alignment is not evidence of a border: {xml}");
        assert!(xml.contains("<w:insideH w:val=\"nil\"/>"));
    }

    #[test]
    fn word_keeps_a_page_with_a_clear_image_and_big_title_in_flow() {
        // page() has a picture between paragraphs and a 24 pt title over 11 pt body text, but
        // nothing overlaps: it must stay flowing paragraphs with the image inline.
        let xml = part(&docx(&[page()], "Report"), "word/document.xml");
        assert!(!xml.contains("position:absolute"), "nothing overlaps, so nothing floats: {xml}");
        assert!(xml.contains("<wp:inline>"), "the picture stays inline: {xml}");
        assert!(xml.contains("<w:pStyle w:val=\"Heading1\"/>"), "the title stays a heading: {xml}");
        let (title, body) = (xml.find(">Annual Report</w:t>"), xml.find(">Body text after the picture"));
        assert!(title.is_some() && body.is_some() && title < body, "reading order kept: {xml}");
        // The same page with the picture moved over the body text floats.
        let mut over = page();
        over.images[0].rect = [72.0, 380.0, 272.0, 530.0];
        let xml = part(&docx(&[over], "Report"), "word/document.xml");
        assert!(xml.contains("position:absolute"), "an image over text floats: {xml}");
    }

    #[test]
    fn word_overlap_does_not_consume_invoice_body_space() {
        let mut p = table_page();
        p.blocks.push(Block {
            text: "DRAFT".into(),
            rect: [100.0, 200.0, 500.0, 700.0],
            size: 100.0,
            bold: false,
            italic: false,
            color: [0.8; 3],
            rotation: 0.0,
        });
        p.images = page().images;
        let xml = part(&docx(&[p], "Invoice"), "word/document.xml");
        assert!(xml.contains("position:absolute"), "overlapping text must float: {xml}");
        assert!(!xml.contains("<wp:inline>"), "images must not displace invoice text");
        assert!(!xml.contains("<w:tbl>"), "the source text boxes must keep their positions");
        assert_eq!(xml.matches(">DRAFT</w:t>").count(), 1);
        assert_eq!(xml.matches("w:type=\"page\"").count(), 0);
        assert!(xml.contains("margin-left:72.000pt;margin-top:160.000pt"), "Meter field keeps its source position: {xml}");
        let watermark = xml.split(">DRAFT</w:t>").next().unwrap().rsplit("<v:rect ").next().unwrap();
        assert!(watermark.contains("z-index:-2"), "watermark is behind foreground graphics");
        let image = xml.split("<v:imagedata ").next().unwrap().rsplit("<v:rect ").next().unwrap();
        assert!(image.contains("z-index:-1"), "foreground graphics cover the watermark");
    }

    #[test]
    fn word_rotated_pages_keep_separate_page_sizes_and_unique_shapes() {
        let mut p = table_page();
        p.blocks[0].rotation = 45.0;
        let mut second = p.clone();
        second.width = 400.0;
        second.height = 500.0;
        let xml = part(&docx(&[p, second], "Two invoices"), "word/document.xml");
        assert!(xml.contains("rotation:315.000"), "PDF y-up becomes Word y-down");
        assert_eq!(xml.matches("<w:sectPr>").count(), 2);
        assert!(xml.contains("<w:pgSz w:w=\"8000\" w:h=\"10000\"/>"));
        assert_eq!(xml.matches("id=\"Text1\"").count(), 1);
        assert_eq!(xml.matches(">Meter</w:t>").count(), 2);
    }

    /// One part of a package written by [`Zip`], inflated.
    pub(crate) fn part(zip: &[u8], name: &str) -> String {
        let at = |i: usize, n: usize| zip[i..i + n].iter().rev().fold(0usize, |v, b| v << 8 | *b as usize);
        let mut i = 0;
        while zip[i..].starts_with(b"PK\x03\x04") {
            let (size, name_len) = (at(i + 18, 4), at(i + 26, 2));
            let data = i + 30 + name_len;
            if &zip[i + 30..data] == name.as_bytes() {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut flate2::read::DeflateDecoder::new(&zip[data..data + size]), &mut s).unwrap();
                return s;
            }
            i = data + size;
        }
        panic!("{name} missing")
    }

    #[test]
    fn word_opens_text_with_control_characters_and_long_pages() {
        // #72: text extracted without a usable /ToUnicode holds C0 controls, which XML forbids;
        // Word then refused the whole file. A till receipt is also taller than Word's 22 in.
        let mut p = page();
        p.height = 2400.0;
        p.blocks.push(Block {
            text: "Total\u{0}\u{3}\u{c} 4.50\u{FFFF}\tGBP".into(),
            rect: [72.0, 100.0, 300.0, 110.0],
            size: f64::NAN,
            bold: false,
            italic: false,
            color: [0.0; 3],
            rotation: 0.0,
        });
        let xml = part(&docx(&[p.clone()], "Receipt\u{1}"), "word/document.xml");
        assert!(xml.contains(">Total 4.50\tGBP</w:t>"), "{xml}");
        assert!(!xml.chars().any(|c| !allowed(c)));
        assert!(xml.contains("<w:pgSz w:w=\"12240\" w:h=\"31680\"/>"), "clamped to 22 in: {xml}");
        assert!(xml.contains("<w:sz w:val=\"24\"/>"), "a non-finite size falls back to 12 pt");
        assert!(rtf(&[p]).contains("Total 4.50\tGBP"));
        // A narrow page keeps room for text: margins shrink.
        let narrow = Page { width: 100.0, height: 200.0, ..page() };
        let xml = part(&docx(&[narrow], "Narrow"), "word/document.xml");
        assert!(xml.contains("<w:pgMar w:top=\"500\" w:right=\"250\" w:bottom=\"500\" w:left=\"250\""), "{xml}");
    }

    #[test]
    fn text_colour_survives_every_format() {
        // #526: a dark green heading (0.05 0.23 0.18 rg) came out black in Word.
        let mut p = page();
        p.blocks[1].color = [0.05, 0.23, 0.18];
        p.blocks[3].color = [f64::NAN, 2.0, -1.0];
        let xml = part(&docx(&[p.clone()], "Colour"), "word/document.xml");
        assert!(xml.contains("<w:color w:val=\"0D3B2E\"/>"), "{xml}");
        assert!(xml.contains("<w:color w:val=\"00FF00\"/>"), "out-of-range values clamp: {xml}");
        // Black text keeps Word's default colour.
        assert_eq!(xml.matches("<w:color ").count(), 2, "{xml}");
        let h = html(&[p.clone()], "Colour");
        assert!(h.contains("<h1 style=\"color:#0D3B2E\">Annual Report</h1>"), "{h}");
        assert!(h.contains("<p style=\"color:#00FF00\">"), "{h}");
        let r = rtf(&[p.clone()]);
        assert!(r.contains("{\\colortbl;\\red13\\green59\\blue46;\\red0\\green255\\blue0;}"), "{r}");
        assert!(r.contains("\\cf1 Annual Report"), "{r}");
        let ppt = pptx(&[p], "Colour");
        let pxml = part(&ppt, "ppt/slides/slide1.xml");
        assert!(pxml.contains("<a:srgbClr val=\"0D3B2E\"/>"), "{pxml}");
        assert!(pxml.contains("<a:srgbClr val=\"00FF00\"/>"), "out-of-range values clamp: {pxml}");
        // A table cell keeps its colour too.
        let mut t = table_page();
        t.blocks[0].color = [1.0, 0.0, 0.0];
        let xml = part(&docx(&[t.clone()], "Grid"), "word/document.xml");
        assert!(xml.contains("<w:color w:val=\"FF0000\"/>"), "{xml}");
        assert!(html(&[t], "Grid").contains("style=\"color:#FF0000\""));
    }

    fn thai_page() -> Page {
        let b = |t: &str, y: f64, size: f64, bold: bool, italic: bool| Block {
            text: t.into(),
            rect: [72.0, y, 500.0, y + size],
            size,
            bold,
            italic,
            color: [0.0; 3],
            rotation: 0.0,
        };
        Page {
            rules: Vec::new(),
            width: 612.0,
            height: 792.0,
            blocks: vec![
                b("รายงานประจำปี ๒๕๖๙", 700.0, 24.0, true, false),
                b("น้ำใจ ผู้ใช้ ที่นี่ ปนกับ English และตัวเลข 1,234.56 บาท ยาวพอจะเป็นขนาดตัวอักษรหลักของหน้า", 650.0, 14.0, false, true),
            ],
            images: Vec::new(),
        }
    }

    /// Thai is complex script in Office: without the `Cs` twins Word draws it regular at its
    /// default size, and without a named face and language it may pick one that stacks marks badly.
    #[test]
    fn thai_keeps_its_size_weight_face_and_language_in_every_format() {
        let pages = [thai_page()];
        let doc = part(&docx(&pages, "ไทย"), "word/document.xml");
        assert!(doc.contains("รายงานประจำปี ๒๕๖๙") && doc.contains("น้ำใจ ผู้ใช้ ที่นี่"), "{doc}");
        assert!(
            doc.contains(
                "<w:rPr><w:rFonts w:cs=\"Tahoma\"/><w:b/><w:bCs/><w:sz w:val=\"48\"/><w:szCs w:val=\"48\"/><w:lang w:bidi=\"th-TH\"/></w:rPr>"
            ),
            "{doc}"
        );
        assert!(doc.contains("<w:i/><w:iCs/><w:sz w:val=\"28\"/><w:szCs w:val=\"28\"/>"), "{doc}");
        let slide = part(&pptx(&pages, "ไทย"), "ppt/slides/slide1.xml");
        assert!(slide.contains("<a:rPr lang=\"th-TH\"") && slide.contains("<a:cs typeface=\"Tahoma\"/></a:rPr>"), "{slide}");
        let h = html(&pages, "ไทย");
        assert!(h.contains("<html lang=\"th\">") && h.contains("Tahoma,sans-serif;line-height:1.7"), "{h}");
        let r = rtf(&pages);
        assert!(r.contains("{\\f1\\fcharset222 Tahoma;}") && r.contains("\\fs48\\b\\af1\\afs48\\ab\\alang1054 "), "{r}");
        assert!(r.contains("\\fs28\\i\\af1\\afs28\\ai\\alang1054 \\u3609?\\u3657?\\u3635?"), "{r}");
        // Text without Thai keeps the plain output.
        let plain = [page()];
        assert!(html(&plain, "t").contains("<html>\n") && !rtf(&plain).contains("\\af1") && !rtf(&plain).contains("Tahoma"));
        let doc = part(&docx(&plain, "t"), "word/document.xml");
        assert!(!doc.contains("w:rFonts") && !doc.contains("th-TH") && doc.contains("<w:sz w:val=\"48\"/><w:szCs w:val=\"48\"/>"), "{doc}");
    }

    /// A ruled grid is a table even when nothing in it shares a left edge (a centred header),
    /// and a box whose side has no rule spans into its neighbour.
    #[test]
    fn a_ruled_grid_is_a_bordered_table_with_spans() {
        let b = |t: &str, x: f64, y: f64, bold: bool| Block {
            text: t.into(),
            rect: [x, y, x + 40.0, y + 12.0],
            size: 12.0,
            bold,
            italic: false,
            color: [0.0; 3],
            rotation: 0.0,
        };
        let h = |y: f64| [100.0, y - 0.3, 400.0, y + 0.3];
        let v = |x: f64, y0: f64, y1: f64| [x - 0.3, y0, x + 0.3, y1];
        let page = Page {
            width: 612.0,
            height: 792.0,
            blocks: vec![
                b("รายการ", 130.0, 574.0, true),
                b("จำนวน", 240.0, 574.0, true),
                b("ราคา", 330.0, 574.0, true),
                b("น้ำปลา", 105.0, 544.0, false),
                b("2 ขวด", 205.0, 544.0, false),
                b("90.00", 305.0, 544.0, false),
                b("รวมทั้งหมด", 105.0, 514.0, false),
                b("90.00", 305.0, 514.0, false),
                b("ข้อความนอกตารางที่ยาวพอจะเป็นเนื้อความของหน้านี้", 100.0, 700.0, false),
            ],
            images: Vec::new(),
            // Three rows; the last has no rule between its first two columns.
            rules: vec![
                h(600.0),
                h(570.0),
                h(540.0),
                h(510.0),
                v(100.0, 510.0, 600.0),
                v(400.0, 510.0, 600.0),
                v(300.0, 510.0, 600.0),
                v(200.0, 540.0, 600.0),
            ],
        };
        let (found, consumed) = page_tables(&page);
        assert_eq!(found.len(), 1, "{found:?}");
        let t = &found[0];
        assert!(t.ruled && t.cols == [100.0, 200.0, 300.0] && t.rect == [100.0, 510.0, 400.0, 600.0], "{t:?}");
        let texts: Vec<Vec<(&str, usize)>> = t.rows.iter().map(|r| r.iter().map(|c| (c.text.as_str(), c.span)).collect()).collect();
        assert_eq!(
            texts,
            [vec![("รายการ", 1), ("จำนวน", 1), ("ราคา", 1)], vec![("น้ำปลา", 1), ("2 ขวด", 1), ("90.00", 1)], vec![("รวมทั้งหมด", 2), ("90.00", 1)]]
        );
        assert!(t.rows[0][0].bold && !t.rows[1][0].bold);
        assert_eq!(consumed, [true, true, true, true, true, true, true, true, false]);
        let doc = part(&docx(std::slice::from_ref(&page), "t"), "word/document.xml");
        assert!(doc.contains("<w:insideV w:val=\"single\"") && doc.contains("<w:gridSpan w:val=\"2\"/>"), "{doc}");
        assert!(html(std::slice::from_ref(&page), "t").contains("<td colspan=\"2\">รวมทั้งหมด</td>"));
        // A single box, or rules with no text inside, is not a table.
        let frame = Page { rules: vec![h(600.0), h(510.0), v(100.0, 510.0, 600.0), v(400.0, 510.0, 600.0)], ..page.clone() };
        assert!(ruled_tables(&frame.blocks, &frame.rules).0.is_empty());
        let empty = Page { blocks: vec![b("นอกตาราง", 100.0, 700.0, false)], ..page };
        assert!(ruled_tables(&empty.blocks, &empty.rules).0.is_empty());
    }

    #[test]
    fn rtf_escapes_and_sizes() {
        let mut p = page();
        p.blocks.push(Block {
            text: "Café {x}".into(),
            rect: [72.0, 100.0, 200.0, 110.0],
            size: 10.0,
            bold: false,
            italic: true,
            color: [0.0; 3],
            rotation: 0.0,
        });
        let r = rtf(&[p]);
        assert!(r.starts_with("{\\rtf1") && r.ends_with('}'));
        assert!(r.contains("\\fs48\\b Annual Report"));
        assert!(r.contains("\\fs20\\i Caf\\u233? \\{x\\}"), "{r}");
    }

    /// A 3-column table with a spanning header row and a hole in the last row.
    fn table_page() -> Page {
        let cell = |t: &str, x: f64, y: f64, w: f64| Block {
            text: t.into(),
            rect: [x, y, x + w, y + 12.0],
            size: 11.0,
            bold: false,
            italic: false,
            color: [0.0; 3],
            rotation: 0.0,
        };
        Page {
            rules: Vec::new(),
            width: 612.0,
            height: 792.0,
            blocks: vec![
                cell("Meter", 72.0, 620.0, 120.0),
                cell("Unit", 232.0, 620.0, 80.0),
                cell("Reading", 392.0, 620.0, 100.0),
                cell("A1", 72.0, 590.0, 120.0),
                cell("kWh", 232.0, 590.0, 80.0),
                cell("37414.00", 392.0, 590.0, 100.0),
                cell("A2", 72.0, 560.0, 120.0),
                cell("kW", 232.0, 560.0, 80.0),
                // A2's row is missing its third cell (an empty cell in the grid).
                cell("Totals", 72.0, 530.0, 240.0),
                Block {
                    text: "Notes follow the table and are long enough to be the body text size of the page here.".into(),
                    rect: [72.0, 460.0, 500.0, 471.0],
                    size: 11.0,
                    bold: false,
                    italic: false,
                    color: [0.0; 3],
                    rotation: 0.0,
                },
            ],
            images: Vec::new(),
        }
    }

    #[test]
    fn grid_text_becomes_a_real_table_in_every_format() {
        let p = table_page();
        let h = html(std::slice::from_ref(&p), "Bill");
        assert!(h.contains("<table>"), "{h}");
        assert!(h.contains("<td colspan=\"2\">Totals</td>"), "spanning cell: {h}");
        assert_eq!(h.matches("<tr>").count(), 4, "{h}");
        assert!(h.contains("<td></td>"), "the hole is an empty cell: {h}");
        assert!(h.contains("<p>Notes follow"), "the paragraph after the table stays: {h}");

        let d = docx(std::slice::from_ref(&p), "Bill");
        assert!(d.starts_with(b"PK"));
        assert!(d.windows(b"word/document.xml".len()).any(|w| w == b"word/document.xml"));
        let (tables, _) = tables(&p.blocks);
        assert_eq!(tables.len(), 1);
        let table_xml = docx_table(&tables[0]);
        assert!(table_xml.contains("<w:tblGrid>"));
        assert!(table_xml.contains("<w:gridSpan w:val=\"2\"/>"), "{table_xml}");
        assert_eq!(table_xml.matches("<w:tr>").count(), 4);

        let r = rtf(std::slice::from_ref(&p));
        assert!(r.contains("\\trowd"), "{r}");
        assert_eq!(r.matches("\\row").count(), 4);
        // Boundaries precede the cells; "Totals" spans two columns, so it ends at the third edge.
        assert!(r.contains("\\trowd\\trgaph108\\cellx4640\\cellx7840\\cellx9920{"), "{r}");
        assert!(r.contains("\\trowd\\trgaph108\\cellx7840{\\intbl\\fs22 Totals}\\cell\\row"), "{r}");

        let pt = pptx(std::slice::from_ref(&p), "Bill");
        assert!(pt.starts_with(b"PK"));
        assert!(pt.windows(b"ppt/slides/slide1.xml".len()).any(|w| w == b"ppt/slides/slide1.xml"));
        let slide_xml = part(&pt, "ppt/slides/slide1.xml");
        assert!(slide_xml.contains("<a:tbl>"));
        assert!(slide_xml.contains("gridSpan=\"2\""));
        assert_eq!(slide_xml.matches("<a:tr ").count(), 4);
    }

    #[test]
    fn a_staircase_of_blocks_is_not_a_huge_table() {
        // Row k holds blocks in columns k and k + 1: every column has two members and every row
        // two columns, so without a cap this would be one table of (n / 2)² cells.
        let n = 4000;
        let blocks: Vec<Block> = (0..n)
            .map(|i| {
                let (row, col) = (i / 2, i / 2 + i % 2);
                let (x, y) = (10.0 + col as f64 * 20.0, 10_000.0 - row as f64 * 14.0);
                Block { text: "x".into(), rect: [x, y, x + 8.0, y + 12.0], size: 11.0, bold: false, italic: false, color: [0.0; 3], rotation: 0.0 }
            })
            .collect();
        let started = std::time::Instant::now();
        let (tables, _) = tables(&blocks);
        assert!(tables.iter().all(|t| t.cols.len() <= MAX_COLS), "{} columns", tables.iter().map(|t| t.cols.len()).max().unwrap_or(0));
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "took {:?}", started.elapsed());
    }

    #[test]
    fn word_opens_tables_up_to_its_column_limit() {
        // Rows of short numbers on a regular grid, n columns wide.
        let grid = |n: usize| Page {
            rules: Vec::new(),
            width: 1500.0,
            height: 792.0,
            blocks: (0..3)
                .flat_map(|row| {
                    (0..n).map(move |col| {
                        let (x, y) = (20.0 + col as f64 * 22.0, 700.0 - row as f64 * 14.0);
                        Block {
                            text: (col + 1).to_string(),
                            rect: [x, y, x + 8.0, y + 8.0],
                            size: 8.0,
                            bold: false,
                            italic: false,
                            color: [0.0; 3],
                            rotation: 0.0,
                        }
                    })
                })
                .collect(),
            images: Vec::new(),
        };
        let grid_cols = |p: &Page| part(&docx(std::slice::from_ref(p), "Grid"), "word/document.xml").matches("<w:gridCol ").count();
        assert_eq!(grid_cols(&grid(63)), 63, "63 columns is still a table");
        // Word refuses to open a document with a 64-column table, so that grid stays text.
        assert_eq!(grid_cols(&grid(64)), 0);
    }

    #[test]
    fn two_column_body_text_is_not_mistaken_for_a_table() {
        let para = |t: &str, x: f64, y: f64| Block {
            text: t.into(),
            rect: [x, y, x + 200.0, y + 90.0], // nine lines tall: body text, not a cell
            size: 11.0,
            bold: false,
            italic: false,
            color: [0.0; 3],
            rotation: 0.0,
        };
        let p = Page {
            rules: Vec::new(),
            width: 612.0,
            height: 792.0,
            blocks: vec![
                para("Left column text of the page, long enough to be a flowing paragraph.", 72.0, 500.0),
                para("Right column text of the page, long enough to be a flowing paragraph.", 320.0, 500.0),
                para("Second left block, still a tall flowing paragraph rather than a cell.", 72.0, 400.0),
                para("Second right block, still a tall flowing paragraph rather than a cell.", 320.0, 400.0),
            ],
            images: Vec::new(),
        };
        let h = html(&[p], "Paper");
        assert!(!h.contains("<table>"), "tall two-column text must stay paragraphs: {h}");
    }
}
