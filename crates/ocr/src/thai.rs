//! Thai text recognition: PP-OCRv5's text-line detector and its Thai recogniser (Apache-2.0),
//! run with rten from their ONNX files.
//!
//! The detector marks the pixels that belong to text; each connected patch of them is a line.
//! The recogniser reads one line at a time, 48 pixels high, and names a character (or nothing)
//! for each step across it. Its alphabet is Thai with the Latin letters, digits and punctuation
//! that Thai documents mix in.

use std::path::{Path, PathBuf};

use rten::Model;
use rten_tensor::NdTensor;
use rten_tensor::prelude::*;

use crate::{Line, OcrError, Word};

/// The model files, as [`ThaiModels::in_dir`] looks for them.
pub const THAI_DETECTION_MODEL: &str = "thai-detection.onnx";
pub const THAI_RECOGNITION_MODEL: &str = "thai-recognition.onnx";
/// The recogniser's own configuration file (PaddleOCR's `inference.yml`), which lists its
/// alphabet in class order under `character_dict`.
pub const THAI_ALPHABET: &str = "thai-recognition.yml";

/// The longest side the detector sees (pixels); larger pages are scaled down for it.
const DETECT_SIDE: f32 = 960.0;
/// The line height the recogniser reads, and the widest line it is given.
const LINE_HEIGHT: usize = 48;
const MAX_LINE_WIDTH: usize = 3200;
/// More lines than this on a page are not read.
const MAX_LINES: usize = 2000;
/// A pixel is text above this probability; a patch is a line above this mean.
const PIXEL_THRESHOLD: f32 = 0.3;
const LINE_THRESHOLD: f32 = 0.6;
/// How far a patch is grown to its line's full box (the detector marks a shrunken core).
const UNCLIP: f32 = 1.5;

/// Where the Thai model files are.
#[derive(Clone, Debug, PartialEq)]
pub struct ThaiModels {
    pub detection: PathBuf,
    pub recognition: PathBuf,
    pub alphabet: PathBuf,
}

impl ThaiModels {
    /// The models in `dir`, if all three files are there.
    pub fn in_dir(dir: &Path) -> Option<ThaiModels> {
        let m = ThaiModels {
            detection: dir.join(THAI_DETECTION_MODEL),
            recognition: dir.join(THAI_RECOGNITION_MODEL),
            alphabet: dir.join(THAI_ALPHABET),
        };
        (m.detection.is_file() && m.recognition.is_file() && m.alphabet.is_file()).then_some(m)
    }

    /// Look where [`crate::Models::find`] looks.
    pub fn find() -> Option<ThaiModels> {
        crate::Models::search_dirs().iter().find_map(|d| Self::in_dir(d))
    }
}

/// A loaded Thai recogniser.
pub struct ThaiOcr {
    detection: Model,
    recognition: Model,
    /// Class `i + 1` is `alphabet[i]` (class 0 is "nothing here").
    alphabet: Vec<String>,
}

/// An image as rows of RGB bytes.
struct Rgb<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
}

impl Rgb<'_> {
    /// The pixel at (`x`, `y`), clamped to the image; white when the data is short.
    fn at(&self, x: usize, y: usize) -> [f32; 3] {
        let (x, y) = (x.min(self.width.saturating_sub(1)), y.min(self.height.saturating_sub(1)));
        let i = (y * self.width + x) * 3;
        match self.data.get(i..i + 3) {
            Some(p) => [f32::from(p[0]), f32::from(p[1]), f32::from(p[2])],
            None => [255.0; 3],
        }
    }

    /// The region `[left, top, right, bottom]` resampled to `w` × `h`, as planes in blue, green,
    /// red order (how the models were trained), each value mapped by `norm(channel, value)`.
    fn planes(&self, region: [f32; 4], w: usize, h: usize, norm: impl Fn(usize, f32) -> f32) -> Vec<f32> {
        let [left, top, right, bottom] = region;
        let (sx, sy) = ((right - left) / w.max(1) as f32, (bottom - top) / h.max(1) as f32);
        let mut out = vec![0f32; 3 * w * h];
        for y in 0..h {
            // Bilinear, sampling pixel centres.
            let fy = (top + (y as f32 + 0.5) * sy - 0.5).max(0.0);
            let (y0, ty) = (fy.floor() as usize, fy.fract());
            for x in 0..w {
                let fx = (left + (x as f32 + 0.5) * sx - 0.5).max(0.0);
                let (x0, tx) = (fx.floor() as usize, fx.fract());
                let (a, b, c, d) = (self.at(x0, y0), self.at(x0 + 1, y0), self.at(x0, y0 + 1), self.at(x0 + 1, y0 + 1));
                for plane in 0..3 {
                    let ch = 2 - plane;
                    let v = (a[ch] * (1.0 - tx) + b[ch] * tx) * (1.0 - ty) + (c[ch] * (1.0 - tx) + d[ch] * tx) * ty;
                    if let Some(slot) = out.get_mut(plane * w * h + y * w + x) {
                        *slot = norm(plane, v);
                    }
                }
            }
        }
        out
    }
}

