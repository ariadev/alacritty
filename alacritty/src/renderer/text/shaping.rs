//! OpenType shaping in the same face used by the platform rasterizer.
//!
//! The caller supplies a single direction run in logical order. Cluster
//! values remain UTF-8 offsets into that original input, including fallback runs.
use std::collections::{HashMap, hash_map::Entry};
use std::ops::Range;

use crossfont::{Error, FontKey, GlyphKey, Rasterizer};
use harfrust::{Buffer, Direction, Font, Script, ShapeOptions, ShaperFont};

#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    pub font_key: FontKey,
    pub glyph_id: u32,
    pub cluster: u32,
    pub x_advance: f32,
    pub y_advance: f32,
    pub x_offset: f32,
    pub y_offset: f32,
}

#[derive(Default)]
pub struct Shaper {
    fonts: HashMap<FontKey, Font>,
    runs: HashMap<(String, GlyphKey, bool), Vec<ShapedGlyph>>,
}

impl Shaper {
    pub fn clear(&mut self) {
        self.fonts.clear();
        self.runs.clear();
    }

    /// Shape a logical run; RTL results are in visual, left-to-right drawing
    /// order. Advances and offsets are pixels (positive y points upwards).
    pub fn shape(
        &mut self,
        rasterizer: &mut Rasterizer,
        text: &str,
        key: GlyphKey,
        rtl: bool,
    ) -> Result<Vec<ShapedGlyph>, Error> {
        let cache_key = (text.to_owned(), key, rtl);
        if let Some(glyphs) = self.runs.get(&cache_key) {
            return Ok(glyphs.clone());
        }
        let mut runs = Vec::new();
        let mut previous_script = Script::UNKNOWN;
        let mut previous_whitespace = false;
        for (offset, character) in text.char_indices() {
            let mut script = harfrust::unicode::script(character as u32);
            if matches!(script, Script::COMMON | Script::INHERITED | Script::UNKNOWN) {
                script = if previous_script != Script::UNKNOWN {
                    previous_script
                } else {
                    text[offset..]
                        .chars()
                        .map(|c| harfrust::unicode::script(c as u32))
                        .find(|s| {
                            !matches!(*s, Script::COMMON | Script::INHERITED | Script::UNKNOWN)
                        })
                        .unwrap_or(Script::UNKNOWN)
                };
            }
            previous_script = script;
            // Join controls and combining marks must stay with their base face.
            // Resolving them alone can select a different fallback font and
            // sever the shaping context of an otherwise supported Arabic word.
            let inherited = character == '\u{200c}'
                || character == '\u{200d}'
                || unicode_width::UnicodeWidthChar::width(character) == Some(0);
            let face = if inherited && !runs.is_empty() {
                runs.last().map(|&(_, face, _)| face).unwrap()
            } else {
                rasterizer.resolve_font(GlyphKey { character, ..key })
            };
            if character.is_whitespace()
                || previous_whitespace
                || runs
                    .last()
                    .is_none_or(|&(_, last, last_script)| last != face || script != last_script)
            {
                runs.push((offset, face, script));
            }
            previous_whitespace = character.is_whitespace();
        }
        // Prefer one face for a complete word. Otherwise a primary font
        // covering Arabic but lacking Persian yeh can sever a cursive word.
        let mut preferred = Vec::new();
        let mut start = 0;
        while start < runs.len() {
            let begin = runs[start].0;
            let mut end = start + 1;
            let whitespace = text[begin..].chars().next().is_some_and(char::is_whitespace);
            while end < runs.len()
                && !whitespace
                && runs[end].2 == runs[start].2
                && !text[runs[end].0..].chars().next().is_some_and(char::is_whitespace)
            {
                end += 1;
            }
            let finish = runs.get(end).map_or(text.len(), |run| run.0);
            let word = &text[begin..finish];
            let mut candidates: Vec<_> = runs[start..end].iter().map(|run| run.1).collect();
            for character in word.chars().filter(|&c| !join_control(c)) {
                if !candidates.iter().any(|&face| rasterizer.has_glyph(face, character)) {
                    candidates.push(rasterizer.resolve_font(GlyphKey { character, ..key }));
                }
            }
            let complete = candidates.into_iter().find(|&face| {
                word.chars().all(|c| join_control(c) || rasterizer.has_glyph(face, c))
            });
            if let Some(face) = complete {
                preferred.push((begin, face, runs[start].2));
            } else {
                preferred.extend_from_slice(&runs[start..end]);
            }
            start = end;
        }
        let runs = preferred;
        let mut output = Vec::new();
        let run_indices: Vec<_> =
            if rtl { (0..runs.len()).rev().collect() } else { (0..runs.len()).collect() };
        for index in run_indices {
            let (start, face, script) = runs[index];
            let end = runs.get(index + 1).map_or(text.len(), |&(offset, _, _)| offset);
            if self.fonts.len() >= 64 && !self.fonts.contains_key(&face) {
                self.fonts.clear();
            }
            if let Entry::Vacant(entry) = self.fonts.entry(face) {
                let (data, face_index) = rasterizer.font_data(face)?;
                let font = Font::new(data, face_index)
                    .ok_or_else(|| Error::PlatformError("invalid shaping font".into()))?;
                entry.insert(font);
            }
            let font = &self.fonts[&face];
            output.extend(shape_range(
                font,
                text,
                start..end,
                face,
                key.size.as_px(),
                rtl,
                script,
            )?);
        }
        // Keep redraw cost bounded and prevent long terminal lines from
        // retaining unbounded amounts of text/glyph output.
        if text.len() <= 4096 {
            if self.runs.len() >= 256 {
                self.runs.clear();
            }
            self.runs.insert(cache_key, output.clone());
        }
        Ok(output)
    }
}

