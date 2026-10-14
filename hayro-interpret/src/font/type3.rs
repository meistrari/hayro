use crate::CMapResolverFn;
use crate::context::Context;
use crate::device::Device;
use crate::font::glyph_simulator::GlyphSimulator;
use crate::font::true_type::{Width, read_encoding, read_widths};
use crate::font::{
    Encoding, Glyph, Type3Glyph, UNITS_PER_EM, agl_glyph_name_to_unicode, read_to_unicode,
};
use crate::interpret::state::TextState;
use crate::soft_mask::SoftMask;
use crate::util::RectExt;
use crate::{BlendMode, interpret};
use crate::{CacheKey, ClipPath, GlyphDrawMode, PathDrawMode};
use crate::{Image, Paint};
use hayro_cmap::{BfString, CMap};
use hayro_syntax::content::TypedIter;
use hayro_syntax::content::ops::TypedInstruction;
use hayro_syntax::object::Dict;
use hayro_syntax::object::Stream;
use hayro_syntax::object::dict::keys::{CHAR_PROCS, FONT_BBOX, FONT_MATRIX, RESOURCES};
use hayro_syntax::page::Resources;
use kurbo::{Affine, BezPath, Rect};
use skrifa::GlyphId;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct Type3<'a> {
    widths: Vec<Width>,
    missing_width: f32,
    encoding: Encoding,
    encodings: HashMap<u8, String>,
    dict: Dict<'a>,
    char_procs: HashMap<String, Stream<'a>>,
    glyph_simulator: GlyphSimulator,
    font_bbox: Rect,
    matrix: Affine,
    to_unicode: Option<CMap>,
}

impl<'a> Type3<'a> {
    pub(crate) fn new(dict: &Dict<'a>, cmap_resolver: &CMapResolverFn) -> Option<Self> {
        let (encoding, encodings) = read_encoding(dict);
        let (widths, missing_width) = read_widths(dict, dict)?;
        let font_bbox = dict
            .get::<hayro_syntax::object::Rect>(FONT_BBOX)
            .unwrap_or(hayro_syntax::object::Rect::ZERO)
            .to_kurbo();

        let matrix = Affine::new(
            dict.get::<[f64; 6]>(FONT_MATRIX)
                .unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]),
        );

        let char_procs = {
            let mut procs = HashMap::new();
            let dict = dict.get::<Dict<'_>>(CHAR_PROCS).unwrap_or_default();

            for name in dict.keys() {
                if let Some(prog) = dict.get::<Stream<'_>>(name.as_ref()) {
                    procs.insert(name.as_str().to_string(), prog);
                }
            }

            procs
        };

        let to_unicode = read_to_unicode(dict, cmap_resolver);

        Some(Self {
            glyph_simulator: GlyphSimulator::new(),
            encoding,
            font_bbox,
            char_procs,
            widths,
            missing_width,
            encodings,
            matrix,
            dict: dict.clone(),
            to_unicode,
        })
    }

    pub(crate) fn map_code(&self, code: u8) -> GlyphId {
        self.glyph_name(code)
            .map(|g| self.glyph_simulator.string_to_glyph(g))
            .unwrap_or(GlyphId::NOTDEF)
    }

    fn glyph_name(&self, code: u8) -> Option<&str> {
        self.encodings
            .get(&code)
            .map(|s| s.as_str())
            .or_else(|| self.encoding.map_code(code))
    }

    pub(crate) fn matrix(&self) -> Affine {
        self.matrix
    }

    pub(crate) fn bbox(&self) -> Rect {
        self.font_bbox
    }

    pub(crate) fn glyph_width(&self, code: u8) -> f32 {
        let w = match self.widths.get(code as usize).copied() {
            Some(Width::Value(w)) => w,
            _ => self.missing_width,
        };
        (w * self.matrix.as_coeffs()[0] as f32) * UNITS_PER_EM
    }

    pub(crate) fn char_code_to_unicode(&self, char_code: u32) -> Option<BfString> {
        if let Some(unicode) = self
            .to_unicode
            .as_ref()
            .and_then(|t| t.lookup_bf_string(char_code))
        {
            return Some(unicode);
        }

        // Without a ToUnicode CMap, fall back to the glyph name the encoding
        // assigns to the code, as for simple outline fonts.
        let code = u8::try_from(char_code).ok()?;
        let name = self.glyph_name(code)?;
        let unicode =
            agl_glyph_name_to_unicode(name).or_else(|| numeric_glyph_name_to_unicode(name, code));
        if unicode.is_none() {
            warn!("failed to map Type3 glyph name {} to unicode", name);
        }
        unicode
    }

    pub(crate) fn render_glyph(
        &self,
        glyph: &Type3Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'a>,
        device: &mut impl Device<'a>,
    ) -> Option<()> {
        let mut state = glyph.state.clone();
        let root_transform =
            transform * glyph_transform * self.matrix * Affine::scale(UNITS_PER_EM as f64);
        state.ctm = root_transform;

        // Not sure if this is mentioned anywhere, but I do think we need to reset the text state
        // (though the graphics state itself should be preserved).
        state.text_state = TextState::default();

        let name = self.glyph_simulator.glyph_to_string(glyph.glyph_id)?;
        let program = self.char_procs.get(&name)?;
        let decoded = program.decoded().ok()?;
        let iter = TypedIter::new(decoded.as_ref());

        let is_shape_glyph = {
            let mut iter = iter.clone();
            let mut is_shape_glyph = true;

            while let Some(op) = iter.next() {
                match op {
                    TypedInstruction::ShapeGlyph(_) => {
                        break;
                    }
                    TypedInstruction::ColorGlyph(_) => {
                        is_shape_glyph = false;
                        break;
                    }
                    _ => {}
                }
            }

            is_shape_glyph
        };

        let mut context = Context::new_with(
            state.ctm,
            self.font_bbox,
            &glyph.cache,
            glyph.xref,
            glyph.settings.clone(),
            state,
            glyph.nesting_depth,
        );

        let mut resources = Resources::from_parent(
            self.dict.get(RESOURCES).unwrap_or_default(),
            glyph.parent_resources.clone(),
        );

        // Technically not valid, but also support by Adobe Acrobat. See PDFBOX-5294.
        if let Some(procs_resources) = program.dict().get::<Dict<'_>>(RESOURCES) {
            resources = Resources::from_parent(procs_resources, resources);
        }

        if is_shape_glyph {
            let mut device = Type3ShapeGlyphDevice::new(device, paint.clone());
            interpret(iter, &resources, &mut context, &mut device);
        } else {
            interpret(iter, &resources, &mut context, device);
        }

        Some(())
    }
}

