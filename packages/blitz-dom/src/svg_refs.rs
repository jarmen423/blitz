//! known-future (wave 2): CSS references to SVG definitions of the same document:
//! `clip-path: url(#id)`, `mask: url(#id)` and `filter: url(#id)` on an HTML element, naming a
//! `<clipPath>`, `<mask>` or `<filter>` of an inline `<svg>`.
//!
//! The definition is not interpreted here. It is written out, with the rest of the `<svg>` it
//! is in (what it may name in turn: a gradient, another clip path, a shape it `<use>`s), as the
//! `<defs>` of a small SVG document of its own, in which one rectangle the size of the
//! referencing element's border box carries the reference:
//!
//! ```text
//! <svg width="W" height="H"><defs>...</defs><rect width="W" height="H" clip-path="url(#id)"/></svg>
//! ```
//!
//! usvg resolves that document as it resolves any SVG: `objectBoundingBox` units against the
//! rectangle (the border box, which is the reference box of an HTML element), `userSpaceOnUse`
//! against the document's user space (CSS pixels from the border box's top-left corner), the
//! clip rules, the mask's region and `mask-type`, the filter's region and each primitive's
//! subregion and inputs. The painter draws the resulting tree as the element's clip or mask,
//! or takes the filter out of it (blitz-paint, `render/svg_refs.rs`).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use markup5ever::{local_name, ns};

use crate::{BaseDocument, NodeId};

/// Which definition a CSS property's `url(#id)` must name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SvgReferenceKind {
    /// `clip-path: url(#id)`: a `<clipPath>`
    ClipPath,
    /// `mask-image: url(#id)`: a `<mask>`
    Mask,
    /// `filter: url(#id)`: a `<filter>`
    Filter,
}

impl SvgReferenceKind {
    /// The local name of the SVG element this kind of reference names
    pub fn element_name(self) -> &'static str {
        match self {
            Self::ClipPath => "clipPath",
            Self::Mask => "mask",
            Self::Filter => "filter",
        }
    }

    /// The presentation attribute that carries the reference in the document made for usvg
    fn attribute(self) -> &'static str {
        match self {
            Self::ClipPath => "clip-path",
            Self::Mask => "mask",
            Self::Filter => "filter",
        }
    }
}

/// The SVG document made for one reference (see the module documentation), parsed.
#[derive(Clone, Debug)]
pub struct SvgReference {
    /// The tree: one rectangle, clipped, masked or filtered by the definition. Its user space is
    /// the referencing element's: CSS pixels from the top-left corner of its border box.
    pub tree: Arc<usvg::Tree>,
    /// Whether the rectangle is the border box itself (the definition measures in
    /// `objectBoundingBox` units, so what the tree draws ends at the border box) or a rectangle
    /// far larger than any page (`userSpaceOnUse`: the definition alone bounds what it draws).
    pub bounded: bool,
}

/// How far the rectangle of a `userSpaceOnUse` definition reaches around the border box
const UNBOUNDED_EXTENT: f32 = 100_000.0;

/// The parsed documents of the references painted so far, by the hash of their source: a
/// definition, or a box, that changes makes another source.
#[derive(Default)]
pub(crate) struct SvgReferenceCache(std::sync::Mutex<HashMap<u64, Option<SvgReference>>>);

impl BaseDocument {
    /// The inline SVG definition of the given kind that `url` names, if `url` is a reference
    /// into this document (CSS Values 4, 4.5.1: a fragment-only `url(#id)`, which stylo has
    /// resolved against the document's base URL) to an element of that kind.
    pub fn svg_reference_target(&self, url: &url::Url, kind: SvgReferenceKind) -> Option<NodeId> {
        let fragment = url.fragment().filter(|f| !f.is_empty())?;
        if !self.url.is_same_document(url) {
            return None;
        }
        let id = percent_encoding::percent_decode_str(fragment).decode_utf8().ok()?;
        let node_id = self.get_element_by_id(&id)?;
        let element = self.get_node(node_id)?.element_data()?;
        (element.name.ns == ns!(svg) && &*element.name.local == kind.element_name()).then_some(node_id)
    }

