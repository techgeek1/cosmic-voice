//! Painting the candidate window.
//!
//! Everything here is pure: a [`View`] and a [`Palette`] go in, a
//! [`tiny_skia::Pixmap`] comes out. No Wayland, no IBus, no state that outlives
//! a frame except the font cache. That is what lets the layout be checked by a
//! unit test that writes a PNG and looks at it, rather than only by a human
//! squinting at a nested compositor.
//!
//! # Why these two crates
//!
//! An input-popup surface is a bare `wl_surface`. There is no toolkit that can
//! target one — iced draws into a window it owns, GTK the same — so the pixels
//! are ours to produce. `tiny-skia` is a rasteriser and nothing else, which is
//! all the boxes and rules here need, and `cosmic-text` does the part that
//! actually matters: shaping. The candidates are Japanese. A renderer that
//! cannot shape and cannot fall back across fonts renders a row of tofu, which
//! would make the whole phase pointless.
//!
//! # Fonts
//!
//! [`Renderer::new`] picks a family by name rather than asking fontconfig for
//! `sans-serif`, because on this machine `fc-match sans-serif:lang=ja` answers
//! *WenQuanYi Zen Hei* — a Chinese font, whose kanji are the Chinese glyph
//! variants. It renders Japanese without tofu and it is still wrong: 直, 骨 and
//! 対 are visibly not the Japanese forms. So the preference list is explicit,
//! Japanese-first, and what was actually chosen goes in the log.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};

use super::theme::{Palette, Rgba};

// --- Metrics ---

/// Candidate text size, in logical pixels.
///
/// Slightly larger than UI text: the whole job of this window is to let someone
/// tell 講 from 構 at a glance, and CJK needs the pixels for that in a way that
/// Latin does not.
const FONT_SIZE: f32 = 15.0;

/// Line height for that size. 1.4× is roughly what CJK ascenders and descenders
/// need before rows start touching.
const LINE_HEIGHT: f32 = 21.0;

/// Space between a label and the candidate it labels.
const LABEL_GAP: f32 = 8.0;

/// Horizontal inset inside a candidate row, so the highlight is not flush with
/// the text.
const ROW_PAD_X: f32 = 6.0;

/// Vertical inset inside a candidate row.
const ROW_PAD_Y: f32 = 3.0;

/// Space between candidates when they run left to right.
const CELL_GAP: f32 = 10.0;

/// The widest window we will draw, in logical pixels.
///
/// A candidate can be an arbitrarily long phrase — mozc will happily offer a
/// whole clause — and a popup wider than the screen is worse than a truncated
/// one, because cosmic-comp positions from the buffer size and would push it
/// entirely off the far edge.
const MAX_WIDTH: f32 = 720.0;

/// The tallest window we will draw. A page is at most sixteen candidates
/// (`ibuslookuptable.c:224` asserts it), plus the auxiliary and page lines.
const MAX_HEIGHT: f32 = 560.0;

// --- What to draw ---

/// One candidate and the key that picks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The label, already resolved: either the engine's or IBus's default.
    pub label: String,
    /// The candidate text.
    pub text : String,
}

/// A single frame's worth of content, with every IBus quirk already resolved.
///
/// The paging arithmetic, the label defaults and the orientation choice all
/// happen in [`super::popup`]; by the time a view exists there is nothing left
/// to decide, which is what makes this testable.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    /// The candidates on the current page, in order.
    pub rows    : Vec<Row>,
    /// Which of them is focused, if the engine says to show a focus at all.
    pub cursor  : Option<usize>,
    /// The auxiliary text, e.g. mozc's 「Tabキーで選択」.
    pub aux     : Option<String>,
    /// Current page and page count, 1-based, when there is more than one.
    pub page    : Option<(u32, u32)>,
    /// Whether candidates run top to bottom rather than left to right.
    pub vertical: bool,
}

impl View {
    /// Whether there is anything to show. An empty view is not drawn at all —
    /// the surface is unmapped instead.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.aux.is_none()
    }
}

// --- The renderer ---

/// Font machinery, kept across frames because building it is expensive.
///
/// `FontSystem::new` scans every font on the system through fontconfig, which
/// costs a few hundred milliseconds; the swash cache holds rasterised glyphs,
/// which is what makes a redraw per keystroke cheap. Neither can live in a
/// frame.
pub struct Renderer {
    /// The font database and shaping cache.
    fonts : FontSystem,
    /// Rasterised glyph bitmaps, keyed by glyph and size.
    cache : SwashCache,
    /// The family chosen from [`PREFERRED_FAMILIES`], if any was found.
    family: Option<String>,
}

