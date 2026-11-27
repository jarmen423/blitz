//! Rendering for the CSS `mask` properties (`mask-image`, `mask-position`,
//! `mask-size`, `mask-repeat`, `mask-clip`, `mask-origin`).
//!
//! Masks are applied by:
//!
//!  1. Pushing an isolation layer (clipped to the mask painting area) before the
//!     element is painted
//!  2. Painting the element (and its descendants) as normal
//!  3. Pushing a mask layer and drawing the mask image layers into it, which
//!     multiplies the alpha of the already-painted element by the mask
//!  4. Popping both layers
//!
//! A mask with a single image layer uses that layer's `mask-mode` for the mask
//! layer. With several image layers, the mask layer is an alpha mask and each
//! `luminance` layer is converted to alpha before it is composited with the
//! layers below it.
//!
//! The mask image layers themselves are positioned/sized/repeated using the same
//! code as `background-image` layers (see `background.rs`), as the `mask-*` and
//! `background-*` properties share computed value types.
//!
//! ## `mask-clip: no-clip` (known-future, wave 2)
//!
//! A layer with `mask-clip: no-clip` is clipped by no box: its image is painted, and repeated,
//! over everything the element and its descendants paint. Then the masked content is no longer
//! held to the border box either, so the isolation layer is not clipped, and the mask is applied
//! as a `destination-in` layer over the [`no_clip_mask_region`].
//!
//! That region is Chrome's. Chrome records the mask as one display item whose bounds are the
//! element's own box (the border box, whatever its descendants overflow), and its software
//! rasteriser plays a raster tile's display items back by their bounds: a tile that the border
//! box does not reach never plays the mask, and what the element's descendants paint there is
//! left unmasked. Where a tile does play it, the mask is drawn with no clip and covers the whole
//! tile. So the mask applies over the raster tiles that the border box touches, and nowhere
//! else: a 240px-wide box at x=480 with a child overflowing it to x=790 is masked up to x=763
//! (the tile edge) and unmasked from there on. `BLITZ_NO_CLIP_MASK=all` gives the mask of the
//! specification instead (everywhere), `BLITZ_NO_CLIP_MASK=tiles` (the default) Chrome's.
//!
//! The tiles are those of the compositor layer the element paints into (fix round 1). That is
//! the page's own layer, whose tiles start at the picture's corner, unless Chrome composites the
//! element or an ancestor of it (`compositing.rs`: `will-change: transform`, a 3D transform, or
//! painting over an earlier layer): the layer is then as large as what it holds and its tiles
//! start at its own corner, in its own space, so a layer under 512 pixels each way is a single
//! tile and the mask covers everything the element paints.
use crate::render::background::get_cyclic;

use super::background::ImageLayerStyles;
use super::{ALMOST_OPAQUE, ElementCx};
use anyrender::{AntiAliasing, MaskMode, PaintScene};
use kurbo::{Affine, Rect, Shape};
use peniko::{BlendMode, Color, Compose, Fill, Mix};
use style::properties::generated::longhands::mask_clip::single_value::computed_value::T as StyloMaskClip;
use style::properties::generated::longhands::mask_composite::single_value::computed_value::T as StyloMaskComposite;
use style::properties::generated::longhands::mask_mode::single_value::computed_value::T as StyloMaskMode;
use style::values::generics::image::GenericImage;

impl ElementCx<'_, '_> {
    /// Whether the element has any CSS `mask-image` layers
    pub(super) fn has_css_mask(&self) -> bool {
        self.style
            .get_svg()
            .mask_image
            .0
            .iter()
            .any(|image| !matches!(image, GenericImage::None))
    }

    /// Whether the mask image layer at `idx` has `mask-clip: no-clip`
    fn mask_layer_has_no_clip(&self, idx: usize) -> bool {
        *get_cyclic(&self.style.get_svg().mask_clip.0, idx) == StyloMaskClip::NoClip
    }

    /// Whether any mask image layer has `mask-clip: no-clip`: the mask is then not held to the
    /// border box (see the module documentation)
    pub(super) fn css_mask_has_no_clip(&self) -> bool {
        let svg_styles = self.style.get_svg();
        (0..svg_styles.mask_image.0.len()).any(|idx| self.mask_layer_has_no_clip(idx))
    }

