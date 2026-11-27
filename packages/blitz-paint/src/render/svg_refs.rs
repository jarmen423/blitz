//! known-future (wave 2): painting with CSS references to SVG definitions of the same document:
//! `clip-path: url(#id)`, `mask-image: url(#id)` and `filter: url(#id)` on an HTML element.
//!
//! blitz-dom writes the definition out as an SVG document of its own, in which one rectangle
//! the size of the element's border box carries the reference, and has usvg resolve it
//! (`blitz_dom::SvgReference`). Here that document's tree is what gets painted:
//!
//!  - a clip path: the tree draws opaque black where the clip path lets the element show. It is
//!    drawn into an alpha mask layer over the element (which handles the union of several
//!    shapes, `clip-rule` and a clip path on the clip path alike), the same way a CSS mask is
//!    applied: isolation layer, the element, mask layer, the tree.
//!  - a mask: the tree draws opaque white through the `<mask>`, so its alpha is the mask's value
//!    whatever the `mask-type`. It is drawn as that mask image layer, an alpha layer.
//!  - a filter: the tree is drawn into a recording, and the filter graph that the SVG renderer
//!    attaches to the rectangle's layer is taken from it and attached to the element's layer
//!    (`crate::filters`), its coordinates being the element's own.

use std::sync::Arc;

use anyrender::recording::RenderCommand;
use anyrender::{Filter, MaskMode, PaintScene};
use blitz_dom::{SvgReference, SvgReferenceKind};
use kurbo::{Affine, Rect};
use peniko::{Fill, Mix};
use style::values::computed::basic_shape::ClipPath;
use style::values::computed::url::ComputedUrl;
use style::values::generics::image::GenericImage;

use super::{ALMOST_OPAQUE, ElementCx};

/// A clip that clips nothing: around what a `userSpaceOnUse` definition draws, which no box bounds
const UNBOUNDED: Rect = Rect::new(-1.0e7, -1.0e7, 1.0e7, 1.0e7);

impl ElementCx<'_, '_> {
    /// The document made for `url` if it names an inline SVG definition of the given kind
    fn svg_reference(&self, url: &ComputedUrl, kind: SvgReferenceKind) -> Option<SvgReference> {
        let url = url.url()?;
        let dom = self.context.dom;
        let target = dom.svg_reference_target(url, kind)?;
        let border_box = self.frame.border_box;
        dom.svg_reference(
            target,
            kind,
            (border_box.width() / self.scale) as f32,
            (border_box.height() / self.scale) as f32,
        )
    }

    /// The transform from a reference's user space (CSS pixels from the border box's corner)
    fn svg_reference_transform(&self) -> Affine {
        self.transform * Affine::scale(self.scale)
    }

    /// What bounds the drawing of a reference, in its user space
    fn svg_reference_bounds(&self, reference: &SvgReference) -> Rect {
        match reference.bounded {
            true => Rect::new(
                0.0,
                0.0,
                self.frame.border_box.width() / self.scale,
                self.frame.border_box.height() / self.scale,
            ),
            false => UNBOUNDED,
        }
    }

    /// `clip-path: url(#id)` that names an inline `<clipPath>`: the document whose drawing clips
    /// the element. A reference to anything else clips nothing (CSS Masking 1, 5.1).
    pub(super) fn svg_clip_reference(&self) -> Option<SvgReference> {
        match self.style.get_clip_path() {
            ClipPath::Url(url) => self.svg_reference(url, SvgReferenceKind::ClipPath),
            _ => None,
        }
    }

    /// Before the element is painted: the isolation layer that an SVG clip path is applied to
    pub(super) fn push_svg_clip_layer(&self, scene: &mut impl PaintScene, clip: Option<&SvgReference>) {
        let Some(clip) = clip else {
            return;
        };
        scene.push_layer(
            Fill::NonZero,
            Mix::Normal,
            ALMOST_OPAQUE,
            self.svg_reference_transform(),
            &self.svg_reference_bounds(clip),
            None,
            None,
        );
    }

    /// After the element is painted: clip it by drawing the clip path's document as its alpha
    /// mask, and pop the isolation layer
    pub(super) fn pop_svg_clip_layer(&self, scene: &mut impl PaintScene, clip: Option<&SvgReference>) {
        let Some(clip) = clip else {
            return;
        };
        let transform = self.svg_reference_transform();
        scene.push_mask_layer(
            Fill::NonZero,
            MaskMode::Alpha,
            transform,
            &self.svg_reference_bounds(clip),
        );
        anyrender_svg::render_svg_tree_with(scene, &clip.tree, transform, &mut |_, _| {});
        scene.pop_layer(); // Mask layer
        scene.pop_layer(); // Isolation layer
    }

    /// The mask image layer at `idx` if it is a `url(#id)` that names an inline `<mask>`
    pub(super) fn svg_mask_reference(&self, idx: usize) -> Option<SvgReference> {
        match self.style.get_svg().mask_image.0.get(idx)? {
            GenericImage::Url(url) => self.svg_reference(url, SvgReferenceKind::Mask),
            _ => None,
        }
    }

    /// Draw a `<mask>` reference as a mask image layer: its document's alpha is the mask
    pub(super) fn draw_svg_mask_layer(&self, scene: &mut impl PaintScene, mask: &SvgReference) {
        anyrender_svg::render_svg_tree_with(
            scene,
            &mask.tree,
            self.svg_reference_transform(),
            &mut |_, _| {},
        );
    }

    /// The filter graph of `url` if it names an inline `<filter>`, in the element's own space
    /// (CSS pixels from the border box's corner)
    pub(super) fn svg_filter_reference(&self, url: &ComputedUrl) -> Option<Arc<Filter>> {
        let reference = self.svg_reference(url, SvgReferenceKind::Filter)?;
        filter_of_reference(&reference)
    }
}

/// The filter graph the SVG renderer makes for the filtered rectangle of a `<filter>` reference's
/// document: it is drawn into a recording, where the graph is the filter of the rectangle's layer.
/// `None` for a filter the renderer cannot express (it then draws the rectangle unfiltered).
pub(crate) fn filter_of_reference(reference: &SvgReference) -> Option<Arc<Filter>> {
    let mut recording = anyrender::Scene::new();
    anyrender_svg::render_svg_tree_with(
        &mut recording,
        &reference.tree,
        Affine::IDENTITY,
        &mut |_, _| {},
    );
    recording.commands.into_iter().find_map(|command| match command {
        RenderCommand::PushLayer(layer) => layer.filter,
        _ => None,
    })
}