/// Families to try, in order, before falling back to generic sans-serif.
///
/// Japanese-first for the reason in the module docs. `Noto Sans CJK JP` is the
/// packaged default on this machine; the rest are what other distributions ship
/// for the same job.
const PREFERRED_FAMILIES: &[&str] = &[
    "Noto Sans CJK JP",
    "Source Han Sans JP",
    "Noto Sans JP",
    "IPAPGothic",
    "VL PGothic",
];

impl Renderer {
    /// Builds the font machinery and picks a family.
    ///
    /// Logs which family was chosen, because "the candidate window is full of
    /// boxes" and "the candidate window has Chinese kanji in it" are both
    /// font-selection bugs that are invisible from the code.
    pub fn new() -> Self {
        let started = std::time::Instant::now();
        let fonts = FontSystem::new();

        let available: Vec<&str> = PREFERRED_FAMILIES
            .iter()
            .copied()
            .filter(|wanted| {
                fonts.db().faces().any(|face| {
                    face.families
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case(wanted))
                })
            })
            .collect();

        let family = available.first().map(|name| (*name).to_string());
        match &family {
            Some(name) => tracing::info!(
                "candidate font {name:?} ({} faces, {:.0?} to load)",
                fonts.db().len(),
                started.elapsed(),
            ),
            None => tracing::warn!(
                "no Japanese font found among {PREFERRED_FAMILIES:?}; falling back to \
                 sans-serif, which fontconfig may resolve to a font with Chinese kanji forms",
            ),
        }

        Self {
            fonts : fonts,
            cache : SwashCache::new(),
            family: family,
        }
    }

    /// The attribute set every string is shaped with.
    fn attrs(&self) -> Attrs<'_> {
        match &self.family {
            Some(name) => Attrs::new().family(Family::Name(name)),
            None       => Attrs::new(),
        }
    }

    /// Shapes one string and measures it.
    ///
    /// Unbounded width on purpose: the layout wants the natural width of every
    /// piece before it can decide how wide the window is, and clamping happens
    /// once, at the end, against [`MAX_WIDTH`].
    fn measure(&mut self, text: &str) -> Line {
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        buffer.set_size(None, None);
        buffer.set_text(text, &self.attrs(), Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.fonts, false);

        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max);

        Line {
            buffer: buffer,
            width : width,
        }
    }

    /// Draws a view, returning the pixmap and its size in pixels.
    ///
    /// Never fails: a view that lays out larger than [`MAX_WIDTH`] by
    /// [`MAX_HEIGHT`] is clipped rather than rejected, because the caller's only
    /// alternative would be to show nothing.
    pub fn render(&mut self, view: &View, palette: &Palette) -> Pixmap {
        let mut layout = self.lay_out(view, palette);

        let mut pixmap = Pixmap::new(layout.width.max(1.0) as u32, layout.height.max(1.0) as u32)
            .unwrap_or_else(|| Pixmap::new(1, 1).expect("a 1x1 pixmap always allocates"));

        self.paint(&mut pixmap, view, palette, &mut layout);

        pixmap
    }
}

// --- Layout ---

/// One shaped string, with the width it wants.
struct Line {
    /// The shaped glyphs.
    buffer: Buffer,
    /// Natural width in logical pixels.
    width : f32,
}

/// Where everything goes, computed before anything is drawn.
///
/// Separated from painting because the window's size is a *result* of the
/// layout — cosmic-comp positions the popup from the committed buffer's extent
/// (`xdg_shell/popup.rs:181`), so the size has to be known before the pixmap
/// can even be allocated.
struct Layout {
    /// Window width in pixels.
    width      : f32,
    /// Window height in pixels.
    height     : f32,
    /// The auxiliary line, if any, and its baseline origin.
    aux        : Option<(Line, f32)>,
    /// The page counter, if any, and its origin.
    page       : Option<(Line, f32, f32)>,
    /// One entry per candidate: label, text, and the row rectangle they sit in.
    rows       : Vec<PlacedRow>,
    /// Where the rule under the auxiliary text goes, if it is drawn.
    divider_y  : Option<f32>,
}

/// A candidate placed on the canvas.
struct PlacedRow {
    /// The shaped label.
    label : Line,
    /// The shaped candidate.
    text  : Line,
    /// The highlight rectangle, which is also what the row's contents are
    /// positioned inside.
    rect  : Rect,
    /// Where the label's glyphs start.
    label_x: f32,
    /// Where the candidate's glyphs start.
    text_x : f32,
}

