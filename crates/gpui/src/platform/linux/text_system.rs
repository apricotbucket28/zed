use std::{borrow::Cow, sync::Arc};

use anyhow::{anyhow, Context, Result};
use collections::HashMap;
use font_kit::{
    canvas::{Canvas, Format, RasterizationOptions},
    font::Font as FontKitFont,
    handle::Handle,
    hinting::HintingOptions,
    source::SystemSource,
    sources::mem::MemSource,
};
use harfbuzz_rs::Font as HbFont;
use parking_lot::RwLock;
use pathfinder_geometry::{transform2d::Transform2F, vector::Vector2I};
use smallvec::SmallVec;

use crate::{
    point, Bounds, DevicePixels, Font, FontFeatures, FontId, FontMetrics, FontRun, GlyphId,
    LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams, ShapedGlyph, ShapedRun,
    SharedString, Size,
};

pub(crate) struct LinuxTextSystem(RwLock<LinuxTextSystemState>);

struct LinuxTextSystemState {
    memory_source: MemSource,
    system_source: SystemSource,
    /// Contains all already loaded fonts, including all faces. Indexed by `FontId`.
    loaded_fonts_store: HashMap<FontId, (FontKitFont, Box<()>)>, // TODO: store harfbuzz font
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<SharedString, SmallVec<[FontId; 4]>>,
}

unsafe impl Sync for LinuxTextSystemState {}
unsafe impl Send for LinuxTextSystemState {}

impl LinuxTextSystem {
    pub(crate) fn new() -> Self {
        Self(RwLock::new(LinuxTextSystemState {
            memory_source: MemSource::empty(),
            system_source: SystemSource::new(),
            loaded_fonts_store: HashMap::default(),
            font_ids_by_family_cache: HashMap::default(),
        }))
    }
}

impl Default for LinuxTextSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformTextSystem for LinuxTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    // todo(linux): Return the proper values, though it doesn't seem possible without loading fonts first
    fn all_font_names(&self) -> Vec<String> {
        self.all_font_families()
    }

    fn all_font_families(&self) -> Vec<String> {
        let lock = self.0.read();
        let mut names = Vec::new();
        names.extend(lock.system_source.all_families().unwrap_or_default());
        names.extend(lock.memory_source.all_families().unwrap_or_default());
        names
    }

    fn font_id(&self, font: &Font) -> Result<crate::FontId> {
        let mut state = self.0.write();

        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&font.family) {
            font_ids
        } else {
            let font_ids = state.load_family(&font.family, &font.features)?;
            state
                .font_ids_by_family_cache
                .insert(font.family.clone(), font_ids);
            state.font_ids_by_family_cache[&font.family].as_ref()
        };

        let candidate_properties = candidates
            .iter()
            .map(|font_id| {
                let (font, _) = state.loaded_fonts_store.get(&font_id).unwrap();
                font.properties()
            })
            .collect::<SmallVec<[_; 4]>>();

        let ix =
            font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
                .context("requested font family contains no font matching the other parameters")?;

        dbg!(state.loaded_fonts_store.get(&candidates[ix]).unwrap());

        Ok(candidates[ix])
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let metrics = self
            .0
            .read()
            .loaded_fonts_store
            .get(&font_id)
            .unwrap()
            .0
            .metrics();

        FontMetrics {
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
            line_gap: metrics.line_gap,
            underline_position: metrics.underline_position,
            underline_thickness: metrics.underline_thickness,
            cap_height: metrics.cap_height,
            x_height: metrics.x_height,
            bounding_box: metrics.bounding_box.into(),
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<crate::Bounds<f32>> {
        let lock = self.0.read();
        let font = lock.loaded_fonts_store.get(&font_id).unwrap();
        let bounds = font.0.typographic_bounds(glyph_id.0)?;
        Ok(bounds.into())
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.read().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.read().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> crate::LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }
}

