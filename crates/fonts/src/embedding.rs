//! Unicode embedding of an explicitly supplied, embeddable TrueType face. Font files are
//! document inputs or the optional craft-fonts build input, never system-interface fallbacks.

use std::fmt::Write as _;

use pdfcraft_cos::{Dict, Document, Object, PdfString, Stream};
use skrifa::{
    FontRef, MetadataProvider,
    instance::{LocationRef, Size},
    raw::TableProvider,
};

/// Compare PDF subset/PostScript names with the full face's family name.
pub fn face_key(name: &str) -> String {
    name.rsplit('+').next().unwrap_or(name).chars().filter(char::is_ascii_alphanumeric).flat_map(char::to_lowercase).collect()
}

/// Full, approved faces of the source family. This never silently selects another family.
pub fn matching_faces(name: &str) -> Vec<&'static crate::CraftFont> {
    let key = face_key(name);
    crate::CRAFT_FONTS.iter().filter(|f| face_key(f.family) == key || face_key(&f.name()) == key).collect()
}

/// Whether a complete font program can show every non-newline character.
pub fn covers(bytes: &[u8], text: &str) -> bool {
    FontRef::new(bytes).is_ok_and(|f| text.chars().all(|c| matches!(c, '\n' | '\r') || f.charmap().map(c).is_some_and(|g| g.to_u32() != 0)))
}

/// Names come from the program, never a caller's guessed family.
pub fn names(bytes: &[u8]) -> Result<(String, String), String> {
    use skrifa::string::StringId;
    validate(bytes)?;
    let f = FontRef::new(bytes).map_err(|e| e.to_string())?;
    let name = |id| {
        f.localized_strings(id)
            .find(|s| s.language() == Some("en"))
            .or_else(|| f.localized_strings(id).next())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty() && s.len() <= 128)
    };
    let family = name(StringId::FAMILY_NAME).ok_or("the font has no family name")?;
    let postscript = name(StringId::POSTSCRIPT_NAME).unwrap_or_else(|| family.replace(' ', ""));
    Ok((family, postscript))
}

/// A selected external font is document input. Require its own open-licence declaration;
/// never turn a proprietary system-interface fallback into an exported PDF font.
pub fn validate_document_input(bytes: &[u8]) -> Result<(), String> {
    use skrifa::string::StringId;
    let (family, _) = names(bytes)?;
    let key = face_key(&family);
    if ["sourcehan", "sourceserif", "sourcesans", "notosanscjk", "notoserifcjk"].iter().any(|n| key.starts_with(n)) {
        return Err("this font family is excluded by the project's asset policy".into());
    }
    let f = FontRef::new(bytes).map_err(|e| e.to_string())?;
    let attribution: String =
        [StringId::COPYRIGHT_NOTICE, StringId::MANUFACTURER].into_iter().flat_map(|id| f.localized_strings(id)).map(|s| s.to_string()).collect();
    if attribution.to_ascii_lowercase().contains("adobe") {
        return Err("Adobe-authored font assets are excluded by the project's asset policy".into());
    }
    let license: String = f.localized_strings(StringId::LICENSE_DESCRIPTION).map(|s| s.to_string()).collect();
    if !["SIL OPEN FONT", "SIL Open Font", "Apache License", "Ubuntu Font Licence", "Bitstream Vera", "MIT License"]
        .iter()
        .any(|l| license.contains(l))
    {
        return Err("select an openly licensed font whose name table declares its licence (OFL, Apache, Ubuntu, Bitstream Vera or MIT)".into());
    }
    Ok(())
}