impl ThaiOcr {
    pub fn load(models: &ThaiModels) -> Result<ThaiOcr, OcrError> {
        let load = |p: &Path| Model::load_file(p).map_err(|e| OcrError::Load(p.display().to_string(), e.to_string()));
        let text = std::fs::read_to_string(&models.alphabet).map_err(|e| OcrError::Load(models.alphabet.display().to_string(), e.to_string()))?;
        let alphabet = character_dict(&text);
        if alphabet.len() < 50 || alphabet.len() > 50_000 {
            return Err(OcrError::Load(models.alphabet.display().to_string(), "not a recognition alphabet".into()));
        }
        Ok(ThaiOcr { detection: load(&models.detection)?, recognition: load(&models.recognition)?, alphabet })
    }

    /// The text lines of an RGB image, top to bottom. Each line is one [`Word`] holding the whole
    /// line, so its characters stay in reading order; its [`Word::stops`] say where each of its
    /// words starts (Thai words are found by the language model in `pdfcraft_fonts::thai_break`,
    /// as there are no spaces between them).
    pub fn recognize(&self, rgb: &[u8], width: u32, height: u32) -> Result<Vec<Line>, OcrError> {
        let image = Rgb { data: rgb, width: width as usize, height: height as usize };
        if image.width == 0 || image.height == 0 || rgb.len() < image.width.saturating_mul(image.height).saturating_mul(3) {
            return Err(OcrError::EmptyImage);
        }
        let mut lines = Vec::new();
        for rect in self.detect(&image)? {
            if let Some(line) = self.read(&image, rect)? {
                lines.push(Line { words: vec![line] });
            }
        }
        Ok(lines)
    }

