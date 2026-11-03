This is crossfont 0.8.1 (Apache-2.0), vendored for the experimental
FreeType/HarfRust path. Changes are limited to src/ft/mod.rs:

- Keep each resolved face's source path and collection index.
- Expose resolve_font, has_glyph and font_data so shaping uses the exact fallback face.
- Extract existing rasterization into get_glyph_by_index; the original
  character API delegates to it. This preserves fontconfig hinting, rendering,
  synthetic style, LCD filtering, color bitmap and transform behavior.

The new API is available only on FreeType platforms. CoreText and DirectWrite
implementations retain the original crossfont API. Named variable font instances
and arbitrary fontconfig transforms do not yet have matching shaping metrics.
