//! Text layout against numbers taken independently from the font file.
//!
//! # Where the reference numbers come from
//!
//! Every number below marked "from the font" was read with fontTools 4.63
//! (Python), not with the crates this crate uses (`skrifa`, `harfrust`):
//!
//! - For each point of the variation space, the font was instantiated with
//!   `fontTools.varLib.instancer.instantiateVariableFont(font, {"wght": w,
//!   "opsz": o})`, which applies `avar`, `gvar`, `HVAR`, `MVAR` and the
//!   variation deltas of `GPOS`.
//! - Advances are `hmtx` of that instance, looked up through its `cmap`.
//! - Vertical metrics are `head.unitsPerEm`, `hhea` and `OS/2` (`sTypo*`,
//!   with `USE_TYPO_METRICS` set; both tables agree: 1984, -494, 0) and
//!   `OS/2.sCapHeight` and `sxHeight` of the instance.
//! - Kerning is the sum of the `XAdvance` of the first `PairPos` subtable
//!   (format 1 or 2, through `Extension` lookups) that covers the pair, in
//!   each lookup of the `kern` feature, in lookup order.
//! - Tabular figures are the targets of the single substitution of the
//!   `tnum` feature, with their `hmtx` advances and glyph ids.
//!
//! Text is laid out at 2048 pixels, the font's units per em, so a font unit
//! is a pixel and every advance is an exact whole number.

use manguebyte_gfx::{Align, OpticalSize, Point, TextLayout, TextStyle};

/// One font unit per pixel.
const EM: f32 = 2048.0;

fn style(weight: f32, optical_size: f32) -> TextStyle {
    TextStyle::new(EM)
        .with_weight(weight)
        .with_optical_size(OpticalSize::Fixed(optical_size))
}

/// The advance of a whole string: where the caret goes after it.
fn advance(text: &str, style: &TextStyle) -> f32 {
    let layout = TextLayout::new(text, style, None);
    layout.caret(text.len()).expect("the end is a boundary").x
}

#[test]
fn vertical_metrics_match_the_font_tables() {
    // From the font: unitsPerEm 2048; ascender 1984, descender -494, line
    // gap 0; cap height 1490; x-height 1118 at opsz 14, 1084 at opsz 24 and
    // 1056 at opsz 32 (MVAR `xhgt`).
    let m = style(400.0, 14.0).metrics();
    assert_eq!(m.units_per_em, 2048);
    assert_eq!((m.ascent, m.descent, m.line_gap), (1984.0, 494.0, 0.0));
    assert_eq!(m.line_height, 2478.0);
    assert_eq!(m.cap_height, 1490.0);
    assert_eq!(m.x_height, 1118.0);
    for (opsz, x_height) in [(24.0, 1084.0), (32.0, 1056.0)] {
        let m = style(400.0, opsz).metrics();
        assert_eq!(m.x_height.round(), x_height, "opsz {opsz}: {}", m.x_height);
        // The line metrics do not vary.
        assert_eq!((m.ascent, m.descent), (1984.0, 494.0));
    }
    // At 12 pixels everything scales by 12 / 2048.
    let small = TextStyle::new(12.0).metrics();
    assert_eq!(small.ascent, (1984.0f64 * 12.0 / 2048.0) as f32);
    assert_eq!(small.line_height, (2478.0f64 * 12.0 / 2048.0) as f32);

    // A layout's lines are a line height apart, and its first baseline is
    // the ascent below its top (Inter has no line gap to share out).
    let layout = TextLayout::new("A\nB\nC", &style(400.0, 14.0), None);
    assert_eq!(layout.baseline(), 1984.0);
    assert_eq!(layout.line_height(), 2478.0);
    let baselines: Vec<f32> = layout.lines().iter().map(|l| l.baseline).collect();
    assert_eq!(baselines, [1984.0, 1984.0 + 2478.0, 1984.0 + 2.0 * 2478.0]);
    assert_eq!(layout.size().height, 3.0 * 2478.0);
}

