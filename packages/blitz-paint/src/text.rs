use anyrender::{AntiAliasing, PaintScene};
use blitz_dom::{BaseDocument, NodeId, node::TextBrush};
use kurbo::{Affine, Rect};
use parley::{Affinity, Cursor, GlyphRun, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::sync::Arc;

use crate::color::Color;
use crate::text_decorations::{
    DecorationRunGeometry, DecorationStackEntry, LineDecoration, ResolvedTextShadow,
    WinAscentCache, draw_line_text_shadows, flush_line_decorations, resolve_decoration_entry,
};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// known-future: which glyph runs of an inline layout to paint: those whose style's entry in
/// `owners` (one per style of the layout) is `want`. Used to paint the text of a positioned
/// inline element apart from the in-flow text around it (see `render/inline_box.rs`).
#[derive(Clone, Copy)]
pub(crate) struct RunFilter<'a> {
    pub(crate) owners: &'a [u32],
    pub(crate) want: u32,
}

/// No positioned inline element owns the style (see [`RunFilter`]).
pub(crate) const NO_OWNER: u32 = u32::MAX;

impl RunFilter<'_> {
    #[inline]
    pub(crate) fn keeps(&self, style_index: usize) -> bool {
        self.owners.get(style_index).copied().unwrap_or(NO_OWNER) == self.want
    }
}

/// Per glyph run values recorded while walking a line, so the line can be painted in
/// passes (shadows beneath, then glyphs) without re-resolving styles.
pub(crate) struct RunRecord {
    pub(crate) fill_color: Color,
    /// known-future (wave 2): the run's `-webkit-text-stroke` (colour, width in device pixels)
    pub(crate) text_stroke: Option<(Color, f64)>,
    pub(crate) shadows: Option<Arc<[ResolvedTextShadow]>>,
}

/// Reusable scratch storage for drawing text across all inline formatting contexts in a
/// document. The vectors are cleared between uses without releasing their allocations.
#[derive(Default)]
pub(crate) struct DrawTextContext {
    stack: Vec<DecorationStackEntry>,
    path_scratch: Vec<NodeId>,
    deco_boxes: Vec<LineDecoration>,
    run_records: Vec<RunRecord>,
    win_ascent_ratios: WinAscentCache,
}

#[derive(Clone, Copy)]
pub(crate) struct TextPaintOptions {
    pub scale: f64,
    pub brush_override: Option<Color>,
    /// known-future (wave 2): which of Blink's text rules the text was laid out by
    /// (`blitz_dom::Node::text_mode` of the inline root): its glyphs are drawn by the same.
    pub text_mode: Option<parley::TextMode>,
    /// known-future (wave 2): where the line being drawn is cut for its ellipsis (the
    /// layout's x): glyphs that end past it are not drawn. `stroke_text` sets it line by
    /// line; callers leave it `None`.
    pub cut: Option<f32>,
}

/// known-future (wave 2): how a line that ends with an ellipsis is drawn (`LineEllipsis`):
/// its glyphs up to `x`, then the ellipsis.
struct LineCut {
    /// Glyphs that end past this x of the layout are not drawn; the ellipsis starts here
    x: f32,
    /// The run (among those `glyph_runs` yields for the line) the ellipsis follows: it is
    /// drawn on that run's baseline, in its colour
    run: usize,
    /// The ellipsis: glyph ids of the first run's font and each one's advance
    glyphs: Vec<(u32, f32)>,
}