/// The default instance of a variable TrueType font as a static program: the same tables without
/// the variation data (`fvar`, `gvar`, `avar`, `HVAR`, `MVAR`, `STAT`, `cvar`). `glyf` and `hmtx`
/// already hold the default instance, and glyph ids do not change, so text shaped with the
/// variable font draws the same with this one. A static font comes back unchanged.
pub fn static_default(bytes: &[u8]) -> Result<Vec<u8>, String> {
    const VARIATION: [&[u8; 4]; 7] = [b"fvar", b"gvar", b"avar", b"HVAR", b"MVAR", b"STAT", b"cvar"];
    let bad = || String::from("the font's table directory is damaged");
    let u16_at = |at: usize| bytes.get(at..at.checked_add(2)?).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |at: usize| bytes.get(at..at.checked_add(4)?).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    if bytes.len() > 16 << 20 {
        return Err("the font exceeds 16 MiB".into());
    }
    let count = usize::from(u16_at(4).ok_or_else(bad)?);
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(count);
    let mut variable = false;
    for i in 0..count {
        let record = 12 + i * 16;
        let tag: [u8; 4] = bytes.get(record..record + 4).and_then(|t| t.try_into().ok()).ok_or_else(bad)?;
        if VARIATION.iter().any(|v| **v == tag) {
            variable = true;
            continue;
        }
        let (offset, length) = (u32_at(record + 8).ok_or_else(bad)? as usize, u32_at(record + 12).ok_or_else(bad)? as usize);
        let mut data = bytes.get(offset..offset.checked_add(length).ok_or_else(bad)?).ok_or_else(bad)?.to_vec();
        if &tag == b"head" {
            // checkSumAdjustment is computed over the file with this field zero.
            data.get_mut(8..12).ok_or_else(bad)?.fill(0);
        }
        tables.push((tag, data));
    }
    if !variable {
        return Ok(bytes.to_vec());
    }
    let sum = |data: &[u8]| {
        data.chunks(4).fold(0u32, |acc, c| {
            let mut word = [0u8; 4];
            for (w, b) in word.iter_mut().zip(c) {
                *w = *b;
            }
            acc.wrapping_add(u32::from_be_bytes(word))
        })
    };
    let n = u16::try_from(tables.len()).map_err(|_| bad())?;
    // The largest power of two not above the table count, as the binary-search header wants.
    let entry_selector = 15u16.saturating_sub(n.max(1).leading_zeros() as u16);
    let search_range = (1u16 << entry_selector).saturating_mul(16);
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(bytes.get(0..4).ok_or_else(bad)?);
    for v in [n, search_range, entry_selector, n.saturating_mul(16).saturating_sub(search_range)] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut offset = 12 + tables.len() * 16;
    let mut head_at = None;
    for (tag, data) in &tables {
        out.extend_from_slice(tag);
        out.extend_from_slice(&sum(data).to_be_bytes());
        out.extend_from_slice(&u32::try_from(offset).map_err(|_| bad())?.to_be_bytes());
        out.extend_from_slice(&u32::try_from(data.len()).map_err(|_| bad())?.to_be_bytes());
        if tag == b"head" {
            head_at = Some(offset);
        }
        offset += data.len().div_ceil(4) * 4;
    }
    for (_, data) in &tables {
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(4) * 4, 0);
    }
    let adjustment = 0xB1B0_AFBAu32.wrapping_sub(sum(&out));
    if let Some(field) = head_at.and_then(|at| out.get_mut(at + 8..at + 12)) {
        field.copy_from_slice(&adjustment.to_be_bytes());
    }
    Ok(out)
}

/// Validate before allocating any PDF objects, so an invalid font cannot partly mutate a doc.
pub fn validate(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > 16 << 20 {
        return Err("the selected font exceeds 16 MiB".into());
    }
    let font = FontRef::new(bytes).map_err(|_| "select a standalone TrueType/OpenType font (not a font collection)")?;
    if font.fvar().is_ok() {
        return Err("select a static instance of this variable font before embedding".into());
    }
    if (font.glyf().is_err() && font.cff().is_err()) || font.head().is_err() || font.hhea().is_err() || font.hmtx().is_err() {
        return Err("the selected font has no usable TrueType/CFF outlines and metrics".into());
    }
    if font.os2().is_ok_and(|t| t.fs_type() & (0x0002 | 0x0004 | 0x0200) != 0) {
        return Err("this font's embedding permissions prohibit editable outline embedding".into());
    }
    let m = font.metrics(Size::unscaled(), LocationRef::default());
    if m.units_per_em == 0 || font.charmap().mappings().next().is_none() {
        return Err("the selected font has no Unicode mapping or valid units per em".into());
    }
    if font.charmap().mappings().take(65_535).count() >= 65_535 {
        return Err("the selected font has too many Unicode mappings for one PDF font".into());
    }
    if font.charmap().mappings().any(|(_, g)| g.to_u32() >= u32::from(m.glyph_count)) {
        return Err("the selected font maps Unicode to a glyph outside its program".into());
    }
    Ok(())
}