#[test]
fn advances_match_hmtx_at_every_weight_and_optical_size() {
    // From the font: hmtx advances of A V T o 1 . space and ellipsis.
    let cases: [(f32, f32, [f32; 8]); 6] = [
        (
            400.0,
            14.0,
            [1413.0, 1413.0, 1322.0, 1228.0, 833.0, 590.0, 576.0, 1770.0],
        ),
        (
            100.0,
            14.0,
            [1310.0, 1310.0, 1262.0, 1186.0, 752.0, 554.0, 576.0, 1662.0],
        ),
        (
            700.0,
            14.0,
            [1529.0, 1529.0, 1367.0, 1256.0, 883.0, 684.0, 485.0, 2052.0],
        ),
        (
            900.0,
            14.0,
            [1628.0, 1628.0, 1406.0, 1280.0, 926.0, 764.0, 408.0, 2292.0],
        ),
        (
            400.0,
            24.0,
            [1371.0, 1371.0, 1282.0, 1170.0, 782.0, 512.0, 540.0, 1537.0],
        ),
        (
            400.0,
            32.0,
            [1338.0, 1338.0, 1250.0, 1124.0, 742.0, 450.0, 512.0, 1350.0],
        ),
    ];
    for (weight, opsz, expected) in cases {
        let style = style(weight, opsz);
        let measured = ["A", "V", "T", "o", "1", ".", " ", "\u{2026}"].map(|c| advance(c, &style));
        assert_eq!(measured, expected, "wght {weight} opsz {opsz}");
    }
}

#[test]
fn the_weight_axis_widens_text() {
    let text = "Hamburgefonstiv";
    let widths: Vec<f32> = [100.0, 300.0, 400.0, 500.0, 700.0, 900.0]
        .iter()
        .map(|&w| TextLayout::new(text, &style(w, 14.0), None).size().width)
        .collect();
    assert!(widths.windows(2).all(|w| w[0] < w[1]), "{widths:?}");
    // Weights beyond the axis are clamped to its ends.
    let clamped = |w| TextLayout::new(text, &style(w, 14.0), None).size().width;
    assert_eq!(clamped(50.0), widths[0]);
    assert_eq!(clamped(2000.0), widths[5]);
    assert_eq!(clamped(f32::NAN), widths[2]);
}

#[test]
fn kerning_brings_kerned_pairs_closer() {
    // From the font: kern A V -140, T o -160, W a -104 at wght 400 opsz 14;
    // A V -162 at wght 700; A V -173 and T o -204 at opsz 24.
    let regular = style(400.0, 14.0);
    for (pair, kern) in [("AV", -140.0), ("To", -160.0), ("Wa", -104.0)] {
        let (first, second) = pair.split_at(1);
        let apart = advance(first, &regular) + advance(second, &regular);
        let together = TextLayout::new(pair, &regular, None).size().width;
        assert!(together < apart, "{pair}");
        assert_eq!(together, apart + kern, "{pair}");
    }
    let bold = style(700.0, 14.0);
    assert_eq!(advance("AV", &bold), 1529.0 * 2.0 - 162.0);
    let display = style(400.0, 24.0);
    assert_eq!(advance("AV", &display), 1371.0 * 2.0 - 173.0);
    assert_eq!(advance("To", &display), 1282.0 + 1170.0 - 204.0);
    // The second glyph starts where the kerned advance puts it.
    let layout = TextLayout::new("AV", &regular, None);
    assert_eq!(
        layout.glyphs()[1].position,
        Point::new(1413.0 - 140.0, 1984.0)
    );
}