    /// The boxes of the image's text lines `[left, top, right, bottom]`, top to bottom.
    fn detect(&self, image: &Rgb) -> Result<Vec<[f32; 4]>, OcrError> {
        let err = |e: &dyn std::fmt::Display| OcrError::Recognize(e.to_string());
        let (iw, ih) = (image.width as f32, image.height as f32);
        let k = (DETECT_SIDE / iw.max(ih)).min(1.0);
        // The detector wants sides that are multiples of 32.
        let side = |v: f32| (((v * k) / 32.0).round().max(1.0) * 32.0) as usize;
        let (w, h) = (side(iw), side(ih));
        const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
        const STD: [f32; 3] = [0.229, 0.224, 0.225];
        let data =
            image.planes([0.0, 0.0, iw, ih], w, h, |c, v| (v / 255.0 - MEAN.get(c).copied().unwrap_or(0.5)) / STD.get(c).copied().unwrap_or(0.25));
        let input = NdTensor::from_data([1, 3, h, w], data);
        let out: NdTensor<f32, 4> = self.detection.run_one(input.view().into(), None).map_err(|e| err(&e))?.try_into().map_err(|e| err(&e))?;
        let [_, _, mh, mw] = out.shape();
        let map = out.to_vec();
        if mh == 0 || mw == 0 || map.len() < mh * mw {
            return Ok(Vec::new());
        }
        let text = |i: usize| map.get(i).is_some_and(|p| *p >= PIXEL_THRESHOLD);
        let mut seen = vec![false; mh * mw];
        let mut boxes: Vec<[f32; 4]> = Vec::new();
        let mut stack = Vec::new();
        for start in 0..mh * mw {
            if seen.get(start).copied().unwrap_or(true) || !text(start) {
                continue;
            }
            // One connected patch of text pixels.
            let (mut x0, mut y0, mut x1, mut y1) = (start % mw, start / mw, start % mw, start / mw);
            let (mut sum, mut count) = (0f32, 0usize);
            stack.push(start);
            if let Some(s) = seen.get_mut(start) {
                *s = true;
            }
            while let Some(i) = stack.pop() {
                let (x, y) = (i % mw, i / mw);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
                sum += map.get(i).copied().unwrap_or(0.0);
                count += 1;
                let near = [(x > 0).then(|| i - 1), (x + 1 < mw).then(|| i + 1), (y > 0).then(|| i - mw), (y + 1 < mh).then(|| i + mw)];
                for n in near.into_iter().flatten() {
                    if text(n)
                        && let Some(s) = seen.get_mut(n).filter(|s| !**s)
                    {
                        *s = true;
                        stack.push(n);
                    }
                }
            }
            let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
            if count < 10 || sum / (count as f32) < LINE_THRESHOLD || bw.min(bh) < 3.0 {
                continue;
            }
            let grow = bw * bh * UNCLIP / (2.0 * (bw + bh));
            let (kx, ky) = (iw / mw as f32, ih / mh as f32);
            boxes.push([
                ((x0 as f32 - grow) * kx).max(0.0),
                ((y0 as f32 - grow) * ky).max(0.0),
                ((x1 as f32 + 1.0 + grow) * kx).min(iw),
                ((y1 as f32 + 1.0 + grow) * ky).min(ih),
            ]);
            if boxes.len() >= MAX_LINES {
                break;
            }
        }
        // Reading order: by rows (boxes whose centres share a band), each left to right.
        boxes.sort_by(|a, b| {
            let (ca, cb) = ((a[1] + a[3]) / 2.0, (b[1] + b[3]) / 2.0);
            let band = (a[3] - a[1]).min(b[3] - b[1]) / 2.0;
            if (ca - cb).abs() < band { a[0].total_cmp(&b[0]) } else { ca.total_cmp(&cb) }
        });
        Ok(boxes)
    }

    /// The line in `rect`, with where its words start. `None` if nothing was read.
    fn read(&self, image: &Rgb, rect: [f32; 4]) -> Result<Option<Word>, OcrError> {
        let err = |e: &dyn std::fmt::Display| OcrError::Recognize(e.to_string());
        let (w, h) = (rect[2] - rect[0], rect[3] - rect[1]);
        if !(w >= 2.0 && h >= 2.0) {
            return Ok(None);
        }
        let width = ((LINE_HEIGHT as f32 * w / h).ceil() as usize).clamp(16, MAX_LINE_WIDTH);
        let data = image.planes(rect, width, LINE_HEIGHT, |_, v| (v / 255.0 - 0.5) / 0.5);
        let input = NdTensor::from_data([1, 3, LINE_HEIGHT, width], data);
        let out: NdTensor<f32, 3> = self.recognition.run_one(input.view().into(), None).map_err(|e| err(&e))?.try_into().map_err(|e| err(&e))?;
        let [_, steps, classes] = out.shape();
        let scores = out.to_vec();
        Ok(line(&decode(&scores, steps, classes, &self.alphabet), steps, rect))
    }
}

