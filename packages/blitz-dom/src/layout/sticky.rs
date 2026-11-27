//! known-future (wave 2): `position: sticky` (CSS Positioned Layout 3, 3.4).
//!
//! Taffy lays a sticky box out in normal flow and leaves its insets alone: they are thresholds
//! against a scrollport, which a layout of boxes knows nothing of. The shift they ask for is
//! worked out here, after layout, and kept on the node ([`Node::sticky_offset`], CSS pixels).
//! Everything that places the box adds it to `Layout.location`: the painter, the overflow of
//! the box's ancestors, hit testing and the page-relative positions. Layout itself never sees
//! it, as in CSS, where a sticky box keeps its place in the flow.
//!
//! The shift is Blink's (`StickyPositionScrollingConstraints::ComputeStickyOffset`): each inset
//! that is not `auto` keeps that edge of the border box at least that far inside the nearest
//! scroll container's scrollport (its padding box less its padding; the viewport for a box
//! that no element scrolls), and the box never leaves its containing block's content box less
//! the box's own margins. Where the two ends of an axis disagree, the start edge (`top`,
//! `left`) has the last word.

use blitz_traits::node_id::NodeId;
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::values::computed::{CSSPixelLength, Overflow};
use style::values::generics::position::GenericInset;
use taffy::Point;

use crate::{BaseDocument, Node};

fn is_scroll_container(style: &ComputedValues) -> bool {
    let b = style.get_box();
    [b.overflow_x, b.overflow_y]
        .iter()
        .any(|o| matches!(o, Overflow::Hidden | Overflow::Scroll | Overflow::Auto))
}

pub(crate) fn is_sticky(node: &Node) -> bool {
    node.primary_styles()
        .is_some_and(|s| s.get_box().position == Position::Sticky)
}

impl BaseDocument {
    /// Work out again the shift of every sticky box met so far (they are met when their
    /// transform is resolved, see `resolve_transforms`), ancestors before descendants. Returns
    /// whether any shift changed: the overflow of the boxes around it is then stale.
    pub(crate) fn resolve_sticky_offsets(&mut self) -> bool {
        if self.sticky_nodes.is_empty() {
            return false;
        }
        let mut ids = std::mem::take(&mut self.sticky_nodes);
        ids.retain(|id| self.nodes.get(*id).is_some_and(is_sticky));
        ids.sort_unstable();
        ids.dedup();
        let depth = |doc: &Self, id: NodeId| {
            let mut n = 0u32;
            let mut cur = doc.nodes[id].containing_block();
            while let Some(p) = cur.and_then(|p| doc.nodes.get(p)) {
                n += 1;
                cur = p.containing_block();
                if n > 1 << 16 {
                    break;
                }
            }
            n
        };
        let mut keyed: Vec<(u32, NodeId)> = ids.iter().map(|&id| (depth(self, id), id)).collect();
        keyed.sort_unstable();
        let mut changed = false;
        for &(_, id) in &keyed {
            let offset = self.compute_sticky_offset(id);
            if self.nodes[id].sticky_offset.replace(offset) != offset {
                changed = true;
            }
        }
        self.sticky_nodes = ids;
        if changed {
            self.nodes.bump_geometry_generation();
        }
        changed
    }

    /// Note a sticky box (so that later resolves work its shift out again: a scroll or a layout
    /// change around it moves it with no damage of its own) and set its shift now.
    pub(crate) fn note_sticky_node(&mut self, id: NodeId) {
        if !self.sticky_nodes.contains(&id) {
            self.sticky_nodes.push(id);
        }
        let offset = self.compute_sticky_offset(id);
        self.nodes[id].sticky_offset.set(offset);
    }