#[test]
fn tabular_figures_give_every_digit_one_advance() {
    // From the font: `tnum` maps 0-9 to glyphs 257-266, of advance 1328 at
    // wght 400, 1324 at 700 and 1320 at 900 (opsz 14); proportional
    // figures differ (1 is 833, 0 is 1292 at 400).
    for (weight, tabular_advance) in [(400.0, 1328.0), (700.0, 1324.0), (900.0, 1320.0)] {
        let tabular = style(weight, 14.0).with_tabular_figures(true);
        let advances: Vec<f32> = ('0'..='9')
            .map(|d| advance(&d.to_string(), &tabular))
            .collect();
        assert_eq!(advances, [tabular_advance; 10], "wght {weight}");
        let ids: Vec<u16> = TextLayout::new("0123456789", &tabular, None)
            .glyphs()
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(ids, (257..=266).collect::<Vec<u16>>());
    }
    let proportional = style(400.0, 14.0);
    assert_eq!(advance("1", &proportional), 833.0);
    assert_eq!(advance("0", &proportional), 1292.0);
    // A changing value keeps its width with tabular figures.
    let tabular = style(400.0, 14.0).with_tabular_figures(true);
    assert_eq!(advance("-11.1 dB", &tabular), advance("-80.8 dB", &tabular));
    assert_ne!(
        advance("-11.1 dB", &proportional),
        advance("-80.8 dB", &proportional)
    );
}

#[test]
fn contextual_ligatures_join_characters_and_share_their_carets() {
    // From the font: the `calt` feature replaces "->" by glyph 450
    // (`arrowright`), one glyph for two characters.
    let regular = style(400.0, 14.0);
    let layout = TextLayout::new("a->b", &regular, None);
    let ids: Vec<u16> = layout.glyphs().iter().map(|g| g.id).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[1], 450);
    let indices: Vec<usize> = layout.glyphs().iter().map(|g| g.index).collect();
    assert_eq!(indices, [0, 1, 3]);
    // The caret between "-" and ">" sits half way across the arrow.
    let before = layout.caret(1).expect("boundary").x;
    let middle = layout.caret(2).expect("boundary").x;
    let after = layout.caret(3).expect("boundary").x;
    assert!(before < middle && middle < after);
    assert_eq!(middle - before, after - middle);
}

fn line_texts(layout: &TextLayout) -> Vec<&str> {
    layout
        .lines()
        .iter()
        .map(|line| &layout.text()[line.text.clone()])
        .collect()
}

#[test]
fn lines_break_after_spaces_to_fit_the_width() {
    let regular = style(400.0, 14.0);
    let whole = advance("Hello world", &regular);
    let hello = advance("Hello", &regular);
    let world = advance("world", &regular);
    // Exactly as wide as the text: one line.
    let one = TextLayout::new("Hello world", &regular, Some(whole));
    assert_eq!(line_texts(&one), ["Hello world"]);
    // A unit narrower: the space stays at the end of the first line and
    // does not count in its width.
    let two = TextLayout::new("Hello world", &regular, Some(whole - 1.0));
    assert_eq!(line_texts(&two), ["Hello ", "world"]);
    let widths: Vec<f32> = two.lines().iter().map(|l| l.width).collect();
    assert_eq!(widths, [hello, world]);
    assert_eq!(two.size().width, hello.max(world));
    assert_eq!(two.size().height, 2.0 * two.line_height());
    // Runs of spaces stay together; lines break after them.
    let spaced = TextLayout::new(
        "one  two   three",
        &regular,
        Some(advance("one  two", &regular)),
    );
    assert_eq!(line_texts(&spaced), ["one  two   ", "three"]);
    // No breaks inside words, nor at hyphens, while words fit.
    let hyphen = TextLayout::new(
        "left-right up",
        &regular,
        Some(advance("left-right", &regular)),
    );
    assert_eq!(line_texts(&hyphen), ["left-right ", "up"]);
}

