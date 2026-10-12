//! Field appearances (§12.7.4.3 and §12.5.5), execution plan M6.2.
//!
//! Text and choice fields are drawn from their `/DA` (font, size, colour) and the widget's `/MK`
//! (background and border colours) and `/BS` (border width and style), inside a `/Tx BMC … EMC`
//! marked-content block as viewers expect. Supported: single-line, multiline (wrapped), comb,
//! password, quadding, auto font size (`0 Tf`), combo boxes and list boxes (selection shown).
//!
//! The `/DA` font is used when the form's `/DR` defines it as a simple font that can encode the
//! displayed text (using its own encoding and Differences), or as a composite font with a
//! predefined Unicode CMap such as `UniJIS-UTF16-H`, as Japanese forms use. Otherwise (unsupported
//! characters, other composite fonts, missing resources) Helvetica is used. Text the chosen font
//! can't hold (outside WinAnsi in Helvetica, or beyond U+FFFF in a UCS-2 CMap) is drawn as `?`, as
//! Acrobat keeps a value its field font can't show: the value itself is stored as typed,
//! [`field_appearance_drawn`] returns the first such character, and the caller marks the widget
//! (`pdfcraft_fonts::UNDRAWN_MARK`) and asks other viewers to redraw the field
//! (`/NeedAppearances`). Widths use the approximate metrics of `pdfcraft-fonts` (Helvetica, or one
//! em per full-width character in composite fonts).
//!
//! `/MK /R` (0, 90, 180, 270, counterclockwise) lays the content out in the upright box and sets
//! the appearance `/Matrix` so that box lands on the widget rectangle. Any other value is 0, and
//! rotation 0 leaves `/Matrix` off.

use pdfcraft_cos::{Dict, Document, Object, Stream};
use pdfcraft_fonts::pdf::Metrics;
use pdfcraft_fonts::{UnicodeCMap, cjk_width, helvetica_width, literal, win_ansi, wrap_with};

use crate::{Field, FieldKind, Widget, acroform, flags};

/// Format a number for content streams.
fn n(v: f64) -> String {
    let s = format!("{:.3}", if v.abs() < 5e-4 { 0.0 } else { v });
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// Format a number for content streams (up to three decimals).
pub fn fmt(v: f64) -> String {
    n(v)
}

/// `/MK /R` as a quadrant, or 0 when it is missing or not a multiple of 90.
pub(crate) fn mk_rotation(doc: &Document, wd: &Dict) -> i64 {
    let Some(raw) = wd.get(b"MK") else { return 0 };
    let mk_obj = doc.resolve(raw);
    let Some(mk) = mk_obj.as_dict() else { return 0 };
    let Some(r_obj) = mk.get(b"R") else { return 0 };
    let resolved = doc.resolve(r_obj);
    let Some(r) = degrees(&resolved) else { return 0 };
    match r.rem_euclid(360) {
        q @ (0 | 90 | 180 | 270) => q,
        _ => 0,
    }
}

fn degrees(o: &Object) -> Option<i64> {
    if let Some(i) = o.as_int() {
        return Some(i);
    }
    let v = o.as_f64()?;
    (v.is_finite() && v.abs() <= 1.0e6).then_some(v.round() as i64)
}

/// Where an appearance is drawn: the upright layout box, and the `/Matrix` that turns it onto
/// the widget rectangle. Rotation 0 has no matrix.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placement {
    pub layout_w: f64,
    pub layout_h: f64,
    pub matrix: Option<[f64; 6]>,
}

pub(crate) fn placement(rotation: i64, page_w: f64, page_h: f64) -> Placement {
    let (layout_w, layout_h, matrix) = match rotation {
        // (x, y) -> (page_w - y, x): text that ran to the right runs upward.
        90 => (page_h, page_w, Some([0.0, 1.0, -1.0, 0.0, page_w, 0.0])),
        180 => (page_w, page_h, Some([-1.0, 0.0, 0.0, -1.0, page_w, page_h])),
        // (x, y) -> (y, page_h - x): text that ran to the right runs downward.
        270 => (page_h, page_w, Some([0.0, -1.0, 1.0, 0.0, 0.0, page_h])),
        _ => (page_w, page_h, None),
    };
    Placement { layout_w, layout_h, matrix }
}

