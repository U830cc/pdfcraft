//! Where a line of Thai may wrap.
//!
//! Thai is written without spaces between words, so a wrap needs a word boundary found by a
//! language model. ICU4X's line segmenter (UAX #14 with its Thai model, compiled into the
//! binary) supplies them.

use std::sync::OnceLock;

use icu_segmenter::{LineSegmenter, LineSegmenterBorrowed, options::LineBreakOptions};

fn segmenter() -> &'static LineSegmenterBorrowed<'static> {
    static SEGMENTER: OnceLock<LineSegmenterBorrowed<'static>> = OnceLock::new();
    SEGMENTER.get_or_init(|| LineSegmenter::new_auto(LineBreakOptions::default()))
}

/// Whether `c` is in the Thai block.
pub fn is_thai(c: char) -> bool {
    matches!(u32::from(c), 0x0E00..=0x0E7F)
}

/// The pieces `text` may wrap between, in order; joined they give `text` back. Each piece is a
/// word (with the spaces and punctuation that stay on its line), never a part of a Thai word or
/// of a cluster. Text over 64 KiB comes back whole.
pub fn wrap_pieces(text: &str) -> Vec<&str> {
    if text.len() > 65_536 {
        return vec![text];
    }
    // The model reports a space after Thai as its own segment; it stays on the word's line.
    let mut ends: Vec<usize> = Vec::new();
    let mut start = 0;
    for end in segmenter().segment_str(text).chain(std::iter::once(text.len())) {
        let Some(piece) = text.get(start..end).filter(|p| !p.is_empty()) else { continue };
        if piece.chars().all(char::is_whitespace) && !ends.is_empty() {
            ends.pop();
        }
        ends.push(end);
        start = end;
    }
    let mut start = 0;
    ends.into_iter()
        .filter_map(|end| {
            let piece = text.get(start..end)?;
            start = end;
            Some(piece)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thai_wraps_between_words_and_never_inside_a_cluster() {
        let text = "การพัฒนาซอฟต์แวร์เป็นกระบวนการที่เปิดโอกาสให้นักพัฒนาจากทั่วโลกเข้าร่วม";
        let pieces = wrap_pieces(text);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.len() > 8, "{pieces:?}");
        for word in ["การ", "พัฒนา", "กระบวนการ", "โอกาส", "จาก", "ทั่ว", "โลก"] {
            assert!(pieces.contains(&word), "{word} in {pieces:?}");
        }
        // No piece starts with a mark that belongs to the letter before it.
        assert!(pieces.iter().all(|p| !p.starts_with(|c: char| matches!(u32::from(c), 0x0E31 | 0x0E33..=0x0E3A | 0x0E47..=0x0E4E))), "{pieces:?}");
        assert_eq!(wrap_pieces("น้ำ ผู้ใช้ Hello 123"), ["น้ำ ", "ผู้", "ใช้ ", "Hello ", "123"]);
        assert!(wrap_pieces("").is_empty());
    }
}