fn shape_range(
    font: &Font,
    text: &str,
    range: Range<usize>,
    font_key: FontKey,
    pixels: f32,
    rtl: bool,
    script: Script,
) -> Result<Vec<ShapedGlyph>, Error> {
    let shaper = ShaperFont::new(font);
    let mut buffer = Buffer::new();
    buffer.push_str(&text[range.clone()]);
    buffer.set_pre_context(&text[..range.start]);
    buffer.set_post_context(&text[range.end..]);
    buffer.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
    buffer.set_script(Some(script));
    buffer.guess_segment_properties();
    harfrust::shape(&shaper, &mut buffer, ShapeOptions::default())
        .map_err(|err| Error::PlatformError(format!("shaping failed: {err:?}")))?;
    let scale = pixels / f32::from(font.units_per_em());
    Ok(buffer
        .glyph_infos()
        .iter()
        .zip(buffer.glyph_positions())
        .map(|(info, pos)| ShapedGlyph {
            font_key,
            glyph_id: info.glyph_id,
            cluster: info.cluster + range.start as u32,
            x_advance: pos.x_advance as f32 * scale,
            y_advance: pos.y_advance as f32 * scale,
            x_offset: pos.x_offset as f32 * scale,
            y_offset: pos.y_offset as f32 * scale,
        })
        .collect())
}