    /// The shift of the sticky box `id` at the present layout and scroll positions (zero for
    /// any other box). The shifts of its sticky ancestors must be up to date.
    pub(crate) fn compute_sticky_offset(&self, id: NodeId) -> Point<f32> {
        let Some(node) = self.nodes.get(id) else {
            return Point::ZERO;
        };
        let Some(style) = node.primary_styles() else {
            return Point::ZERO;
        };
        if style.get_box().position != Position::Sticky {
            return Point::ZERO;
        }
        let layout = *node.final_layout();
        let Some(cb_id) = node.containing_block() else {
            return Point::ZERO;
        };
        let Some(cb) = self.nodes.get(cb_id) else {
            return Point::ZERO;
        };

        // The box and its containing block's content box (less the box's margins), first in the
        // containing block's own space, then carried up to the scroll container's
        let (mut x, mut y) = (layout.location.x, layout.location.y);
        let cl = cb.final_layout();
        let mut cb_rect = [
            cl.border.left + cl.padding.left + layout.margin.left,
            cl.border.top + cl.padding.top + layout.margin.top,
            cl.size.width - cl.border.right - cl.padding.right - layout.margin.right,
            cl.size.height - cl.border.bottom - cl.padding.bottom - layout.margin.bottom,
        ];
        // The scrollport, in the same space
        let scrollport: [f32; 4];
        let root_id = self.try_root_element().map(|n| n.id);
        let mut current = cb;
        let mut steps = 0u32;
        loop {
            // The root element's `overflow` is the viewport's: the viewport scrolls it
            let parent = match Some(current.id) == root_id {
                true => None,
                false => current.containing_block().and_then(|p| self.nodes.get(p)),
            };
            let scrolls = current.primary_styles().is_some_and(|s| is_scroll_container(&s));
            if scrolls && parent.is_some() {
                let l = current.final_layout();
                let scroll = current.scroll_offset();
                let (sx, sy) = (scroll.x as f32, scroll.y as f32);
                x -= sx;
                y -= sy;
                cb_rect = [cb_rect[0] - sx, cb_rect[1] - sy, cb_rect[2] - sx, cb_rect[3] - sy];
                scrollport = [
                    l.border.left + l.padding.left,
                    l.border.top + l.padding.top,
                    l.size.width - l.border.right - l.padding.right,
                    l.size.height - l.border.bottom - l.padding.bottom,
                ];
                break;
            }
            let Some(parent) = parent.filter(|_| steps < 1 << 16) else {
                // No element scrolls the box: the viewport does. `current` is the root; its own
                // place and the viewport's scroll position give the box's place in the viewport.
                let l = current.final_layout();
                let scroll = self.viewport_scroll();
                let (dx, dy) = (l.location.x - scroll.x as f32, l.location.y - scroll.y as f32);
                x += dx;
                y += dy;
                cb_rect = [cb_rect[0] + dx, cb_rect[1] + dy, cb_rect[2] + dx, cb_rect[3] + dy];
                let scale = self.viewport.scale();
                scrollport = [
                    0.0,
                    0.0,
                    self.viewport.window_size.0 as f32 / scale,
                    self.viewport.window_size.1 as f32 / scale,
                ];
                break;
            };
            let l = current.final_layout();
            let own = current.sticky_offset.get();
            let (dx, dy) = (l.location.x + own.x, l.location.y + own.y);
            x += dx;
            y += dy;
            cb_rect = [cb_rect[0] + dx, cb_rect[1] + dy, cb_rect[2] + dx, cb_rect[3] + dy];
            current = parent;
            steps += 1;
        }

        let (w, h) = (layout.size.width, layout.size.height);
        let pos = style.get_position();
        let inset = |v: &style::values::computed::Inset, basis: f32| match v {
            GenericInset::LengthPercentage(lp) => Some(lp.resolve(CSSPixelLength::new(basis)).px()),
            _ => None,
        };
        let (port_w, port_h) = (scrollport[2] - scrollport[0], scrollport[3] - scrollport[1]);
        let x_shift = sticky_axis_shift(
            (x, x + w),
            (scrollport[0], scrollport[2]),
            (cb_rect[0], cb_rect[2]),
            inset(&pos.left, port_w),
            inset(&pos.right, port_w),
        );
        let y_shift = sticky_axis_shift(
            (y, y + h),
            (scrollport[1], scrollport[3]),
            (cb_rect[1], cb_rect[3]),
            inset(&pos.top, port_h),
            inset(&pos.bottom, port_h),
        );
        Point { x: x_shift, y: y_shift }
    }
}