/// Producers that write Type3 fonts without a `ToUnicode` `CMap` often name each
/// glyph after its own character code — `/C41`, `/G41`, `/g0041` — instead of
/// using Adobe Glyph List names (poppler and pdf.js both recognize these).
/// The name carries no Unicode by itself, so it is only trusted when the number
/// it spells, read as hex or decimal, IS the code it is mapped from; the code
/// is then read as Latin-1, as pdf.js does. A name that spells another number
/// says nothing reliable and yields `None`, as do C0/C1 control codes.
fn numeric_glyph_name_to_unicode(name: &str, code: u8) -> Option<BfString> {
    let digits = name.strip_prefix(['C', 'c', 'G', 'g'])?;
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }

    let spells_code = u32::from_str_radix(digits, 16).ok() == Some(code as u32)
        || digits.parse::<u32>().ok() == Some(code as u32);
    let character = char::from(code);
    (spells_code && !character.is_control()).then_some(BfString::Char(character))
}

impl CacheKey for Type3<'_> {
    fn cache_key(&self) -> u128 {
        self.dict.cache_key()
    }
}

struct Type3ShapeGlyphDevice<'a, 'b, T: Device<'a>> {
    inner: &'b mut T,
    paint: Paint<'a>,
}

impl<'a, 'b, T: Device<'a>> Type3ShapeGlyphDevice<'a, 'b, T> {
    pub(crate) fn new(device: &'b mut T, paint: Paint<'a>) -> Self {
        Self {
            inner: device,
            paint,
        }
    }
}

// Only filling, stroking of paths and stencil masks are allowed.
impl<'a, T: Device<'a>> Device<'a> for Type3ShapeGlyphDevice<'a, '_, T> {
    fn set_soft_mask(&mut self, m: Option<SoftMask<'a>>) {
        self.inner.set_soft_mask(m);
    }

    fn draw_path(
        &mut self,
        path: &BezPath,
        transform: Affine,
        _: &Paint<'_>,
        draw_mode: &PathDrawMode,
    ) {
        self.inner
            .draw_path(path, transform, &self.paint, draw_mode);
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        self.inner.push_clip_path(clip_path);
    }

    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'_>>, _: BlendMode) {}

    fn draw_glyph(
        &mut self,
        g: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        p: &Paint<'a>,
        draw_mode: &GlyphDrawMode,
    ) {
        self.inner
            .draw_glyph(g, transform, glyph_transform, p, draw_mode);
    }

    fn pop_clip_path(&mut self) {
        self.inner.pop_clip_path();
    }

    fn pop_transparency_group(&mut self) {}

    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        if let Image::Stencil(mut s) = image {
            s.paint = self.paint.clone();
            self.inner.draw_image(Image::Stencil(s), transform);
        }
    }

    fn set_blend_mode(&mut self, _: BlendMode) {}
}

#[cfg(test)]
mod tests {
    use super::numeric_glyph_name_to_unicode;
    use hayro_cmap::BfString;

    #[test]
    fn numeric_glyph_name_maps_when_it_spells_its_own_code() {
        // Hex, as some producers write them (`32 /C20 ... 65 /C41`).
        assert_eq!(
            numeric_glyph_name_to_unicode("C41", 0x41),
            Some(BfString::Char('A'))
        );
        assert_eq!(
            numeric_glyph_name_to_unicode("C2d", 0x2d),
            Some(BfString::Char('-'))
        );
        assert_eq!(
            numeric_glyph_name_to_unicode("CCA", 0xCA),
            Some(BfString::Char('Ê'))
        );
        // Decimal and the other prefixes poppler and pdf.js recognize.
        assert_eq!(
            numeric_glyph_name_to_unicode("C65", 65),
            Some(BfString::Char('A'))
        );
        assert_eq!(
            numeric_glyph_name_to_unicode("G41", 0x41),
            Some(BfString::Char('A'))
        );
        assert_eq!(
            numeric_glyph_name_to_unicode("g0041", 0x41),
            Some(BfString::Char('A'))
        );
    }

    #[test]
    fn numeric_glyph_name_is_not_trusted_when_it_spells_another_code() {
        assert_eq!(numeric_glyph_name_to_unicode("C42", 0x41), None);
        assert_eq!(numeric_glyph_name_to_unicode("Cxyz", 0x41), None);
        assert_eq!(numeric_glyph_name_to_unicode("C", 0x41), None);
        assert_eq!(numeric_glyph_name_to_unicode("A41", 0x41), None);
    }

    #[test]
    fn numeric_glyph_name_skips_control_codes() {
        assert_eq!(numeric_glyph_name_to_unicode("C0a", 0x0a), None);
        assert_eq!(numeric_glyph_name_to_unicode("C85", 0x85), None);
    }
}