/// The glyphs of an ellipsis in `run`'s font, with their advances: U+2026 where the font has
/// it, else three full stops (as Blink does), else nothing.
fn ellipsis_glyphs(run: &parley::layout::Run<'_, TextBrush>, text_mode: Option<parley::TextMode>) -> Vec<(u32, f32)> {
    use skrifa::MetadataProvider as _;
    let font = run.font();
    let Ok(font_ref) = skrifa::FontRef::from_index(font.data.as_ref(), font.index) else {
        return Vec::new();
    };
    let coords: &[skrifa::instance::NormalizedCoord] = bytemuck::cast_slice(run.normalized_coords());
    let location = skrifa::instance::LocationRef::new(coords);
    let font_size = run.font_size();
    let units_per_em = font_ref.metrics(skrifa::instance::Size::unscaled(), location).units_per_em as f32;
    let glyph_metrics = font_ref.glyph_metrics(skrifa::instance::Size::unscaled(), location);
    let charmap = font_ref.charmap();
    let advance = |glyph: skrifa::GlyphId| -> f32 {
        let width = glyph_metrics.advance_width(glyph).unwrap_or(0.0);
        match text_mode {
            Some(parley::TextMode::WholePixel) => parley::hinted_advance_width(width, font_size, units_per_em),
            _ if units_per_em > 0.0 => width * font_size / units_per_em,
            _ => 0.0,
        }
    };
    if let Some(glyph) = charmap.map('\u{2026}').filter(|g| g.to_u32() != 0) {
        return vec![(glyph.to_u32(), advance(glyph))];
    }
    match charmap.map('.').filter(|g| g.to_u32() != 0) {
        Some(glyph) => vec![(glyph.to_u32(), advance(glyph)); 3],
        None => Vec::new(),
    }
}

