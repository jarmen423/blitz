use crate::node::{Node, NodeData};
use color::{AlphaColor, Srgb};
use keyboard_types::Modifiers;
use std::borrow::Cow;
use style::color::AbsoluteColor;

#[cfg(target_os = "macos")]
pub(crate) const ACTION_MOD: Modifiers = Modifiers::META;
#[cfg(not(target_os = "macos"))]
pub(crate) const ACTION_MOD: Modifiers = Modifiers::CONTROL;

pub type Color = AlphaColor<Srgb>;

/// Decode raw font bytes, decompressing WOFF/WOFF2 if the `woff` feature is enabled.
/// Returns the original slice unchanged for TTF/OTF input, and also on decompression
/// failure. With the `woff` feature disabled, all input passes through unchanged.
pub fn decode_font_bytes(bytes: &[u8]) -> Cow<'_, [u8]> {
    if bytes.len() < 4 {
        return Cow::Borrowed(bytes);
    }
    match &bytes[0..4] {
        #[cfg(feature = "woff")]
        b"wOFF" => wuff::decompress_woff1(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff1 font");
                Cow::Borrowed(bytes)
            }),
        #[cfg(feature = "woff")]
        b"wOF2" => wuff::decompress_woff2(bytes)
            .map(Cow::Owned)
            .unwrap_or_else(|_| {
                #[cfg(feature = "tracing")]
                tracing::warn!("Failed to decompress woff2 font");
                Cow::Borrowed(bytes)
            }),
        _ => Cow::Borrowed(bytes),
    }
}

#[cfg(feature = "svg")]
use std::sync::{Arc, LazyLock};
#[cfg(feature = "svg")]
use usvg::fontdb;
#[cfg(feature = "svg")]
pub(crate) static FONT_DB: LazyLock<Arc<fontdb::Database>> = LazyLock::new(|| {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    // Extra font folders (for example a project's own fonts converted to TTF), separated as PATH is (`:`).
    // known-future (wave 0): every folder named is read, once each, with the folders under it; one that is
    // missing is passed over. fontdb reads TTF, OTF and collections; the web fonts a render downloads (WOFF and
    // WOFF2: known_future/webfonts.py's <work>/_fonts, known_future/weblocal.py's <work>/_web) are decoded here,
    // so SVG text finds them without a converted copy.
    if let Some(dirs) = std::env::var_os("BLITZ_FONT_DIRS") {
        let mut seen: Vec<std::path::PathBuf> = Vec::new();
        for dir in std::env::split_paths(&dirs) {
            if dir.as_os_str().is_empty() || seen.contains(&dir) {
                continue;
            }
            db.load_fonts_dir(&dir);
            load_web_fonts_dir(&mut db, &dir, 0);
            seen.push(dir);
        }
    }
    // fontdb's generic families default to Times New Roman, Arial and Courier New; where those aren't
    // installed (most Linux machines) SVG text in a generic family was dropped. Use what is installed.
    let has = |db: &fontdb::Database, name: &str| db.faces().any(|f| f.families.iter().any(|(n, _)| n == name));
    let pick = |db: &fontdb::Database, names: &[&str]| names.iter().find(|n| has(db, n)).map(|n| n.to_string());
    if let Some(f) = pick(&db, &["Times New Roman", "Noto Serif", "DejaVu Serif", "Liberation Serif"]) {
        db.set_serif_family(f);
    }
    if let Some(f) = pick(&db, &["Arial", "Noto Sans", "DejaVu Sans", "Liberation Sans"]) {
        db.set_sans_serif_family(f);
    }
    if let Some(f) = pick(&db, &["Courier New", "Noto Mono", "Noto Sans Mono", "DejaVu Sans Mono", "Liberation Mono"]) {
        db.set_monospace_family(f);
    }
    if let Some(f) = pick(&db, &["Comic Sans MS", "Noto Sans", "DejaVu Sans"]) {
        db.set_cursive_family(f);
    }
    if let Some(f) = pick(&db, &["Impact", "Noto Sans", "DejaVu Sans"]) {
        db.set_fantasy_family(f);
    }
    Arc::new(db)
});

