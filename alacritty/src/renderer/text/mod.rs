use bitflags::bitflags;
use crossfont::{GlyphKey, RasterizedGlyph};

use alacritty_terminal::term::cell::Flags;

use crate::display::SizeInfo;
use crate::display::content::RenderableCell;
use crate::gl;
use crate::gl::types::*;

mod atlas;
mod builtin_font;
mod gles2;
mod glsl3;
pub mod glyph_cache;
#[cfg(not(any(target_os = "macos", windows)))]
mod shaping;

use atlas::Atlas;
pub use gles2::Gles2Renderer;
pub use glsl3::Glsl3Renderer;
pub use glyph_cache::GlyphCache;
use glyph_cache::{Glyph, LoadGlyph};

// NOTE: These flags must be in sync with their usage in the text.*.glsl shaders.
bitflags! {
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct RenderingGlyphFlags: u8 {
        const COLORED   = 0b0000_0001;
        const WIDE_CHAR = 0b0000_0010;
    }
}

/// Rendering passes, for both GLES2 and GLSL3 renderer.
#[repr(u8)]
enum RenderingPass {
    /// Rendering pass used to render background color in text shaders.
    Background = 0,

    /// The first pass to render text with both GLES2 and GLSL3 renderers.
    SubpixelPass1 = 1,

    /// The second pass to render text with GLES2 renderer.
    SubpixelPass2 = 2,

    /// The third pass to render text with GLES2 renderer.
    SubpixelPass3 = 3,
}

pub trait TextRenderer<'a> {
    type Shader: TextShader;
    type RenderBatch: TextRenderBatch;
    type RenderApi: TextRenderApi<Self::RenderBatch>;

    /// Get loader API for the renderer.
    fn loader_api(&mut self) -> LoaderApi<'_>;

    /// Draw cells.
    fn draw_cells<'b: 'a, I: Iterator<Item = RenderableCell>>(
        &'b mut self,
        size_info: &'b SizeInfo,
        glyph_cache: &'a mut GlyphCache,
        cells: I,
    ) {
        self.with_api(size_info, |mut api| {
            #[cfg(not(any(target_os = "macos", windows)))]
            if glyph_cache.shaping {
                api.draw_shaped_cells(cells.collect(), glyph_cache, size_info);
                return;
            }
            for cell in cells {
                api.draw_cell(cell, glyph_cache, size_info);
            }
        })
    }

    fn with_api<'b: 'a, F, T>(&'b mut self, size_info: &'b SizeInfo, func: F) -> T
    where
        F: FnOnce(Self::RenderApi) -> T;

    fn program(&self) -> &Self::Shader;

    /// Resize the text rendering.
    fn resize(&self, size: &SizeInfo) {
        unsafe {
            let program = self.program();
            gl::UseProgram(program.id());
            update_projection(program.projection_uniform(), size);
            gl::UseProgram(0);
        }
    }

    /// Invoke renderer with the loader.
    fn with_loader<F: FnOnce(LoaderApi<'_>) -> T, T>(&mut self, func: F) -> T {
        unsafe {
            gl::ActiveTexture(gl::TEXTURE0);
        }

        func(self.loader_api())
    }
}

pub trait TextRenderBatch {
    /// Check if `Batch` is empty.
    fn is_empty(&self) -> bool;

    /// Check whether the `Batch` is full.
    fn full(&self) -> bool;

    /// Get texture `Batch` is using.
    fn tex(&self) -> GLuint;

    /// Add item to the batch.
    fn add_item(&mut self, cell: &RenderableCell, glyph: &Glyph, size_info: &SizeInfo);
}

pub trait TextRenderApi<T: TextRenderBatch>: LoadGlyph {
    /// Get `Batch` the api is using.
    fn batch(&mut self) -> &mut T;

    /// Render the underlying data.
    fn render_batch(&mut self);

    /// Add item to the rendering queue.
    #[inline]
    fn add_render_item(&mut self, cell: &RenderableCell, glyph: &Glyph, size_info: &SizeInfo) {
        // Flush batch if tex changing.
        if !self.batch().is_empty() && self.batch().tex() != glyph.tex_id {
            self.render_batch();
        }

        self.batch().add_item(cell, glyph, size_info);

        // Render batch and clear if it's full.
        if self.batch().full() {
            self.render_batch();
        }
    }

