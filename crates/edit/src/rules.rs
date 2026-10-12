//! The ruling lines a page draws: thin filled rectangles and stroked axis-aligned segments.
//!
//! Tables are usually ruled. The lines say where cells end, which text positions alone cannot
//! (a centred header shares no edge with the cells under it), so Export keeps a ruled table a
//! table and the paragraphs of two neighbouring cells apart.

use pdfcraft_content::{Matrix, Op, parse};
use pdfcraft_cos::{Document, Object};

use crate::EditError;

/// A rule thicker than this (points) is artwork, not a ruling line.
const MAX_THICKNESS: f64 = 3.0;
/// A rule shorter than this (points) is a tick or a dot.
const MIN_LENGTH: f64 = 6.0;
/// More rules than this on a page is a drawing; the page then reports none.
const MAX_RULES: usize = 4000;

/// The page's ruling lines as rectangles `[x0, y0, x1, y1]` in user space (0-based page). Only the
/// page's own content streams are read. A page with more than a few thousand is a drawing and
/// reports none.
pub fn page_rules(doc: &Document, page: usize) -> Result<Vec<[f64; 4]>, EditError> {
    let p = pdfcraft_model::pages(doc).into_iter().nth(page).ok_or(EditError::NoSuchPage(page))?;
    let list: Vec<Object> = match p.dict.get(b"Contents") {
        None => Vec::new(),
        Some(c) => match &*doc.resolve(c) {
            Object::Array(a) => a.clone(),
            _ => vec![c.clone()],
        },
    };
    let mut data = Vec::new();
    for o in list {
        if let Object::Stream(s) = &*doc.resolve(&o) {
            data.extend(s.decoded().unwrap_or_default());
            data.push(b'\n');
        }
    }
    Ok(rules(&parse(&data).ops))
}

fn rules(ops: &[Op]) -> Vec<[f64; 4]> {
    let mut out: Vec<[f64; 4]> = Vec::new();
    let mut ctm = Matrix::IDENTITY;
    let mut width = 1.0f64;
    let mut stack: Vec<(Matrix, f64)> = Vec::new();
    // The current path: whole rectangles, and straight segments of open subpaths.
    let mut rects: Vec<[f64; 4]> = Vec::new();
    let mut segments: Vec<[f64; 4]> = Vec::new();
    let mut start: Option<(f64, f64)> = None;
    let mut at: Option<(f64, f64)> = None;
    for op in ops {
        if out.len() > MAX_RULES {
            return Vec::new();
        }
        match op.op.as_slice() {
            b"q" => {
                if stack.len() < 256 {
                    stack.push((ctm, width));
                }
            }
            b"Q" => {
                if let Some((m, w)) = stack.pop() {
                    (ctm, width) = (m, w);
                }
            }
            b"cm" => {
                if let Some(m) = Matrix::from_operands(&op.operands) {
                    ctm = m.then(&ctm);
                }
            }
            b"w" => width = op.num(0).filter(|w| w.is_finite()).unwrap_or(width),
            b"re" => {
                if let Some([x, y, w, h]) = op.nums::<4>() {
                    let (a, b) = (ctm.apply(x, y), ctm.apply(x + w, y + h));
                    rects.push([a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]);
                }
                (start, at) = (None, None);
            }
            b"m" => {
                let point = op.nums::<2>().map(|[x, y]| ctm.apply(x, y));
                (start, at) = (point, point);
            }
            b"l" => {
                let point = op.nums::<2>().map(|[x, y]| ctm.apply(x, y));
                if let (Some(a), Some(b)) = (at, point) {
                    segments.push([a.0, a.1, b.0, b.1]);
                }
                at = point;
            }
            b"h" => {
                if let (Some(a), Some(b)) = (at, start) {
                    segments.push([a.0, a.1, b.0, b.1]);
                }
                at = start;
            }
            // Curves end the straight run; what they draw is not a rule.
            b"c" | b"v" | b"y" => at = None,
            b"f" | b"F" | b"f*" | b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" => {
                let stroke = !matches!(op.op.as_slice(), b"f" | b"F" | b"f*");
                let fill = !matches!(op.op.as_slice(), b"S" | b"s");
                // The line width in user space (the CTM's mean scale).
                let scale = (ctm.0[0] * ctm.0[3] - ctm.0[1] * ctm.0[2]).abs().sqrt();
                let half = (width * scale / 2.0).clamp(0.05, MAX_THICKNESS);
                for r in rects.drain(..) {
                    if fill && thin(r) {
                        out.push(r);
                    } else if stroke {
                        out.extend(
                            [[r[0], r[1], r[2], r[1]], [r[0], r[3], r[2], r[3]], [r[0], r[1], r[0], r[3]], [r[2], r[1], r[2], r[3]]]
                                .into_iter()
                                .filter_map(|s| stroked(s, half)),
                        );
                    }
                }
                if stroke {
                    out.extend(segments.drain(..).filter_map(|s| stroked(s, half)));
                }
                segments.clear();
                (start, at) = (None, None);
            }
            b"n" => {
                rects.clear();
                segments.clear();
                (start, at) = (None, None);
            }
            _ => {}
        }
    }
    out.retain(|r| r.iter().all(|v| v.is_finite()));
    if out.len() > MAX_RULES { Vec::new() } else { out }
}

