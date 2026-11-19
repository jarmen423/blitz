//! Serialization of nodes to HTML, and of inline `<svg>` subtrees to SVG source for usvg.

use html_escape::{encode_quoted_attribute_to_string, encode_text_to_string};
use markup5ever::{QualName, local_name, ns};
use style_traits::values::ToCss;
use stylo_taffy::CopyStyleGetters;

use super::{Node, NodeData};

#[derive(Clone, Copy)]
enum OutputStyle {
    Normal,
    Pretty,
}

#[derive(Clone, Copy)]
struct SerializeOpts {
    style: OutputStyle,
    /// Replace `currentColor` in attribute values with the element's computed `color`
    resolve_current_color: bool,
    /// Declare the `xlink` prefix on the root element so that the output is well-formed XML
    declare_xlink_ns: bool,
    /// Leave out the root element's `transform` attribute, which is applied as a CSS transform
    skip_root_transform: bool,
}

impl SerializeOpts {
    const fn html(style: OutputStyle) -> Self {
        Self {
            style,
            resolve_current_color: false,
            declare_xlink_ns: false,
            skip_root_transform: false,
        }
    }
}

const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// A style attribute's declarations without `opacity` (split at semicolons outside parentheses and quotes).
#[cfg(feature = "svg")]
fn style_without_opacity(style: &str) -> String {
    let mut decls: Vec<&str> = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    for (i, c) in style.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"') | (None, '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ';') if depth <= 0 => {
                decls.push(&style[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    decls.push(&style[start..]);
    decls
        .into_iter()
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.split(':').next().unwrap_or("").trim().eq_ignore_ascii_case("opacity"))
        .collect::<Vec<_>>()
        .join(";")
}

/// known-future: the computed value of the custom property `name` (with or without its `--`) on
/// `node`, serialized.
fn custom_property(node: &Node, name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches("--");
    let styles = node.primary_styles()?;
    let props = styles.custom_properties();
    let mut i = 0;
    while let Some((n, value)) = props.property_at(i) {
        if (&**n as &str).trim_start_matches("--") == name {
            return value.as_ref().map(|v| v.to_css_string());
        }
        i += 1;
    }
    None
}

/// known-future: `var(--name)` and `var(--name, fallback)` in an attribute value replaced with the
/// element's computed custom property (else the fallback, itself resolved). Chrome resolves them
/// in an SVG presentation attribute (`<path stroke="var(--fg)">`); usvg does not, and drew no
/// stroke (an outside project's infinity sign was missing).
fn resolve_vars(value: &str, node: &Node, depth: u8) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("var(") {
        out.push_str(&rest[..start]);
        let inner_start = start + 4;
        // the matching closing parenthesis, and the first top-level comma before it
        let (mut level, mut comma, mut end) = (0i32, None, None);
        for (k, c) in rest[inner_start..].char_indices() {
            match c {
                '(' => level += 1,
                ')' if level == 0 => {
                    end = Some(inner_start + k);
                    break;
                }
                ')' => level -= 1,
                ',' if level == 0 && comma.is_none() => comma = Some(inner_start + k),
                _ => {}
            }
        }
        let Some(end) = end else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &rest[inner_start..comma.unwrap_or(end)];
        let resolved = custom_property(node, name).or_else(|| {
            comma.map(|c| match depth {
                0..8 => resolve_vars(rest[c + 1..end].trim(), node, depth + 1),
                _ => String::new(),
            })
        });
        out.push_str(resolved.as_deref().unwrap_or("").trim());
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Writes an attribute's serialized name, per the HTML fragment serialization algorithm
fn write_attr_name(name: &QualName, writer: &mut String) {
    match name.ns {
        ns!() | ns!(html) => {}
        ns!(xml) => writer.push_str("xml:"),
        ns!(xmlns) if name.local.as_str() == "xmlns" => {}
        ns!(xmlns) => writer.push_str("xmlns:"),
        ns!(xlink) => writer.push_str("xlink:"),
        _ => {
            if let Some(prefix) = &name.prefix {
                writer.push_str(prefix);
                writer.push(':');
            }
        }
    }
    writer.push_str(&name.local);
}

fn is_xlink_ns_decl(name: &QualName) -> bool {
    (name.ns == ns!(xmlns) && name.local.as_str() == "xlink")
        || name.local.as_str() == "xmlns:xlink"
}

impl Node {
    /// Renders the HTML of this node and all its children as a `String` without extra whitespace.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html><head /><body><main id="main"><div class="arbitrary-class" /></main></body></html>
    /// ```
    pub fn outer_html(&self) -> String {
        let mut output = String::new();
        self.write_outer_html(&mut output);
        output
    }

    /// Renders the HTML of this node and all its children as a `String` with whitespace for human
    /// readability.
    ///
    /// Example output:
    ///
    /// ```text
    /// <html>
    ///   <head />
    ///   <body>
    ///     <main id="main">
    ///       <div class="arbitrary-class" />
    ///     </main>
    ///   </body>
    /// </html>
    /// ```
    pub fn outer_html_pretty(&self) -> String {
        let mut output = String::new();
        self.write_outer_html_pretty(&mut output);
        output
    }

    pub fn write_outer_html(&self, writer: &mut String) {
        self.write_outer_html_in_style(writer, SerializeOpts::html(OutputStyle::Normal), 0);
    }

    pub fn write_outer_html_pretty(&self, writer: &mut String) {
        self.write_outer_html_in_style(writer, SerializeOpts::html(OutputStyle::Pretty), 0);
    }

    /// Serializes this `<svg>` subtree as a standalone SVG document for usvg, with
    /// `currentColor` resolved against each element's computed `color`.
    #[cfg(feature = "svg")]
    pub(crate) fn svg_source(&self) -> String {
        let mut output = String::new();
        let opts = SerializeOpts {
            style: OutputStyle::Normal,
            resolve_current_color: true,
            declare_xlink_ns: true,
            skip_root_transform: true,
        };
        self.write_outer_html_in_style(&mut output, opts, 0);
        output
    }

    fn write_outer_html_in_style(&self, writer: &mut String, opts: SerializeOpts, nesting: usize) {
        const INDENT: &str = "  ";
        let style = opts.style;
        let has_children = !self.children.is_empty();
        let current_color = opts
            .resolve_current_color
            .then(|| self.primary_styles())
            .flatten()
            .map(|style| style.clone_color())
            .map(|color| color.to_css_string());

        match &self.data {
            NodeData::Document(_) => {}
            NodeData::Comment { .. } => {}
            NodeData::AnonymousBlock(_) => {}
            // NodeData::Doctype { name, .. } => write!(s, "DOCTYPE {name}"),
            NodeData::Text(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                let in_raw_text_element = self
                    .parent
                    .and_then(|id| self.tree()[id].data.downcast_element())
                    .is_some_and(|el| {
                        // Documents are parsed with scripting disabled, so `<noscript>` is not raw text
                        el.name.ns == ns!(html)
                            && matches!(
                                el.name.local.as_ref(),
                                "style"
                                    | "script"
                                    | "xmp"
                                    | "iframe"
                                    | "noembed"
                                    | "noframes"
                                    | "plaintext"
                            )
                    });
                if in_raw_text_element {
                    writer.push_str(data.content.as_str());
                } else {
                    encode_text_to_string(data.content.as_str(), writer);
                }
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }
            }
            NodeData::Element(data) => {
                if matches!(style, OutputStyle::Pretty) {
                    for _ in 0..nesting {
                        writer.push_str(INDENT);
                    }
                }
                writer.push('<');
                writer.push_str(&data.name.local);

                if opts.declare_xlink_ns
                    && nesting == 0
                    && !data.attrs().iter().any(|attr| is_xlink_ns_decl(&attr.name))
                {
                    writer.push_str(" xmlns:xlink=\"");
                    writer.push_str(XLINK_NS);
                    writer.push('"');
                }

                for attr in data.attrs() {
                    if opts.skip_root_transform
                        && nesting == 0
                        && attr.name.ns == ns!()
                        && attr.name.local == local_name!("transform")
                    {
                        continue;
                    }
                    // The root's `opacity` set in its style attribute is the element's CSS opacity, which the
                    // box is painted with; usvg read it again and multiplied it in (a shadow <svg style="...;
                    // opacity:0"> faded in by GSAP stayed invisible: prepare's `opacity:1 !important` reached the
                    // box, not the style attribute usvg reads). The `opacity` attribute is usvg's alone.
                    if opts.skip_root_transform
                        && nesting == 0
                        && attr.name.ns == ns!()
                        && attr.name.local == local_name!("style")
                        && attr.value.to_ascii_lowercase().contains("opacity")
                    {
                        let kept = style_without_opacity(&attr.value);
                        if !kept.trim().is_empty() {
                            writer.push_str(" style=\"");
                            encode_quoted_attribute_to_string(&kept, writer);
                            writer.push('"');
                        }
                        continue;
                    }
                    writer.push(' ');
                    write_attr_name(&attr.name, writer);
                    writer.push_str("=\"");
                    let resolved;
                    let attr_value: &str = if opts.resolve_current_color && attr.value.contains("var(") {
                        resolved = resolve_vars(&attr.value, self, 0);
                        &resolved
                    } else {
                        &attr.value
                    };
                    #[allow(clippy::unnecessary_unwrap)] // Convert to if-let chain once stabilised
                    if current_color.is_some() && attr_value.contains("currentColor") {
                        let value = attr_value.replace("currentColor", current_color.as_ref().unwrap());
                        encode_quoted_attribute_to_string(&value, writer);
                    } else {
                        encode_quoted_attribute_to_string(attr_value, writer);
                    }
                    writer.push('"');
                }
                if !has_children {
                    writer.push_str(" /");
                }
                writer.push('>');
                if matches!(style, OutputStyle::Pretty) {
                    writer.push('\n');
                }

                if has_children {
                    for &child_id in &self.children {
                        self.tree()[child_id].write_outer_html_in_style(writer, opts, nesting + 1);
                    }

                    if matches!(style, OutputStyle::Pretty) {
                        for _ in 0..nesting {
                            writer.push_str(INDENT);
                        }
                    }
                    writer.push_str("</");
                    writer.push_str(&data.name.local);
                    writer.push('>');
                    if matches!(style, OutputStyle::Pretty) {
                        writer.push('\n');
                    }
                }
            }
        }
    }
}