/// Where the line is cut for the ellipsis that `ellipsis` asks for, as Blink's line truncator
/// cuts it: the text keeps the characters that end within the room the line has less the
/// ellipsis's own width (one character at least), and the ellipsis follows them. The last line
/// that `line-clamp` leaves keeps all its text if the ellipsis fits after it. Lines with
/// right-to-left runs are left alone.
fn line_cut(
    line: &Line<'_, TextBrush>,
    ellipsis: &blitz_dom::node::LineEllipsis,
    text_mode: Option<parley::TextMode>,
    filter: Option<RunFilter<'_>>,
) -> Option<LineCut> {
    let first = glyph_runs(line, filter).next()?;
    if glyph_runs(line, filter).any(|run| run.run().is_rtl()) {
        return None;
    }
    let glyphs = ellipsis_glyphs(first.run(), text_mode);
    let ellipsis_width: f32 = glyphs.iter().map(|(_, advance)| advance).sum();
    let limit = ellipsis.max_advance - ellipsis_width;
    let mut cut: Option<(f32, usize)> = None;
    let mut all_fit = true;
    'runs: for (index, run) in glyph_runs(line, filter).enumerate() {
        for glyph in run.positioned_glyphs() {
            let end = glyph.x + glyph.advance;
            if end <= limit + 0.01 || cut.is_none() {
                cut = Some((end, index));
            } else {
                all_fit = false;
                break 'runs;
            }
        }
    }
    let (mut x, run) = cut?;
    if all_fit {
        if !ellipsis.clamped {
            return None;
        }
        // The line's own text fits with the ellipsis: it follows the text, not the space
        // that hangs at the line's end
        let metrics = line.metrics();
        x = x.min(metrics.offset + metrics.advance - metrics.hanging_advance);
    }
    Some(LineCut { x, run, glyphs })
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    options: TextPaintOptions,
    inline_root_id: NodeId,
    context: &mut DrawTextContext,
    filter: Option<RunFilter<'_>>,
    ellipses: &[blitz_dom::node::LineEllipsis],
) {
    let TextPaintOptions {
        scale,
        brush_override,
        text_mode,
        ..
    } = options;
    let DrawTextContext {
        stack,
        path_scratch,
        deco_boxes,
        run_records,
        win_ascent_ratios,
    } = context;
    stack.clear();
    path_scratch.clear();
    deco_boxes.clear();
    run_records.clear();

    // Persistent stack mirroring the ancestor path (inline root -> current run's
    // node) as we walk the runs. The `text-decoration-*` properties are *not*
    // inherited; instead a decoration set on an ancestor is propagated to the
    // descendant text it wraps. Rather than re-resolving styles for the whole
    // ancestor chain on every run, we cache each node's resolved values here and,
    // for each run, only resolve styles for the nodes newly descended into (popping
    // as we ascend). `path_scratch` is a reusable buffer for the run's node path.
    for (line_index, line) in lines.enumerate() {
        // known-future (wave 2): the lines after the one `line-clamp` ends at are not drawn, and
        // a line that ends with an ellipsis is cut for it
        if ellipses.iter().any(|e| e.clamped && line_index > e.line) {
            continue;
        }
        let cut = ellipses
            .iter()
            .find(|e| e.line == line_index)
            .and_then(|e| line_cut(&line, e, text_mode, filter));
        let cut_end = cut
            .as_ref()
            .map(|cut| (cut.x + cut.glyphs.iter().map(|(_, advance)| advance).sum::<f32>()) as f64);
        let options = TextPaintOptions {
            cut: cut.as_ref().map(|cut| cut.x),
            ..options
        };

        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();
        run_records.clear();
        let mut line_has_shadows = false;

        // Pass 1: resolve styles for every glyph run on the line (recording the values
        // needed to paint it) and accumulate decorations. Painting is deferred so text
        // shadows can be drawn beneath *all* of the line's text and decorations.
        for glyph_run in glyph_runs(&line, filter) {
            let run = glyph_run.run();
            let font = run.font();
            let font_size = run.font_size();
            let metrics = run.font_metrics();
            let style = glyph_run.style();

            let css_font_size = font_size as f64 / scale;

            // Reconcile the stack with this run's ancestor path. Build the path
            // (inline root -> run node), keep the shared prefix already on the stack,
            // pop the rest, and resolve styles only for the newly-descended nodes.
            path_scratch.clear();
            let mut walk_id = Some(style.brush.id);
            while let Some(node_id) = walk_id {
                path_scratch.push(node_id);
                if node_id == inline_root_id {
                    break;
                }
                walk_id = doc.get_node(node_id).and_then(|node| node.parent);
            }
            path_scratch.reverse();

            let shared = stack
                .iter()
                .zip(path_scratch.iter())
                .take_while(|(entry, node_id)| entry.node_id == **node_id)
                .count();
            stack.truncate(shared);
            for &node_id in &path_scratch[shared..] {
                let entry = resolve_decoration_entry(doc, node_id, stack.last(), scale);
                stack.push(entry);
            }

            // The glyph colour and `text-shadow` come from the run's own node (the stack
            // top): both inherit, so the innermost inline element already carries the
            // right values. Shadows are not part of a `background-clip: text` mask, so
            // they are dropped when the brush is overridden.
            let (fill_color, text_stroke, shadows) = stack
                .last()
                .map(|e| (e.fill_color, e.text_stroke, e.text_shadows.clone()))
                .unwrap_or((Color::BLACK, None, None));
            let fill_color = brush_override.unwrap_or(fill_color);
            // (wave 2) a stroke is part of the text's shape: a `background-clip: text` mask
            // (the brush override) takes it in the mask's colour
            let text_stroke = text_stroke.map(|(color, width)| (brush_override.unwrap_or(color), width));
            let shadows = shadows.filter(|_| brush_override.is_none());
            line_has_shadows |= shadows.is_some();
            run_records.push(RunRecord {
                fill_color,
                text_stroke,
                shadows,
            });

            // Accumulate this run's contribution to each decorating box on its ancestor
            // path. The decoration is drawn once per box after the whole line has been
            // walked (see `flush_line_decorations`), so mixed font sizes within a box
            // produce a single straight line rather than one stepped segment per run.
            let geometry = DecorationRunGeometry {
                baseline: glyph_run.baseline(),
                ascent: metrics.ascent,
                descent: metrics.descent,
                underline_offset: metrics.underline_offset,
                underline_size: metrics.underline_size,
                strikethrough_size: metrics.strikethrough_size,
                font: font.clone(),
                font_size,
                css_font_size,
            };
            let run_node_id = style.brush.id;
            let run_x0 = glyph_run.offset() as f64;
            let run_x1 = run_x0 + glyph_run.advance() as f64;
            // (wave 2) a decoration ends where the line's ellipsis does
            let (run_x0, run_x1) = match cut_end {
                Some(end) => (run_x0.min(end), run_x1.min(end)),
                None => (run_x0, run_x1),
            };

            for entry in stack.iter() {
                if entry.decoration.is_none() {
                    continue;
                }
                let idx = match deco_boxes.iter().position(|d| d.node_id == entry.node_id) {
                    Some(idx) => idx,
                    None => {
                        deco_boxes.push(LineDecoration {
                            node_id: entry.node_id,
                            deco: entry.decoration.clone().unwrap(),
                            min_x: f64::INFINITY,
                            max_x: f64::NEG_INFINITY,
                            own: None,
                            first: None,
                        });
                        deco_boxes.len() - 1
                    }
                };
                let acc = &mut deco_boxes[idx];
                acc.min_x = acc.min_x.min(run_x0);
                acc.max_x = acc.max_x.max(run_x1);
                if acc.first.is_none() {
                    acc.first = Some(geometry.clone());
                }
                // A run whose innermost node is the box itself is the box's own text, so
                // its font is the one Firefox positions and sizes the decoration with.
                if entry.node_id == run_node_id {
                    acc.own = Some(geometry.clone());
                }
            }
        }

        // Pass 2: text shadows, beneath all of the line's text and decorations.
        if line_has_shadows {
            draw_line_text_shadows(
                scene,
                &line,
                run_records,
                deco_boxes,
                win_ascent_ratios,
                transform,
                options,
                inline_root_id,
                filter,
            );
        }

        // Pass 3: the glyphs themselves.
        for (glyph_run, record) in glyph_runs(&line, filter).zip(run_records.iter()) {
            draw_glyph_run(scene, &glyph_run, record.fill_color, transform, text_mode, options.cut);
            // known-future (wave 2): `-webkit-text-stroke`, over the fill
            if let Some((color, width)) = record.text_stroke {
                stroke_glyph_run(scene, &glyph_run, color, width, transform, text_mode, options.cut);
            }
        }
        // known-future (wave 2): the line's ellipsis, after what is left of its text
        if let Some(cut) = &cut
            && let Some(first) = glyph_runs(&line, filter).next()
            && let Some((after, record)) = glyph_runs(&line, filter).zip(run_records.iter()).nth(cut.run)
        {
            draw_ellipsis(scene, &first, cut, after.baseline(), record.fill_color, transform, text_mode);
        }

        // Pass 4: decorations, on top of the glyphs.
        flush_line_decorations(
            scene,
            transform,
            options,
            deco_boxes,
            win_ascent_ratios,
            inline_root_id,
            line.metrics().baseline,
            None,
        );
    }
}