#[test]
fn text_laid_out_at_its_own_measured_width_keeps_its_lines() {
    // At sizes where a font unit is not a power of two of a pixel (any
    // zoom, any conversion from points), the measured width is rounded to
    // f32; fitting compares that same number, so the text still fits.
    let labels = [
        "Gain",
        "Resonance",
        "Low cut frequency",
        "Wet/Dry mix",
        "AVATAR",
        "Hello, world.",
        "\u{2212}12.5 dB",
        "two\nlines here",
    ];
    let sizes = [
        9.6f32, 10.0, 11.0, 12.3, 13.333, 14.4, 15.7, 17.1, 19.2, 21.33, 26.6, 33.3,
    ];
    for text in labels {
        for size in sizes {
            for weight in [400.0, 700.0] {
                let free = TextStyle::new(size).with_weight(weight);
                let measured = TextLayout::new(text, &free, None);
                let width = measured.size().width;
                let again = TextLayout::new(text, &free, Some(width));
                assert_eq!(
                    line_texts(&again),
                    line_texts(&measured),
                    "{text:?} at {size} px, weight {weight}, given its own width {width}"
                );
                assert_eq!(again.size(), measured.size());
                // With a limit of lines, nothing is cut.
                let limited = free.with_max_lines(measured.lines().len() as u32);
                let label = TextLayout::new(text, &limited, Some(width));
                assert!(!label.is_truncated(), "{text:?} at {size} px is cut");
                assert_eq!(label.glyphs(), measured.glyphs());
            }
        }
    }
}

#[test]
fn every_line_is_drawn_as_its_text_shaped_alone() {
    // However a paragraph is broken, between words or inside one, and
    // however it is cut, each line's glyphs and width are those of the
    // line's own text laid out by itself (with the ellipsis, if the line
    // has one): no kerning or contextual form reaches across a break.
    let regular = style(400.0, 14.0);
    let mut r = Random(0x5DEE_CE66_D1B5_4A32);
    let words = [
        "AVATAR",
        "Toyota",
        "WAVE",
        "Hello,",
        "world.",
        "f",
        "T.",
        "1920x1080",
        "a->b",
        "Wet/Dry",
        "L'Tw",
        "\u{c5}r\u{f8}",
        "Yo-yo",
        "ff",
        "7.",
        "P.",
        "Vy",
        "r.",
    ];
    for case in 0..400 {
        let mut text = String::new();
        for i in 0..1 + r.below(7) {
            if i > 0 {
                text.push_str([" ", " ", "  ", "\n"][r.below(4)]);
            }
            text.push_str(words[r.below(words.len())]);
        }
        let width = 600.0 + r.below(9000) as f32;
        let style = if r.below(2) == 0 {
            regular.with_max_lines(1 + r.below(3) as u32)
        } else {
            regular
        };
        let layout = TextLayout::new(&text, &style, Some(width));
        for (i, line) in layout.lines().iter().enumerate() {
            let mut own = layout.text()[line.text.clone()].to_owned();
            let cut = layout.glyphs()[line.glyphs.clone()]
                .last()
                .is_some_and(|g| g.index >= line.text.end && layout.is_truncated());
            if cut {
                own.push('\u{2026}');
            }
            let alone = TextLayout::new(&own, &regular, None);
            let placed: Vec<(u16, f32)> = layout.glyphs()[line.glyphs.clone()]
                .iter()
                .map(|g| (g.id, g.position.x - line.x))
                .collect();
            let shaped: Vec<(u16, f32)> = alone
                .glyphs()
                .iter()
                .map(|g| (g.id, g.position.x))
                .collect();
            assert_eq!(
                placed, shaped,
                "case {case}, line {i} of {text:?} at {width}"
            );
            assert_eq!(
                line.width,
                alone.size().width,
                "case {case}, line {i} of {text:?}"
            );
            // And it fits, unless it holds a single character (or the
            // arrow, one glyph for two).
            let held = own.trim_end();
            let single = held.chars().count() == 1 || held == "->";
            assert!(line.width <= width || single, "case {case}: {own:?}");
        }
    }
}

