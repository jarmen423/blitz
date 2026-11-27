//! known-future (wave 2, fix round 1): which of Chrome's compositor layers an element paints into.
//!
//! One thing drawn here depends on it: where a mask with a `mask-clip: no-clip` layer is applied
//! (`mask.rs`). Chrome's software rasteriser applies such a mask over the raster tiles that the
//! element's border box touches, and the tiles are those of the compositor layer the element
//! paints into: the page's own layer for most elements, whose tiles start at the picture's
//! corner, but a layer of the element's own (or of an ancestor's) once Chrome composites it. That
//! layer is as large as what it holds, and its tiles start at its own corner: a 180 x 120 box
//! with `will-change: transform` and a child that overflows it to 420 x 200 is one 420 x 200
//! layer, a single tile, and the mask covers all of it, where the same box with no `will-change`
//! is masked up to the page's tile edge at x = 255 and left unmasked from there on.
//!
//! What gives an element a layer of its own was read off headless Chrome (the pages and the
//! measurements are kept with the round's artifacts, `fix1/exp`):
//!
//!  - a reason of its own: `will-change` naming `transform`, `translate`, `rotate`, `scale` or
//!    `opacity`, a 3D transform function (`translateZ(0)`, `translate3d()`, `rotateX()`,
//!    `perspective()`... whatever it comes to), a `translate`, `rotate` or `scale` with a third
//!    dimension, or `backface-visibility: hidden`;
//!  - overlap: it paints after another layer and over it, so it cannot be drawn into the layer
//!    below that one.
//!
//! A plain 2D `transform` (`translateX(10px)`, `rotate(3deg)`, `scale: 1.2`), `opacity`,
//! `filter` and `isolation` give no layer: such an element is masked over the page's tiles like
//! any other.
//!
//! Not modelled: layers Chrome makes for running animations and transitions, for scrollers,
//! videos and canvases, `will-change: filter` (Chrome then applies the mask over the border box
//! alone), and the layers that content with no mask of its own is given by overlap.

use kurbo::{Affine, Rect};
use style::properties::ComputedValues;
use style::properties::generated::longhands::backface_visibility::computed_value::T as BackfaceVisibility;
use style::values::generics::transform::{GenericRotate, GenericScale, GenericTranslate, TransformOperation};
use style::values::specified::box_::WillChangeBits;

/// A compositor layer other than the page's own.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompositedLayer {
    /// From the layer's own space to the scene's. An element with a compositing reason of its
    /// own is rastered in its own space (before its transform); a layer made by overlap in the
    /// scene's.
    pub to_scene: Affine,
    /// What the layer holds, in its own space, on whole pixels: the raster tiles start at its
    /// corner, and its size is what they are sized for
    pub bounds: Rect,
    /// `bounds` in the scene's space (the box around it, where the layer is turned)
    pub scene_bounds: Rect,
}

impl CompositedLayer {
    /// A layer that holds `bounds` (in the layer's own space)
    pub fn new(to_scene: Affine, bounds: Rect) -> Self {
        // (Chrome's layers are on whole pixels: the rectangle that encloses what they hold)
        let bounds = Rect::new(bounds.x0.floor(), bounds.y0.floor(), bounds.x1.ceil(), bounds.y1.ceil());
        Self { to_scene, bounds, scene_bounds: to_scene.transform_rect_bbox(bounds) }
    }
}

/// The compositor layers met so far in a paint pass, in paint order.
#[derive(Default, Debug)]
pub(crate) struct Compositing {
    /// Every layer other than the page's that something has been painted into
    layers: Vec<CompositedLayer>,
    /// Those among them (indices) that the element being painted is inside of: its ancestors'
    /// and its own
    open: Vec<usize>,
}

impl Compositing {
    /// The layer that an element with no layer of its own paints into, if it is not the page's
    pub fn current(&self) -> Option<CompositedLayer> {
        self.open.last().map(|&index| self.layers[index])
    }

    /// Whether something that covers `scene_bounds` paints over a layer it is not inside of:
    /// Chrome then gives it a layer of its own (it cannot be drawn into a layer below that one)
    pub fn overlaps_earlier_layer(&self, scene_bounds: Rect) -> bool {
        self.layers
            .iter()
            .enumerate()
            .any(|(index, layer)| !self.open.contains(&index) && share_area(layer.scene_bounds, scene_bounds))
    }