/// The glyph runs of a line that `filter` keeps (all without one), in order (matching the order
/// of the recorded `RunRecord`s).
pub(crate) fn glyph_runs<'a, 'b, 'f: 'b>(
    line: &'b Line<'a, TextBrush>,
    filter: Option<RunFilter<'f>>,
) -> impl Iterator<Item = GlyphRun<'a, TextBrush>> + 'b {
    line.items().filter_map(move |item| match item {
        PositionedLayoutItem::GlyphRun(glyph_run)
            if filter.is_none_or(|f| f.keeps(glyph_run.style_index() as usize)) =>
        {
            Some(glyph_run)
        }
        _ => None,
    })
}

/// Whether glyph outlines are hinted. FSHOT_TEXT_HINT=0 turns hinting off, as Chrome does for
/// `text-rendering: geometricPrecision` (which HyperFrames sets on every element), and
/// FSHOT_TEXT_HINT=1 on; otherwise text is hinted unless it follows the rules of
/// `geometricPrecision` (known-future, wave 2: by the mode the text was laid out by, where there is
/// one; else as FSHOT_GEOMETRIC_PRECISION=1 says).
fn text_hinting(text_mode: Option<parley::TextMode>) -> bool {
    static HINT: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();
    let forced = *HINT.get_or_init(|| match std::env::var("FSHOT_TEXT_HINT").as_deref() {
        Ok("0") | Ok("none") => Some(false),
        Ok("1") => Some(true),
        _ => None,
    });
    forced.unwrap_or_else(|| match text_mode {
        Some(mode) => mode != parley::TextMode::GeometricPrecision,
        None => !parley::geometric_precision(),
    })
}