#[test]
fn a_line_broken_inside_a_word_is_as_wide_as_what_it_holds() {
    // From the font: A 1413, kern A/V -140. The line that keeps "A" alone
    // is as wide as "A" alone, not as "A" kerned against the "V" now on
    // the next line; the width feeds alignment and fitting.
    let regular = style(400.0, 14.0);
    let broken = TextLayout::new("AV", &regular, Some(1413.0));
    assert_eq!(line_texts(&broken), ["A", "V"]);
    assert_eq!(broken.lines()[0].width, 1413.0);
    // Aligned to the end of a box exactly that wide, it starts at 0.
    let end = TextLayout::new("AV", &regular.with_align(Align::End), Some(1413.0));
    assert_eq!(end.lines()[0].x, 0.0);
}

#[test]
fn a_word_wider_than_the_line_breaks_between_characters() {
    let regular = style(400.0, 14.0);
    let width = advance("Freq", &regular);
    let layout = TextLayout::new("Frequency", &regular, Some(width));
    let lines = line_texts(&layout);
    assert_eq!(lines.concat(), "Frequency");
    assert_eq!(lines[0], "Freq");
    // Each line but the last holds as many characters as fit: one more
    // would not.
    let mut start = 0;
    for line in &lines[..lines.len() - 1] {
        let end = start + line.len();
        let next = &layout.text()[start..=end];
        assert!(advance(line, &regular) <= width, "{line:?}");
        assert!(advance(next, &regular) > width, "{next:?} would have fit");
        start = end;
    }
    // No line is wider than allowed unless it holds a single character.
    let narrow = TextLayout::new("Frequency response", &regular, Some(900.0));
    for line in narrow.lines() {
        let text = &narrow.text()[line.text.clone()];
        assert!(
            line.width <= 900.0 || text.trim_end().chars().count() == 1,
            "{text:?} is {} wide",
            line.width
        );
    }
    // A width of 0 puts one character on each line.
    let zero = TextLayout::new("abc", &regular, Some(0.0));
    assert_eq!(line_texts(&zero), ["a", "b", "c"]);
    let negative = TextLayout::new("abc", &regular, Some(-5.0));
    assert_eq!(line_texts(&negative), ["a", "b", "c"]);
}

#[test]
fn forced_breaks_start_new_lines() {
    let regular = style(400.0, 14.0);
    let layout = TextLayout::new("one\ntwo\r\n\nthree\n", &regular, None);
    assert_eq!(line_texts(&layout), ["one", "two", "", "three", ""]);
    assert_eq!(layout.lines().len(), 5);
    // The caret after the last line feed is on the last, empty line.
    let end = layout.caret(layout.text().len()).expect("end");
    assert_eq!((end.line, end.x), (4, 0.0));
    // An empty text has one empty line.
    let empty = TextLayout::new("", &regular, None);
    assert_eq!(empty.lines().len(), 1);
    assert_eq!(empty.size().width, 0.0);
    assert_eq!(empty.size().height, empty.line_height());
    assert!(empty.glyphs().is_empty());
    assert_eq!(empty.ink_bounds(), None);
}

#[test]
fn lines_align_to_the_start_centre_or_end() {
    let regular = style(400.0, 14.0);
    let short = advance("ab", &regular);
    let long = advance("abcdef", &regular);
    for (align, x_short) in [
        (Align::Start, 0.0),
        (Align::Center, (long - short) / 2.0),
        (Align::End, long - short),
    ] {
        // With no maximum width, lines align within the widest line.
        let layout = TextLayout::new("ab\nabcdef", &regular.with_align(align), None);
        let xs: Vec<f32> = layout.lines().iter().map(|l| l.x).collect();
        assert_eq!(xs, [x_short, 0.0], "{align:?}");
        assert_eq!(layout.glyphs()[0].position.x, x_short);
    }
    // With a maximum width, within that width.
    let box_width = 10_000.0;
    let centred = TextLayout::new("ab", &regular.with_align(Align::Center), Some(box_width));
    assert_eq!(centred.lines()[0].x, (box_width - short) / 2.0);
    let end = TextLayout::new("ab", &regular.with_align(Align::End), Some(box_width));
    assert_eq!(end.lines()[0].x, box_width - short);
    // The measured width is the text's, not the box's.
    assert_eq!(end.size().width, short);
}