pub(crate) fn set_form_box(d: &mut Dict, place: Placement) {
    let box_ = [0.0, 0.0, place.layout_w, place.layout_h];
    d.set(b"BBox".to_vec(), Object::Array(box_.iter().map(|v| Object::Real(*v)).collect()));
    if let Some(m) = place.matrix {
        d.set(b"Matrix".to_vec(), Object::Array(m.iter().map(|v| Object::Real(*v)).collect()));
    }
}

/// A parsed default appearance string: font resource name, size (0 = auto) and colour operator.
#[derive(Clone, Debug, PartialEq)]
pub struct Da {
    pub font: String,
    pub size: f64,
    /// The colour operation as written (`0 g`, `1 0 0 rg`, `0 0 0 1 k`).
    pub color: String,
}

pub fn parse_da(da: &str) -> Da {
    let toks: Vec<&str> = da.split_whitespace().collect();
    let mut out = Da { font: "Helv".into(), size: 0.0, color: "0 g".into() };
    for (i, t) in toks.iter().enumerate() {
        match *t {
            "Tf" if i >= 2 => {
                out.font = toks[i - 2].trim_start_matches('/').to_string();
                out.size = toks[i - 1].parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).unwrap_or(0.0).min(300.0);
            }
            "g" if i >= 1 && toks[i - 1].parse::<f64>().is_ok() => out.color = format!("{} g", toks[i - 1]),
            "rg" if i >= 3 && toks[i - 3..i].iter().all(|x| x.parse::<f64>().is_ok()) => out.color = format!("{} rg", toks[i - 3..i].join(" ")),
            "k" if i >= 4 && toks[i - 4..i].iter().all(|x| x.parse::<f64>().is_ok()) => out.color = format!("{} k", toks[i - 4..i].join(" ")),
            _ => {}
        }
    }
    out
}

/// How a field font's text is encoded and measured.
#[derive(Clone, Debug, PartialEq)]
enum FontText {
    WinAnsi,
    /// A reused simple font's encoding, including indirect dictionaries and Differences.
    Simple(Box<Metrics>),
    /// A composite font with a predefined Unicode CMap.
    Unicode(UnicodeCMap),
}

impl FontText {
    fn encode(&self, s: &str) -> Vec<u8> {
        match self {
            Self::WinAnsi => win_ansi(s),
            // Selection checks every displayed string first; wrapping only removes line breaks.
            Self::Simple(metrics) => metrics.encode(&s.replace('\t', " ")).unwrap_or_default(),
            Self::Unicode(cmap) => cmap.encode(s),
        }
    }

    fn can_encode(&self, s: &str) -> bool {
        match self {
            Self::Simple(metrics) => metrics.encode(&s.replace('\t', " ")).is_some(),
            _ => true,
        }
    }

    /// The first character of `s` the encoding can't hold, which [`Self::encode`] writes as `?`
    /// (a reused simple font is only chosen when it holds them all): reported by
    /// [`field_appearance_drawn`], so that `?` is never silent (AGENTS.md §4).
    fn first_unencodable(&self, s: &str) -> Option<char> {
        match self {
            Self::WinAnsi => pdfcraft_fonts::first_undrawable(s),
            Self::Simple(metrics) => {
                s.chars().map(|c| if c == '\t' { ' ' } else { c }).find(|c| metrics.encode(c.encode_utf8(&mut [0; 4])).is_none())
            }
            Self::Unicode(cmap) => cmap.first_unencodable(s),
        }
    }

    fn width(&self, s: &str, size: f64) -> f64 {
        match self {
            Self::WinAnsi | Self::Simple(_) => helvetica_width(s, size),
            Self::Unicode(_) => cjk_width(s, size),
        }
    }
}

/// `s` split into runs of Thai (`true`) and of everything else, in order.
fn thai_runs(s: &str) -> Vec<(bool, &str)> {
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    let mut kind: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let thai = pdfcraft_fonts::thai_break::is_thai(c);
        if kind.is_some_and(|k| k != thai)
            && let (Some(k), Some(run)) = (kind, s.get(start..i))
        {
            out.push((k, run));
            start = i;
        }
        kind = Some(thai);
    }
    if let (Some(k), Some(run)) = (kind, s.get(start..)) {
        out.push((k, run));
    }
    out
}