    /// Where a mask with a `no-clip` layer is applied: a rectangle, and the transform to the
    /// scene's (device) space from the space the rectangle is in. That space is the scene's for
    /// an element that paints into the page's own layer, and the layer's own for one that Chrome
    /// composites (`compositing.rs`): the raster tiles are the layer's.
    fn no_clip_mask_region(&self) -> (Affine, Rect) {
        let picture = (
            (self.context.initial_x, self.context.initial_y),
            (self.context.width as f64, self.context.height as f64),
        );
        let Some(layer) = self.no_clip_layer.filter(|layer| layer.to_scene.determinant().abs() > 1e-12) else {
            let device_box = self.transform.transform_rect_bbox(self.frame.border_box);
            return (Affine::IDENTITY, no_clip_mask_region(device_box, picture.0, picture.1));
        };
        // The border box in the layer's space, among the layer's tiles
        let layer_box = (layer.to_scene.inverse() * self.transform).transform_rect_bbox(self.frame.border_box);
        let region = no_clip_mask_region(
            layer_box,
            (layer.bounds.x0, layer.bounds.y0),
            (layer.bounds.width(), layer.bounds.height()),
        );
        (layer.to_scene, region)
    }

    /// If the element has a CSS mask, push an isolation layer for the masked content
    /// to be drawn into. Returns whether a layer was pushed, which should later be
    /// passed to [`maybe_pop_css_mask_layer`](Self::maybe_pop_css_mask_layer).
    pub(super) fn maybe_push_css_mask_layer(&self, scene: &mut impl PaintScene) -> bool {
        if !self.has_css_mask() {
            return false;
        }

        if self.css_mask_has_no_clip() {
            // No box bounds what a `no-clip` mask shows (known-future, wave 2)
            scene.push_layer(
                Fill::NonZero,
                Mix::Normal,
                ALMOST_OPAQUE,
                Affine::IDENTITY,
                &UNBOUNDED,
                None,
                None,
            );
            return true;
        }

        // Content outside of the mask painting area (at largest the border box) has
        // a mask alpha of 0, so we can clip the isolation layer to the border box.
        scene.push_layer(
            Fill::NonZero,
            Mix::Normal,
            ALMOST_OPAQUE,
            self.transform,
            &self.frame.border_box_shape(),
            None,
            None,
        );
        true
    }

    /// Apply the CSS mask to the content drawn since the corresponding
    /// [`maybe_push_css_mask_layer`](Self::maybe_push_css_mask_layer) by drawing the
    /// mask image layers into a mask layer, then pop the isolation layer.
    pub(super) fn maybe_pop_css_mask_layer(&self, scene: &mut impl PaintScene, layer_pushed: bool) {
        if !layer_pushed {
            return;
        }

        if self.css_mask_has_no_clip() {
            // The mask is an alpha mask applied over the region alone: outside it the content
            // stays as painted (known-future, wave 2; the module documentation)
            let (region_to_scene, region) = self.no_clip_mask_region();
            scene.push_layer(
                Fill::NonZero,
                BlendMode::new(Mix::Normal, Compose::DestIn),
                1.0,
                region_to_scene,
                &region,
                None,
                None,
            );
            // What a `no-clip` layer is painted over: the region, in the element's own space
            let paint_area = match self.transform.determinant().abs() > 1e-12 {
                true => (self.transform.inverse() * region_to_scene).transform_rect_bbox(region),
                false => self.frame.border_box,
            };
            self.draw_css_mask(scene, true, Some(paint_area.union(self.frame.border_box)));
            scene.pop_layer(); // Mask layer
            scene.pop_layer(); // Isolation layer
            return;
        }

        let svg_styles = self.style.get_svg();
        let single_layer = svg_styles.mask_image.0.len() == 1;
        let mode = if single_layer {
            self.css_mask_mode(0)
        } else {
            MaskMode::Alpha
        };

        scene.push_mask_layer(
            Fill::NonZero,
            mode,
            self.transform,
            &self.frame.border_box_shape(),
        );
        self.draw_css_mask(scene, !single_layer, None);
        scene.pop_layer(); // Mask layer
        scene.pop_layer(); // Isolation layer
    }

    /// The mask mode of the mask image layer at `idx`
    fn css_mask_mode(&self, idx: usize) -> MaskMode {
        // known-future (wave 2): a reference to an inline SVG <mask> is drawn with the mask's own
        // `mask-type` already applied (`svg_refs.rs`): its alpha is the mask
        #[cfg(feature = "svg")]
        if self.svg_mask_reference(idx).is_some() {
            return MaskMode::Alpha;
        }
        match get_cyclic(&self.style.get_svg().mask_mode.0, idx) {
            StyloMaskMode::Luminance => MaskMode::Luminance,
            // `match-source` is `alpha` for images
            StyloMaskMode::Alpha | StyloMaskMode::MatchSource => MaskMode::Alpha,
        }
    }