    /// Draw backgrounds separately so positioned marks cannot be erased by a
    /// subsequent cell. Shape logical runs, retaining visual cell coordinates.
    #[cfg(not(any(target_os = "macos", windows)))]
    fn draw_shaped_cells(
        &mut self,
        cells: Vec<RenderableCell>,
        cache: &mut GlyphCache,
        size: &SizeInfo,
    ) {
        if !cells.iter().any(|cell| {
            matches!(cell.character,
            '\u{0600}'..='\u{06ff}' | '\u{0750}'..='\u{077f}' | '\u{08a0}'..='\u{08ff}')
        }) {
            for mut cell in cells {
                if cell.rtl {
                    cell.character = unicode_bidi_mirroring::get_mirrored(cell.character)
                        .unwrap_or(cell.character);
                }
                self.draw_cell(cell, cache, size);
            }
            return;
        }
        for cell in &cells {
            let mut background = cell.clone();
            background.character = ' ';
            background.extra = None;
            self.draw_cell(background, cache, size);
        }
        let mut start = 0;
        while start < cells.len() {
            let first = &cells[start];
            let mut end = start + 1;
            while end < cells.len() {
                let prev = &cells[end - 1];
                let cell = &cells[end];
                if cell.point.line != first.point.line
                    || cell.rtl != first.rtl
                    || cell.logical_column != prev.logical_column + 1
                    || (cell.rtl && cell.point.column.0 + 1 != prev.point.column.0)
                    || (!cell.rtl && prev.point.column.0 + 1 != cell.point.column.0)
                    || cell.flags & Flags::BOLD_ITALIC != first.flags & Flags::BOLD_ITALIC
                    || cell.flags.contains(Flags::HIDDEN) != first.flags.contains(Flags::HIDDEN)
                {
                    break;
                }
                end += 1;
            }
            let run = &cells[start..end];
            // Only cursive Arabic-script runs need the new path. ASCII and
            // unrelated scripts retain their original glyph cache behavior.
            let arabic = run.iter().any(|cell| {
                matches!(cell.character,
                '\u{0600}'..='\u{06ff}' | '\u{0750}'..='\u{077f}' |
                '\u{08a0}'..='\u{08ff}')
            });
            let mut text = String::new();
            let mut clusters = Vec::new();
            for (index, cell) in run.iter().enumerate() {
                clusters.push((text.len(), index));
                text.push(cell.character);
                if let Some(marks) = cell.extra.as_ref().and_then(|extra| extra.zerowidth.as_ref())
                {
                    text.extend(marks);
                }
            }
            let font_key = match first.flags & Flags::BOLD_ITALIC {
                Flags::BOLD_ITALIC => cache.bold_italic_key,
                Flags::ITALIC => cache.italic_key,
                Flags::BOLD => cache.bold_key,
                _ => cache.font_key,
            };
            let key = GlyphKey { font_key, size: cache.font_size, character: first.character };
            let shaped = (arabic && !first.flags.contains(Flags::HIDDEN))
                .then(|| cache.shape(&text, key, first.rtl))
                .flatten();
            let mut rendered = false;
            if let Some(shaped) = shaped {
                let advance: f32 = shaped.iter().map(|glyph| glyph.x_advance).sum();
                if advance > 0. {
                    // Fit a complete run into its allocated grid width. Scaling
                    // the complete connected run preserves cursive joins.
                    let scale = run.len() as f32 * size.cell_width() / advance;
                    let left = run.iter().map(|cell| cell.point.column.0).min().unwrap();
                    let mut pen = 0.;
                    let mut pen_y = 0.;
                    let mut positioned = Vec::with_capacity(shaped.len());
                    for shaped in shaped {
                        let Some(mut glyph) =
                            cache.shaped_glyph(shaped.font_key, shaped.glyph_id, self)
                        else {
                            positioned.clear();
                            break;
                        };
                        let cluster = clusters
                            .partition_point(|&(byte, _)| byte <= shaped.cluster as usize)
                            .saturating_sub(1);
                        let mut cell = run[clusters[cluster].1].clone();
                        cell.bg_alpha = 0.;
                        let x = left as f32 * size.cell_width() + (pen + shaped.x_offset) * scale;
                        glyph.left = ((x - cell.point.column.0 as f32 * size.cell_width())
                            + glyph.left as f32 * scale)
                            .round() as i16;
                        glyph.width = (glyph.width as f32 * scale).round() as i16;
                        glyph.top += (pen_y + shaped.y_offset).round() as i16;
                        positioned.push((cell, glyph));
                        pen += shaped.x_advance;
                        pen_y += shaped.y_advance;
                    }
                    if !positioned.is_empty() {
                        for (cell, glyph) in positioned {
                            self.add_render_item(&cell, &glyph, size);
                        }
                        rendered = true;
                    }
                }
            }
            if !rendered {
                for cell in run {
                    let mut cell = cell.clone();
                    cell.bg_alpha = 0.;
                    if cell.rtl {
                        cell.character = unicode_bidi_mirroring::get_mirrored(cell.character)
                            .unwrap_or(cell.character);
                    }
                    self.draw_cell(cell, cache, size);
                }
            }
            start = end;
        }
    }