    /// Whether `url` names something in this document rather than a resource to fetch (a
    /// fragment on the document's own address).
    pub fn is_local_reference(&self, url: &url::Url) -> bool {
        url.fragment().is_some_and(|f| !f.is_empty()) && self.url.is_same_document(url)
    }

    /// The document made for a reference of the given kind to the definition `target`, from an
    /// element whose border box is `width` x `height` CSS pixels (see the module documentation).
    /// `None` if usvg cannot make anything of it.
    pub fn svg_reference(
        &self,
        target: NodeId,
        kind: SvgReferenceKind,
        width: f32,
        height: f32,
    ) -> Option<SvgReference> {
        let node = self.get_node(target)?;
        let element = node.element_data()?;
        let id = element.attr(local_name!("id"))?;

        // The outermost <svg> around the definition, whose content becomes the <defs>
        let mut root = None;
        let mut current = node.parent;
        while let Some(parent) = current.and_then(|id| self.get_node(id)) {
            if parent
                .element_data()
                .is_some_and(|el| el.name.ns == ns!(svg) && el.name.local == local_name!("svg"))
            {
                root = Some(parent);
            }
            current = parent.parent;
        }
        let mut defs = String::new();
        match root {
            Some(root) => {
                for &child in &root.children {
                    if let Some(child) = self.get_node(child) {
                        defs.push_str(&child.svg_definition_source());
                    }
                }
            }
            None => defs.push_str(&node.svg_definition_source()),
        }

        // A definition in `objectBoundingBox` units is measured in the rectangle that names it
        let units = |name: &str| {
            element
                .attrs()
                .iter()
                .find(|a| a.name.ns == ns!() && &*a.name.local == name)
                .map(|a| a.value.trim().to_string())
        };
        let bounded = match kind {
            SvgReferenceKind::ClipPath => units("clipPathUnits").as_deref() == Some("objectBoundingBox"),
            // (a mask's region is in objectBoundingBox units unless it says otherwise)
            SvgReferenceKind::Mask => {
                units("maskUnits").as_deref() != Some("userSpaceOnUse")
                    || units("maskContentUnits").as_deref() == Some("objectBoundingBox")
            }
            SvgReferenceKind::Filter => true,
        };
        let (x, y, w, h) = match bounded {
            true => (0.0, 0.0, width, height),
            false => (-UNBOUNDED_EXTENT, -UNBOUNDED_EXTENT, 2.0 * UNBOUNDED_EXTENT, 2.0 * UNBOUNDED_EXTENT),
        };
        // What is drawn where the definition lets it: opaque black under a clip path and a
        // filter, opaque white under a mask (whose luminance usvg has already turned to alpha)
        let fill = match kind {
            SvgReferenceKind::Mask => "#fff",
            _ => "#000",
        };
        let mut id_attr = String::new();
        html_escape::encode_quoted_attribute_to_string(id, &mut id_attr);
        let source = format!(
            concat!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" "#,
                r#"width="{vw}" height="{vh}"><defs>{defs}</defs>"#,
                r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" fill="{fill}" {attr}="url(#{id})"/></svg>"#
            ),
            vw = width.max(1.0),
            vh = height.max(1.0),
            defs = defs,
            x = x,
            y = y,
            w = w,
            h = h,
            fill = fill,
            attr = kind.attribute(),
            id = id_attr,
        );

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        let key = hasher.finish();
        let mut cache = self.svg_reference_cache.0.lock().unwrap();
        if let Some(cached) = cache.get(&key) {
            return cached.clone();
        }
        let loader = self.svg_subresource_loader();
        let reference = crate::util::parse_svg_image(source.as_bytes(), Some(&loader))
            .ok()
            .map(|svg| SvgReference { tree: svg.tree, bounded });
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(key, reference.clone());
        reference
    }
}