#[test]
fn text_that_needs_too_many_lines_ends_with_an_ellipsis() {
    // From the font: glyph 368 is the ellipsis.
    const ELLIPSIS: u16 = 368;
    let one_line = style(400.0, 14.0).with_max_lines(1);
    let width = advance("Low cut freq", &one_line);
    let label = TextLayout::new("Low cut frequency", &one_line, Some(width));
    assert!(label.is_truncated());
    assert_eq!(label.lines().len(), 1);
    assert_eq!(label.glyphs().last().map(|g| g.id), Some(ELLIPSIS));
    assert!(label.size().width <= width);
    // As much as fits beside the ellipsis is kept, cut between characters
    // (the next character would not fit). What is kept is measured shaped
    // together with the ellipsis, kerning included.
    let kept = &label.text()[label.lines()[0].text.clone()];
    let with_ellipsis = |s: &str| advance(&format!("{s}\u{2026}"), &one_line);
    assert!(with_ellipsis(kept) <= width, "{kept:?}");
    let next = &label.text()[..kept.len() + 1];
    assert!(with_ellipsis(next) > width, "{next:?} would have fit");
    // The ellipsis stands for the first character cut.
    assert_eq!(label.glyphs().last().map(|g| g.index), Some(kept.len()));

    // Text that fits is left alone.
    let fits = TextLayout::new("Low", &one_line, Some(width));
    assert!(!fits.is_truncated());
    assert_ne!(fits.glyphs().last().map(|g| g.id), Some(ELLIPSIS));

    // Two lines allowed: the second ends with the ellipsis.
    let two = style(400.0, 14.0).with_max_lines(2);
    let words = "one two three four five six";
    let narrow = advance("three", &two);
    let layout = TextLayout::new(words, &two, Some(narrow));
    assert_eq!(layout.lines().len(), 2);
    assert!(layout.is_truncated());
    let second = &layout.lines()[1];
    assert_eq!(layout.glyphs()[second.glyphs.end - 1].id, ELLIPSIS);
    assert!(second.width <= narrow);

    // A forced break with no width limit: the first line gets the ellipsis.
    let layout = TextLayout::new("Line one\nLine two", &one_line, None);
    assert_eq!(line_texts(&layout), ["Line one"]);
    assert_eq!(layout.glyphs().last().map(|g| g.id), Some(ELLIPSIS));
    assert_eq!(layout.size().width, advance("Line one\u{2026}", &one_line));

    // Trailing spaces before the cut are dropped.
    let spaced = TextLayout::new(
        "ab      cdefghijkl",
        &one_line,
        Some(advance("ab      c", &one_line)),
    );
    assert_eq!(line_texts(&spaced), ["ab"]);

    // Too narrow even for the ellipsis: the ellipsis alone.
    let tiny = TextLayout::new("Gain", &one_line, Some(10.0));
    let ids: Vec<u16> = tiny.glyphs().iter().map(|g| g.id).collect();
    assert_eq!(ids, [ELLIPSIS]);
    assert_eq!(line_texts(&tiny), [""]);

    // No lines at all.
    let none = TextLayout::new("Gain", &style(400.0, 14.0).with_max_lines(0), None);
    assert!(none.lines().is_empty() && none.glyphs().is_empty());
    assert!(none.is_truncated());
    assert_eq!(none.caret(0), None);
    assert_eq!(none.index_at(Point::new(5.0, 5.0)), 0);
}