impl Renderer {
    /// Measures everything and decides where it goes.
    fn lay_out(&mut self, view: &View, palette: &Palette) -> Layout {
        let pad = palette.padding;
        let aux = view.aux.as_ref().map(|text| self.measure(text));
        let page = view
            .page
            .map(|(current, total)| self.measure(&format!("{current}/{total}")));

        let labels: Vec<Line> = view.rows.iter().map(|row| self.measure(&row.label)).collect();
        let texts: Vec<Line> = view.rows.iter().map(|row| self.measure(&row.text)).collect();

        // The label column is as wide as its widest member so the candidates
        // line up, which is the single thing that makes a vertical list
        // scannable. Horizontal lists have no column to align.
        let label_column = labels.iter().map(|line| line.width).fold(0.0_f32, f32::max);
        let row_height = LINE_HEIGHT + 2.0 * ROW_PAD_Y;

        let mut placed = Vec::with_capacity(view.rows.len());
        let mut y = pad;
        let mut width = 0.0_f32;

        // The auxiliary line sits above the candidates and gets a rule under it,
        // the way IBus's own panel packs it (`ui/gtk3/candidatepanel.vala:478`).
        // Indented to the same column as the labels, so the left edge of the
        // window's text is one line rather than two.
        let text_inset = pad + ROW_PAD_X;
        let aux_placed = aux.map(|line| {
            width = width.max(line.width + 2.0 * text_inset);
            let at = y;
            y += LINE_HEIGHT;
            (line, at)
        });
        let divider_y = aux_placed.as_ref().filter(|_| !view.rows.is_empty()).map(|_| {
            let at = y + pad * 0.5;
            y += pad;
            at
        });

        if view.vertical {
            for (label, text) in labels.into_iter().zip(texts) {
                let rect = Rect::from_xywh(pad, y, 0.0, row_height);
                let label_x = pad + ROW_PAD_X;
                let text_x = label_x + label_column + LABEL_GAP;
                width = width.max(text_x + text.width + ROW_PAD_X + pad);
                placed.push(PlacedRow {
                    label  : label,
                    text   : text,
                    rect   : rect.unwrap_or(unit_rect()),
                    label_x: label_x,
                    text_x : text_x,
                });
                y += row_height;
            }
        } else {
            let mut x = pad;
            for (label, text) in labels.into_iter().zip(texts) {
                let cell = ROW_PAD_X + label.width + LABEL_GAP + text.width + ROW_PAD_X;
                let label_x = x + ROW_PAD_X;
                let text_x = label_x + label.width + LABEL_GAP;
                placed.push(PlacedRow {
                    label  : label,
                    text   : text,
                    rect   : Rect::from_xywh(x, y, cell, row_height).unwrap_or(unit_rect()),
                    label_x: label_x,
                    text_x : text_x,
                });
                x += cell + CELL_GAP;
            }
            width = width.max(x - CELL_GAP + pad);
            if !placed.is_empty() {
                y += row_height;
            }
        }

        // The page counter is right-aligned on a line of its own, which is
        // where every IME that shows one puts it.
        let page_placed = page.map(|line| {
            let at_y = y;
            y += LINE_HEIGHT;
            width = width.max(line.width + 2.0 * text_inset);
            (line, at_y)
        });
        let mut height = (y + pad).min(MAX_HEIGHT);
        width = width.clamp(1.0, MAX_WIDTH);
        if height < 1.0 {
            height = 1.0;
        }

        // Vertical rows are stretched to the final width now that it is known;
        // a highlight that stops at the text's edge looks like a mistake.
        let mut layout = Layout {
            width    : width,
            height   : height,
            aux      : aux_placed,
            page     : page_placed.map(|(line, at)| {
                let x = width - text_inset - line.width;
                (line, x, at)
            }),
            rows     : placed,
            divider_y: divider_y,
        };
        if view.vertical {
            let row_width = width - 2.0 * pad;
            for row in &mut layout.rows {
                row.rect = Rect::from_xywh(row.rect.x(), row.rect.y(), row_width, row.rect.height())
                    .unwrap_or(unit_rect());
            }
        }

        layout
    }
}

/// A rectangle for the case `Rect::from_xywh` rejects, which is a zero or
/// negative extent. Nothing here should produce one; a degenerate rectangle is
/// still better than a panic inside an input method.
fn unit_rect() -> Rect {
    Rect::from_xywh(0.0, 0.0, 1.0, 1.0).expect("a 1x1 rect is valid")
}