    /// Start painting into `layer` (an element's own, and its descendants'). To be paired with
    /// [`Self::close`].
    pub fn open(&mut self, layer: CompositedLayer) {
        self.open.push(self.layers.len());
        self.layers.push(layer);
    }

    /// The element whose layer was opened last has been painted: what follows is outside it
    pub fn close(&mut self) {
        self.open.pop();
    }
}

/// Whether two rectangles have area in common (sharing an edge is not enough)
fn share_area(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

/// Whether Chrome gives an element with this style a compositor layer of its own whatever is
/// around it (see the module documentation).
pub(crate) fn has_direct_compositing_reason(style: &ComputedValues) -> bool {
    let box_style = style.get_box();
    if box_style
        .will_change
        .bits
        .intersects(WillChangeBits::TRANSFORM | WillChangeBits::OPACITY)
    {
        return true;
    }
    if box_style.backface_visibility == BackfaceVisibility::Hidden {
        return true;
    }
    // (Blink's `TransformOperation::Is3DOperation`: the function, not the matrix it comes to;
    // `rotateZ()` is a rotation in the plane)
    let is_3d_function = |op: &style::values::computed::transform::TransformOperation| {
        matches!(
            op,
            TransformOperation::Matrix3D(..)
                | TransformOperation::Translate3D(..)
                | TransformOperation::TranslateZ(..)
                | TransformOperation::Scale3D(..)
                | TransformOperation::ScaleZ(..)
                | TransformOperation::Rotate3D(..)
                | TransformOperation::RotateX(..)
                | TransformOperation::RotateY(..)
                | TransformOperation::Perspective(..)
        )
    };
    if box_style.transform.0.iter().any(is_3d_function) {
        return true;
    }
    if let GenericTranslate::Translate(_, _, z) = &box_style.translate
        && z.px() != 0.0
    {
        return true;
    }
    if let GenericRotate::Rotate3D(x, y, _, _) = &box_style.rotate
        && (*x != 0.0 || *y != 0.0)
    {
        return true;
    }
    if let GenericScale::Scale(_, _, z) = &box_style.scale
        && *z != 1.0
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layer_is_on_whole_pixels() {
        let layer = CompositedLayer::new(Affine::translate((10.0, 20.0)), Rect::new(-20.5, -20.25, 400.0, 180.75));
        assert_eq!(layer.bounds, Rect::new(-21.0, -21.0, 400.0, 181.0));
        assert_eq!(layer.scene_bounds, Rect::new(-11.0, -1.0, 410.0, 201.0));
    }

    #[test]
    fn what_paints_over_an_earlier_layer_overlaps_it_but_not_its_own_ancestors() {
        let mut compositing = Compositing::default();
        assert!(compositing.current().is_none());
        assert!(!compositing.overlaps_earlier_layer(Rect::new(0.0, 0.0, 800.0, 600.0)));

        // audit 1, adv1b #g3 (will-change: transform): its layer holds 20..440 x 310..510
        let g3 = CompositedLayer::new(Affine::translate((40.0, 330.0)), Rect::new(-20.0, -20.0, 400.0, 180.0));
        assert_eq!(g3.scene_bounds, Rect::new(20.0, 310.0, 440.0, 510.0));
        compositing.open(g3);
        // while #g3 is painted, its children paint into its layer: they do not overlap it
        assert_eq!(compositing.current().map(|layer| layer.scene_bounds), Some(g3.scene_bounds));
        assert!(!compositing.overlaps_earlier_layer(Rect::new(20.0, 310.0, 440.0, 510.0)));
        compositing.close();
        assert!(compositing.current().is_none());

        // #g4 paints after it, over 430..750 x 310..510: 10px of it over #g3's layer
        assert!(compositing.overlaps_earlier_layer(Rect::new(430.0, 310.0, 750.0, 510.0)));
        // a box that only touches the layer's edge, or is clear of it, does not
        assert!(!compositing.overlaps_earlier_layer(Rect::new(440.0, 310.0, 750.0, 510.0)));
        assert!(!compositing.overlaps_earlier_layer(Rect::new(20.0, 20.0, 340.0, 220.0)));
    }
}