#[test]
fn the_ellipsis_is_kerned_against_the_last_character_kept() {
    // From the font: T 1322, A 1413, ellipsis 1770; kern T/ellipsis -174,
    // A/V -140, A/ellipsis 0 (wght 400, opsz 14). Glyphs: A 1, T 89,
    // ellipsis 368.
    let one_line = style(400.0, 14.0).with_max_lines(1);
    let placed = |l: &TextLayout| -> Vec<(u16, f32)> {
        l.glyphs().iter().map(|g| (g.id, g.position.x)).collect()
    };
    // "T…" is 2918 units wide, and a box of that width shows it.
    let whole = TextLayout::new("T\u{2026}", &one_line, None);
    assert_eq!(whole.size().width, 1322.0 + 1770.0 - 174.0);
    let cut = TextLayout::new("TLL", &one_line, Some(whole.size().width));
    assert_eq!(placed(&cut), [(89, 0.0), (368, 1322.0 - 174.0)]);
    assert_eq!(cut.size().width, 2918.0);
    // The character kept keeps no kerning with the one it dropped: in
    // room for "A…" (3183 units) the ellipsis sits after the A's own
    // advance, not 140 units into it.
    let cut = TextLayout::new("AVATAR", &one_line, Some(1413.0 + 1770.0));
    assert_eq!(placed(&cut), [(1, 0.0), (368, 1413.0)]);
    assert_eq!(cut.size().width, 3183.0);

    // At an ordinary size: the label of the golden image `text-turned`
    // puts its ellipsis where shaping "Low cut f…" whole puts it (kern
    // f/ellipsis -116 units, 0.62 pixels at 11 pixels).
    let label_style = TextStyle::new(11.0).with_max_lines(1);
    let label = TextLayout::new("Low cut frequency", &label_style, Some(60.0));
    assert_eq!(line_texts(&label), ["Low cut f"]);
    let shaped = TextLayout::new("Low cut f\u{2026}", &label_style, None);
    assert_eq!(label.glyphs().len(), shaped.glyphs().len());
    for (a, b) in label.glyphs().iter().zip(shaped.glyphs()) {
        assert_eq!((a.id, a.position), (b.id, b.position));
    }
    assert_eq!(label.size().width, shaped.size().width);
}

#[test]
fn positions_and_character_indices_map_both_ways() {
    let regular = style(400.0, 14.0);
    let texts = [
        "Hello world",
        "Gain \u{2212}6.0 dB",
        "na\u{ef}ve caf\u{e9} -> \u{c5}r\u{f8}",
        "two\nlines",
        "  spaced  out  ",
    ];
    for text in texts {
        for width in [None, Some(5000.0)] {
            let layout = TextLayout::new(text, &regular, width);
            let mut boundaries: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
            boundaries.push(text.len());
            let mut previous: Option<(usize, f32)> = None;
            for &i in &boundaries {
                let caret = layout.caret(i).unwrap_or_else(|| panic!("{text:?}: {i}"));
                // Carets move right along a line, and down from line to line.
                if let Some((line, x)) = previous {
                    assert!(
                        caret.line > line || (caret.line == line && caret.x >= x),
                        "{text:?}: {i}"
                    );
                }
                previous = Some((caret.line, caret.x));
                assert_eq!(caret.bottom - caret.top, layout.line_height());
                // Clicking on a caret gives its index back, unless an
                // earlier boundary is at the same place.
                let back = layout.index_at(Point::new(caret.x, caret.top + 1.0));
                let back_caret = layout.caret(back).expect("a boundary");
                assert_eq!(
                    (back_caret.line, back_caret.x),
                    (caret.line, caret.x),
                    "{text:?}: {i}"
                );
                assert!(back <= i);
            }
            // Not a boundary.
            if let Some((i, c)) = text.char_indices().find(|(_, c)| c.len_utf8() > 1) {
                assert_eq!(layout.caret(i + 1), None, "{text:?} inside {c:?}");
            }
            assert_eq!(layout.caret(text.len() + 1), None);
        }
    }
    // A soft break belongs to the start of the next line.
    let wrapped = TextLayout::new("Hello world", &regular, Some(advance("Hello", &regular)));
    let at_break = wrapped.caret(6).expect("boundary");
    assert_eq!((at_break.line, at_break.x), (1, 0.0));
    // Points beyond the text clamp to the nearest line and boundary.
    assert_eq!(wrapped.index_at(Point::new(-100.0, -100.0)), 0);
    assert_eq!(wrapped.index_at(Point::new(1e9, 1e9)), 11);
    assert_eq!(wrapped.index_at(Point::new(f32::NAN, f32::NAN)), 0);
}