/// known-future (wave 0): the WOFF and WOFF2 files of a `BLITZ_FONT_DIRS` folder (and the folders under it),
/// decoded and added to the SVG text font database. In name order, so the faces are the same on every run.
#[cfg(feature = "svg")]
fn load_web_fonts_dir(db: &mut fontdb::Database, dir: &std::path::Path, depth: u32) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            load_web_fonts_dir(db, &path, depth + 1);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        if !matches!(ext.as_deref(), Some("woff" | "woff2")) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            // (a file that is no font, or cannot be decoded, is one fontdb finds no face in)
            db.load_font_data(decode_font_bytes(&bytes).into_owned());
        }
    }
}

/// Which kind of CSS image layer list (`background-image` or `mask-image`) to
/// flush from style to dedicated storage on the node.
#[derive(Clone, Copy, Debug)]
pub enum ImageLayerKind {
    Background,
    Mask,
}

impl ImageLayerKind {
    pub fn image_type(self, idx: usize) -> ImageType {
        match self {
            Self::Background => ImageType::Background(idx),
            Self::Mask => ImageType::Mask(idx),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ImageType {
    Image,
    Background(usize),
    Mask(usize),
}

/// A point
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub struct Point<T> {
    /// The x coordinate
    pub x: T,
    /// The y coordinate
    pub y: T,
}

impl Point<f64> {
    pub const ZERO: Self = Point { x: 0.0, y: 0.0 };
}

// Debug print an RcDom
pub fn walk_tree(indent: usize, node: &Node) {
    // Skip all-whitespace text nodes entirely
    if let NodeData::Text(data) = &node.data {
        if data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    print!("{}", " ".repeat(indent));
    let id = node.id;
    match &node.data {
        NodeData::Document(_) => println!("#Document {id}"),

        NodeData::Text(data) => {
            if data.content.chars().all(|c| c.is_ascii_whitespace()) {
                println!("{id} #text: <whitespace>");
            } else {
                let content = data.content.trim();
                if content.len() > 10 {
                    println!(
                        "#text {id}: {}...",
                        content
                            .split_at(content.char_indices().take(10).last().unwrap().0)
                            .0
                            .escape_default()
                    )
                } else {
                    println!("#text {id}: {}", data.content.trim().escape_default())
                }
            }
        }

        NodeData::Comment { .. } => println!("<!-- COMMENT {id} -->"),

        NodeData::AnonymousBlock(_) => println!("{id} AnonymousBlock"),

        NodeData::Element(data) => {
            print!("<{} {id}", data.name.local);
            for attr in data.attrs.iter() {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            if !node.children.is_empty() {
                println!(">");
            } else {
                println!("/>");
            }
        } // NodeData::Doctype {
          //     ref name,
          //     ref public_id,
          //     ref system_id,
          // } => println!("<!DOCTYPE {} \"{}\" \"{}\">", name, public_id, system_id),
          // NodeData::ProcessingInstruction { .. } => unreachable!(),
    }

    if !node.children.is_empty() {
        for child_id in node.children.iter() {
            walk_tree(indent + 2, node.with(*child_id));
        }

        if let NodeData::Element(data) = &node.data {
            println!("{}</{}>", " ".repeat(indent), data.name.local);
        }
    }
}

/// known-future (wave 0): what an inline `<svg>` fetches the files its `<image href>` (or `xlink:href`) names with.
///
/// usvg's own resolver took the address for a path on this machine, relative to the process's working folder, so a
/// relative address, a `file:` address and an address relative to `<base href>` drew nothing, while an absolute path
/// was read from the disk past the document's [`NetProvider`]. Here the address is resolved against the document's
/// base URL, as every other address of the page is, and the file is asked of the document's net provider
/// ([`NetProvider::fetch_sync`]): a provider that guards what a document may reach decides for these too, and one
/// with no synchronous path leaves them out. Nothing is read from the disk or the network past it.
#[cfg(feature = "svg")]
#[derive(Clone)]
pub(crate) struct SvgSubresourceLoader {
    pub(crate) base_url: url::Url,
    pub(crate) net_provider: Arc<dyn blitz_traits::net::NetProvider>,
    pub(crate) doc_id: usize,
    /// Each address is asked for once per document, whether it came or not: an inline `<svg>` is parsed again
    /// every time its box is built again (a refused address is then reported once, too).
    pub(crate) cache: SvgSubresourceCache,
}

/// The files the inline `<svg>` elements of a document have asked for, by resolved address (`None`: not to be had).
#[cfg(feature = "svg")]
pub(crate) type SvgSubresourceCache =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Option<Arc<Vec<u8>>>>>>;

#[cfg(feature = "svg")]
impl SvgSubresourceLoader {
    fn load(&self, href: &str) -> Option<(url::Url, Arc<Vec<u8>>)> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let url = self.base_url.join(href).ok()?;
        if let Some(known) = self.cache.lock().unwrap().get(url.as_str()) {
            return known.clone().map(|bytes| (url, bytes));
        }
        let request = blitz_traits::net::Request::get(url.clone());
        let fetched = match self.net_provider.fetch_sync(self.doc_id, request) {
            Some(Ok((_resolved_url, bytes))) => Some(Arc::new(bytes.to_vec())),
            Some(Err(_)) | None => None,
        };
        self.cache
            .lock()
            .unwrap()
            .insert(url.as_str().to_string(), fetched.clone());
        fetched.map(|bytes| (url, bytes))
    }
}

/// What kind of picture the fetched bytes of an SVG `<image>` are, by their first bytes (an SVG document by its
/// address or its first character). The raster kinds are decoded by the renderer.
#[cfg(feature = "svg")]
fn svg_image_kind(url: &url::Url, data: Arc<Vec<u8>>, opts: &usvg::Options) -> Option<usvg::ImageKind> {
    let d = data.as_slice();
    if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(usvg::ImageKind::PNG(data));
    }
    if d.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(usvg::ImageKind::JPEG(data));
    }
    if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") {
        return Some(usvg::ImageKind::GIF(data));
    }
    if d.len() >= 12 && &d[0..4] == b"RIFF" && &d[8..12] == b"WEBP" {
        return Some(usvg::ImageKind::WEBP(data));
    }
    let path = url.path().to_ascii_lowercase();
    let first = d.iter().copied().find(|b| !b.is_ascii_whitespace());
    let svg = path.ends_with(".svg") || path.ends_with(".svgz") || d.starts_with(&[0x1f, 0x8b]) || first == Some(b'<');
    // (an SVG drawn by an <image> loads no file of its own: usvg's `from_data_nested`)
    svg.then(|| usvg::Tree::from_data_nested(d, opts).ok().map(usvg::ImageKind::SVG))
        .flatten()
}

/// Parse an SVG image.
///
/// `loader` fetches what the SVG's `<image>` elements name (an inline `<svg>`, [`SvgSubresourceLoader`]). With
/// `None` (an SVG that is itself a picture: `<img src>`, a CSS `url()`) only `data:` addresses are drawn, as in a
/// browser, where an SVG used as an image loads no other file.
#[cfg(feature = "svg")]
pub(crate) fn parse_svg_image(
    source: &[u8],
    loader: Option<&SvgSubresourceLoader>,
) -> Result<crate::node::SvgImageData, usvg::Error> {
    let options = usvg::Options {
        fontdb: Arc::clone(&*FONT_DB),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(move |href: &str, opts: &usvg::Options| {
                let (url, data) = loader?.load(href)?;
                svg_image_kind(&url, data, opts)
            }),
        },
        ..Default::default()
    };
    crate::node::SvgImageData::from_data(source, &options)
}