/// known-future (wave 2): whether Chrome's rasteriser changes the outlines of `font` when it hints
/// lightly (`FT_LOAD_TARGET_LIGHT`, fontconfig's `hintslight`, its setting for text that is not
/// placed at sub-pixel positions), as FreeType's `FT_Load_Glyph` decides it:
///
///  - a font with TrueType outlines and no programs at all (no `fpgm`, no `prep`, no glyph
///    instructions) goes to the auto-hinter: hinted;
///  - one with programs goes to its own interpreter. With no glyph instructions (the `prep`-only
///    fonts that Google Fonts and Fontsource ship as "unhinted": EB Garamond) the interpreter has
///    nothing to move, and the outlines stay as designed: not hinted. With glyph instructions (IBM
///    Plex, a ttfautohint font) it snaps them vertically; the auto-hinter Skia has here for
///    "slight" is the closest there is to that;
///  - a font with CFF outlines is hinted lightly by its own hinter in both.
///
/// Answers are kept by font (the blob's id and the face's index).
fn font_takes_light_hinting(font: &parley::FontData) -> bool {
    use skrifa::raw::{FontRef, TableProvider as _, types::Tag};
    static CACHE: std::sync::Mutex<Option<std::collections::HashMap<(u64, u32), bool>>> =
        std::sync::Mutex::new(None);
    let key = (font.data.id(), font.index);
    if let Some(known) = CACHE.lock().unwrap().get_or_insert_with(Default::default).get(&key) {
        return *known;
    }
    let takes = (|| {
        let font_ref = FontRef::from_index(font.data.as_ref(), font.index).ok()?;
        if font_ref.table_data(Tag::new(b"glyf")).is_none() {
            return Some(true);
        }
        let program = |tag: &[u8; 4]| font_ref.table_data(Tag::new(tag)).is_some_and(|d| !d.as_bytes().is_empty());
        let glyph_instructions = font_ref
            .maxp()
            .ok()
            .and_then(|maxp| maxp.max_size_of_instructions())
            .unwrap_or(0);
        let has_programs = program(b"fpgm") || program(b"prep") || glyph_instructions > 0;
        Some(!has_programs || glyph_instructions > 0)
    })()
    .unwrap_or(true);
    CACHE.lock().unwrap().get_or_insert_with(Default::default).insert(key, takes);
    takes
}

/// known-future (wave 2): where a glyph is drawn under [`parley::TextMode::WholePixel`]: Chrome
/// without sub-pixel positioning draws each glyph at the whole device pixel nearest its place
/// (Skia rounds the mapped position half up), along both axes. `transform` is the run's; a run
/// that is rotated or skewed keeps its positions (Skia then positions it by sub-pixels too).
fn snap_glyph(x: f32, y: f32, transform: Affine) -> (f32, f32) {
    let [a, b, c, d, e, f] = transform.as_coeffs();
    if b != 0.0 || c != 0.0 || a <= 0.0 || d <= 0.0 {
        return (x, y);
    }
    let snap = |v: f32, scale: f64, origin: f64| -> f32 {
        (((v as f64 * scale + origin + 0.5).floor() - origin) / scale) as f32
    };
    (snap(x, a, e), snap(y, d, f))
}

/// Synthetic bold on (default; FSHOT_SYNTHETIC_BOLD=0 turns it off).
fn synthetic_bold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("FSHOT_SYNTHETIC_BOLD").as_deref(), Ok("0")))
}

static GLYPH_RECTS: std::sync::Mutex<Option<Vec<Rect>>> = std::sync::Mutex::new(None);

/// Start (or stop) recording the device-space box of every glyph run drawn, for a caller that needs to know
/// where text landed (fshot's LCD text: where it landed on pixels that are not opaque it is drawn grayscale).
pub fn record_glyph_rects(on: bool) {
    *GLYPH_RECTS.lock().unwrap() = if on { Some(Vec::new()) } else { None };
}

/// The boxes recorded since the last call (recording stays on).
pub fn take_glyph_rects() -> Vec<Rect> {
    GLYPH_RECTS.lock().unwrap().as_mut().map(std::mem::take).unwrap_or_default()
}