fn stream(doc: &mut Document, bytes: Vec<u8>) -> Object {
    Object::Ref(doc.add(Object::Stream(Stream::from_raw(Dict::new(), bytes))))
}

/// Embed the complete font once and provide separate CIDs for Unicode aliases. /ToUnicode,
/// /CIDToGIDMap and widths agree, including supplementary characters, so reopening needs no
/// installed font. Keeping the full program also lets subsequent edits add other glyphs.
pub fn embed(doc: &mut Document, fonts: &mut Dict, bytes: &[u8], family: &str) -> Result<String, String> {
    validate(bytes)?;
    let font = FontRef::new(bytes).map_err(|e| e.to_string())?;
    let mappings: Vec<_> = font
        .charmap()
        .mappings()
        .filter(|(_, gid)| gid.to_u32() > 0)
        .filter_map(|(cp, gid)| char::from_u32(cp).map(|c| (c.to_string(), gid)))
        .collect();
    embed_mapping(doc, fonts, bytes, family, &mappings)
}

/// Explicit glyph mapping for shaped text. A CID can represent a contextual glyph or a
/// ligature, while its ToUnicode retains the logical cluster. The full source program stays exact.
pub fn embed_mapping(
    doc: &mut Document,
    fonts: &mut Dict,
    bytes: &[u8],
    family: &str,
    mappings: &[(String, skrifa::GlyphId)],
) -> Result<String, String> {
    validate(bytes)?;
    if mappings.len() > 65_534 || mappings.iter().any(|(s, _)| s.len() > 4096) {
        return Err("shaped font mapping exceeds its budget".into());
    }
    let font = FontRef::new(bytes).map_err(|e| e.to_string())?;
    let cff = font.glyf().is_err();
    let loc = LocationRef::default();
    let m = font.metrics(Size::unscaled(), loc);
    let scale = 1000.0 / f64::from(m.units_per_em);
    let advances = font.glyph_metrics(Size::unscaled(), loc);
    let mut cid_encoding = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CMapName /PCGlyphs def\n/CMapType 1 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let mut cff_widths = std::collections::BTreeMap::new();
    let mut gids = vec![0, 0];
    let mut widths = Vec::with_capacity(mappings.len());
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CMapName /PCUnicode def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    for (chunk_index, chunk) in mappings.chunks(100).enumerate() {
        let _ = writeln!(cmap, "{} beginbfchar", chunk.len());
        let _ = writeln!(cid_encoding, "{} begincidchar", chunk.len());
        for (offset, (unicode, gid)) in chunk.iter().enumerate() {
            let cid = u16::try_from(chunk_index.saturating_mul(100).saturating_add(offset).saturating_add(1)).map_err(|_| "font mapping limit")?;
            let glyph = u16::try_from(gid.to_u32()).map_err(|_| "font glyph limit")?;
            if glyph == 0 || u32::from(glyph) >= u32::from(m.glyph_count) {
                return Err("font contains an unavailable shaped glyph".into());
            }
            let width = advances.advance_width(*gid).unwrap_or(0.0) as f64 * scale;
            if !width.is_finite() || !(0.0..=16_000.0).contains(&width) {
                return Err("font contains an invalid advance".into());
            }
            gids.extend_from_slice(&glyph.to_be_bytes());
            widths.push(Object::Real(width));
            cff_widths.insert(glyph, width);
            let _ = writeln!(cid_encoding, "<{cid:04X}> {glyph}");
            let hex: String = unicode.encode_utf16().map(|u| format!("{u:04X}")).collect();
            let _ = writeln!(cmap, "<{cid:04X}> <{hex}>");
        }
        cmap.push_str("endbfchar\n");
        cid_encoding.push_str("endcidchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    cid_encoding.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    let name = family.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect::<String>();
    if name.is_empty() || name.len() > 128 {
        return Err("the font family name is invalid".into());
    }
    let mut descriptor = Dict::new();
    descriptor.set(b"Type".to_vec(), Object::name("FontDescriptor"));
    descriptor.set(b"FontName".to_vec(), Object::name(&name));
    descriptor.set(b"FontFamily".to_vec(), Object::String(PdfString::literal(family.as_bytes().to_vec())));
    descriptor.set(b"Flags".to_vec(), Object::Int(4));
    descriptor.set(b"ItalicAngle".to_vec(), Object::Real(f64::from(m.italic_angle)));
    descriptor.set(b"Ascent".to_vec(), Object::Real(f64::from(m.ascent) * scale));
    descriptor.set(b"Descent".to_vec(), Object::Real(f64::from(m.descent) * scale));
    descriptor.set(b"CapHeight".to_vec(), Object::Real(f64::from(m.cap_height.unwrap_or(m.ascent)) * scale));
    descriptor.set(b"StemV".to_vec(), Object::Int(80));
    let bounds = m.bounds.ok_or("font has no outline bounds")?;
    descriptor.set(
        b"FontBBox".to_vec(),
        Object::Array([bounds.x_min, bounds.y_min, bounds.x_max, bounds.y_max].map(|v| Object::Real(f64::from(v) * scale)).to_vec()),
    );
    // Reuse a full program registered on another page; source subset programs stay untouched.
    let existing = doc.scan_objects().take(8192).find_map(|(r, o)| match o.as_ref() {
        Object::Stream(s) if s.dict.get(b"PCFullFont").is_some() && s.raw.as_ref() == bytes => Some(Object::Ref(r)),
        _ => None,
    });
    let program = existing.unwrap_or_else(|| {
        let mut d = Dict::new();
        d.set(b"Length1".to_vec(), Object::Int(bytes.len() as i64));
        if cff {
            d.set(b"Subtype".to_vec(), Object::name("OpenType"));
        }
        d.set(b"PCFullFont".to_vec(), Object::Bool(true));
        Object::Ref(doc.add(Object::Stream(Stream::from_raw(d, bytes.to_vec()))))
    });
    descriptor.set(if cff { b"FontFile3".to_vec() } else { b"FontFile2".to_vec() }, program);
    let desc = Object::Ref(doc.add(Object::Dict(descriptor)));
    let mut system = Dict::new();
    system.set(b"Registry".to_vec(), Object::String(PdfString::literal(b"Adobe".to_vec())));
    system.set(b"Ordering".to_vec(), Object::String(PdfString::literal(b"Identity".to_vec())));
    system.set(b"Supplement".to_vec(), Object::Int(0));
    let mut cid = Dict::new();
    cid.set(b"Type".to_vec(), Object::name("Font"));
    cid.set(b"Subtype".to_vec(), Object::name(if cff { "CIDFontType0" } else { "CIDFontType2" }));
    cid.set(b"BaseFont".to_vec(), Object::name(&name));
    cid.set(b"CIDSystemInfo".to_vec(), Object::Dict(system));
    cid.set(b"FontDescriptor".to_vec(), desc);
    if cff {
        cid.set(
            b"W".to_vec(),
            Object::Array(cff_widths.into_iter().flat_map(|(gid, w)| [Object::Int(i64::from(gid)), Object::Array(vec![Object::Real(w)])]).collect()),
        );
    } else {
        cid.set(b"CIDToGIDMap".to_vec(), stream(doc, gids));
        cid.set(b"W".to_vec(), Object::Array(vec![Object::Int(1), Object::Array(widths)]));
    }
    let descendant = Object::Ref(doc.add(Object::Dict(cid)));
    let mut top = Dict::new();
    top.set(b"Type".to_vec(), Object::name("Font"));
    top.set(b"Subtype".to_vec(), Object::name("Type0"));
    top.set(b"BaseFont".to_vec(), Object::name(&name));
    top.set(b"Encoding".to_vec(), if cff { stream(doc, cid_encoding.into_bytes()) } else { Object::name("Identity-H") });
    top.set(b"DescendantFonts".to_vec(), Object::Array(vec![descendant]));
    top.set(b"ToUnicode".to_vec(), stream(doc, cmap.into_bytes()));
    let mut index = 0usize;
    while fonts.contains(format!("PCFull{index}").as_bytes()) {
        index = index.saturating_add(1);
    }
    let resource = format!("PCFull{index}");
    fonts.set(resource.as_bytes().to_vec(), Object::Ref(doc.add(Object::Dict(top))));
    Ok(resource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::Metrics;

    #[test]
    fn full_unicode_embedding_keeps_outlines_advances_and_roundtrip_codes() {
        let bytes = include_bytes!("../../../assets/fonts/Inter-Regular.ttf");
        let mut doc = Document::new_empty();
        let mut fonts = Dict::new();
        let name = embed(&mut doc, &mut fonts, bytes, "Inter").unwrap();
        let font = doc.dict(fonts.get(name.as_bytes()).unwrap()).unwrap();
        let m = Metrics::from_dict(&doc, &font);
        let text = "Aé ΩЖ €";
        let codes = m.encode(text).unwrap();
        assert_eq!(m.decode(&codes), text);
        assert_eq!(crate::pdf::embedded_sfnt(&doc, &font).unwrap(), bytes);
        let sfnt = FontRef::new(bytes).unwrap();
        for (ch, (code, _)) in text.chars().zip(m.codes(&codes)) {
            let gid = sfnt.charmap().map(ch).unwrap();
            let w = sfnt.glyph_metrics(Size::unscaled(), LocationRef::default()).advance_width(gid).unwrap() as f64
                / f64::from(sfnt.metrics(Size::unscaled(), LocationRef::default()).units_per_em);
            assert!((m.width(code) - w).abs() < 1e-8);
        }
        let before = doc.object_numbers().len();
        assert!(embed(&mut doc, &mut fonts, b"broken font", "Broken").is_err());
        assert!(embed_mapping(&mut doc, &mut fonts, bytes, "Inter", &[("X".into(), skrifa::GlyphId::new(65_535))]).is_err());
        assert_eq!(doc.object_numbers().len(), before);
        let programs = || doc.scan_objects().filter(|(_, o)| matches!(o.as_ref(), Object::Stream(s) if s.dict.get(b"PCFullFont").is_some())).count();
        assert_eq!(programs(), 1);
        let mut second = Dict::new();
        embed(&mut doc, &mut second, bytes, "Inter").unwrap();
        assert_eq!(doc.scan_objects().filter(|(_, o)| matches!(o.as_ref(), Object::Stream(s) if s.dict.get(b"PCFullFont").is_some())).count(), 1);
        assert_eq!(face_key("ABCDEF+Droid-Sans-Fallback"), face_key("Droid Sans Fallback"));
    }
}

#[cfg(test)]
mod static_default_tests {
    use super::*;

    #[test]
    fn the_thai_variable_face_becomes_an_embeddable_static_font_with_the_same_glyphs() {
        // Damaged or truncated input is refused, never a panic.
        assert!(static_default(b"").is_err() && static_default(&[0, 1, 0, 0, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0]).is_err());
        let Some(face) = crate::document_thai_font() else {
            assert!(crate::document_thai_program().is_none());
            return;
        };
        let fixed = static_default(face.bytes).unwrap();
        validate(&fixed).unwrap();
        let (variable, fixed_font) = (FontRef::new(face.bytes).unwrap(), FontRef::new(&fixed).unwrap());
        assert!(fixed_font.fvar().is_err() && fixed_font.gsub().is_ok() && fixed_font.gpos().is_ok());
        for c in "กขค้ำิู่๙A1 ".chars() {
            assert_eq!(variable.charmap().map(c), fixed_font.charmap().map(c), "{c}");
        }
        assert!(covers(&fixed, "สวัสดี น้ำ ผู้ใช้"));
        // A static font is returned as it is.
        assert_eq!(static_default(&fixed).unwrap(), fixed);
        assert_eq!(crate::document_thai_program(), Some(fixed.as_slice()));
    }
}