    /// Draw cell.
    fn draw_cell(
        &mut self,
        mut cell: RenderableCell,
        glyph_cache: &mut GlyphCache,
        size_info: &SizeInfo,
    ) {
        // Get font key for cell.
        let font_key = match cell.flags & Flags::BOLD_ITALIC {
            Flags::BOLD_ITALIC => glyph_cache.bold_italic_key,
            Flags::ITALIC => glyph_cache.italic_key,
            Flags::BOLD => glyph_cache.bold_key,
            _ => glyph_cache.font_key,
        };

        // Ignore hidden cells and render tabs as spaces to prevent font issues.
        let hidden = cell.flags.contains(Flags::HIDDEN);
        if cell.character == '\t' || hidden {
            cell.character = ' ';
        }

        let mut glyph_key =
            GlyphKey { font_key, size: glyph_cache.font_size, character: cell.character };

        // Add cell to batch.
        let glyph = glyph_cache.get(glyph_key, self, true);
        self.add_render_item(&cell, &glyph, size_info);

        // Render visible zero-width characters.
        if let Some(zerowidth) =
            cell.extra.as_mut().and_then(|extra| extra.zerowidth.take().filter(|_| !hidden))
        {
            for character in zerowidth {
                glyph_key.character = character;
                let glyph = glyph_cache.get(glyph_key, self, false);
                self.add_render_item(&cell, &glyph, size_info);
            }
        }
    }
}

pub trait TextShader {
    fn id(&self) -> GLuint;

    /// Id of the projection uniform.
    fn projection_uniform(&self) -> GLint;
}

#[derive(Debug)]
pub struct LoaderApi<'a> {
    active_tex: &'a mut GLuint,
    atlas: &'a mut Vec<Atlas>,
    current_atlas: &'a mut usize,
}

impl LoadGlyph for LoaderApi<'_> {
    fn load_glyph(&mut self, rasterized: &RasterizedGlyph) -> Glyph {
        Atlas::load_glyph(self.active_tex, self.atlas, self.current_atlas, rasterized)
    }

    fn clear(&mut self) {
        Atlas::clear_atlas(self.atlas, self.current_atlas)
    }
}

fn update_projection(u_projection: GLint, size: &SizeInfo) {
    let width = size.width();
    let height = size.height();
    let padding_x = size.padding_x();
    let padding_y = size.padding_y();

    // Bounds check.
    if (width as u32) < (2 * padding_x as u32) || (height as u32) < (2 * padding_y as u32) {
        return;
    }

    // Compute scale and offset factors, from pixel to ndc space. Y is inverted.
    //   [0, width - 2 * padding_x] to [-1, 1]
    //   [height - 2 * padding_y, 0] to [-1, 1]
    let scale_x = 2. / (width - 2. * padding_x);
    let scale_y = -2. / (height - 2. * padding_y);
    let offset_x = -1.;
    let offset_y = 1.;

    unsafe {
        gl::Uniform4f(u_projection, offset_x, offset_y, scale_x, scale_y);
    }
}
