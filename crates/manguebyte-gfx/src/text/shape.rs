//! Shaping with `harfrust`.
//!
//! This is the only module that names `harfrust`. It turns characters into
//! glyphs of the embedded typeface with their advances and offsets in font
//! units, applying the font's default features for Latin text (kerning and
//! contextual alternates among them) and tabular figures on request.

use harfrust::{
    BufferFlags, Direction, Feature, FontRef, NormalizedCoord, ShapeOptions, ShapePlan, ShaperData,
    ShaperInstance, Tag, UnicodeBuffer, script,
};

use super::font::{Coords, FONT};

/// One shaped glyph, in font units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shaped {
    pub(crate) glyph: u16,
    /// The byte offset, in the text being laid out, of the first character
    /// the glyph comes from.
    pub(crate) cluster: u32,
    pub(crate) advance: i32,
    pub(crate) x_offset: i32,
    pub(crate) y_offset: i32,
    /// Whether text cut before this glyph's cluster comes out differently,
    /// on either side, from shaping each side on its own (kerning or a
    /// contextual rule reaches across). Where this is false the two sides
    /// are exactly what shaping them apart gives.
    pub(crate) unsafe_to_break: bool,
}

/// Shapes runs of text at one point of the variation space with one set
/// of features.
pub(crate) struct Shaper {
    font: FontRef<'static>,
    data: ShaperData,
    instance: ShaperInstance,
    plan: ShapePlan,
    features: Vec<Feature>,
    buffer: Option<UnicodeBuffer>,
}

impl Shaper {
    /// A shaper for the embedded typeface at `coords`, with tabular figures
    /// if `tabular`.
    pub(crate) fn new(coords: Coords, tabular: bool) -> Shaper {
        // The bytes are part of this crate; see `Face::embedded`.
        let font = FontRef::new(FONT).expect("the embedded typeface has a valid table directory");
        let data = ShaperData::new(&font);
        let instance = ShaperInstance::from_coords(&font, coords.0.map(NormalizedCoord::from_bits));
        let mut features = Vec::new();
        if tabular {
            features.push(Feature::new(Tag::new(b"tnum"), 1, ..));
        }
        let plan = {
            let shaper = data.shaper(&font).instance(Some(&instance)).build();
            ShapePlan::new(
                &shaper,
                Direction::LeftToRight,
                Some(script::LATIN),
                None,
                &features,
            )
        };
        Shaper {
            font,
            data,
            instance,
            plan,
            features,
            buffer: Some(UnicodeBuffer::new()),
        }
    }

    /// Shapes `chars`, each given with its byte offset in the text, and
    /// appends the glyphs to `out` in visual order (left to right).
    /// `first` and `last` say whether the run starts or ends the text.
    pub(crate) fn shape(
        &mut self,
        chars: impl Iterator<Item = (usize, char)>,
        first: bool,
        last: bool,
        out: &mut Vec<Shaped>,
    ) {
        let mut buffer = self.buffer.take().unwrap_or_default();
        for (offset, c) in chars {
            buffer.add(c, u32::try_from(offset).unwrap_or(u32::MAX));
        }
        if buffer.is_empty() {
            self.buffer = Some(buffer);
            return;
        }
        buffer.set_direction(Direction::LeftToRight);
        buffer.set_script(script::LATIN);
        let mut flags = BufferFlags::empty();
        if first {
            flags |= BufferFlags::BEGINNING_OF_TEXT;
        }
        if last {
            flags |= BufferFlags::END_OF_TEXT;
        }
        buffer.set_flags(flags);
        let shaper = self
            .data
            .shaper(&self.font)
            .instance(Some(&self.instance))
            .build();
        let options = ShapeOptions::new()
            .plan(Some(&self.plan))
            .features(&self.features);
        let glyphs = shaper.shape(buffer, options);
        out.extend(
            glyphs
                .glyph_infos()
                .iter()
                .zip(glyphs.glyph_positions())
                .map(|(info, position)| Shaped {
                    glyph: u16::try_from(info.glyph_id).unwrap_or(0),
                    cluster: info.cluster,
                    advance: position.x_advance,
                    x_offset: position.x_offset,
                    y_offset: position.y_offset,
                    unsafe_to_break: info.unsafe_to_break(),
                }),
        );
        self.buffer = Some(glyphs.clear());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(text: &str, tabular: bool) -> Vec<Shaped> {
        let mut shaper = Shaper::new(Coords::default(), tabular);
        let mut out = Vec::new();
        shaper.shape(text.char_indices(), true, true, &mut out);
        out
    }

    #[test]
    fn clusters_are_byte_offsets_and_advances_are_font_units() {
        // "é" takes two bytes, so the glyph after it starts at byte 3.
        let glyphs = shape("Aé.", false);
        let clusters: Vec<u32> = glyphs.iter().map(|g| g.cluster).collect();
        assert_eq!(clusters, [0, 1, 3]);
        // The advance of "A" at the default point, from `hmtx` (see the
        // integration tests for how the reference numbers were taken).
        assert_eq!(glyphs[0].advance, 1413);
    }

    #[test]
    fn kerned_pairs_are_marked_unsafe_to_break() {
        // "AV" is kerned (-140 units), so cutting between A and V changes
        // the A; "AH" is not kerned, so cutting there changes nothing.
        let kerned = shape("AV", false);
        assert!(kerned[1].unsafe_to_break);
        let plain = shape("AH", false);
        assert!(!plain[1].unsafe_to_break);
        // The A shaped alone has its own advance, not the kerned one.
        assert_eq!(kerned[0].advance, 1413 - 140);
        assert_eq!(shape("A", false)[0].advance, 1413);
    }

    #[test]
    fn a_shaper_is_reused_across_runs() {
        let mut shaper = Shaper::new(Coords::default(), false);
        let mut out = Vec::new();
        shaper.shape("AV".char_indices(), true, false, &mut out);
        shaper.shape("".char_indices(), false, false, &mut out);
        shaper.shape("AV".char_indices(), false, true, &mut out);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], out[2]);
        assert_eq!(out[1], out[3]);
    }
}