pub trait ToColorColor {
    /// Converts a color into the `AlphaColor<Srgb>` type from the `color` crate
    fn as_color_color(&self) -> Color;
}
impl ToColorColor for AbsoluteColor {
    fn as_color_color(&self) -> Color {
        Color::new(
            *self
                .to_color_space(style::color::ColorSpace::Srgb)
                .raw_components(),
        )
    }
}

#[cfg(all(test, feature = "svg"))]
mod svg_tests {
    use super::parse_svg_image;

    #[test]
    fn svg_without_xmlns_parses() {
        let src = br#"<svg viewBox="0 0 24 24"><path d="M0 0h24v24H0z"/></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert!(!svg.tree.root().children().is_empty());
    }

    #[test]
    fn missing_height_is_computed_from_width_and_viewbox_ratio() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100" width="200"><rect width="100%" height="100%" fill="green"/></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(200.0));
        assert_eq!(svg.intrinsic_height(), None);
        assert_eq!(svg.viewbox_aspect_ratio(), Some(1.0));
        assert_eq!(svg.tree.size().width(), 200.0);
        assert_eq!(svg.intrinsic_size(), (200.0, 200.0));
    }

    #[test]
    fn viewbox_only_has_no_intrinsic_dimensions() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 485 58"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
        // The aspect ratio is still available from the viewBox.
        assert!((svg.aspect_ratio() - (485.0 / 58.0)).abs() < 1e-3);
    }

    #[test]
    fn absolute_dimensions_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="16" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), Some(24.0));
        assert_eq!(svg.intrinsic_height(), Some(16.0));
    }

    #[test]
    fn percentage_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100%" height="50%" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }

    #[test]
    fn unit_lengths_are_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24px" height="1.5em" viewBox="0 0 48 32"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert!(svg.intrinsic_width().is_some());
        assert!(svg.intrinsic_height().is_some());
    }

    #[test]
    fn non_numeric_dimensions_are_not_intrinsic() {
        let src = br#"<svg xmlns="http://www.w3.org/2000/svg" width="auto" height="foo" viewBox="0 0 200 100"></svg>"#;
        let svg = parse_svg_image(src, None).unwrap();
        assert_eq!(svg.intrinsic_width(), None);
        assert_eq!(svg.intrinsic_height(), None);
    }
}

