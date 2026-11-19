//! Backgrounds and borders of inline boxes (CSS 2.1 §14.2 and §8.6, CSS Fragmentation 3
//! `box-decoration-break`), painted as Blink paints them.
//!
//! An inline element (`<span style="background:...">`) generates one box fragment on each line it
//! is on. A fragment spans, horizontally, the glyph runs and atomic inlines of the element *and of
//! its inline descendants* on that line, without the white space that hangs at the end of the line;
//! vertically, the element's own content area (the ascent and descent of its first available font,
//! whatever fonts its descendants use), plus its vertical padding and border, which move nothing.
//! With `box-decoration-break: slice` (the initial value) the inline-start padding and border are
//! on the first fragment only and the inline-end ones on the last, and the corners where the box is
//! broken are square; with `clone` every fragment has all four sides. Fragments are snapped to
//! whole pixels, as Blink snaps box decorations, and painted line by line, outer boxes first.
//!
//! Blitz does not make room in the line for an inline box's padding, border or margin, so the
//! horizontal padding and border are painted over what they would have pushed aside.

use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use crate::text::{NO_OWNER, RunFilter, TextPaintOptions};
use crate::kurbo_css::{CssBox, NonUniformRoundedRectRadii};
use anyrender::PaintScene;
use blitz_dom::node::TextBrush;
use blitz_dom::{BaseDocument, CopyStyleGetters, Node, NodeId};
use kurbo::{Affine, Insets, Point, Rect, Vec2};
use parley::{Layout as TextLayout, PositionedLayoutItem};
use peniko::BlendMode;
use style::properties::ComputedValues;
use style::properties::generated::longhands::background_clip::single_value::computed_value::T as StyloBackgroundClip;
use style::properties::generated::longhands::visibility::computed_value::T as StyloVisibility;
use style::servo_arc::Arc;
use style::values::computed::{BorderCornerRadius, BorderStyle, BoxDecorationBreak, CSSPixelLength, Inset};
use style::values::generics::image::GenericImage;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

/// An inline element of the formatting context whose box paints something.
struct Decorated<'dom> {
    node: &'dom Node,
    style: Arc<ComputedValues>,
    /// Its style table entry (parents come before their children in the table).
    style_index: usize,
    /// Its opacity times that of its inline ancestors (Blitz paints inline text without them).
    opacity: f32,
    first_line: usize,
    last_line: usize,
}

/// One line's share of a decorated box, in the layout's coordinates.
struct Fragment {
    deco: usize,
    line: usize,
    x0: f64,
    x1: f64,
    baseline: f64,
}

/// Whether an inline element's own box paints anything: a background colour or image, or a border.
fn paints_box(style: &ComputedValues) -> bool {
    let bg = style.get_background();
    if bg.background_clip.0.iter().any(|c| *c == StyloBackgroundClip::Text) {
        return false; // gradient text: not painted as a box
    }
    let current_color = style.clone_color();
    if bg
        .background_color
        .resolve_to_absolute(&current_color)
        .as_srgb_color()
        != Color::TRANSPARENT
        || bg.background_image.0.iter().any(|i| !matches!(i, GenericImage::None))
    {
        return true;
    }
    let b = style.get_border();
    [
        (b.border_top_style, &b.border_top_width),
        (b.border_right_style, &b.border_right_width),
        (b.border_bottom_style, &b.border_bottom_width),
        (b.border_left_style, &b.border_left_width),
    ]
    .iter()
    .any(|(s, w)| !s.none_or_hidden() && w.0.to_f32_px() > 0.0)
}

fn decorated<'dom>(doc: &'dom BaseDocument, id: NodeId, root: NodeId) -> Option<(&'dom Node, Arc<ComputedValues>, f32)> {
    let node = doc.get_node(id)?;
    node.element_data()?;
    let style: Arc<ComputedValues> = (*node.primary_styles()?).clone();
    let display = style.clone_display();
    if display.outside() != DisplayOutside::Inline || display.inside() != DisplayInside::Flow {
        return None;
    }
    if style.get_inherited_box().visibility != StyloVisibility::Visible || !paints_box(&style) {
        return None;
    }
    let mut opacity = style.get_effects().opacity;
    let mut k = node.parent;
    while let Some(pid) = k {
        if pid == root {
            break;
        }
        let Some(p) = doc.get_node(pid) else { break };
        if let Some(ps) = p.primary_styles() {
            opacity *= ps.get_effects().opacity;
        }
        k = p.parent;
    }
    if opacity <= 0.0 {
        return None;
    }
    Some((node, style, opacity))
}

fn side(style: BorderStyle, width: &style::values::computed::BorderSideWidth) -> f64 {
    if style.none_or_hidden() { 0.0 } else { width.0.to_f32_px() as f64 }
}