/// Whether Thai in `s` can be drawn: there is some, and this build has the Thai face.
fn draws_thai(s: &str) -> bool {
    s.chars().any(pdfcraft_fonts::thai_break::is_thai) && pdfcraft_fonts::document_thai_font().is_some()
}

/// A field's text width at `size`: Thai by its shaped advances, the rest in the field's font.
fn text_width(enc: &FontText, s: &str, size: f64) -> f64 {
    if !draws_thai(s) {
        return enc.width(s, size);
    }
    thai_runs(s)
        .into_iter()
        .map(|(thai, run)| match pdfcraft_fonts::shape_thai(run) {
            Ok(clusters) if thai => clusters.iter().map(|c| c.advance).sum::<f64>() * size,
            _ => enc.width(run, size),
        })
        .sum()
}

/// A shaped Thai run as filled outlines with its origin at (`x`, `y`), and its width. No font
/// resource is involved: the appearance holds the shapes, with every mark where the Thai face's
/// own tables put it, and the field's value keeps the text. `None` if a glyph can't be read.
fn thai_outlines(run: &str, x: f64, y: f64, size: f64) -> Option<(Vec<u8>, f64)> {
    let clusters = pdfcraft_fonts::shape_thai(run).ok()?;
    let mut out = format!("q {} 0 0 {} {} {} cm\n", n(size), n(size), n(x), n(y)).into_bytes();
    let mut pen = 0.0;
    // Em units, three decimals: a thousandth of an em is below a pixel at any field size.
    let em = |v: f64| format!("{:.3}", v).trim_end_matches('0').trim_end_matches('.').to_string();
    for cluster in &clusters {
        for (id, [dx, dy]) in &cluster.glyphs {
            let glyph = pdfcraft_fonts::shaped_glyph(cluster.script, *id).ok()?;
            for contour in &glyph.contours {
                for (i, p) in contour.iter().enumerate() {
                    out.extend(format!("{} {} {}\n", em(pen + dx + p[0]), em(dy + p[1]), if i == 0 { "m" } else { "l" }).bytes());
                }
                out.extend_from_slice(b"h\n");
            }
        }
        pen += cluster.advance;
    }
    out.extend_from_slice(b"f\nQ\n");
    Some((out, pen * size))
}

/// [`wrap_with`] for text with Thai, which has no spaces between words: lines break between
/// words (`pdfcraft_fonts::thai_break`), never inside a word or a cluster unless one word is
/// wider than the field.
fn wrap_thai(text: &str, size: f64, width: f64, measure: impl Fn(&str, f64) -> f64) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split(['\n', '\r']) {
        let mut line = String::new();
        for piece in pdfcraft_fonts::thai_break::wrap_pieces(para) {
            let fits = measure(format!("{line}{piece}").trim_end(), size) <= width;
            if !fits && !line.is_empty() {
                lines.push(std::mem::take(&mut line).trim_end().to_string());
            }
            line.push_str(piece);
        }
        lines.push(line.trim_end().to_string());
    }
    lines
}

/// The font resource for `name` from the form's `/DR`, if it is a font we can encode for.
fn dr_font(doc: &Document, name: &str) -> Option<(Object, FontText)> {
    let af = acroform(doc)?;
    let dr = doc.resolve(af.get(b"DR")?);
    let fonts = doc.resolve(dr.as_dict()?.get(b"Font")?);
    let entry = fonts.as_dict()?.get(name.as_bytes())?.clone();
    let font = doc.resolve(&entry);
    let fd = font.as_dict()?;
    match fd.name(b"Subtype") {
        Some(b"Type1" | b"TrueType" | b"MMType1") => {}
        // Japanese, Chinese and Korean forms: a CID font addressed by Unicode (a non-embedded
        // `/HeiseiMin-W3` with `/UniJIS-UCS2-H`, say). Embedded CMap streams and Identity-H need
        // the font's own mapping, so those still fall back to Helvetica.
        Some(b"Type0") => return Some((entry, FontText::Unicode(UnicodeCMap::from_name(fd.name(b"Encoding")?)?))),
        _ => return None,
    }
    // Symbolic fonts (ZapfDingbats, Symbol) can't show WinAnsi text.
    if matches!(fd.name(b"BaseFont"), Some(b"ZapfDingbats" | b"Symbol")) {
        return None;
    }
    Some((entry, FontText::Simple(Box::new(Metrics::from_dict(doc, fd)))))
}