/// The shift of a sticky box along one axis (Blink's `ComputeStickyOffset`). `edges` are the
/// box's start and end in the scroll container's space, `port` the scrollport's, `limits` those
/// of the containing block's content box less the box's margins; `start` and `end` are the
/// insets on the two sides (`top` and `bottom`, or `left` and `right`), `None` for `auto`.
///
/// The end inset is applied first and the start inset to the box *as that left it*: where the
/// scrollport is too small for both (`top: 40px; bottom: 40px` on a 50px box in a 100px
/// scrollport), the start edge then truly has the last word, and the box ends `start` inside
/// the scrollport however far the end inset had pulled it the other way.
fn sticky_axis_shift(
    edges: (f32, f32),
    port: (f32, f32),
    limits: (f32, f32),
    start: Option<f32>,
    end: Option<f32>,
) -> f32 {
    let (mut near, mut far) = edges;
    if let Some(end) = end {
        // back towards the start, no further than the containing block lets the box go
        let delta = (port.1 - end - far).max(limits.0 - near).min(0.0);
        near += delta;
        far += delta;
    }
    if let Some(start) = start {
        let delta = (port.0 + start - near).min(limits.1 - far).max(0.0);
        near += delta;
    }
    near - edges.0
}

#[cfg(test)]
mod tests {
    use super::sticky_axis_shift;

    #[test]
    fn one_inset_keeps_its_edge_inside_the_scrollport() {
        // top: 10px on a box scrolled 30px above the scrollport's top
        assert_eq!(sticky_axis_shift((-30.0, 10.0), (0.0, 100.0), (-30.0, 400.0), Some(10.0), None), 40.0);
        // bottom: 0 on a box 60px below the scrollport's bottom
        assert_eq!(sticky_axis_shift((160.0, 200.0), (0.0, 100.0), (0.0, 400.0), None, Some(0.0)), -100.0);
        // a box already inside does not move
        assert_eq!(sticky_axis_shift((20.0, 60.0), (0.0, 100.0), (0.0, 400.0), Some(10.0), Some(10.0)), 0.0);
        // and never leaves its containing block
        assert_eq!(sticky_axis_shift((-80.0, -40.0), (0.0, 100.0), (-80.0, -20.0), Some(0.0), None), 20.0);
    }

    #[test]
    fn opposing_insets_give_the_start_edge_the_last_word() {
        // audit 1, adv7 #c1: a 50px box at y = 50 in a 100px scrollport, top: 40px; bottom: 40px.
        // `bottom` pulls it up to 10 (its end 40px inside), then `top` pushes it from there to
        // 40: a shift of -10. Measured from the unshifted box, `top` asked for nothing and the
        // box stayed at 10.
        assert_eq!(sticky_axis_shift((50.0, 100.0), (0.0, 100.0), (0.0, 300.0), Some(40.0), Some(40.0)), -10.0);
        // #c2: the same box at y = 20: `bottom` moves it up to 10, `top` down to 40
        assert_eq!(sticky_axis_shift((20.0, 70.0), (0.0, 100.0), (0.0, 300.0), Some(40.0), Some(40.0)), 20.0);
        // adv4 #sc3: a 60px box at y = 20 in a 102px scrollport, top: 35px; bottom: 35px
        assert_eq!(sticky_axis_shift((20.0, 80.0), (0.0, 102.0), (20.0, 300.0), Some(35.0), Some(35.0)), 15.0);
        // the same along x (left and right)
        assert_eq!(sticky_axis_shift((300.0, 380.0), (0.0, 200.0), (0.0, 700.0), Some(150.0), Some(150.0)), -150.0);
    }
}