impl<'dom, 'a> ElementCx<'dom, 'a> {
    /// Paint the backgrounds and borders of the inline boxes of this inline formatting context
    /// (the inline root's own box is painted as a block), or of those `filter` keeps. `transform`
    /// maps the layout's coordinates to the scene, and is this element's transform moved by `origin`.
    pub(super) fn draw_inline_box_decorations(
        &self,
        scene: &mut impl PaintScene,
        layout: &TextLayout<TextBrush>,
        transform: Affine,
        origin: Vec2,
        filter: Option<RunFilter<'_>>,
    ) {
        let doc = self.context.dom;
        let root = self.node.id;
        let styles = layout.styles();

        // 1. The inline elements that paint a box: one style table entry opens each.
        let mut decos: Vec<Decorated<'dom>> = Vec::new();
        for (i, st) in styles.iter().enumerate().skip(1) {
            let id = st.brush.id;
            if id == root || filter.is_some_and(|f| !f.keeps(i)) {
                continue;
            }
            let Some(sb) = layout.style_span_box(i) else { continue };
            if styles.get(sb.parent).is_some_and(|p| p.brush.id == id) {
                continue; // a style change inside the same element (`<br>`, `display: contents`)
            }
            if let Some((node, style, opacity)) = decorated(doc, id, root) {
                decos.push(Decorated { node, style, style_index: i, opacity, first_line: usize::MAX, last_line: 0 });
            }
        }
        if decos.is_empty() {
            return;
        }
        let mut deco_of_style = vec![usize::MAX; styles.len()];
        for (d, deco) in decos.iter().enumerate() {
            deco_of_style[deco.style_index] = d;
        }

        // 2. Their fragments, line by line.
        let mut frags: Vec<Fragment> = Vec::new();
        let add = |frags: &mut Vec<Fragment>, start: usize, deco: usize, line: usize, x0: f64, x1: f64, baseline: f64| {
            if let Some(f) = frags[start..].iter_mut().find(|f| f.deco == deco) {
                f.x0 = f.x0.min(x0);
                f.x1 = f.x1.max(x1);
            } else {
                frags.push(Fragment { deco, line, x0, x1, baseline });
            }
        };
        for (li, line) in layout.lines().enumerate() {
            let start = frags.len();
            let line_baseline = line.metrics().baseline as f64;
            let mut line_end = f64::NEG_INFINITY;
            for item in line.items() {
                match item {
                    PositionedLayoutItem::GlyphRun(run) => {
                        let x0 = run.offset() as f64;
                        let x1 = x0 + run.advance() as f64;
                        line_end = line_end.max(x1);
                        let si = run.style_index() as usize;
                        let Some(own) = layout.style_span_box(si) else { continue };
                        let mut s = si;
                        while s != 0 {
                            let Some(sb) = layout.style_span_box(s) else { break };
                            let d = deco_of_style.get(s).copied().unwrap_or(usize::MAX);
                            if d != usize::MAX {
                                let baseline = if sb.aligned_subtree_root == own.aligned_subtree_root {
                                    run.baseline() as f64 + (own.baseline_offset - sb.baseline_offset) as f64
                                } else {
                                    line_baseline - sb.baseline_offset as f64
                                };
                                add(&mut frags, start, d, li, x0, x1, baseline);
                            }
                            if sb.parent >= s {
                                break;
                            }
                            s = sb.parent;
                        }
                    }
                    PositionedLayoutItem::InlineBox(ib) => {
                        let x0 = ib.x as f64;
                        let x1 = x0 + ib.width as f64;
                        line_end = line_end.max(x1);
                        let mut k = doc.get_node(NodeId::from_u64(ib.id)).and_then(|n| n.parent);
                        while let Some(id) = k {
                            if id == root {
                                break;
                            }
                            if let Some(d) = decos.iter().position(|d| d.node.id == id) {
                                let sb = layout.style_span_box(decos[d].style_index).unwrap();
                                let baseline = line_baseline - sb.baseline_offset as f64;
                                add(&mut frags, start, d, li, x0, x1, baseline);
                            }
                            k = doc.get_node(id).and_then(|n| n.parent);
                        }
                    }
                }
            }
            // White space hanging at the end of the line is not part of any box.
            let hang = line.metrics().hanging_advance as f64;
            if hang > 0.0 && line_end.is_finite() {
                let end = line_end - hang;
                for f in &mut frags[start..] {
                    f.x1 = f.x1.min(end);
                }
            }
        }
        frags.retain(|f| f.x1 > f.x0);
        if frags.is_empty() {
            return;
        }
        for f in &frags {
            let d = &mut decos[f.deco];
            d.first_line = d.first_line.min(f.line);
            d.last_line = d.last_line.max(f.line);
        }
        // Outer boxes first on each line (a parent's style comes before its children's).
        frags.sort_by_key(|f| (f.line, f.deco));

        // 3. Paint each fragment as a box of its own.
        let scale = self.scale;
        let basis = (self.frame.content_box().width() / scale).max(0.0) as f32;
        let lp = |v: &style::values::computed::LengthPercentage| -> f64 {
            v.resolve(CSSPixelLength::new(basis)).px() as f64 * scale
        };
        for f in &frags {
            let d = &decos[f.deco];
            let st = &*d.style;
            let Some(sb) = layout.style_span_box(d.style_index) else { continue };
            let border = st.get_border();
            let padding = st.get_padding();
            let clone = border.box_decoration_break == BoxDecorationBreak::Clone;
            let start_edge = clone || f.line == d.first_line;
            let end_edge = clone || f.line == d.last_line;
            let bw = Insets {
                x0: if start_edge { side(border.border_left_style, &border.border_left_width) * scale } else { 0.0 },
                y0: side(border.border_top_style, &border.border_top_width) * scale,
                x1: if end_edge { side(border.border_right_style, &border.border_right_width) * scale } else { 0.0 },
                y1: side(border.border_bottom_style, &border.border_bottom_width) * scale,
            };
            let pw = Insets {
                x0: if start_edge { lp(&padding.padding_left.0) } else { 0.0 },
                y0: lp(&padding.padding_top.0),
                x1: if end_edge { lp(&padding.padding_right.0) } else { 0.0 },
                y1: lp(&padding.padding_bottom.0),
            };
            let top = f.baseline - sb.metrics.ascent as f64 - pw.y0 - bw.y0;
            let bottom = f.baseline + sb.metrics.descent as f64 + pw.y1 + bw.y1;
            let left = f.x0 - pw.x0 - bw.x0;
            let right = f.x1 + pw.x1 + bw.x1;
            // Snap the edges to whole pixels of this element's space.
            let x0 = (left + origin.x).round() - origin.x;
            let y0 = (top + origin.y).round() - origin.y;
            let x1 = (right + origin.x).round() - origin.x;
            let y1 = (bottom + origin.y).round() - origin.y;
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let (w, h) = (x1 - x0, y1 - y0);
            let rw = CSSPixelLength::new((w / scale) as f32);
            let rh = CSSPixelLength::new((h / scale) as f32);
            let radius = |r: &BorderCornerRadius, keep: bool| -> Vec2 {
                if !keep {
                    return Vec2::ZERO;
                }
                Vec2 {
                    x: scale * r.0.width.0.resolve(rw).px() as f64,
                    y: scale * r.0.height.0.resolve(rh).px() as f64,
                }
            };
            let radii = NonUniformRoundedRectRadii {
                top_left: radius(&border.border_top_left_radius, start_edge),
                top_right: radius(&border.border_top_right_radius, end_edge),
                bottom_right: radius(&border.border_bottom_right_radius, end_edge),
                bottom_left: radius(&border.border_bottom_left_radius, start_edge),
            };
            let Some(element) = d.node.element_data() else { continue };
            let cx = ElementCx {
                context: self.context,
                frame: CssBox::new(Rect::new(0.0, 0.0, w, h), bw, pw, 0.0, radii),
                style: d.style.clone(),
                scale,
                node: d.node,
                element,
                transform: transform * Affine::translate((x0, y0)),
                #[cfg(feature = "svg")]
                svg: None,
                text_input: None,
                list_item: None,
                devtools: self.devtools,
                custom_widget_scene: None,
            };
            let faded = self.context.layer_manager.maybe_push_layer(
                scene,
                d.opacity < 1.0,
                BlendMode::default(),
                d.opacity,
                cx.transform,
                &cx.frame.border_box,
                None,
                None,
            );
            cx.draw_background(scene);
            cx.draw_border(scene);
            self.context.layer_manager.maybe_pop_layer(scene, faded);
        }
    }