fn helvetica() -> Object {
    let mut f = Dict::new();
    f.set(b"Type".to_vec(), Object::name("Font"));
    f.set(b"Subtype".to_vec(), Object::name("Type1"));
    f.set(b"BaseFont".to_vec(), Object::name("Helvetica"));
    f.set(b"Encoding".to_vec(), Object::name("WinAnsiEncoding"));
    Object::Dict(f)
}

fn color_array(doc: &Document, mk: Option<&Dict>, key: &[u8]) -> Option<String> {
    let a = doc.resolve(mk?.get(key)?);
    let v: Vec<f64> = a.as_array()?.iter().filter_map(|x| doc.resolve(x).as_f64()).collect();
    match v.as_slice() {
        [g] => Some(format!("{} g", n(*g))),
        [r, g, b] => Some(format!("{} {} {} rg", n(*r), n(*g), n(*b))),
        [c, m, y, k] => Some(format!("{} {} {} {} k", n(*c), n(*m), n(*y), n(*k))),
        _ => None,
    }
}

fn stroke_op(fill: &str) -> String {
    match fill.rsplit_once(' ') {
        Some((nums, "g")) => format!("{nums} G"),
        Some((nums, "rg")) => format!("{nums} RG"),
        Some((nums, "k")) => format!("{nums} K"),
        _ => "0 G".into(),
    }
}

/// Background and border from `/MK` and `/BS`; returns (content, border width).
fn frame(doc: &Document, wd: &Dict, w: f64, h: f64) -> (String, f64) {
    let mk = wd.get(b"MK").map(|m| doc.resolve(m)).and_then(|m| m.as_dict().cloned());
    let mut c = String::new();
    if let Some(bg) = color_array(doc, mk.as_ref(), b"BG") {
        c.push_str(&format!("{bg}\n0 0 {} {} re f\n", n(w), n(h)));
    }
    let bs = wd.get(b"BS").map(|b| doc.resolve(b)).and_then(|b| b.as_dict().cloned());
    let bw = bs.as_ref().and_then(|b| b.get(b"W")).and_then(|x| doc.resolve(x).as_f64()).unwrap_or(1.0).max(0.0);
    if let Some(bc) = color_array(doc, mk.as_ref(), b"BC")
        && bw > 0.0
    {
        let style = bs.as_ref().and_then(|b| b.name(b"S").map(<[u8]>::to_vec)).unwrap_or_default();
        if style == b"U" {
            // Underline: only the bottom edge.
            c.push_str(&format!("{}\n{} w\n0 {} m {} {} l S\n", stroke_op(&bc), n(bw), n(bw / 2.0), n(w), n(bw / 2.0)));
            return (c, bw);
        }
        let dashed = style == b"D";
        c.push_str(&format!(
            "{}\n{} w\n{}{} {} {} {} re S\n[] 0 d\n",
            stroke_op(&bc),
            n(bw),
            if dashed { "[3] 0 d\n" } else { "" },
            n(bw / 2.0),
            n(bw / 2.0),
            n(w - bw),
            n(h - bw)
        ));
        if style == b"B" || style == b"I" {
            // Beveled: a light top-left and a darker bottom-right inside the border; inset:
            // grey top-left and light grey bottom-right.
            let (tl, br) = if style == b"B" { ("1 g", "0.5 g") } else { ("0.5 g", "0.75 g") };
            let (x0, y0, x1, y1) = (bw, bw, w - bw, h - bw);
            let k = bw;
            c.push_str(&format!(
                "{tl}\n{} {} m {} {} l {} {} l {} {} l {} {} l {} {} l f\n{br}\n{} {} m {} {} l {} {} l {} {} l {} {} l {} {} l f\n",
                n(x0),
                n(y0),
                n(x0),
                n(y1),
                n(x1),
                n(y1),
                n(x1 - k),
                n(y1 - k),
                n(x0 + k),
                n(y1 - k),
                n(x0 + k),
                n(y0 + k),
                n(x1),
                n(y1),
                n(x1),
                n(y0),
                n(x0),
                n(y0),
                n(x0 + k),
                n(y0 + k),
                n(x1 - k),
                n(y0 + k),
                n(x1 - k),
                n(y1 - k)
            ));
            return (c, 2.0 * bw);
        }
        return (c, bw);
    }
    (c, 0.0)
}