impl LinuxTextSystemState {
    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let handles = fonts.into_iter().map(|bytes| {
            match bytes {
                Cow::Borrowed(embedded_font) => {
                    // todo(linux): Can we remove this allocation?
                    Handle::from_memory(Arc::new(embedded_font.to_vec()), 0)
                }
                Cow::Owned(bytes) => Handle::from_memory(Arc::new(bytes), 0),
            }
        });
        self.memory_source.add_fonts(handles)?;
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
    ) -> Result<SmallVec<[FontId; 4]>> {
        dbg!(features);

        let name = if name == ".SystemUIFont" {
            "sans-serif"
        } else {
            name
        };

        let mut font_ids = SmallVec::new();
        let family = self
            .memory_source
            .select_family_by_name(name)
            .or_else(|_| self.system_source.select_family_by_name(name))?;
        for font in family.fonts() {
            let font = font.load()?;

            // Remove bad fonts
            let has_m_glyph = font.glyph_for_char('m').is_some();
            let allowed_bad_font = font
                .postscript_name()
                .is_some_and(|n| n != "NotoColorEmoji");

            if !has_m_glyph && !allowed_bad_font {
                log::warn!(
                    "font '{}' has no 'm' character and was not loaded",
                    font.full_name()
                );
                continue;
            }

            // let hb_face = harfbuzz_rs::Face::from_bytes(data_slice, 0);
            // let hb_font = HbFont::new(hb_face);

            let font_id = FontId(self.loaded_fonts_store.len());
            self.loaded_fonts_store
                .insert(font_id, (font, Box::new(())));
            font_ids.push(font_id);
        }

        Ok(font_ids)
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let font = self.loaded_fonts_store.get(&font_id).unwrap();
        let advance = font.0.advance(glyph_id.0)?;
        Ok(advance.into())
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let font = self.loaded_fonts_store.get(&font_id).unwrap();
        font.0.glyph_for_char(ch).map(|g| GlyphId(g))
    }

    // fn is_emoji(&self, font_id: FontId) -> bool {
    //     // TODO: Include other common emoji fonts
    //     self.postscript_names
    //         .get(&font_id)
    //         .map_or(false, |postscript_name| postscript_name == "NotoColorEmoji")
    // }

    fn raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let font = self.loaded_fonts_store.get(&params.font_id).unwrap();
        let scale = Transform2F::from_scale(params.scale_factor);
        let bounds = font.0.raster_bounds(
            params.glyph_id.0,
            params.font_size.into(),
            scale,
            HintingOptions::None,
            font_kit::canvas::RasterizationOptions::GrayscaleAa,
        )?;
        Ok(bounds.into())
    }

    #[profiling::function]
    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            return Err(anyhow!("glyph bounds are empty"));
        }

        // todo(linux) handle subpixel variants

        // Inside value is ignored by font-kit
        let hinting_options = HintingOptions::Full(0.);
        let rasterization_options = if params.is_emoji {
            RasterizationOptions::SubpixelAa
        } else {
            RasterizationOptions::GrayscaleAa
        };

        let (font, _) = self.loaded_fonts_store.get(&params.font_id).unwrap();

        let scale = Transform2F::from_scale(params.scale_factor);
        // TODO: use glyph_bounds instead
        let raster_bounds = font.raster_bounds(
            params.glyph_id.0,
            params.font_size.into(),
            scale,
            hinting_options,
            rasterization_options,
        )?;

        // TODO: do we need a different format for emojis?
        let mut canvas = Canvas::new(raster_bounds.size(), Format::A8);
        font.rasterize_glyph(
            &mut canvas,
            params.glyph_id.0,
            params.font_size.into(),
            Transform2F::from_translation(-raster_bounds.origin().to_f32()) * scale,
            hinting_options,
            rasterization_options,
        )?;

        debug_assert_eq!(
            <Vector2I as Into<Size<DevicePixels>>>::into(canvas.size),
            glyph_bounds.size
        );

        // // Define the characters to represent brightness levels
        // let gradient = " .:-=+*#%@";

        // // Function to map a pixel value to a character
        // fn pixel_to_char(value: u8, gradient: &str) -> char {
        //     let num_levels = gradient.len() as u8;
        //     let level = (value as usize * (num_levels as usize - 1)) / 255;
        //     gradient.chars().nth(level).unwrap()
        // }

        // // Print the canvas
        // for y in 0..canvas.size.y() {
        //     for x in 0..canvas.size.x() {
        //         let index = y * raster_bounds.width() + x;
        //         let pixel_value = canvas.pixels[index as usize];
        //         let pixel_char = pixel_to_char(pixel_value, gradient);
        //         print!("{}", pixel_char);
        //     }
        //     println!("-----------------------------------------------------");
        // }

        if params.is_emoji {
            // Convert from RGBA to BGRA
            for pixel in canvas.pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }

        Ok((canvas.size.into(), canvas.pixels))
    }

    #[profiling::function]
    fn layout_line(
        &self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
    ) -> crate::LineLayout {
        // println!("layout_line: {text:?}");

        // TODO: enable features specified in load_family
        let features = vec![
            harfbuzz_rs::Feature::new(b"kern", 0, 0..),
            harfbuzz_rs::Feature::new(b"liga", 0, 0..),
            harfbuzz_rs::Feature::new(b"clig", 0, 0..),
        ];

        let mut utf8_offset = 0;

        let mut x_offset = 0.;
        let mut ascent: f32 = 0.;
        let mut descent: f32 = 0.;
        let mut runs = Vec::with_capacity(font_runs.len());

        for run in font_runs {
            let (font, _hb_font) = self.loaded_fonts_store.get(&run.font_id).unwrap();
            let font_metrics = font.metrics();
            ascent =
                ascent.max(font_metrics.ascent * font_size.0 / font_metrics.units_per_em as f32);
            descent = descent
                .max(font_metrics.descent.abs() * font_size.0 / font_metrics.units_per_em as f32);

            // TODO: load font in load_family
            let data = font.copy_font_data().unwrap();
            let face = harfbuzz_rs::Face::from_bytes(&data, 0);
            let hb_font = harfbuzz_rs::Font::new(face);
            // hb_font.get_*

            let text = &text[utf8_offset..(utf8_offset + run.len)];
            utf8_offset += run.len;

            let buffer = harfbuzz_rs::UnicodeBuffer::new()
                .add_str(text)
                .guess_segment_properties();

            let shape_info = harfbuzz_rs::shape(&hb_font, buffer, &features);
            if shape_info.is_empty() {
                continue;
            }

            let glyph_infos = shape_info.get_glyph_infos();
            let glyph_positions = shape_info.get_glyph_positions();

            let mut glyphs = SmallVec::with_capacity(glyph_infos.len());
            for (info, pos) in glyph_infos.iter().zip(glyph_positions) {
                if info.codepoint == 0 {
                    x_offset += pos.x_advance as f32 / 64.;
                    continue; // TODO: font fallback
                }

                let position = point(x_offset.into(), Pixels::ZERO);

                // TODO: cache
                let advance = font
                    .advance(info.codepoint)
                    .expect("glyph should always be found");
                x_offset += advance.x() * font_size.0 / font_metrics.units_per_em as f32;

                glyphs.push(ShapedGlyph {
                    id: GlyphId(info.codepoint),
                    position,
                    index: info.cluster as usize,
                    is_emoji: false, // TODO
                });
            }

            runs.push(ShapedRun {
                font_id: run.font_id,
                glyphs,
            });
        }

        LineLayout {
            font_size,
            width: x_offset.into(),
            ascent: ascent.into(),
            descent: descent.into(),
            runs,
            len: text.len(),
        }
    }
}