/// A recognised line with where each of its words starts. `read` holds the line's characters
/// with the step (of `steps` across `rect`) each was read at.
///
/// A character is read near its middle, so a word starts half an average character before the
/// step of its first one. The first word starts at the line's left end.
fn line(read: &[(String, usize)], steps: usize, rect: [f32; 4]) -> Option<Word> {
    let full: String = read.iter().map(|(s, _)| s.as_str()).collect();
    let text = full.trim();
    let visible = read.iter().filter(|(s, _)| !s.trim().is_empty()).count();
    if visible == 0 || steps == 0 {
        return None;
    }
    let lead = full.len() - full.trim_start().len();
    let width = rect[2] - rect[0];
    let half = width / visible as f32 / 2.0;
    // Where the character starting at byte `at` of `full` begins on the page.
    let mut starts: Vec<(usize, f32)> = Vec::with_capacity(read.len());
    let mut at = 0usize;
    for (s, step) in read {
        starts.push((at, rect[0] + (*step as f32 + 0.5) / steps as f32 * width - half));
        at += s.len();
    }
    let mut stops: Vec<(usize, f32)> = Vec::new();
    let mut at = 0usize;
    for piece in pdfcraft_fonts::thai_break::wrap_pieces(text) {
        let start = at;
        at += piece.len();
        if piece.trim().is_empty() {
            continue;
        }
        let x = starts.iter().find(|(byte, _)| *byte >= start + lead).map_or(rect[2], |(_, x)| *x);
        // Ascending, inside the line, and the first word at its left end.
        let x = if stops.is_empty() { rect[0] } else { x.clamp(stops.last().map_or(rect[0], |s| s.1), rect[2]) };
        stops.push((start, x));
    }
    Some(Word { text: text.to_string(), rect, stops })
}

/// The `character_dict` list of a PaddleOCR `inference.yml`: the entries of the YAML sequence
/// that follows that key, unquoted (plain, `'single'` with `''` for a quote, or `"double"` with
/// backslash escapes). Empty when the file has no such list.
fn character_dict(yml: &str) -> Vec<String> {
    let mut lines = yml.lines().skip_while(|l| l.trim() != "character_dict:").skip(1);
    let mut out = Vec::new();
    for line in lines.by_ref() {
        let Some(entry) = line.trim_start().strip_prefix("- ") else { break };
        let entry = entry.trim_end_matches('\r');
        let plain = if let Some(inner) = entry.strip_prefix('\'').and_then(|e| e.strip_suffix('\'')).filter(|_| entry.len() >= 2) {
            inner.replace("''", "'")
        } else if let Some(inner) = entry.strip_prefix('"').and_then(|e| e.strip_suffix('"')).filter(|_| entry.len() >= 2) {
            let mut text = String::new();
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    text.push(c);
                    continue;
                }
                match chars.next() {
                    Some('u') => {
                        let hex: String = chars.by_ref().take(4).collect();
                        text.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
                    }
                    Some('n') => text.push('\n'),
                    Some('t') => text.push('\t'),
                    Some(other) => text.push(other),
                    None => {}
                }
            }
            text
        } else {
            entry.to_string()
        };
        out.push(plain);
        if out.len() > 50_000 {
            break;
        }
    }
    out
}