#[test]
fn glyph_positions_follow_the_advances_and_lines() {
    let regular = style(400.0, 14.0);
    let layout = TextLayout::new("To\nAV", &regular, None);
    let placed: Vec<(u16, f32, f32, usize)> = layout
        .glyphs()
        .iter()
        .map(|g| (g.id, g.position.x, g.position.y, g.index))
        .collect();
    // From the font: T is 89, o 187, A 1, V 103; T advances 1322 and is
    // kerned -160 before o.
    assert_eq!(
        placed,
        [
            (89, 0.0, 1984.0, 0),
            (187, 1322.0 - 160.0, 1984.0, 1),
            (1, 0.0, 1984.0 + 2478.0, 3),
            (103, 1413.0 - 140.0, 1984.0 + 2478.0, 4),
        ]
    );
    // The ink box holds every glyph's outline: from the cap height (1490)
    // to below the baseline of the second line.
    let ink = layout.ink_bounds().expect("ink");
    assert!(ink.y <= 1984.0 - 1490.0 && ink.y > 0.0);
    assert!(ink.bottom() >= 1984.0 + 2478.0);
}

/// A small generator of pseudo-random numbers (xorshift64*), so the cases
/// are the same on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick(&mut self, from: &[f32]) -> f32 {
        from[self.below(from.len())]
    }
}

#[test]
fn hostile_input_lays_out_without_harm() {
    // Sizes, weights and widths that are not numbers, and text that the
    // typeface cannot draw.
    let mut r = Random(0x2545_F491_4F6C_DD1D);
    let alphabet = [
        "a",
        "W",
        " ",
        "  ",
        "\t",
        "\n",
        "\r",
        "\r\n",
        "\u{2028}",
        "\u{a0}",
        "-",
        ">",
        "->",
        "\u{301}",
        "\u{5d0}",
        "\u{627}",
        "\u{1f600}",
        "\u{0}",
        "\u{200d}",
        "\u{feff}",
        "\u{e9}",
        "1",
        "\u{2026}",
        "Te",
        "fi",
    ];
    let numbers = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -1.0,
        0.0,
        1e-30,
        1e30,
        f32::MAX,
        12.0,
        33.3,
    ];
    for _ in 0..2_000 {
        let mut text = String::new();
        for _ in 0..r.below(24) {
            text.push_str(alphabet[r.below(alphabet.len())]);
        }
        let style = TextStyle::new(r.pick(&numbers))
            .with_weight(r.pick(&numbers))
            .with_optical_size(if r.below(2) == 0 {
                OpticalSize::Auto
            } else {
                OpticalSize::Fixed(r.pick(&numbers))
            })
            .with_tabular_figures(r.below(2) == 0)
            .with_align([Align::Start, Align::Center, Align::End][r.below(3)]);
        let style = if r.below(3) == 0 {
            style.with_max_lines(r.below(3) as u32)
        } else {
            style
        };
        let width = if r.below(3) == 0 {
            None
        } else {
            Some(r.pick(&numbers))
        };
        let layout = TextLayout::new(&text, &style, width);
        let size = layout.size();
        assert!(
            size.width.is_finite() && size.height.is_finite(),
            "{text:?}"
        );
        for glyph in layout.glyphs() {
            assert!(glyph.position.is_finite(), "{text:?} {style:?}");
            assert!(text.is_char_boundary(glyph.index));
        }
        for line in layout.lines() {
            assert!(text.get(line.text.clone()).is_some());
            assert!(line.glyphs.end <= layout.glyphs().len());
        }
        let index = layout.index_at(Point::new(r.pick(&numbers), r.pick(&numbers)));
        assert!(text.is_char_boundary(index));
        if !layout.lines().is_empty() {
            for (i, _) in text.char_indices() {
                assert!(layout.caret(i).is_some(), "{text:?}: {i}");
            }
        }
    }
}