    /// The positioned inline elements this inline root paints apart from its in-flow text (those in
    /// its paint list, see blitz-dom's paint tree) and, for each style of its text layout, the index
    /// of the one it belongs to ([`NO_OWNER`]: none). `None` when there are none.
    pub(super) fn positioned_inline_owners(&self, layout: &TextLayout<TextBrush>) -> Option<(Vec<NodeId>, Vec<u32>)> {
        let doc = self.context.dom;
        let children = self.node.paint_children.borrow();
        let positioned: Vec<NodeId> = children
            .as_ref()?
            .iter()
            .copied()
            .filter(|id| doc.get_node(*id).is_some_and(|n| n.is_positioned_inline()))
            .collect();
        if positioned.is_empty() {
            return None;
        }
        let styles = layout.styles();
        let mut owners = vec![NO_OWNER; styles.len()];
        for i in 1..styles.len() {
            owners[i] = match positioned.iter().position(|p| *p == styles[i].brush.id) {
                Some(k) => k as u32,
                None => layout
                    .style_span_box(i)
                    .filter(|sb| sb.parent < i)
                    .map_or(NO_OWNER, |sb| owners[sb.parent]),
            };
        }
        Some((positioned, owners))
    }

    /// Paint the positioned inline element `id` of this inline formatting context at its place in
    /// the positioned paint level (CSS 2.1 Appendix E): its inline boxes and the text of it and its
    /// descendants, moved by its relative offset, with its opacity. It is also recorded in the
    /// paint log as a box covering its fragments, so a caller of the log sees it in paint order.
    pub(super) fn draw_positioned_inline(&self, scene: &mut impl PaintScene, id: NodeId) {
        let Some(text_layout) = self.element.inline_layout_data.as_ref() else { return };
        let layout = &text_layout.layout;
        let Some((positioned, owners)) = self.positioned_inline_owners(layout) else { return };
        let Some(k) = positioned.iter().position(|p| *p == id) else { return };
        let Some(node) = self.context.dom.get_node(id) else { return };
        let Some(style) = node.primary_styles() else { return };
        if style.get_inherited_box().visibility != StyloVisibility::Visible {
            return;
        }
        let opacity = style.get_effects().opacity;
        if opacity == 0.0 {
            return;
        }
        let filter = RunFilter { owners: &owners, want: k as u32 };
        let scale = self.scale;

        // Where draw_inline_layout puts the text, plus the relative offset.
        let lb = self.node.final_layout();
        let scroll = self.node.scroll_offset();
        let subpixel = text_layout.content_box_subpixel_offset;
        let pos = Point {
            x: (lb.padding.left + lb.border.left) as f64 - scroll.x + subpixel.x as f64,
            y: (lb.padding.top + lb.border.top) as f64 - scroll.y + subpixel.y as f64 + text_layout.block_offset as f64,
        };
        let content = self.frame.content_box();
        let inset = |v: &Inset, basis: f64| -> Option<f64> {
            match v {
                Inset::LengthPercentage(lp) => Some(lp.resolve(CSSPixelLength::new(basis as f32)).px() as f64),
                _ => None,
            }
        };
        let sp = style.get_position();
        let (cw, ch) = (content.width() / scale, content.height() / scale);
        let dx = inset(&sp.left, cw).or_else(|| inset(&sp.right, cw).map(|r| -r)).unwrap_or(0.0);
        let dy = inset(&sp.top, ch).or_else(|| inset(&sp.bottom, ch).map(|b| -b)).unwrap_or(0.0);
        let origin = Vec2::new((pos.x + dx) * scale, (pos.y + dy) * scale);
        let transform = self.transform * Affine::translate(origin);

        // Its extent: the runs of it and its descendants, each line's box height at least.
        let mut bbox: Option<Rect> = None;
        for line in layout.lines() {
            let m = line.metrics();
            for run in crate::text::glyph_runs(&line, Some(filter)) {
                let fm = run.run().font_metrics();
                let b = run.baseline() as f64;
                let r = Rect::new(
                    run.offset() as f64,
                    (b - fm.ascent as f64).min(m.block_min_coord as f64),
                    (run.offset() + run.advance()) as f64,
                    (b + fm.descent as f64).max(m.block_max_coord as f64),
                );
                bbox = Some(bbox.map_or(r, |u| u.union(r)));
            }
        }
        let Some(bbox) = bbox else { return };
        let screen = Affine::translate(Vec2::new(-self.context.initial_x, -self.context.initial_y)) * transform;
        if let Some(log) = crate::PAINT_LOG.lock().unwrap().as_mut() {
            let sb = screen.transform_rect_bbox(bbox);
            log.push((
                id,
                (transform * Affine::translate(Vec2::new(bbox.x0, bbox.y0))).as_coeffs(),
                bbox.width() / scale,
                bbox.height() / scale,
                [sb.x0, sb.y0, sb.x1, sb.y1],
            ));
        }

        let pushed = self.context.layer_manager.maybe_push_layer(
            scene,
            opacity < 1.0,
            BlendMode::default(),
            opacity,
            transform,
            &bbox.inflate(4.0 * scale, 4.0 * scale + bbox.height()),
            None,
            None,
        );
        self.draw_inline_box_decorations(scene, layout, transform, origin, Some(filter));
        let mut cx = self.context.draw_text_context.borrow_mut();
        crate::text::stroke_text(
            scene,
            layout.lines(),
            self.context.dom,
            transform,
            TextPaintOptions { scale, brush_override: None },
            self.node.id,
            &mut cx,
            Some(filter),
        );
        drop(cx);
        self.context.layer_manager.maybe_pop_layer(scene, pushed);
    }
}