    /// Draw the image of the mask image layer at `idx`: an inline SVG <mask> it names, or the
    /// image itself
    fn draw_mask_image(&self, scene: &mut impl PaintScene, idx: usize, layer: &ImageLayerStyles) {
        #[cfg(feature = "svg")]
        if let Some(mask) = self.svg_mask_reference(idx) {
            self.draw_svg_mask_layer(scene, &mask);
            return;
        }
        let _ = idx;
        self.draw_image_layer(scene, layer);
    }

    /// Draw the mask image layers (analogous to `draw_background` for `background-image`).
    ///
    /// If `convert_luminance_layers` is set, `luminance` layers are converted to alpha
    /// so that they can be composited with the other layers into an alpha mask.
    ///
    /// `no_clip_area` is what a `mask-clip: no-clip` layer is painted over (in the element's
    /// own space), when the mask has such a layer.
    fn draw_css_mask(
        &self,
        scene: &mut impl PaintScene,
        convert_luminance_layers: bool,
        no_clip_area: Option<Rect>,
    ) {
        let svg_styles = self.style.get_svg();
        let image_data = &self.element.mask_images;
        let layer_count = svg_styles.mask_image.0.len();

        for idx in (0..layer_count).rev() {
            let mut layer = ImageLayerStyles::from_svg(svg_styles, image_data, idx);
            // A layer is clipped to its `mask-clip` box, or with `no-clip` to no box at all
            match no_clip_area.filter(|_| self.mask_layer_has_no_clip(idx)) {
                Some(area) => {
                    layer.paint_area = Some(area);
                    self.draw_css_mask_layer(scene, idx, &layer, &area, convert_luminance_layers);
                }
                None => {
                    let clip = self.box_path(layer.clip);
                    self.draw_css_mask_layer(scene, idx, &layer, &clip, convert_luminance_layers);
                }
            }
        }
    }

    /// Draw the mask image layer at `idx`, clipped to `mask_clip_path`, over the layers below it
    fn draw_css_mask_layer(
        &self,
        scene: &mut impl PaintScene,
        idx: usize,
        layer: &ImageLayerStyles,
        mask_clip_path: &impl Shape,
        convert_luminance_layers: bool,
    ) {
        let svg_styles = self.style.get_svg();
        let layer_count = svg_styles.mask_image.0.len();

        // Each mask layer is composited with the (already drawn) layers below it
        // using the Porter-Duff operator given by its `mask-composite` value. The
        // bottommost layer has no layers below it and is always drawn with SrcOver.
        let compose = if idx == layer_count - 1 {
            Compose::SrcOver
        } else {
            let composite_list = &svg_styles.mask_composite.0;
            match get_cyclic(composite_list, idx) {
                StyloMaskComposite::Add => Compose::SrcOver,
                StyloMaskComposite::Subtract => Compose::SrcOut,
                StyloMaskComposite::Intersect => Compose::SrcIn,
                StyloMaskComposite::Exclude => Compose::Xor,
            }
        };

        // The layer is pushed unconditionally (even for `mask-image: none` layers,
        // which draw nothing) as compositing a transparent black layer with e.g.
        // `intersect` clears the mask built up so far.
        scene.push_layer(
            Fill::NonZero,
            BlendMode::new(Mix::Normal, compose),
            1.0,
            self.transform,
            mask_clip_path,
            None,
            None,
        );
        if convert_luminance_layers && self.css_mask_mode(idx) == MaskMode::Luminance {
            // Mask an opaque fill with the luminance of the layer. The bottommost
            // layer's (SrcOver) layer may not be isolated, but it has nothing
            // below it for the luminance mask layer to erase.
            scene.fill(
                Fill::NonZero,
                self.transform,
                Color::WHITE,
                None,
                mask_clip_path,
                AntiAliasing::Enabled,
            );
            scene.push_mask_layer(
                Fill::NonZero,
                MaskMode::Luminance,
                self.transform,
                mask_clip_path,
            );
            self.draw_mask_image(scene, idx, layer);
            scene.pop_layer();
        } else {
            self.draw_mask_image(scene, idx, layer);
        }
        scene.pop_layer();
    }
}