// --- Painting ---

impl Renderer {
    /// Draws a laid-out view into a pixmap.
    fn paint(&mut self, pixmap: &mut Pixmap, view: &View, palette: &Palette, layout: &mut Layout) {
        let width = layout.width;
        let height = layout.height;

        fill_round_rect(pixmap, 0.0, 0.0, width, height, palette.radius, palette.background);
        stroke_round_rect(pixmap, width, height, palette.radius, palette.border);

        if let Some((line, y)) = &mut layout.aux {
            let x = palette.padding + ROW_PAD_X;
            draw_line(pixmap, &mut self.fonts, &mut self.cache, line, x, *y, palette.dim);
        }

        if let Some(y) = layout.divider_y {
            fill_rect(pixmap, palette.padding, y, width - 2.0 * palette.padding, 1.0, palette.divider);
        }

        for (index, row) in layout.rows.iter_mut().enumerate() {
            let focused = view.cursor == Some(index);
            if focused {
                fill_round_rect(
                    pixmap,
                    row.rect.x(),
                    row.rect.y(),
                    row.rect.width(),
                    row.rect.height(),
                    palette.radius * 0.5,
                    palette.selection,
                );
            }

            let (label_colour, text_colour) = if focused {
                (palette.on_selection, palette.on_selection)
            } else {
                (palette.dim, palette.text)
            };
            let text_y = row.rect.y() + ROW_PAD_Y;
            let (label_x, text_x) = (row.label_x, row.text_x);
            draw_line(pixmap, &mut self.fonts, &mut self.cache, &mut row.label, label_x, text_y, label_colour);
            draw_line(pixmap, &mut self.fonts, &mut self.cache, &mut row.text, text_x, text_y, text_colour);
        }

        if let Some((line, x, y)) = &mut layout.page {
            let (x, y) = (*x, *y);
            draw_line(pixmap, &mut self.fonts, &mut self.cache, line, x, y, palette.dim);
        }
    }
}

/// Blits one shaped line at a position.
///
/// `Buffer::draw` hands back one callback per *pixel* of glyph coverage for
/// mask glyphs, with the coverage already multiplied into the alpha, so the
/// blend is done by hand into the pixmap rather than by a tiny-skia fill per
/// pixel. Colour glyphs (emoji, which a candidate list can contain) arrive as
/// larger rectangles and take the same path.
fn draw_line(
    pixmap: &mut Pixmap,
    fonts : &mut FontSystem,
    cache : &mut SwashCache,
    line  : &mut Line,
    x     : f32,
    y     : f32,
    colour: Rgba,
) {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    let origin_x = x.round() as i32;
    let origin_y = y.round() as i32;
    let pixels = pixmap.pixels_mut();

    line.buffer.draw(fonts, cache, colour.text(), |gx, gy, gw, gh, glyph| {
        if glyph.a() == 0 {
            return;
        }
        for row in 0..gh as i32 {
            let py = origin_y + gy + row;
            if py < 0 || py >= height {
                continue;
            }
            for column in 0..gw as i32 {
                let px = origin_x + gx + column;
                if px < 0 || px >= width {
                    continue;
                }
                blend(&mut pixels[(py * width + px) as usize], glyph);
            }
        }
    });
}

/// Source-over of a straight-alpha colour onto a premultiplied pixel.
///
/// tiny-skia stores premultiplied RGBA and cosmic-text produces straight alpha,
/// so the source is premultiplied on the way in. Getting this backwards is the
/// classic way to end up with text that has a dark halo.
fn blend(destination: &mut tiny_skia::PremultipliedColorU8, source: cosmic_text::Color) {
    let alpha = source.a() as u32;
    let inverse = 255 - alpha;
    let over = |src: u8, dst: u8| ((src as u32 * alpha + dst as u32 * inverse + 127) / 255) as u8;

    let red = over(source.r(), destination.red());
    let green = over(source.g(), destination.green());
    let blue = over(source.b(), destination.blue());
    let out_alpha = (alpha + (destination.alpha() as u32 * inverse + 127) / 255).min(255) as u8;

    *destination = tiny_skia::PremultipliedColorU8::from_rgba(red, green, blue, out_alpha)
        .unwrap_or(*destination);
}

/// Fills an axis-aligned rectangle.
fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, width: f32, height: f32, colour: Rgba) {
    let Some(rect) = Rect::from_xywh(x, y, width, height) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(colour.skia());
    paint.anti_alias = true;

    pixmap.fill_rect(rect, &paint, Transform::identity(), None);
}