fn join_control(c: char) -> bool {
    matches!(c, '\u{200c}' | '\u{200d}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
fn shape_font(
    font: &Font,
    text: &str,
    key: FontKey,
    pixels: f32,
    rtl: bool,
    base: usize,
    script: Script,
) -> Result<Vec<ShapedGlyph>, Error> {
    let mut glyphs = shape_range(font, text, 0..text.len(), key, pixels, rtl, script)?;
    for glyph in &mut glyphs {
        glyph.cluster += base as u32;
    }
    Ok(glyphs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font() -> Font {
        Font::new(include_bytes!("../../../tests/font/NotoSansArabic-Regular.ttf").to_vec(), 0)
            .unwrap()
    }

    fn shape(text: &str) -> Vec<ShapedGlyph> {
        shape_font(&font(), text, FontKey::next(), 20., true, 0, Script::ARABIC).unwrap()
    }

    #[test]
    fn arabic_letters_use_contextual_glyphs() {
        let isolated = shape("ب");
        let joined = shape("ببب");
        let isolated_base = isolated.iter().find(|glyph| glyph.x_advance > 0.).unwrap().glyph_id;
        let bases: Vec<_> = joined.iter().filter(|glyph| glyph.x_advance > 0.).collect();
        assert_eq!(bases.len(), 3);
        assert!(joined.iter().all(|glyph| glyph.glyph_id != 0));
        assert!(bases.iter().all(|glyph| glyph.glyph_id != isolated_base));
        assert_ne!(bases[0].glyph_id, bases[1].glyph_id);
        assert_ne!(bases[1].glyph_id, bases[2].glyph_id);
        assert_eq!(bases.iter().map(|glyph| glyph.cluster).collect::<Vec<_>>(), [4, 2, 0]);
    }

    #[test]
    fn lam_alef_uses_required_contextual_forms() {
        // Noto Sans Arabic represents lam-alef with two contextual glyphs,
        // rather than a single ligature. A shaper must respect that choice.
        let joined = shape("لا");
        let lam = shape("ل");
        assert!(joined.iter().all(|glyph| glyph.glyph_id != 0));
        let joined_lam = joined.iter().find(|glyph| glyph.cluster == 0).unwrap();
        assert_ne!(joined_lam.glyph_id, lam[0].glyph_id);
        let isolated_advance: f32 =
            lam.iter().chain(shape("ا").iter()).map(|glyph| glyph.x_advance).sum();
        assert!(joined.iter().map(|glyph| glyph.x_advance).sum::<f32>() < isolated_advance);
    }

    #[test]
    fn persian_zwnj_breaks_joining() {
        let joined = shape("بب");
        let unjoined = shape("ب\u{200c}ب");
        let isolated = shape("ب").iter().find(|glyph| glyph.x_advance > 0.).unwrap().glyph_id;
        assert!(
            joined
                .iter()
                .filter(|glyph| glyph.x_advance > 0.)
                .all(|glyph| glyph.glyph_id != isolated)
        );
        assert_eq!(unjoined.iter().filter(|glyph| glyph.glyph_id == isolated).count(), 2);
        assert_eq!(unjoined.iter().filter(|glyph| glyph.x_advance > 0.).count(), 2);
        assert!(shape("می\u{200c}روم").iter().all(|glyph| glyph.glyph_id != 0));
    }

    #[test]
    fn arabic_marks_have_zero_advance_and_positioning() {
        let base = shape("ب");
        let shaped = shape("بِ");
        assert!(shaped.len() > base.len());
        assert_eq!(shaped.iter().filter(|glyph| glyph.x_advance > 0.).count(), 1);
        let mark = shaped
            .iter()
            .find(|glyph| !base.iter().any(|base| base.glyph_id == glyph.glyph_id))
            .unwrap();
        assert_eq!(mark.x_advance, 0.);
        assert!(mark.x_offset != 0. || mark.y_offset != 0.);
    }

    #[test]
    fn fallback_boundaries_keep_surrounding_joining_context() {
        let font = font();
        let text = "ببب";
        let whole = shape(text);
        let middle =
            shape_range(&font, text, 2..4, FontKey::next(), 20., true, Script::ARABIC).unwrap();
        assert_eq!(
            middle.iter().map(|glyph| glyph.glyph_id).collect::<Vec<_>>(),
            whole
                .iter()
                .filter(|glyph| glyph.cluster == 2)
                .map(|glyph| glyph.glyph_id)
                .collect::<Vec<_>>(),
        );
        assert!(middle.iter().all(|glyph| glyph.cluster == 2));
    }

    #[test]
    fn units_are_scaled_and_original_clusters_preserved() {
        let font = font();
        let small =
            shape_font(&font, "سلام", FontKey::next(), 10., true, 5, Script::ARABIC).unwrap();
        let large =
            shape_font(&font, "سلام", FontKey::next(), 20., true, 5, Script::ARABIC).unwrap();
        assert!(small.iter().all(|glyph| glyph.cluster >= 5));
        for (small, large) in small.iter().zip(large) {
            assert_eq!(small.glyph_id, large.glyph_id);
            assert!((small.x_advance * 2. - large.x_advance).abs() < 0.001);
            assert_eq!(small.cluster, large.cluster);
        }
    }
}