/// Draw a glyph run's glyphs in a single colour.
pub(crate) fn draw_glyph_run(
    scene: &mut impl PaintScene,
    glyph_run: &GlyphRun<'_, TextBrush>,
    color: Color,
    transform: Affine,
    text_mode: Option<parley::TextMode>,
    cut: Option<f32>,
) {
    let run = glyph_run.run();
    let font_size = run.font_size();
    // Zero-size text is invisible, but hinting can give its glyphs ink. Fully
    // transparent text is skipped too: backends draw color (emoji) glyphs opaque.
    if font_size <= 0.0 || color.components[3] <= 0.0 {
        return;
    }
    let glyph_xform = run
        .synthesis()
        .skew()
        .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

    let embolden = if FONT_EMBOLDEN_ENABLED {
        font_embolden(font_size as f64, color, transform)
    } else {
        kurbo::Vec2::default()
    };

    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
    // (known-future, wave 2) whole-pixel text: each glyph at a whole device pixel, and hinted
    // where Chrome's rasteriser hints the font
    let whole_pixel = text_mode == Some(parley::TextMode::WholePixel) && glyph_xform.is_none();
    let hinting = text_hinting(text_mode)
        && (text_mode != Some(parley::TextMode::WholePixel) || font_takes_light_hinting(run.font()));
    // (wave 2) a line cut for its ellipsis draws the glyphs that end before the cut
    let kept = move |glyph: &parley::layout::Glyph| cut.is_none_or(|cut| glyph.x + glyph.advance <= cut + 0.01);
    let place = move |glyph: parley::layout::Glyph| {
        let (x, y) = match whole_pixel {
            true => snap_glyph(glyph.x, glyph.y, transform),
            false => (glyph.x, glyph.y),
        };
        anyrender::Glyph {
            id: glyph.id as _,
            x,
            y,
        }
    };
    scene.draw_glyphs(
        run.font(),
        font_size,
        !FONT_EMBOLDEN_ENABLED && hinting,
        normalized_coords,
        embolden,
        Fill::NonZero,
        &anyrender::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        glyph_run.positioned_glyphs().filter(kept).map(place),
        AntiAliasing::Enabled,
    );
    // Synthetic bold (a bold weight asked of a family that has no bold face), as Chrome draws it on Linux:
    // Skia's fake bold, the outline grown by a stroke of font size x (1/24 at 9 px .. 1/32 at 36 px), miter joins.
    // (FONT_EMBOLDEN_ENABLED builds embolden every glyph run already.)
    if !FONT_EMBOLDEN_ENABLED && run.synthesis().embolden() && synthetic_bold_enabled() {
        let fs = font_size as f64;
        let t = ((fs - 9.0) / 27.0).clamp(0.0, 1.0);
        let width = fs * (1.0 / 24.0 + (1.0 / 32.0 - 1.0 / 24.0) * t);
        let stroke = kurbo::Stroke::new(width).with_join(kurbo::Join::Miter).with_miter_limit(4.0);
        scene.draw_glyphs(
            run.font(),
            font_size,
            hinting,
            normalized_coords,
            kurbo::Vec2::default(),
            &stroke,
            &anyrender::Paint::from(color),
            1.0,
            transform,
            glyph_xform,
            glyph_run.positioned_glyphs().filter(kept).map(place),
            AntiAliasing::Enabled,
        );
    }
    if let Some(v) = GLYPH_RECTS.lock().unwrap().as_mut() {
        let r = glyph_run_rect(glyph_run);
        let r = Rect::new(r.x0 - font_size as f64 * 0.5, r.y0 - font_size as f64 * 0.25, r.x1 + font_size as f64 * 0.5, r.y1 + font_size as f64 * 0.25);
        v.push(transform.transform_rect_bbox(r));
    }
}

/// known-future (wave 2): draw the outlines of a glyph run's glyphs as `-webkit-text-stroke`
/// draws them: a stroke of `width` device pixels centred on each outline, with mitred joins
/// (Blink strokes the text with Skia's defaults: a miter join, limit 4), over whatever fills
/// the glyphs.
pub(crate) fn stroke_glyph_run(
    scene: &mut impl PaintScene,
    glyph_run: &GlyphRun<'_, TextBrush>,
    color: Color,
    width: f64,
    transform: Affine,
    text_mode: Option<parley::TextMode>,
    cut: Option<f32>,
) {
    let run = glyph_run.run();
    let font_size = run.font_size();
    if font_size <= 0.0 || color.components[3] <= 0.0 || !(width > 0.0) {
        return;
    }
    let glyph_xform = run
        .synthesis()
        .skew()
        .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));
    let whole_pixel = text_mode == Some(parley::TextMode::WholePixel) && glyph_xform.is_none();
    let hinting = text_hinting(text_mode)
        && (text_mode != Some(parley::TextMode::WholePixel) || font_takes_light_hinting(run.font()));
    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
    let stroke = kurbo::Stroke::new(width)
        .with_join(kurbo::Join::Miter)
        .with_miter_limit(4.0);
    scene.draw_glyphs(
        run.font(),
        font_size,
        hinting,
        normalized_coords,
        kurbo::Vec2::default(),
        &stroke,
        &anyrender::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        glyph_run
            .positioned_glyphs()
            .filter(move |glyph| cut.is_none_or(|cut| glyph.x + glyph.advance <= cut + 0.01))
            .map(move |glyph| {
            let (x, y) = match whole_pixel {
                true => snap_glyph(glyph.x, glyph.y, transform),
                false => (glyph.x, glyph.y),
            };
            anyrender::Glyph {
                id: glyph.id as _,
                x,
                y,
            }
        }),
        AntiAliasing::Enabled,
    );
    if let Some(v) = GLYPH_RECTS.lock().unwrap().as_mut() {
        let r = glyph_run_rect(glyph_run).inflate(
            font_size as f64 * 0.5 + width,
            font_size as f64 * 0.25 + width,
        );
        v.push(transform.transform_rect_bbox(r));
    }
}