/// A clip that clips nothing (the isolation layer of a `no-clip` mask)
const UNBOUNDED: Rect = Rect::new(-1.0e7, -1.0e7, 1.0e7, 1.0e7);

/// Whether a `no-clip` mask is applied everywhere (`BLITZ_NO_CLIP_MASK=all`, the specification)
/// rather than over Chrome's raster tiles (the default).
fn no_clip_mask_everywhere() -> bool {
    static ALL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALL.get_or_init(|| matches!(std::env::var("BLITZ_NO_CLIP_MASK").as_deref(), Ok("all")))
}

/// Where a mask with a `mask-clip: no-clip` layer is applied (see the module documentation): the
/// raster tiles of Chrome's software rasteriser that `device_box` (the element's border box, in
/// the scene's space) touches. `origin` is where the picture starts in the scene and `size` its
/// size: the tiles are the picture's.
///
/// The region is cut to the picture: nothing is painted past it, and a mask image is repeated
/// over the region.
fn no_clip_mask_region(device_box: Rect, origin: (f64, f64), size: (f64, f64)) -> Rect {
    let picture = Rect::new(origin.0, origin.1, origin.0 + size.0, origin.1 + size.1);
    if no_clip_mask_everywhere() {
        return picture;
    }
    let (tile_w, tile_h) = raster_tile_size(size.0, size.1);
    let (x0, x1) = tile_span(device_box.x0 - origin.0, device_box.x1 - origin.0, tile_w);
    let (y0, y1) = tile_span(device_box.y0 - origin.1, device_box.y1 - origin.1, tile_h);
    Rect::new(x0 + origin.0, y0 + origin.1, x1 + origin.0, y1 + origin.1).intersect(picture)
}

/// The size of the raster tiles of a layer of `width` x `height` pixels in Chrome's software
/// rasteriser (cc's `PictureLayerImpl::CalculateTileSize`): 256 pixels each way, but one tile for
/// a layer under 512 pixels both ways, and 512-pixel tiles along the long side of a layer under
/// 256 pixels the other way. `f64::INFINITY` stands for a tile that holds the whole axis.
fn raster_tile_size(width: f64, height: f64) -> (f64, f64) {
    const TILE: f64 = 256.0;
    const UNTILED: f64 = 512.0;
    if width < UNTILED && height < UNTILED {
        return (f64::INFINITY, f64::INFINITY);
    }
    let tile_w = if height < TILE { UNTILED } else { TILE };
    let tile_h = if width < TILE { UNTILED } else { TILE };
    (
        if width <= tile_w { f64::INFINITY } else { tile_w },
        if height <= tile_h { f64::INFINITY } else { tile_h },
    )
}