/// The appearance of a text or choice field's widget showing `values`. Characters its font can't
/// hold come out as `?`; [`field_appearance_drawn`] says which.
pub fn field_appearance(doc: &Document, f: &Field, w: &Widget, values: &[String]) -> Stream {
    field_appearance_as(doc, f, w, values, true)
}

/// [`field_appearance`], where `format` false means `values` are already what to show (a
/// custom Format script ran).
pub fn field_appearance_as(doc: &Document, f: &Field, w: &Widget, values: &[String], format: bool) -> Stream {
    field_appearance_drawn(doc, f, w, values, format).0
}

/// [`field_appearance_as`] with the first character it drew as `?` because the field's font can't
/// hold it: the form's font is a simple (WinAnsi) font, Helvetica stands in for one PdfCraft can't
/// write with (an embedded subset, `Identity-H`), or a UCS-2 CMap meets a character beyond U+FFFF.
pub fn field_appearance_drawn(doc: &Document, f: &Field, w: &Widget, values: &[String], format: bool) -> (Stream, Option<char>) {
    // The Format event: what is shown, not what is stored.
    let formatted: Vec<String>;
    let values = if format
        && matches!(f.kind, crate::FieldKind::Text | crate::FieldKind::Combo)
        && values.len() == 1
        && f.actions.format != crate::af::Format::None
    {
        formatted = vec![crate::af::format_value(&f.actions.format, &values[0])];
        &formatted[..]
    } else {
        values
    };
    let wobj = doc.get(w.obj);
    let wd = wobj.as_dict().cloned().unwrap_or_default();
    let (page_w, page_h) = ((w.rect[2] - w.rect[0]).max(1.0), (w.rect[3] - w.rect[1]).max(1.0));
    let place = placement(mk_rotation(doc, &wd), page_w, page_h);
    let (width, height) = (place.layout_w, place.layout_h);
    let da = parse_da(wd.get(b"DA").and_then(|o| doc.resolve(o).as_string().map(|s| s.to_text())).as_deref().unwrap_or(&f.da));
    let text: String = if f.kind == FieldKind::Combo {
        values.iter().map(|v| f.options.iter().find(|(e, _)| e == v).map_or(v.clone(), |(_, d)| d.clone())).collect::<Vec<_>>().join(", ")
    } else {
        values.first().cloned().unwrap_or_default()
    };
    let text = if f.has(flags::PASSWORD) { "*".repeat(text.chars().count()) } else { text };
    // Thai is drawn as outlines whatever the font, so only the rest has to fit the font.
    let latin = |s: &str| -> String {
        if draws_thai(s) { thai_runs(s).into_iter().filter(|(thai, _)| !thai).map(|(_, run)| run).collect() } else { s.to_string() }
    };
    let can_show = |enc: &FontText| {
        if f.kind == FieldKind::List {
            f.options.iter().all(|(_, display)| enc.can_encode(&latin(display)))
        } else if f.kind == FieldKind::Text && f.has(flags::MULTILINE) {
            text.split(['\n', '\r']).all(|line| enc.can_encode(&latin(line)))
        } else {
            enc.can_encode(&latin(&text))
        }
    };
    let (font_name, font_obj, enc) = match dr_font(doc, &da.font).filter(|(_, enc)| can_show(enc)) {
        Some((o, enc)) => (da.font.clone(), o, enc),
        None => ("Helv".to_string(), helvetica(), FontText::WinAnsi),
    };
    let width_of = |text: &str, size: f64| text_width(&enc, text, size);
    let (mut c, bw) = frame(doc, &wd, width, height);
    let pad = 2.0 + bw;
    let inner_w = (width - 2.0 * pad).max(1.0);
    let q = wd.get(b"Q").and_then(|o| doc.resolve(o).as_int()).unwrap_or(f.quadding);
    let mut body: Vec<u8> = Vec::new();
    // The first character drawn that the font can't hold (drawn as `?`), for the caller to report.
    let mut unencodable: Option<char> = None;
    // Thai outlines, painted after the text object (paths can't be inside one).
    let mut outlines: Vec<u8> = Vec::new();
    let mut show = |body: &mut Vec<u8>, x: f64, y: f64, text: &str, size: f64| {
        // A line break in a single-line value is a space, as in viewers (multiline text is split).
        let text: String = text.chars().map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c }).collect();
        let mut tj = |body: &mut Vec<u8>, x: f64, run: &str| {
            if unencodable.is_none() {
                unencodable = enc.first_unencodable(run);
            }
            body.extend(format!("1 0 0 1 {} {} Tm ", n(x), n(y)).bytes());
            body.extend(literal(&enc.encode(run)));
            body.extend_from_slice(b" Tj\n");
        };
        if !draws_thai(&text) {
            tj(body, x, &text);
            return;
        }
        let mut pen = x;
        for (thai, run) in thai_runs(&text) {
            match thai_outlines(run, pen, y, size) {
                Some((paths, advance)) if thai => {
                    outlines.extend(paths);
                    pen += advance;
                }
                _ => {
                    tj(body, pen, run);
                    pen += enc.width(run, size);
                }
            }
        }
    };
    let wrap = |text: &str, size: f64| {
        if draws_thai(text) { wrap_thai(text, size, inner_w, width_of) } else { wrap_with(text, size, inner_w, width_of) }
    };
    let x_for = |text: &str, size: f64| -> f64 {
        let tw = width_of(text, size);
        match q {
            1 => pad + (inner_w - tw) / 2.0,
            2 => width - pad - tw,
            _ => pad,
        }
    };
    let mut size = da.size;
    match f.kind {
        FieldKind::List => {
            // Every option, one per line from the top; selected ones highlighted.
            if size == 0.0 {
                size = 12.0;
            }
            let line = size * 1.15;
            let top = wd.get(b"TI").and_then(|o| doc.resolve(o).as_int()).unwrap_or(0).max(0) as usize;
            let mut y = height - pad;
            for (export, display) in f.options.iter().skip(top) {
                if y - line < 0.0 {
                    break;
                }
                if values.contains(export) {
                    c.push_str(&format!("0.6 0.75 0.86 rg\n{} {} {} {} re f\n", n(bw), n(y - line), n(width - 2.0 * bw), n(line)));
                }
                show(&mut body, pad, y - line + size * 0.25, display, size);
                y -= line;
            }
        }
        _ => {
            let comb = f.has(flags::COMB) && !f.has(flags::MULTILINE) && !f.has(flags::PASSWORD) && f.max_len.is_some_and(|m| m > 0);
            if f.kind == FieldKind::Text && f.has(flags::MULTILINE) {
                if size == 0.0 {
                    // Auto size: the largest size (≤ 12) whose wrapped lines fit the height.
                    size = 12.0;
                    while size > 4.0 && wrap(&text, size).len() as f64 * size * 1.15 > height - 2.0 * pad {
                        size -= 0.5;
                    }
                }
                let mut y = height - pad - size * 0.85;
                for line in wrap(&text, size) {
                    if y < -size {
                        break;
                    }
                    show(&mut body, x_for(&line, size), y, &line, size);
                    y -= size * 1.15;
                }
            } else {
                if size == 0.0 {
                    size = ((height - 2.0 * pad) / 1.15).clamp(4.0, 12.0);
                    let tw = width_of(&text, size);
                    if tw > inner_w && !comb {
                        size = (size * inner_w / tw).max(4.0);
                    }
                }
                // Centre Helvetica's ascent (0.718) and descent (0.207) vertically.
                let y = (height - 0.925 * size) / 2.0 + 0.207 * size;
                if comb {
                    let cells = f.max_len.unwrap_or(1).max(1);
                    let cell = width / cells as f64;
                    for (i, ch) in text.chars().take(cells).enumerate() {
                        let s = ch.to_string();
                        show(&mut body, cell * i as f64 + (cell - width_of(&s, size)) / 2.0, y, &s, size);
                    }
                } else {
                    show(&mut body, x_for(&text, size), y, &text, size);
                }
            }
        }
    }
    let mut content = c.into_bytes();
    content.extend(
        format!(
            "/Tx BMC\nq\n{} {} {} {} re W n\nBT\n/{} {} Tf\n{}\n",
            n(bw),
            n(bw),
            n(width - 2.0 * bw),
            n(height - 2.0 * bw),
            font_name,
            n(size),
            da.color
        )
        .bytes(),
    );
    content.extend(body);
    content.extend_from_slice(b"ET\n");
    if !outlines.is_empty() {
        // The field's text colour, as a fill.
        content.extend(format!("{}\n", da.color).bytes());
        content.extend(outlines);
    }
    content.extend_from_slice(b"Q\nEMC\n");
    let mut fonts = Dict::new();
    fonts.set(font_name.into_bytes(), font_obj);
    let mut res = Dict::new();
    res.set(b"Font".to_vec(), Object::Dict(fonts));
    let mut d = Dict::new();
    d.set(b"Type".to_vec(), Object::name("XObject"));
    d.set(b"Subtype".to_vec(), Object::name("Form"));
    set_form_box(&mut d, place);
    d.set(b"Resources".to_vec(), Object::Dict(res));
    (Stream::flate(d, &content), unencodable)
}