/// known-future (wave 2): draw a line's ellipsis (`LineCut`): its glyphs, in the font and size
/// of the line's first run `first`, from the cut along `baseline`.
fn draw_ellipsis(
    scene: &mut impl PaintScene,
    first: &GlyphRun<'_, TextBrush>,
    cut: &LineCut,
    baseline: f32,
    color: Color,
    transform: Affine,
    text_mode: Option<parley::TextMode>,
) {
    let run = first.run();
    let font_size = run.font_size();
    if font_size <= 0.0 || color.components[3] <= 0.0 || cut.glyphs.is_empty() {
        return;
    }
    let glyph_xform = run
        .synthesis()
        .skew()
        .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));
    let whole_pixel = text_mode == Some(parley::TextMode::WholePixel) && glyph_xform.is_none();
    let hinting = text_hinting(text_mode)
        && (text_mode != Some(parley::TextMode::WholePixel) || font_takes_light_hinting(run.font()));
    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
    let mut x = cut.x;
    let glyphs: Vec<anyrender::Glyph> = cut
        .glyphs
        .iter()
        .map(|(id, advance)| {
            let (gx, gy) = match whole_pixel {
                true => snap_glyph(x, baseline, transform),
                false => (x, baseline),
            };
            x += advance;
            anyrender::Glyph {
                id: *id as _,
                x: gx,
                y: gy,
            }
        })
        .collect();
    scene.draw_glyphs(
        run.font(),
        font_size,
        !FONT_EMBOLDEN_ENABLED && hinting,
        normalized_coords,
        kurbo::Vec2::default(),
        Fill::NonZero,
        &anyrender::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        glyphs.into_iter(),
        AntiAliasing::Enabled,
    );
}

/// The layout-space box covered by a glyph run: its advance horizontally and the font's
/// ascent/descent vertically.
pub(crate) fn glyph_run_rect(glyph_run: &GlyphRun<'_, TextBrush>) -> Rect {
    let metrics = glyph_run.run().font_metrics();
    let x0 = glyph_run.offset() as f64;
    let x1 = x0 + glyph_run.advance() as f64;
    let baseline = glyph_run.baseline() as f64;
    let y0 = baseline - metrics.ascent as f64;
    let y1 = baseline + metrics.descent as f64;
    Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(
            Fill::NonZero,
            transform,
            SELECTION_COLOR,
            None,
            &rect,
            AntiAliasing::Enabled,
        );
    });
}

/// Approximates macOS font smoothing (stem dilation). CoreGraphics dilates each side by an amount
/// in device pixels that grows with the device ppem up to 0.3px (fitted to Safari's ink at 9-72px
/// and 1x-3x scale), and keeps dilating at large sizes, so there is no ppem cutoff. glifo applies
/// the embolden in user units, so the device amounts are divided by the transform's scale.
fn font_embolden(font_size: f64, color: Color, transform: Affine) -> kurbo::Vec2 {
    const MAX_DEVICE_EMBOLDEN: f64 = 0.3;
    let [a, b, c, d, _, _] = transform.as_coeffs();
    let (scale_x, scale_y) = (a.hypot(b), c.hypot(d));
    let device_ppem = font_size * (a * d - b * c).abs().sqrt();
    if scale_x == 0.0 || scale_y == 0.0 || !device_ppem.is_finite() {
        return kurbo::Vec2::default();
    }
    let fs = device_ppem * embolden_scale(color);
    kurbo::Vec2::new(
        (0.0121 * fs).min(MAX_DEVICE_EMBOLDEN) / scale_x,
        (0.015125 * fs).min(MAX_DEVICE_EMBOLDEN) / scale_y,
    )
}