/// The characters of a recogniser output with the step each was read at: at each of `steps` the
/// best of `classes` (class 0 is "nothing"; a class repeated over neighbouring steps is one
/// character). The class after the alphabet's last is a space.
fn decode(scores: &[f32], steps: usize, classes: usize, alphabet: &[String]) -> Vec<(String, usize)> {
    let mut read = Vec::new();
    let mut last = 0usize;
    for (at, step) in scores.chunks(classes.max(1)).take(steps).enumerate() {
        let best = step.iter().enumerate().fold((0usize, f32::MIN), |b, (i, p)| if *p > b.1 { (i, *p) } else { b }).0;
        if best != 0 && best != last {
            read.push((alphabet.get(best - 1).map_or(" ", String::as_str).to_string(), at));
        }
        last = best;
    }
    read
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_collapses_repeats_and_skips_blanks() {
        let alphabet: Vec<String> = ["ก", "า", "ร"].iter().map(|s| s.to_string()).collect();
        // Steps: ก ก _ า ร ร _ ร (space) and a class past the alphabet.
        let classes = 5;
        let step = |best: usize| (0..classes).map(|c| if c == best { 1.0 } else { 0.0 }).collect::<Vec<f32>>();
        let scores: Vec<f32> = [1, 1, 0, 2, 3, 3, 0, 3, 4].iter().flat_map(|b| step(*b)).collect();
        let text = |read: Vec<(String, usize)>| read.into_iter().map(|(s, _)| s).collect::<String>();
        let read = decode(&scores, 9, classes, &alphabet);
        assert_eq!(read.iter().map(|(_, at)| *at).collect::<Vec<_>>(), [0, 3, 4, 7, 8]);
        assert_eq!(text(read), "การร ");
        assert!(decode(&[], 0, 0, &alphabet).is_empty());
        assert_eq!(text(decode(&scores, 2, classes, &alphabet)), "ก");
    }

    #[test]
    fn a_line_keeps_its_text_whole_and_marks_where_each_word_starts() {
        // "ราคา 90 บาท" read at evenly spaced steps across a line 0..220 wide.
        let read: Vec<(String, usize)> = " ราคา 90 บาท ".chars().enumerate().map(|(i, c)| (c.to_string(), i * 2)).collect();
        let found = line(&read, 26, [0.0, 10.0, 260.0, 40.0]).unwrap();
        assert_eq!((found.text.as_str(), found.rect), ("ราคา 90 บาท", [0.0, 10.0, 260.0, 40.0]));
        let starts: Vec<&str> = found.stops.iter().map(|(at, _)| &found.text[*at..]).collect();
        assert_eq!(starts, ["ราคา 90 บาท", "90 บาท", "บาท"]);
        // The first word is at the left end, and the stops move right and stay inside the line.
        assert_eq!(found.stops[0].1, 0.0);
        assert!(found.stops.windows(2).all(|p| p[0].1 < p[1].1) && found.stops.iter().all(|s| (0.0..=260.0).contains(&s.1)), "{:?}", found.stops);
        // A Thai sentence without spaces still gets a stop per word.
        let read: Vec<(String, usize)> = "ประเทศไทยมีพื้นที่".chars().enumerate().map(|(i, c)| (c.to_string(), i)).collect();
        let found = line(&read, 18, [0.0, 0.0, 180.0, 20.0]).unwrap();
        assert!(found.stops.len() >= 3 && found.text == "ประเทศไทยมีพื้นที่", "{found:?}");
        assert!(found.stops.iter().all(|(at, _)| found.text.is_char_boundary(*at)));
        assert!(line(&[], 10, [0.0; 4]).is_none() && line(&[(" ".into(), 1)], 4, [0.0, 0.0, 9.0, 9.0]).is_none());
    }

    #[test]
    fn resampling_keeps_colours_in_bgr_planes_and_never_reads_outside() {
        // 2 × 1 image: a red pixel and a blue one.
        let image = Rgb { data: &[255, 0, 0, 0, 0, 255], width: 2, height: 1 };
        let planes = image.planes([0.0, 0.0, 2.0, 1.0], 2, 1, |_, v| v);
        assert_eq!(planes, [0.0, 255.0, 0.0, 0.0, 255.0, 0.0]);
        // A region past the edge and short data are clamped, not a panic.
        let short = Rgb { data: &[1, 2], width: 4, height: 4 };
        assert_eq!(short.planes([2.0, 2.0, 9.0, 9.0], 3, 3, |_, v| v).len(), 27);
    }

    #[test]
    fn the_alphabet_is_read_from_the_models_own_configuration() {
        let yml = "Global:\n  model_name: x\nPostProcess:\n  name: CTCLabelDecode\n  character_dict:\n  - '!'\n  - \'\'\'\'\n  - \"\\\\\"\n  - \"\\u0E01\"\n  - ก\n  - '- '\n  - ๙\nOther: 1\n  - no\n";
        assert_eq!(character_dict(yml), ["!", "'", "\\", "ก", "ก", "- ", "๙"]);
        assert!(character_dict("character_dict: inline").is_empty() && character_dict("").is_empty());
    }

    #[test]
    fn the_models_are_found_only_when_all_three_files_are_there() {
        let dir = std::env::temp_dir().join(format!("pdfcraft-thai-ocr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(ThaiModels::in_dir(&dir), None);
        for f in [THAI_DETECTION_MODEL, THAI_RECOGNITION_MODEL] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert_eq!(ThaiModels::in_dir(&dir), None);
        std::fs::write(dir.join(THAI_ALPHABET), "PostProcess:\n  character_dict:\n  - ก\n  - ข\n").unwrap();
        let found = ThaiModels::in_dir(&dir).unwrap();
        // Files that are not models are refused, not a panic.
        assert!(ThaiOcr::load(&found).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