/// On/Off appearances for a check box or radio button that has none (a check mark or a dot,
/// drawn as paths, so no symbol font is needed).
/// The streams are added to `doc` as indirect objects (streams can't be direct objects).
pub fn check_box_states(doc: &mut Document, w: &Widget, kind: FieldKind, on_name: &str) -> Dict {
    let wobj = doc.get(w.obj);
    let wd = wobj.as_dict().cloned().unwrap_or_default();
    let (page_w, page_h) = ((w.rect[2] - w.rect[0]).max(1.0), (w.rect[3] - w.rect[1]).max(1.0));
    let place = placement(mk_rotation(doc, &wd), page_w, page_h);
    let (width, height) = (place.layout_w, place.layout_h);
    let (frame_c, _) = frame(doc, &wd, width, height);
    let style = crate::author::CheckStyle::of_widget(doc, &wd, kind);
    let mut form = |content: String| -> Object {
        let mut d = Dict::new();
        d.set(b"Type".to_vec(), Object::name("XObject"));
        d.set(b"Subtype".to_vec(), Object::name("Form"));
        set_form_box(&mut d, place);
        Object::Ref(doc.add(Object::Stream(Stream::flate(d, content.as_bytes()))))
    };
    let s = width.min(height);
    let mark = check_mark(style, width / 2.0, height / 2.0, s);
    let mut nd = Dict::new();
    nd.set(crate::name_bytes(on_name), form(format!("{frame_c}{mark}")));
    nd.set(b"Off".to_vec(), form(frame_c));
    let mut ap = Dict::new();
    ap.set(b"N".to_vec(), Object::Dict(nd));
    ap
}