/// Scales Pathfinder's stem-darkening factors by the text's relative luminance. CoreText
/// dilates light text more than dark text; the points are fitted to Safari's ink for text
/// colours from black to white on black and white backgrounds.
fn embolden_scale(color: Color) -> f64 {
    const POINTS: [(f64, f64); 4] = [(0.1, 0.5), (0.125, 0.72), (0.35, 0.72), (1.0, 1.3)];
    let lum = color.discard_alpha().relative_luminance() as f64;
    let mut prev = POINTS[0];
    if lum <= prev.0 {
        return prev.1;
    }
    for point in &POINTS[1..] {
        if lum <= point.0 {
            return prev.1 + (point.1 - prev.1) * (lum - prev.0) / (point.0 - prev.0);
        }
        prev = *point;
    }
    prev.1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// known-future (wave 2): the advance width Blink has for a glyph when it does not place glyphs at sub-pixel
    /// positions is FreeType's hinted one (patches/parley_engine-0.13.0, `hinted_advance_width`; the test is here
    /// because that crate's own tests need font files that are not vendored with it).
    #[test]
    fn hinted_advance_widths_are_freetypes() {
        // EB Garamond SemiBold's figures are 513 units wide: 19.494px at 38px, which FreeType scales to
        // 1248/64 = 19.5px and rounds up. Chrome gives each 20px (measured: tests/test_w2_blitz.py).
        assert_eq!(parley::hinted_advance_width(513., 38., 1000.), 20.);
        // its "K" (736 units, 27.968px) is 28px, its "o" (500 units, 19.0px) 19px
        assert_eq!(parley::hinted_advance_width(736., 38., 1000.), 28.);
        assert_eq!(parley::hinted_advance_width(500., 38., 1000.), 19.);
        // the regular weight's figures (480 units, 18.24px) are 18px
        assert_eq!(parley::hinted_advance_width(480., 38., 1000.), 18.);
        // IBM Plex Mono: 600 units at 30px is 18px exactly
        assert_eq!(parley::hinted_advance_width(600., 30., 1000.), 18.);
        // a glyph of no width, and a font that says nothing of its em
        assert_eq!(parley::hinted_advance_width(0., 30., 1000.), 0.);
        assert_eq!(parley::hinted_advance_width(600., 30., 0.), 0.);
    }

    /// known-future (wave 2): whole-pixel text is drawn at the whole device pixel nearest its place, half up
    #[test]
    fn whole_pixel_glyphs_are_drawn_at_whole_pixels() {
        let at = |x: f64, y: f64| Affine::translate((x, y));
        assert_eq!(snap_glyph(10.4, 20.6, Affine::IDENTITY), (10.0, 21.0));
        assert_eq!(snap_glyph(10.5, 20.5, Affine::IDENTITY), (11.0, 21.0));
        // the run's place counts: 10.4 + 0.3 is 10.7, drawn at 11, which is 10.7 in the run's own space
        let (x, y) = snap_glyph(10.4, 0.0, at(0.3, 0.25));
        assert!((x - 10.7).abs() < 1e-5 && (y + 0.25).abs() < 1e-5, "{x}, {y}");
        // under a scale the pixel is the device's
        let (x, _) = snap_glyph(10.2, 0.0, Affine::scale(2.0));
        assert_eq!(x, 10.0);
        let (x, _) = snap_glyph(10.3, 0.0, Affine::scale(2.0));
        assert_eq!(x, 10.5);
        // a run that is turned keeps its places (Skia positions it by sub-pixels then, too)
        assert_eq!(snap_glyph(10.4, 20.6, Affine::rotate(0.3)), (10.4, 20.6));
        assert_eq!(snap_glyph(10.4, 20.6, Affine::new([0.0, 1.0, -1.0, 0.0, 46.0, 0.0])), (10.4, 20.6));
    }

    /// known-future (wave 2): hinting follows the text mode unless FSHOT_TEXT_HINT says otherwise
    #[test]
    fn text_is_hinted_unless_it_is_geometric_precision() {
        if std::env::var_os("FSHOT_TEXT_HINT").is_some() {
            return;
        }
        assert!(text_hinting(Some(parley::TextMode::WholePixel)));
        assert!(text_hinting(Some(parley::TextMode::Plain)));
        assert!(!text_hinting(Some(parley::TextMode::GeometricPrecision)));
    }
}