/// Creates an markup5ever::QualName.
/// Given a local name and an optional namespace
#[macro_export]
macro_rules! qual_name {
    ($local:tt $(, $ns:ident)?) => {
        $crate::QualName {
            prefix: None,
            ns: $crate::ns!($($ns)?),
            local: $crate::local_name!($local),
        }
    };
}

/// known-future (outside round 4): the room an inline element's box takes in its line before its
/// first character and after its last, in CSS px: its inline-start (left) margin, border and padding,
/// and its inline-end (right) ones (CSS 2.1 8.6, 10.8). A percentage margin or padding, which refers to
/// the containing block's width, and `auto` count as zero. Left to right.
pub fn inline_room(style: &style::properties::ComputedValues) -> (f32, f32) {
    let (ml, mr) = inline_margins(style);
    let lp = |v: &style::values::computed::LengthPercentage| v.to_length().map(|l| l.px()).unwrap_or(0.0);
    let p = style.get_padding();
    let b = style.get_border();
    let side = |st: style::values::computed::BorderStyle, w: &style::values::computed::BorderSideWidth| {
        if st.none_or_hidden() { 0.0 } else { w.0.to_f32_px() }
    };
    (
        ml + side(b.border_left_style, &b.border_left_width) + lp(&p.padding_left.0),
        mr + side(b.border_right_style, &b.border_right_width) + lp(&p.padding_right.0),
    )
}

/// known-future: an inline element's left and right margins in CSS px (`auto` and percentages: zero).
pub fn inline_margins(style: &style::properties::ComputedValues) -> (f32, f32) {
    use style::values::generics::length::GenericMargin;
    let m = style.get_margin();
    let mg = |v: &style::values::computed::Margin| match v {
        GenericMargin::LengthPercentage(x) => x.to_length().map(|l| l.px()).unwrap_or(0.0),
        _ => 0.0,
    };
    (mg(&m.margin_left), mg(&m.margin_right))
}