/// The on-state mark for `style` (#94), centred on (`cx`, `cy`) in a box whose smaller side is
/// `s`, drawn in black.
fn check_mark(style: crate::author::CheckStyle, cx: f64, cy: f64, s: f64) -> String {
    use crate::author::CheckStyle;
    // A filled polygon through `pts`.
    let polygon = |pts: &[(f64, f64)]| {
        let mut c = String::from("0 g\n");
        for (i, (x, y)) in pts.iter().enumerate() {
            c.push_str(&format!("{} {} {} ", n(*x), n(*y), if i == 0 { "m" } else { "l" }));
        }
        c.push_str("h f\n");
        c
    };
    let line = n((s * 0.1).max(1.0));
    match style {
        CheckStyle::Check => format!(
            "0 G\n{line} w 1 J 1 j\n{} {} m {} {} l {} {} l S\n",
            n(cx - s * 0.28),
            n(cy),
            n(cx - s * 0.08),
            n(cy - s * 0.22),
            n(cx + s * 0.3),
            n(cy + s * 0.25)
        ),
        CheckStyle::Cross => {
            let d = s * 0.25;
            format!(
                "0 G\n{line} w 1 J\n{} {} m {} {} l S\n{} {} m {} {} l S\n",
                n(cx - d),
                n(cy - d),
                n(cx + d),
                n(cy + d),
                n(cx - d),
                n(cy + d),
                n(cx + d),
                n(cy - d)
            )
        }
        CheckStyle::Square => {
            let d = s * 0.22;
            format!("0 g\n{} {} {} {} re f\n", n(cx - d), n(cy - d), n(2.0 * d), n(2.0 * d))
        }
        CheckStyle::Diamond => {
            let d = s * 0.3;
            polygon(&[(cx, cy + d), (cx + d, cy), (cx, cy - d), (cx - d, cy)])
        }
        CheckStyle::Star => {
            // Five points: outer and inner radii alternate, the first point straight up.
            let (outer, inner) = (s * 0.32, s * 0.32 * 0.382);
            let pts: Vec<(f64, f64)> = (0..10)
                .map(|i| {
                    let a = std::f64::consts::FRAC_PI_2 + i as f64 * std::f64::consts::PI / 5.0;
                    let r = if i % 2 == 0 { outer } else { inner };
                    (cx + r * a.cos(), cy + r * a.sin())
                })
                .collect();
            polygon(&pts)
        }
        CheckStyle::Circle => {
            let r = s * 0.25;
            let k = 0.5523 * r;
            format!(
                "0 g\n{} {} m {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c {} {} {} {} {} {} c f\n",
                n(cx + r),
                n(cy),
                n(cx + r),
                n(cy + k),
                n(cx + k),
                n(cy + r),
                n(cx),
                n(cy + r),
                n(cx - k),
                n(cy + r),
                n(cx - r),
                n(cy + k),
                n(cx - r),
                n(cy),
                n(cx - r),
                n(cy - k),
                n(cx - k),
                n(cy - r),
                n(cx),
                n(cy - r),
                n(cx + k),
                n(cy - r),
                n(cx + r),
                n(cy - k),
                n(cx + r),
                n(cy)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_appearance_strings_parse() {
        assert_eq!(parse_da("/Helv 0 Tf 0 g"), Da { font: "Helv".into(), size: 0.0, color: "0 g".into() });
        assert_eq!(parse_da("0.1 0.2 0.3 rg /F1 11 Tf"), Da { font: "F1".into(), size: 11.0, color: "0.1 0.2 0.3 rg".into() });
        assert_eq!(parse_da("/Cour 9 Tf 0 0 0 1 k").color, "0 0 0 1 k");
        assert_eq!(parse_da("garbage").font, "Helv");
        assert_eq!(stroke_op("1 0 0 rg"), "1 0 0 RG");
    }
}

#[cfg(test)]
mod thai_tests {
    use super::*;

    #[test]
    fn thai_runs_split_text_and_keep_marks_with_their_letters() {
        assert_eq!(thai_runs("ชื่อ น้ำ Test 12๓"), [(true, "ชื่อ"), (false, " "), (true, "น้ำ"), (false, " Test 12"), (true, "๓")]);
        assert!(thai_runs("").is_empty());
    }

    #[test]
    fn thai_is_drawn_as_outlines_and_wraps_between_words() {
        if pdfcraft_fonts::document_thai_font().is_none() {
            // Without the face Thai stays with the field's font, reported as before.
            assert!(!draws_thai("น้ำ") && text_width(&FontText::WinAnsi, "น้ำ", 10.0) == FontText::WinAnsi.width("น้ำ", 10.0));
            return;
        }
        assert!(draws_thai("ชื่อ A") && !draws_thai("Name"));
        let (paths, advance) = thai_outlines("น้ำใจ", 10.0, 20.0, 12.0).unwrap();
        let paths = String::from_utf8(paths).unwrap();
        assert!(
            paths.starts_with("q 12 0 0 12 10 20 cm\n") && paths.ends_with("f\nQ\n") && paths.contains(" m\n") && !paths.contains("Tj"),
            "{paths}"
        );
        assert!(advance > 12.0 && advance < 60.0, "{advance}");
        // Width: Thai by its advances, the rest in the field's font.
        let enc = FontText::WinAnsi;
        assert!((text_width(&enc, "น้ำใจ AB", 12.0) - (advance + enc.width(" AB", 12.0))).abs() < 1e-9);
        // Lines end between words, and every line fits.
        let text = "การพัฒนาซอฟต์แวร์เป็นกระบวนการที่เปิดโอกาสให้นักพัฒนาจากทั่วโลกเข้าร่วม\nบรรทัดสอง";
        let lines = wrap_thai(text, 12.0, 150.0, |s, size| text_width(&enc, s, size));
        assert!(lines.len() >= 4 && lines.last().map(String::as_str) == Some("บรรทัดสอง"), "{lines:?}");
        assert_eq!(lines.concat(), text.replace('\n', ""));
        for line in &lines {
            assert!(text_width(&enc, line, 12.0) <= 150.0, "{line}");
            assert!(!line.starts_with(|c: char| matches!(u32::from(c), 0x0E31 | 0x0E33..=0x0E3A | 0x0E47..=0x0E4E)), "{line}");
        }
    }
}
