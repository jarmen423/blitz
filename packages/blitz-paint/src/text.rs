use anyrender::{AntiAliasing, PaintScene};
use blitz_dom::{BaseDocument, NodeId, node::TextBrush};
use kurbo::{Affine, Rect};
use parley::{Affinity, Cursor, GlyphRun, Layout, Line, PositionedLayoutItem, Selection};
use peniko::Fill;
use std::sync::Arc;

use crate::color::Color;
use crate::text_decorations::{
    DecorationRunGeometry, DecorationStackEntry, LineDecoration, ResolvedDecoration,
    ResolvedTextShadow, WinAscentCache, draw_line_text_shadows, flush_line_decorations,
    resolve_decoration_entry,
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
) {
    let TextPaintOptions {
        scale,
        brush_override,
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
    for line in lines {
        // Decorations accumulated for this line, keyed by decorating box, so each box is
        // painted once (spanning all its runs) using its own font — matching Firefox, which
        // draws one decoration per box rather than one stepped segment per differently-sized
        // run. Clearing preserves the allocation for the next line and inline context.
        deco_boxes.clear();
        run_records.clear();
        let mut line_has_shadows = false;
        // The underline of the text an input method is composing on this line, spanning its runs.
        let mut composing: Option<LineDecoration> = None;

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
            let (fill_color, shadows) = stack
                .last()
                .map(|e| (e.fill_color, e.text_shadows.clone()))
                .unwrap_or((Color::BLACK, None));
            let fill_color = brush_override.unwrap_or(fill_color);
            let shadows = shadows.filter(|_| brush_override.is_none());
            line_has_shadows |= shadows.is_some();
            run_records.push(RunRecord {
                fill_color,
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

            // A text input's editor underlines the text an input method is composing.
            if style.underline.is_some() {
                let acc = composing.get_or_insert_with(|| LineDecoration {
                    node_id: run_node_id,
                    deco: ResolvedDecoration::composition_underline(fill_color),
                    min_x: f64::INFINITY,
                    max_x: f64::NEG_INFINITY,
                    own: None,
                    first: None,
                });
                acc.min_x = acc.min_x.min(run_x0);
                acc.max_x = acc.max_x.max(run_x1);
                acc.first.get_or_insert(geometry);
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
            draw_glyph_run(scene, &glyph_run, record.fill_color, transform);
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
        if let Some(composing) = composing {
            flush_line_decorations(
                scene,
                transform,
                options,
                std::slice::from_ref(&composing),
                win_ascent_ratios,
                inline_root_id,
                line.metrics().baseline,
                None,
            );
        }
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
/// `text-rendering: geometricPrecision` (which HyperFrames sets on every element); off by default
/// under FSHOT_GEOMETRIC_PRECISION=1.
fn text_hinting() -> bool {
    static HINT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HINT.get_or_init(|| match std::env::var("FSHOT_TEXT_HINT").as_deref() {
        Ok("0") | Ok("none") => false,
        Ok("1") => true,
        _ => !parley::geometric_precision(),
    })
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
        .map(|angle| Affine::skew(-angle.to_radians().tan() as f64, 0.0));

    let embolden = if FONT_EMBOLDEN_ENABLED {
        font_embolden(font_size as f64, color, transform)
    } else {
        kurbo::Vec2::default()
    };

    let normalized_coords: &[i16] = bytemuck::cast_slice(run.normalized_coords());
    scene.draw_glyphs(
        run.font(),
        font_size,
        !FONT_EMBOLDEN_ENABLED && text_hinting(),
        normalized_coords,
        embolden,
        Fill::NonZero,
        &anyrender::Paint::from(color),
        1.0,
        transform,
        glyph_xform,
        glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
            id: glyph.id as _,
            x: glyph.x,
            y: glyph.y,
        }),
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
            text_hinting(),
            normalized_coords,
            kurbo::Vec2::default(),
            &stroke,
            &anyrender::Paint::from(color),
            1.0,
            transform,
            glyph_xform,
            glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                id: glyph.id as _,
                x: glyph.x,
                y: glyph.y,
            }),
            AntiAliasing::Enabled,
        );
    }
    if let Some(v) = GLYPH_RECTS.lock().unwrap().as_mut() {
        let r = glyph_run_rect(glyph_run);
        let r = Rect::new(r.x0 - font_size as f64 * 0.5, r.y0 - font_size as f64 * 0.25, r.x1 + font_size as f64 * 0.5, r.y1 + font_size as f64 * 0.25);
        v.push(transform.transform_rect_bbox(r));
    }
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