/// Fills a rounded rectangle, which is the window itself and every highlight.
fn fill_round_rect(
    pixmap: &mut Pixmap,
    x     : f32,
    y     : f32,
    width : f32,
    height: f32,
    radius: f32,
    colour: Rgba,
) {
    let Some(path) = round_rect(x, y, width, height, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(colour.skia());
    paint.anti_alias = true;

    pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
}

/// Strokes the window outline just inside its own edge.
///
/// Inset by half a pixel so the one-pixel stroke lands on whole pixels instead
/// of straddling the boundary and coming out grey on both sides.
fn stroke_round_rect(pixmap: &mut Pixmap, width: f32, height: f32, radius: f32, colour: Rgba) {
    let Some(path) = round_rect(0.5, 0.5, width - 1.0, height - 1.0, radius) else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color(colour.skia());
    paint.anti_alias = true;
    let stroke = tiny_skia::Stroke {
        width: 1.0,
        ..tiny_skia::Stroke::default()
    };

    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// A rounded-rectangle path, or `None` for a degenerate size.
fn round_rect(x: f32, y: f32, width: f32, height: f32, radius: f32) -> Option<tiny_skia::Path> {
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    let radius = radius.min(width * 0.5).min(height * 0.5).max(0.0);
    let (right, bottom) = (x + width, y + height);
    // Kappa: the control-point distance that makes a cubic approximate a
    // quarter circle to within a fraction of a pixel.
    let kappa = radius * (1.0 - 0.552_284_8);

    let mut builder = PathBuilder::new();
    builder.move_to(x + radius, y);
    builder.line_to(right - radius, y);
    builder.cubic_to(right - kappa, y, right, y + kappa, right, y + radius);
    builder.line_to(right, bottom - radius);
    builder.cubic_to(right, bottom - kappa, right - kappa, bottom, right - radius, bottom);
    builder.line_to(x + radius, bottom);
    builder.cubic_to(x + kappa, bottom, x, bottom - kappa, x, bottom - radius);
    builder.line_to(x, y + radius);
    builder.cubic_to(x, y + kappa, x + kappa, y, x + radius, y);
    builder.close();

    builder.finish()
}

/// Copies a rendered pixmap into a `wl_shm` `argb8888` buffer.
///
/// The two formats are the same four bytes in a different order:
/// `wl_shm::Format::Argb8888` is a native-endian 32-bit word, so on the
/// little-endian machines this runs on it is B, G, R, A in memory, while
/// tiny-skia's pixmap is R, G, B, A. Both are premultiplied, so the swap is the
/// whole conversion.
///
/// `stride` is in bytes and may exceed `4 * width`; the rows are copied
/// individually rather than as one block for that reason.
pub fn to_argb8888(pixmap: &Pixmap, destination: &mut [u8], stride: usize) {
    let width = pixmap.width() as usize;
    let source = pixmap.data();

    for row in 0..pixmap.height() as usize {
        let from = &source[row * width * 4..(row + 1) * width * 4];
        let Some(to) = destination.get_mut(row * stride..row * stride + width * 4) else {
            return;
        };
        for (pixel, out) in from.chunks_exact(4).zip(to.chunks_exact_mut(4)) {
            out[0] = pixel[2];
            out[1] = pixel[1];
            out[2] = pixel[0];
            out[3] = pixel[3];
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture that looks like a real mozc conversion: Japanese candidates,
    /// numeric labels, the focus on the second one, and the mode hint mozc puts
    /// in the auxiliary text.
    fn fixture() -> View {
        View {
            rows    : vec![
                Row { label: "1.".into(), text: "今日は".into() },
                Row { label: "2.".into(), text: "こんにちは".into() },
                Row { label: "3.".into(), text: "コンニチハ".into() },
                Row { label: "4.".into(), text: "今日わ".into() },
            ],
            cursor  : Some(1),
            aux     : Some("Tabキーで選択".into()),
            page    : Some((1, 3)),
            vertical: true,
        }
    }

    /// Renders the fixture to a PNG under the scratchpad so the layout can be
    /// *looked at*. A candidate window is a visual artefact; no assertion about
    /// pixel counts tells you the label column is misaligned.
    ///
    /// The assertions are the ones worth automating: it produced a plausibly
    /// sized window, and it actually drew glyphs rather than an empty box.
    #[test]
    fn renders_a_japanese_candidate_list() {
        let mut renderer = Renderer::new();
        let palette = Palette::fallback();
        let view = fixture();

        let pixmap = renderer.render(&view, &palette);
        assert!(pixmap.width() > 80, "suspiciously narrow: {}", pixmap.width());
        assert!(pixmap.height() > 100, "suspiciously short: {}", pixmap.height());

        // Distinct colours in the image: the background, the border, the
        // highlight and antialiased text all differ, so a blank render — the
        // symptom of a font that resolved to nothing — collapses this count.
        let distinct = distinct_colours(&pixmap);
        assert!(distinct > 50, "only {distinct} distinct colours; text probably did not draw");

        write_png(&pixmap, "candidates-vertical.png");
    }

    /// The other orientation. Engines that set `IBUS_ORIENTATION_HORIZONTAL`
    /// are rarer than vertical ones but they exist, and the layout is different
    /// enough that it needs looking at too.
    #[test]
    fn renders_a_horizontal_candidate_list() {
        let mut renderer = Renderer::new();
        let palette = Palette::fallback();
        let view = View {
            vertical: false,
            page    : None,
            ..fixture()
        };

        let pixmap = renderer.render(&view, &palette);
        assert!(pixmap.width() > pixmap.height(), "horizontal should be wide, not tall");

        write_png(&pixmap, "candidates-horizontal.png");
    }

    /// mozc shows the mode hint on its own, with no candidates, while the user
    /// is still typing kana. IBus's panel renders that as a standalone window
    /// (`ui/gtk3/candidatepanel.vala:427-444`) and so do we.
    #[test]
    fn renders_auxiliary_text_alone() {
        let mut renderer = Renderer::new();
        let palette = Palette::fallback();
        let view = View {
            aux     : Some("ひらがな".into()),
            vertical: true,
            ..View::default()
        };

        assert!(!view.is_empty());
        let pixmap = renderer.render(&view, &palette);
        assert!(pixmap.height() < 60, "an aux-only window should be one line tall");

        write_png(&pixmap, "candidates-aux-only.png");
    }

    /// Nothing to show has to stay nothing, because the popup's answer to an
    /// empty view is to unmap rather than to draw a blank box.
    #[test]
    fn an_empty_view_is_empty() {
        assert!(View::default().is_empty());
    }

    /// The same fixture through [`Palette::load`], which is what actually runs.
    ///
    /// Asserts almost nothing on purpose — the palette depends on the machine's
    /// COSMIC config, and on a machine without one this is the fallback path
    /// again. Its value is the PNG: it is the only way to see whether the
    /// user's accent colour is legible behind candidate text before shipping
    /// it to their screen.
    #[test]
    fn renders_with_the_configured_theme() {
        let mut renderer = Renderer::new();
        let palette = Palette::load();
        println!("palette from {}", palette.source);

        let pixmap = renderer.render(&fixture(), &palette);
        assert!(pixmap.width() > 0 && pixmap.height() > 0);

        write_png(&pixmap, "candidates-theme.png");
    }

    /// The `wl_shm` conversion, checked on one pixel of each channel. A wrong
    /// swap here is invisible in greyscale mockups and turns the accent colour
    /// from blue to orange on screen.
    #[test]
    fn swaps_channels_for_wl_shm() {
        let mut pixmap = Pixmap::new(1, 1).unwrap();
        pixmap.pixels_mut()[0] =
            tiny_skia::PremultipliedColorU8::from_rgba(10, 20, 30, 255).unwrap();

        let mut out = [0u8; 4];
        to_argb8888(&pixmap, &mut out, 4);

        assert_eq!(out, [30, 20, 10, 255]);
    }

    /// Counts distinct RGBA values, as a cheap "did anything get drawn" probe.
    fn distinct_colours(pixmap: &Pixmap) -> usize {
        let mut seen = std::collections::HashSet::new();
        for pixel in pixmap.pixels() {
            seen.insert((pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()));
        }

        seen.len()
    }

    /// Writes a render where a human can open it.
    ///
    /// Under the scratchpad rather than the source tree: these are artefacts of
    /// a test run, not fixtures, and the path is printed so the test output
    /// says where to look.
    fn write_png(pixmap: &Pixmap, name: &str) {
        let directory = std::env::var("COSMIC_VOICE_RENDER_DIR")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
        let path = std::path::Path::new(&directory).join(name);
        pixmap.save_png(&path).expect("writing the render");
        println!("wrote {}", path.display());
    }
}
