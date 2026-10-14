//! Type3 glyphs as a text extractor sees them through the public API: the
//! Unicode fallback chain on a parsed font, and the metrics `Type3Glyph`
//! exposes, in the units it promises.

use hayro_interpret::font::Glyph;
use hayro_interpret::hayro_cmap::BfString;
use hayro_interpret::hayro_syntax::Pdf;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect};

/// A Type3 font as some producers write it (`/F1`: no `ToUnicode`, glyphs named
/// after their hex code, identity `/FontMatrix`) and the same font with a
/// `ToUnicode` `CMap` that maps the code elsewhere (`/F2`).
fn type3_pdf() -> Vec<u8> {
    let glyph_a = b"25 0 0 -7 25 26 d1 2 0 20 26 re f";
    let cmap = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap \
/CMapName /T def /CMapType 2 def 1 begincodespacerange <00> <FF> endcodespacerange \
1 beginbfchar <41> <005A> endbfchar endcmap CMapName currentdict /CMap defineresource pop end end";
    let content = b"BT /F1 0.2 Tf 72 720 Td (A) Tj /F2 0.2 Tf (A) Tj ET";
    let font = |extra: &str| {
        format!(
            "<</Type/Font/Subtype/Type3/FontBBox[0 -7 25 26]/FontMatrix[1 0 0 1 0 0]\
/CharProcs<</C41 6 0 R>>/Encoding<</Type/Encoding/Differences[65/C41]>>\
/FirstChar 65/LastChar 65/Widths[25]{extra}>>"
        )
        .into_bytes()
    };
    let stream = |data: &[u8]| {
        let mut out = format!("<</Length {}>>stream\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\nendstream");
        out
    };
    let objects: Vec<Vec<u8>> = vec![
        b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
        b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
        b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]\
/Resources<</Font<</F1 4 0 R/F2 5 0 R>>>>/Contents 7 0 R>>"
            .to_vec(),
        font(""),
        font("/ToUnicode 8 0 R"),
        stream(glyph_a),
        stream(content),
        stream(cmap),
    ];

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[derive(Debug, PartialEq)]
struct SeenGlyph {
    unicode: Option<BfString>,
    advance_width: Option<f32>,
    font_matrix: Affine,
    font_bbox: Rect,
}

#[derive(Default)]
struct GlyphRecorder {
    glyphs: Vec<SeenGlyph>,
}

impl<'a> Device<'a> for GlyphRecorder {
    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        _transform: Affine,
        _glyph_transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &GlyphDrawMode,
    ) {
        let Glyph::Type3(type3) = glyph else {
            panic!("the fixture only uses Type3 fonts");
        };
        self.glyphs.push(SeenGlyph {
            unicode: glyph.as_unicode(),
            advance_width: type3.advance_width(),
            font_matrix: type3.font_matrix(),
            font_bbox: type3.font_bbox(),
        });
    }

    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn record_glyphs() -> Vec<SeenGlyph> {
    let pdf = Pdf::new(type3_pdf()).expect("fixture parses");
    let page = &pdf.pages()[0];
    let cache = InterpreterCache::new();
    let mut context = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 612.0, 792.0),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut recorder = GlyphRecorder::default();
    interpret_page(page, &mut context, &mut recorder);
    recorder.glyphs
}

#[test]
fn type3_glyph_named_after_its_code_resolves_without_tounicode() {
    let glyphs = record_glyphs();
    assert_eq!(glyphs.len(), 2);
    assert_eq!(glyphs[0].unicode, Some(BfString::Char('A')));
}

#[test]
fn type3_tounicode_wins_over_the_glyph_name() {
    let glyphs = record_glyphs();
    assert_eq!(glyphs[1].unicode, Some(BfString::Char('Z')));
}

#[test]
fn type3_metrics_are_exposed_in_outline_glyph_units() {
    let glyph = &record_glyphs()[0];
    // `/Widths` 25 through an identity `/FontMatrix`: 25 text-space units,
    // in the thousandths `OutlineGlyph::advance_width` reports.
    assert_eq!(glyph.advance_width, Some(25_000.0));
    assert_eq!(glyph.font_matrix, Affine::IDENTITY);
    assert_eq!(glyph.font_bbox, Rect::new(0.0, -7.0, 25.0, 26.0));
}