/// Along one axis, the span of the raster tiles that the pixels `from..to` touch.
///
/// A tile is a `tile`-pixel texture with one pixel of border shared with each neighbour, so the
/// tiles advance by `tile - 2`: tile 0 shows the pixels `0..tile - 1`, tile `i` the pixels
/// `i * (tile - 2) + 1..(i + 1) * (tile - 2) + 1` (255, 509, 763... for 256-pixel tiles). A tile
/// is rasterised with its border, so it plays back whatever reaches the border.
fn tile_span(from: f64, to: f64, tile: f64) -> (f64, f64) {
    if !tile.is_finite() {
        return (UNBOUNDED.x0, UNBOUNDED.x1);
    }
    let stride = tile - 2.0;
    let (from, to) = (from.floor(), to.ceil().max(from.floor() + 1.0));
    // tile `i` is rasterised over `i * stride..i * stride + tile`
    let first = (((from - tile) / stride).floor() + 1.0).max(0.0);
    let last = (((to - 1.0) / stride).floor()).max(first);
    let start = if first == 0.0 { UNBOUNDED.x0 } else { first * stride + 1.0 };
    (start, (last + 1.0) * stride + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_tiles_are_256_pixels_with_a_shared_border() {
        // m1_21: a 240px-wide box at x = 480..720 is in the tiles 255..509 and 509..763
        assert_eq!(tile_span(480.0, 720.0, 256.0), (255.0, 763.0));
        // a box at 300..540: the same two tiles
        assert_eq!(tile_span(300.0, 540.0, 256.0), (255.0, 763.0));
        // a box at 200..440: the first tile (which has no start) and the second
        assert_eq!(tile_span(200.0, 440.0, 256.0), (UNBOUNDED.x0, 509.0));
        // one pixel in the third tile
        assert_eq!(tile_span(600.0, 601.0, 256.0), (509.0, 763.0));
        // a tile is rasterised with its border: a box that ends at 508 reaches the tile 509..763's
        // border pixel (508), one that ends at 507 does not
        assert_eq!(tile_span(300.0, 509.0, 256.0), (255.0, 763.0));
        assert_eq!(tile_span(300.0, 508.0, 256.0), (255.0, 509.0));
        // and a box that starts at 510 reaches the border of the tile 255..509 (pixels 254..510)
        assert_eq!(tile_span(509.9, 600.0, 256.0), (255.0, 763.0));
        assert_eq!(tile_span(510.0, 600.0, 256.0), (509.0, 763.0));
    }

    #[test]
    fn a_small_picture_is_one_tile() {
        assert_eq!(raster_tile_size(800.0, 600.0), (256.0, 256.0));
        assert_eq!(raster_tile_size(1920.0, 1080.0), (256.0, 256.0));
        assert_eq!(raster_tile_size(400.0, 300.0), (f64::INFINITY, f64::INFINITY));
        // a wide strip: 512-pixel tiles along it, one tile across it
        assert_eq!(raster_tile_size(1920.0, 200.0), (512.0, f64::INFINITY));
        assert_eq!(tile_span(10.0, 20.0, f64::INFINITY), (UNBOUNDED.x0, UNBOUNDED.x1));
    }

    #[test]
    fn the_region_is_the_tiles_the_border_box_touches() {
        // m1_21's #b: 480..720 x 170..370 in an 800 x 600 picture
        let region = no_clip_mask_region(Rect::new(480.0, 170.0, 720.0, 370.0), (0.0, 0.0), (800.0, 600.0));
        assert_eq!((region.x0, region.x1), (255.0, 763.0));
        assert_eq!((region.y0, region.y1), (0.0, 509.0));
        // a box in the last tiles: the region ends with the picture
        let region = no_clip_mask_region(Rect::new(770.0, 520.0, 790.0, 590.0), (0.0, 0.0), (800.0, 600.0));
        assert_eq!((region.x0, region.y0, region.x1, region.y1), (763.0, 509.0, 800.0, 600.0));
    }

    /// Fix round 1: an element Chrome composites is masked by the tiles of its own layer, which
    /// start at the layer's corner and are sized for the layer
    #[test]
    fn a_composited_layers_tiles_are_its_own() {
        // audit 1, adv1b #g3 (`will-change: transform`): a 180 x 120 box at (40, 330) whose child
        // overflows it to 20..440 x 310..510. The layer is 420 x 200: one tile, so the mask
        // covers all of it (over the page's tiles it stopped at x = 255 and y = 509)
        let layer = Rect::new(20.0, 310.0, 440.0, 510.0);
        let region =
            no_clip_mask_region(Rect::new(40.0, 330.0, 220.0, 450.0), (layer.x0, layer.y0), (layer.width(), layer.height()));
        assert_eq!(region, layer);
        // the same box in the page's own layer
        let region = no_clip_mask_region(Rect::new(40.0, 330.0, 220.0, 450.0), (0.0, 0.0), (800.0, 600.0));
        assert_eq!((region.x0, region.y0, region.x1, region.y1), (0.0, 255.0, 255.0, 509.0));

        // a layer too large for one tile (fix1/exp/e_wc: headless Chrome masks 315..569 x 305..550):
        // a 100 x 100 box at (340, 320) whose child overflows it to 60..760 x 50..550. The tiles
        // are 256 pixels and start at the layer's corner: 255 and 509 pixels from it
        let layer = Rect::new(60.0, 50.0, 760.0, 550.0);
        let region =
            no_clip_mask_region(Rect::new(340.0, 320.0, 440.0, 420.0), (layer.x0, layer.y0), (layer.width(), layer.height()));
        assert_eq!((region.x0, region.y0, region.x1, region.y1), (315.0, 305.0, 569.0, 550.0));
        // the same box with no layer of its own (fix1/exp/e_plain, and e_tx with a 2D transform)
        let region = no_clip_mask_region(Rect::new(340.0, 320.0, 440.0, 420.0), (0.0, 0.0), (800.0, 600.0));
        assert_eq!((region.x0, region.y0, region.x1, region.y1), (255.0, 255.0, 509.0, 509.0));
    }
}