/// Whether a filled rectangle is a ruling line.
fn thin(r: [f64; 4]) -> bool {
    let (w, h) = (r[2] - r[0], r[3] - r[1]);
    w.min(h) <= MAX_THICKNESS && w.max(h) >= MIN_LENGTH
}

/// An axis-aligned stroked segment as the rectangle its ink covers.
fn stroked(s: [f64; 4], half: f64) -> Option<[f64; 4]> {
    let (dx, dy) = ((s[2] - s[0]).abs(), (s[3] - s[1]).abs());
    if half * 2.0 > MAX_THICKNESS || dx.max(dy) < MIN_LENGTH || dx.min(dy) > 0.5 {
        return None;
    }
    Some(if dx >= dy {
        [s[0].min(s[2]), s[1].min(s[3]) - half, s[0].max(s[2]), s[1].max(s[3]) + half]
    } else {
        [s[0].min(s[2]) - half, s[1].min(s[3]), s[0].max(s[2]) + half, s[1].max(s[3])]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filled_hairlines_and_stroked_segments_are_rules_and_artwork_is_not() {
        // Chrome draws borders as thin filled rectangles under a scaling, flipped matrix.
        let chrome = b"0.24 0 0 -0.24 0 792 cm q 3.125 0 0 3.125 100 100 cm 53 325 1 37 re 53 325 118 1 re f Q";
        let r = rules(&parse(chrome).ops);
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r[0][2] - r[0][0] < 1.0 && r[0][3] - r[0][1] > 27.0, "vertical: {:?}", r[0]);
        assert!(r[1][2] - r[1][0] > 88.0 && r[1][3] - r[1][1] < 1.0, "horizontal: {:?}", r[1]);
        // A stroked box is its four edges; a stroked line is one rule.
        let r = rules(&parse(b"0.5 w 10 10 200 50 re S 10 100 m 210 100 l S").ops);
        assert_eq!(r.len(), 5, "{r:?}");
        // Filled panels, curves, short ticks, slanted and heavy lines are not rules.
        let art = b"10 10 200 50 re f 0 0 m 10 10 20 20 30 0 c S 0 0 m 3 0 l S 0 0 m 100 100 l S 8 w 0 0 m 100 0 l S 0 0 m 100 0 l n";
        assert!(rules(&parse(art).ops).is_empty());
        // Unbalanced and malformed operators never panic.
        assert!(rules(&parse(b"Q Q re cm l h f S w 1 2 m (x) l S").ops).is_empty());
    }

    #[test]
    fn a_page_of_thousands_of_lines_is_a_drawing() {
        let mut content = Vec::new();
        for i in 0..5000 {
            content.extend(format!("0 {i} 100 0.5 re f\n").bytes());
        }
        assert!(rules(&parse(&content).ops).is_empty());
    }
}