// impl From<RectF> for Bounds<f32> {
//     fn from(rect: RectF) -> Self {
//         Bounds {
//             origin: point(rect.origin_x(), rect.origin_y()),
//             size: size(rect.width(), rect.height()),
//         }
//     }
// }

// impl From<RectI> for Bounds<DevicePixels> {
//     fn from(rect: RectI) -> Self {
//         Bounds {
//             origin: point(DevicePixels(rect.origin_x()), DevicePixels(rect.origin_y())),
//             size: size(DevicePixels(rect.width()), DevicePixels(rect.height())),
//         }
//     }
// }

// impl From<Vector2I> for Size<DevicePixels> {
//     fn from(value: Vector2I) -> Self {
//         size(value.x().into(), value.y().into())
//     }
// }

// impl From<RectI> for Bounds<i32> {
//     fn from(rect: RectI) -> Self {
//         Bounds {
//             origin: point(rect.origin_x(), rect.origin_y()),
//             size: size(rect.width(), rect.height()),
//         }
//     }
// }

// impl From<Point<u32>> for Vector2I {
//     fn from(size: Point<u32>) -> Self {
//         Vector2I::new(size.x as i32, size.y as i32)
//     }
// }

// impl From<Vector2F> for Size<f32> {
//     fn from(vec: Vector2F) -> Self {
//         size(vec.x(), vec.y())
//     }
// }

fn font_into_properties(font: &crate::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            crate::FontStyle::Normal => font_kit::properties::Style::Normal,
            crate::FontStyle::Italic => font_kit::properties::Style::Italic,
            crate::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: Default::default(),
    }
}
