//! known-future (wave 1a): what `fshot serve` tells of a laid-out page as data (serve.rs has the protocol; this file
//! has what the `analyze` and `resolve` answers are made of). Everything here is read from the layout engine: the
//! DOM as its parser built it, the styles Stylo computed (the cascade, `@layer`, `@supports`, `@media`, `:has()`,
//! `var()` and `calc()` already decided), the boxes Taffy laid out and the rules of the page's style sheets. It is
//! there so the renderer's compiler asks instead of reading CSS and HTML with patterns of its own.
//!
//! Three kinds of value are told apart, as a browser tells them apart:
//!   * `cs`, a computed value: Stylo's, serialised as CSS text. A percentage stays a percentage, `auto` stays
//!     `auto`, a `transform` stays the list of functions that was written (`rotate(10deg)`), a `var()` is replaced.
//!     These are the values `fshot dump` lists under "cs" and "mask".
//!   * `used`, a resolved value: what `getComputedStyle()` answers. `width`, `height`, the insets, margins and
//!     paddings in px from the layout; `transform` as a `matrix()`; `transform-origin` in px.
//!   * `vars`, custom properties: the computed value of each `--name` on the element.
//! A shorthand (`overflow`, `inset`, `mask`) has no computed value of its own: asked for under `cs` it is given as
//! `getComputedStyle()` serialises it.

use super::{ElementBoxes, element_boxes, json_string};
use blitz_dom::{BaseDocument, LocalName, NodeId, node::NodeData, ns};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::rc::Rc;
use style::properties::{ComputedValues, LonghandId, PropertyDeclarationId, PropertyId};
use style::servo_arc::Arc as ServoArc;

/// The properties `analyze` and `resolve` report when a request names none: what decides how the renderer splits
/// a scene into pictures and what the compositor must draw itself (the effects, the transforms, what clips and
/// what stacks), then what its timeline reader starts tweens from.
pub(crate) const DEFAULT_PROPS: &[&str] = &[
    // is it there, and where in the stack
    "display",
    "visibility",
    "position",
    "z-index",
    "float",
    "contain",
    "container-type",
    "content-visibility",
    "isolation",
    "will-change",
    // effects: a group the painter or the compositor draws as one
    "opacity",
    "mix-blend-mode",
    "background-blend-mode",
    "filter",
    "backdrop-filter",
    "clip-path",
    "mask-image",
    "mask-mode",
    "mask-repeat",
    "mask-position-x",
    "mask-position-y",
    "mask-clip",
    "mask-origin",
    "mask-size",
    "mask-composite",
    "mask-type",
    // clipping
    "overflow-x",
    "overflow-y",
    "overflow-clip-margin",
    "clip",
    // transforms
    "transform",
    "transform-origin",
    "transform-box",
    "transform-style",
    "translate",
    "rotate",
    "scale",
    "perspective",
    "perspective-origin",
    "backface-visibility",
    "offset-path",
    "offset-distance",
    // boxes
    "box-sizing",
    "width",
    "height",
    "top",
    "right",
    "bottom",
    "left",
    "object-fit",
    "object-position",
    "aspect-ratio",
    // paint
    "color",
    "background-color",
    "background-image",
    "border-top-color",
    "box-shadow",
    "text-shadow",
    // what moves by itself: a CSS animation whole (wave 2: its delay, timing function, iteration count,
    // direction, fill mode and play state too, which decide what a frame shows as much as its name and duration do;
    // they were given only when asked for by name)
    "animation-name",
    "animation-duration",
    "animation-delay",
    "animation-timing-function",
    "animation-iteration-count",
    "animation-direction",
    "animation-fill-mode",
    "animation-play-state",
    "transition-property",
    "transition-duration",
];

/// The resolved (`getComputedStyle()`) values reported when a request names none
pub(crate) const DEFAULT_USED: &[&str] = &["width", "height", "top", "right", "bottom", "left", "transform", "transform-origin"];

enum PropKind {
    /// a longhand: Stylo's computed value
    Longhand(LonghandId),
    /// a custom property (`--name`): its computed value, as `getComputedStyle()` gives it
    Custom,
    /// a shorthand: as `getComputedStyle()` gives it, from the resolved values of its longhands. Those of
    /// `margin`, `padding` and `inset` are used values, in px from the layout, and `grid-template` lists the
    /// tracks the layout made: a shorthand's value is the style's and the layout's.
    Shorthand,
}

/// A list of property names a request asks for, each looked up once
pub(crate) struct Props {
    pub(crate) names: Vec<String>,
    kinds: Vec<PropKind>,
    /// the places in the list of the shorthands: the values the layout has a say in, which can change on an
    /// element whose style did not (`Report::element`)
    pub(crate) laid_out: Vec<usize>,
    /// names the style engine has no property for (a misspelling, or a property this build does not parse)
    pub(crate) unsupported: Vec<String>,
}

impl Props {
    pub(crate) fn parse<'a>(names: impl IntoIterator<Item = &'a str>) -> Props {
        let mut props = Props { names: Vec::new(), kinds: Vec::new(), laid_out: Vec::new(), unsupported: Vec::new() };
        let mut seen: HashSet<String> = HashSet::new();
        for name in names {
            let name = if name.starts_with("--") { name.to_string() } else { name.trim().to_ascii_lowercase() };
            if !seen.insert(name.clone()) {
                continue;
            }
            match PropertyId::parse_enabled_for_all_content(&name) {
                Ok(id) => {
                    props.kinds.push(match id.as_shorthand() {
                        Err(PropertyDeclarationId::Longhand(longhand)) => PropKind::Longhand(longhand),
                        Err(PropertyDeclarationId::Custom(_)) => PropKind::Custom,
                        Ok(_) => {
                            props.laid_out.push(props.names.len());
                            PropKind::Shorthand
                        }
                    });
                    props.names.push(name);
                }
                Err(_) => props.unsupported.push(name),
            }
        }
        props
    }

    /// Every longhand the style engine has, by name (a request's `"props": "all"`)
    pub(crate) fn all() -> Props {
        Props::parse(blitz_dom::resolved_style_property_names().iter().copied())
    }

    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    /// The value of property `k` on an element: `st` is the element's computed style
    pub(crate) fn value(&self, k: usize, doc: &BaseDocument, id: NodeId, st: &ComputedValues) -> String {
        match &self.kinds[k] {
            PropKind::Longhand(longhand) => st.computed_value_to_string(PropertyDeclarationId::Longhand(*longhand)),
            PropKind::Custom | PropKind::Shorthand => doc.resolved_style_value(id, &self.names[k]),
        }
    }

    /// The values of an element whose computed style is the object `before` was read from, as they are now. What
    /// the style alone decides is as it was; a shorthand is read again, the layout having a say in it (a sibling
    /// that grew moves an `auto` margin and leaves the style alone). The list is `before` itself, shared, when
    /// nothing moved.
    fn again(&self, doc: &BaseDocument, id: NodeId, before: &Rc<Vec<String>>) -> Rc<Vec<String>> {
        let mut now: Option<Vec<String>> = None;
        for &k in &self.laid_out {
            let value = doc.resolved_style_value(id, &self.names[k]);
            if value != before[k] {
                now.get_or_insert_with(|| before.to_vec())[k] = value;
            }
        }
        now.map_or_else(|| Rc::clone(before), Rc::new)
    }

    /// The initial value of property `k` (`None` for a custom property or a shorthand, which have none to compare with)
    pub(crate) fn initial(&self, k: usize, initial: &ComputedValues) -> Option<String> {
        match &self.kinds[k] {
            PropKind::Longhand(longhand) => Some(initial.computed_value_to_string(PropertyDeclarationId::Longhand(*longhand))),
            PropKind::Custom | PropKind::Shorthand => None,
        }
    }
}

/// A number as JSON has it: one that is not finite (a layout that overflowed) is `null`
pub(crate) fn num(v: f64) -> String {
    if v.is_finite() { format!("{v}") } else { "null".to_string() }
}

pub(crate) fn nums(values: &[f64]) -> String {
    let mut out = String::from("[");
    for (k, v) in values.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push_str(&num(*v));
    }
    out.push(']');
    out
}

/// The custom properties of an element as (`--name`, computed value), in the order Stylo holds them: the inherited
/// ones, then those registered not to inherit.
pub(crate) fn custom_properties(st: &ComputedValues) -> Vec<(String, String)> {
    let map = st.custom_properties();
    let mut out = Vec::new();
    let mut index = 0;
    while let Some((name, value)) = map.property_at(index) {
        index += 1;
        // (a name with no value is one whose declaration was invalid at computed-value time)
        if value.is_some() {
            out.push((format!("--{name}"), st.computed_value_to_string(PropertyDeclarationId::Custom(name))));
        }
    }
    out
}

/// The elements a request names. An element is named by `{"i": "<data-i>"}`, `{"n": <node id>}`, `{"id": "<id
/// attribute>"}` or `{"root": true}` (the page's `<html>`); a bare string is a `data-i`, a bare number a node id.
pub(crate) struct Lookup {
    by_i: HashMap<String, NodeId>,
    by_id: HashMap<String, NodeId>,
    root: Option<NodeId>,
}

impl Lookup {
    pub(crate) fn of(doc: &BaseDocument) -> Lookup {
        let (di, idn) = (LocalName::from("data-i"), LocalName::from("id"));
        let (mut by_i, mut by_id) = (HashMap::new(), HashMap::new());
        for id in document_order(doc) {
            let Some(node) = doc.get_node(id) else { continue };
            if !node.is_element() {
                continue;
            }
            if let Some(i) = node.attr(di.clone()) {
                by_i.entry(i.to_string()).or_insert(id);
            }
            if let Some(name) = node.attr(idn.clone()) {
                by_id.entry(name.to_string()).or_insert(id);
            }
        }
        Lookup { by_i, by_id, root: doc.try_root_element().map(|n| n.id) }
    }

    pub(crate) fn find(&self, doc: &BaseDocument, target: &serde_json::Value) -> Result<NodeId, String> {
        use serde_json::Value;
        let by_i = |i: &str| self.by_i.get(i).copied().ok_or_else(|| format!("the page has no element with data-i=\"{i}\""));
        let by_n = |n: u64| {
            let id = NodeId::from_u64(n);
            match doc.get_node(id) {
                Some(node) if node.is_element() && node.flags.is_in_document() => Ok(id),
                _ => Err(format!("the page has no element with node id {n}")),
            }
        };
        match target {
            Value::String(i) => by_i(i),
            Value::Number(n) => by_n(n.as_u64().ok_or("a node id is a whole number")?),
            Value::Object(o) => {
                if let Some(i) = o.get("i") {
                    match i {
                        Value::String(i) => by_i(i),
                        Value::Number(i) => by_i(&i.to_string()),
                        _ => Err("\"i\" is a data-i: a string".into()),
                    }
                } else if let Some(n) = o.get("n") {
                    by_n(n.as_u64().ok_or("\"n\" is a node id: a whole number")?)
                } else if let Some(Value::String(name)) = o.get("id") {
                    self.by_id.get(name).copied().ok_or_else(|| format!("the page has no element with id=\"{name}\""))
                } else if o.get("root") == Some(&Value::Bool(true)) {
                    self.root.ok_or_else(|| "the page has no root element".to_string())
                } else {
                    Err("an element is named by \"i\" (its data-i), \"n\" (its node id), \"id\" or \"root\": true".into())
                }
            }
            _ => Err("an element is named by an object, a data-i string or a node id".into()),
        }
    }
}

/// The nodes of the page in document order: the document, then each node before its children (anonymous boxes the
/// layout made are no part of the DOM and are not listed)
pub(crate) fn document_order(doc: &BaseDocument) -> Vec<NodeId> {
    let mut order = Vec::new();
    let mut stack = vec![doc.root_node().id];
    while let Some(id) = stack.pop() {
        let Some(node) = doc.get_node(id) else { continue };
        order.push(id);
        stack.extend(node.children.iter().rev().copied());
    }
    order
}

fn flag_names(flags: u32) -> Vec<&'static str> {
    [(1, "not-rendered"), (2, "inline"), (4, "transform"), (8, "border-box"), (16, "fixed")]
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// What of an element's boxes a record carries (`"rects"` in a request): none, the border box and its transformed
/// bounds, or every box `fshot boxes` lists
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Rects {
    None,
    Basic,
    Full,
}

/// `"box"`, `"bounds"` and, in full, the rest of an element's boxes, as the members of a JSON object (no braces)
pub(crate) fn boxes_members(doc: &BaseDocument, node: &blitz_dom::Node, b: &ElementBoxes, rects: Rects) -> String {
    let mut out = String::new();
    if rects == Rects::None {
        return out;
    }
    write!(out, "\"box\":{},\"bounds\":{}", nums(&b.rect), nums(&b.bounds)).unwrap();
    if rects == Rects::Full {
        let l = node.unrounded_layout();
        write!(
            out,
            ",\"offset\":{},\"offset_parent\":{},\"client\":{},\"scroll\":{},\"padding\":{},\"border\":{},\"margin\":{},\"location\":{},\"cb\":{}",
            nums(&b.offset),
            b.offset_parent.map_or("null".to_string(), |id| id.as_u64().to_string()),
            nums(&b.client),
            nums(&b.scroll),
            nums(&b.padding),
            nums(&b.border),
            nums(&b.margin),
            nums(&[l.location.x as f64, l.location.y as f64]),
            node.containing_block().filter(|id| doc.get_node(*id).is_some()).map_or("null".to_string(), |id| id.as_u64().to_string()),
        )
        .unwrap();
    }
    out
}

/// What an `analyze` request asks for
pub(crate) struct AnalyzeOptions {
    pub(crate) props: Props,
    pub(crate) used: Props,
    /// only the computed values that differ from the property's initial value (the answer lists the initial values once)
    pub(crate) sparse: bool,
    /// custom properties: "own" (those whose value differs from the parent's), "all" or "none"
    pub(crate) vars: Vars,
    pub(crate) rects: Rects,
    pub(crate) text: bool,
    pub(crate) attrs: bool,
    /// compute the style of an element inside a `display: none` subtree too (Stylo does not, until asked)
    pub(crate) hidden_styles: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Vars {
    None,
    Own,
    All,
}

/// Counts of what a page holds
pub(crate) struct Stats {
    pub(crate) nodes: usize,
    pub(crate) elements: usize,
    pub(crate) text: usize,
    pub(crate) styled: usize,
    pub(crate) stylesheets: usize,
    pub(crate) rules: usize,
}

impl Stats {
    pub(crate) fn of(doc: &BaseDocument) -> Stats {
        let mut stats = Stats { nodes: 0, elements: 0, text: 0, styled: 0, stylesheets: 0, rules: 0 };
        for id in document_order(doc) {
            let Some(node) = doc.get_node(id) else { continue };
            stats.nodes += 1;
            match &node.data {
                NodeData::Element(_) => {
                    stats.elements += 1;
                    if node.primary_styles().is_some() {
                        stats.styled += 1;
                    }
                }
                NodeData::Text(_) => stats.text += 1,
                _ => {}
            }
        }
        for owner in doc.stylesheet_owner_nodes() {
            stats.stylesheets += 1;
            stats.rules += count_rules(doc, owner, &mut Vec::new());
        }
        stats
    }

    pub(crate) fn json(&self) -> String {
        format!(
            "{{\"nodes\":{},\"elements\":{},\"text\":{},\"styled\":{},\"stylesheets\":{},\"rules\":{}}}",
            self.nodes, self.elements, self.text, self.styled, self.stylesheets, self.rules
        )
    }
}

/// The rules of a style sheet at and under `path`, each counted once (a group rule and what is inside it)
fn count_rules(doc: &BaseDocument, owner: NodeId, path: &mut Vec<usize>) -> usize {
    let Some(n) = doc.stylesheet_rule_count(owner, path) else { return 0 };
    let mut total = n;
    if path.len() < 16 {
        for k in 0..n {
            path.push(k);
            total += count_rules(doc, owner, path);
            path.pop();
        }
    }
    total
}

/// The rules at and under `path` as a JSON array: per rule its CSSOM interface (`"type"`), its `cssText`
/// (`"text"`), the interface's own attributes (`selectorText`, `name`, `conditionText`, `keyText`, ...), its
/// declarations as `[name, value, important]` under `"style"` and the rules inside it under `"rules"`.
fn rules_json(doc: &BaseDocument, owner: NodeId, path: &mut Vec<usize>, out: &mut String) {
    out.push('[');
    let n = doc.stylesheet_rule_count(owner, path).unwrap_or(0);
    for k in 0..n {
        path.push(k);
        if k > 0 {
            out.push(',');
        }
        match doc.stylesheet_rule_info(owner, path) {
            Some(info) => {
                write!(out, "{{\"type\":{},\"text\":{}", json_string(info.interface), json_string(&info.css_text)).unwrap();
                for (name, value) in &info.attributes {
                    write!(out, ",{}:{}", json_string(name), json_string(value)).unwrap();
                }
                if info.has_style {
                    out.push_str(",\"style\":[");
                    let names = doc.stylesheet_rule_style_property_names(owner, path).unwrap_or_default();
                    for (j, name) in names.iter().enumerate() {
                        let (value, important) = doc.stylesheet_rule_style_get_property(owner, path, name).unwrap_or_default();
                        if j > 0 {
                            out.push(',');
                        }
                        write!(out, "[{},{},{}]", json_string(name), json_string(&value), important).unwrap();
                    }
                    out.push(']');
                }
                if info.has_child_rules && path.len() < 16 {
                    out.push_str(",\"rules\":");
                    rules_json(doc, owner, path, out);
                }
                out.push('}');
            }
            None => out.push_str("null"),
        }
        path.pop();
    }
    out.push(']');
}

/// The page's author style sheets, in the order the cascade has them: per sheet the node that owns it (a `<style>`
/// or a `<link>`), whether it is disabled, and its rules.
pub(crate) fn sheets_json(doc: &BaseDocument) -> String {
    let mut out = String::from("[");
    for (k, owner) in doc.stylesheet_owner_nodes().enumerate() {
        if k > 0 {
            out.push(',');
        }
        let tag = doc.get_node(owner).and_then(|n| n.element_data()).map_or(String::new(), |e| e.name.local.to_string());
        write!(out, "{{\"owner\":{},\"tag\":{},\"disabled\":{},\"rules\":", owner.as_u64(), json_string(&tag), doc.stylesheet_disabled(owner)).unwrap();
        rules_json(doc, owner, &mut Vec::new(), &mut out);
        out.push('}');
    }
    out.push(']');
    out
}

fn namespace_name(name: &blitz_dom::QualName) -> &'static str {
    if name.ns == ns!(html) {
        "html"
    } else if name.ns == ns!(svg) {
        "svg"
    } else if name.ns == ns!(mathml) {
        "mathml"
    } else {
        "other"
    }
}

/// The elements whose pixels are not the layout engine's own boxes and text: the compiler treats each its own way
const MEDIA_TAGS: &[&str] = &["img", "video", "audio", "canvas", "svg", "iframe", "object", "embed", "picture", "source", "track"];

/// `"cs"`, `"used"` and `"vars"` of one element, as the members of a JSON object (each preceded by a comma)
#[allow(clippy::too_many_arguments)]
fn style_members(
    doc: &BaseDocument,
    id: NodeId,
    st: &ComputedValues,
    parent_vars: Option<&HashMap<String, String>>,
    options: &AnalyzeOptions,
    initial: &[Option<String>],
    styled: bool,
    out: &mut String,
) -> Option<HashMap<String, String>> {
    out.push_str(",\"cs\":{");
    let mut first = true;
    for k in 0..options.props.len() {
        let value = options.props.value(k, doc, id, st);
        if options.sparse && initial[k].as_deref() == Some(value.as_str()) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "{}:{}", json_string(&options.props.names[k]), json_string(&value)).unwrap();
    }
    out.push('}');
    if styled && options.used.len() > 0 {
        out.push_str(",\"used\":{");
        for k in 0..options.used.len() {
            if k > 0 {
                out.push(',');
            }
            write!(out, "{}:{}", json_string(&options.used.names[k]), json_string(&doc.resolved_style_value(id, &options.used.names[k]))).unwrap();
        }
        out.push('}');
    }
    if options.vars == Vars::None {
        return None;
    }
    let vars = custom_properties(st);
    let listed: Vec<&(String, String)> = match (options.vars, parent_vars) {
        (Vars::Own, Some(parent)) => vars.iter().filter(|(name, value)| parent.get(name) != Some(value)).collect(),
        _ => vars.iter().collect(),
    };
    if !listed.is_empty() {
        out.push_str(",\"vars\":{");
        for (k, (name, value)) in listed.iter().enumerate() {
            if k > 0 {
                out.push(',');
            }
            write!(out, "{}:{}", json_string(name), json_string(value)).unwrap();
        }
        out.push('}');
    }
    Some(vars.into_iter().collect())
}

/// The members of an `analyze` answer (no braces): `"viewport"`, `"root"`, `"stats"`, `"props"`, `"used"`,
/// `"unsupported"`, `"initial"` and `"nodes"`.
///
/// `"nodes"` lists the page's nodes in document order. An element:
///   `{"n": node id, "parent": node id, "kind": "element", "ns": "html" | "svg" | "mathml" | "other", "tag": "div",
///     "i": its data-i (when it has one), "attrs": {name: value}, "children": [node ids],
///     "media": "img" | "video" | "canvas" | "svg" | ... (when it is one), "image": [natural w, h] (a decoded <img>),
///     "rendered": false (when it or an ancestor is display: none), "flags": ["inline", "transform", "border-box",
///     "fixed", "stacking"], "box": [x, y, w, h], "bounds": [x0, y0, x1, y1], "cs": {property: computed value},
///     "used": {property: resolved value}, "vars": {"--name": value}}`
/// A text node: `{"n": id, "parent": id, "kind": "text", "text": "..."}`. The document itself is the first record
/// (`"kind": "document"`). Node ids are the layout engine's own and stay the same for as long as the page is open:
/// `resolve` names an element by one, or by its data-i.
pub(crate) fn analyze_members(doc: &BaseDocument, initial_style: &ComputedValues, options: &AnalyzeOptions) -> String {
    let di = LocalName::from("data-i");
    let initial: Vec<Option<String>> = (0..options.props.len()).map(|k| options.props.initial(k, initial_style)).collect();
    let vp = doc.viewport().window_size;
    let mut out = String::new();
    write!(
        out,
        "\"viewport\":[{},{}],\"root\":{},\"stats\":{}",
        vp.0,
        vp.1,
        doc.try_root_element().map_or("null".to_string(), |n| n.id.as_u64().to_string()),
        Stats::of(doc).json()
    )
    .unwrap();
    let list = |names: &[String]| names.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(",");
    write!(out, ",\"props\":[{}],\"used\":[{}]", list(&options.props.names), list(&options.used.names)).unwrap();
    let mut unsupported = options.props.unsupported.clone();
    unsupported.extend(options.used.unsupported.iter().cloned());
    write!(out, ",\"unsupported\":[{}]", list(&unsupported)).unwrap();
    if options.sparse {
        out.push_str(",\"initial\":{");
        let mut first = true;
        for (k, value) in initial.iter().enumerate() {
            let Some(value) = value else { continue };
            if !first {
                out.push(',');
            }
            first = false;
            write!(out, "{}:{}", json_string(&options.props.names[k]), json_string(value)).unwrap();
        }
        out.push('}');
    }
    out.push_str(",\"nodes\":[");
    // (each element's custom properties, kept while its children are written: "own" lists what differs from them)
    let mut vars_of: HashMap<NodeId, HashMap<String, String>> = HashMap::new();
    let mut first = true;
    for id in document_order(doc) {
        let Some(node) = doc.get_node(id) else { continue };
        let parent = node.parent.map_or("null".to_string(), |p| p.as_u64().to_string());
        let n = id.as_u64();
        let mut rec = String::new();
        match &node.data {
            NodeData::Document(_) => {
                write!(rec, "{{\"n\":{n},\"parent\":null,\"kind\":\"document\",\"children\":{}", ids(&node.children)).unwrap();
            }
            NodeData::Text(text) => {
                if !options.text {
                    continue;
                }
                write!(rec, "{{\"n\":{n},\"parent\":{parent},\"kind\":\"text\",\"text\":{}", json_string(&text.content)).unwrap();
            }
            NodeData::Element(el) => {
                let tag = el.name.local.to_string();
                write!(
                    rec,
                    "{{\"n\":{n},\"parent\":{parent},\"kind\":\"element\",\"ns\":\"{}\",\"tag\":{}",
                    namespace_name(&el.name),
                    json_string(&tag)
                )
                .unwrap();
                if let Some(i) = node.attr(di.clone()) {
                    write!(rec, ",\"i\":{}", json_string(i)).unwrap();
                }
                if options.attrs {
                    rec.push_str(",\"attrs\":{");
                    for (k, attr) in el.attrs().iter().enumerate() {
                        if k > 0 {
                            rec.push(',');
                        }
                        let name = match &attr.name.prefix {
                            Some(prefix) => format!("{}:{}", prefix, attr.name.local),
                            None => attr.name.local.to_string(),
                        };
                        write!(rec, "{}:{}", json_string(&name), json_string(&attr.value)).unwrap();
                    }
                    rec.push('}');
                }
                write!(rec, ",\"children\":{}", ids(&node.children)).unwrap();
                if MEDIA_TAGS.contains(&tag.as_str()) {
                    write!(rec, ",\"media\":{}", json_string(&tag)).unwrap();
                }
                if let Some(blitz_dom::node::ImageData::Raster(raster)) = el.image_data() {
                    write!(rec, ",\"image\":[{},{}]", raster.width, raster.height).unwrap();
                }
                let stored = node.primary_styles();
                let parent_vars = node.parent.and_then(|p| vars_of.get(&p));
                match &stored {
                    Some(st) => {
                        let b = element_boxes(doc, id, node, st);
                        if b.flags & 1 != 0 {
                            rec.push_str(",\"rendered\":false");
                        }
                        let mut flags = flag_names(b.flags & !1);
                        if node.stacking_context.is_some() {
                            flags.push("stacking");
                        }
                        if !flags.is_empty() {
                            write!(rec, ",\"flags\":[{}]", flags.iter().map(|f| json_string(f)).collect::<Vec<_>>().join(",")).unwrap();
                        }
                        if b.flags & 1 == 0 && options.rects != Rects::None {
                            write!(rec, ",{}", boxes_members(doc, node, &b, options.rects)).unwrap();
                        }
                        if let Some(vars) = style_members(doc, id, st, parent_vars, options, &initial, true, &mut rec) {
                            vars_of.insert(id, vars);
                        }
                    }
                    None => {
                        // inside a display: none subtree: no box, and no style until one is asked for
                        rec.push_str(",\"rendered\":false");
                        if options.hidden_styles {
                            if let Some(st) = doc.resolve_undisplayed_style(id) {
                                if let Some(vars) = style_members(doc, id, &st, parent_vars, options, &initial, false, &mut rec) {
                                    vars_of.insert(id, vars);
                                }
                            }
                        }
                    }
                }
            }
            // (comments, and the anonymous boxes of the layout, are no part of what a page is to its compiler)
            _ => continue,
        }
        rec.push('}');
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&rec);
    }
    out.push(']');
    out
}

fn ids(list: &[NodeId]) -> String {
    let mut out = String::from("[");
    for (k, id) in list.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        write!(out, "{}", id.as_u64()).unwrap();
    }
    out.push(']');
    out
}

/// What `resolve` reports of one element in one state: the values of `props` (computed), `used` (resolved) and
/// `vars` (custom properties by name), and its boxes
#[derive(Clone, PartialEq)]
pub(crate) struct ElementState {
    /// (shared with the state it was taken from when the element's style is the very same object and the layout
    /// gave no shorthand in the list another value: see `element`)
    pub(crate) cs: Rc<Vec<String>>,
    pub(crate) used: Vec<String>,
    pub(crate) vars: Rc<Vec<String>>,
    pub(crate) rendered: bool,
    pub(crate) boxes: Option<([f64; 4], [f64; 4])>,
}

/// An element's state and the computed style it was read from. The style is held, not just pointed at: while it
/// is held no other style can take its place in memory, so "the same object" below means "nothing was restyled".
pub(crate) type Taken = (ServoArc<ComputedValues>, ElementState);

/// What a `resolve` request asks to be reported
pub(crate) struct Report {
    pub(crate) props: Props,
    pub(crate) used: Props,
    pub(crate) vars: Vec<String>,
    pub(crate) rects: bool,
}

impl Report {
    /// The state of one element as the page is now (resolved); `None` for a node that is no element or has no style.
    ///
    /// `before` is the element as it was taken earlier. When its computed style is still the same object, Stylo
    /// did not restyle it, and its computed values and custom properties are the ones taken then: they are
    /// shared, not serialised again. Its resolved values and boxes are read anew either way (the layout around an
    /// element moves it and resizes it without touching its style), and so is every shorthand in `props`, whose
    /// value is made of resolved ones (`Props::again`: an `auto` margin, a padding in per cent, an inset, the
    /// tracks of a grid). A state that restyles one element out of a thousand is then reported at the cost of
    /// that one.
    pub(crate) fn element(&self, doc: &BaseDocument, id: NodeId, before: Option<&Taken>) -> Option<Taken> {
        let node = doc.get_node(id)?;
        if !node.is_element() {
            return None;
        }
        let st = node.primary_styles()?;
        let b = self.rects.then(|| element_boxes(doc, id, node, &st));
        let rendered = b.as_ref().map_or_else(|| element_boxes(doc, id, node, &st).flags & 1 == 0, |b| b.flags & 1 == 0);
        let (cs, vars) = match before {
            Some((style, state)) if ServoArc::ptr_eq(style, &st) => (self.props.again(doc, id, &state.cs), Rc::clone(&state.vars)),
            _ => (
                Rc::new((0..self.props.len()).map(|k| self.props.value(k, doc, id, &st)).collect()),
                Rc::new(self.vars.iter().map(|name| doc.resolved_style_value(id, name)).collect()),
            ),
        };
        let state = ElementState {
            cs,
            used: self.used.names.iter().map(|name| doc.resolved_style_value(id, name)).collect(),
            vars,
            rendered,
            boxes: b.filter(|b| b.flags & 1 == 0).map(|b| (b.rect, b.bounds)),
        };
        Some((ServoArc::clone(&st), state))
    }

    /// Every styled element of the page, by node id (`before`: the elements as they were taken earlier)
    pub(crate) fn all(&self, doc: &BaseDocument, before: Option<&HashMap<NodeId, Taken>>) -> Vec<(NodeId, Taken)> {
        document_order(doc)
            .into_iter()
            .filter_map(|id| self.element(doc, id, before.and_then(|b| b.get(&id))).map(|s| (id, s)))
            .collect()
    }

    /// One element's record in a `resolve` answer: `{"n": id, "i": data-i, "cs": {...}, "used": {...}, "vars": {...},
    /// "rendered": false, "box": [...], "bounds": [...]}`. With `base` (the element before the state was applied)
    /// only what differs from it is listed.
    pub(crate) fn json(&self, doc: &BaseDocument, id: NodeId, state: &ElementState, base: Option<&ElementState>) -> String {
        let mut out = format!("{{\"n\":{}", id.as_u64());
        if let Some(i) = doc.get_node(id).and_then(|n| n.attr(LocalName::from("data-i"))) {
            write!(out, ",\"i\":{}", json_string(i)).unwrap();
        }
        let group = |out: &mut String, key: &str, names: &[String], values: &[String], before: Option<&Vec<String>>| {
            let listed: Vec<usize> = (0..names.len()).filter(|k| before.is_none_or(|b| b[*k] != values[*k])).collect();
            if listed.is_empty() && (before.is_some() || names.is_empty()) {
                return;
            }
            write!(out, ",\"{key}\":{{").unwrap();
            for (j, k) in listed.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write!(out, "{}:{}", json_string(&names[*k]), json_string(&values[*k])).unwrap();
            }
            out.push('}');
        };
        group(&mut out, "cs", &self.props.names, &state.cs, base.map(|b| &*b.cs));
        group(&mut out, "used", &self.used.names, &state.used, base.map(|b| &b.used));
        group(&mut out, "vars", &self.vars, &state.vars, base.map(|b| &*b.vars));
        if !state.rendered && base.is_none_or(|b| b.rendered) {
            out.push_str(",\"rendered\":false");
        } else if state.rendered && base.is_some_and(|b| !b.rendered) {
            out.push_str(",\"rendered\":true");
        }
        if let Some((rect, bounds)) = &state.boxes {
            if base.is_none_or(|b| b.boxes.as_ref().map(|x| &x.0) != Some(rect)) {
                write!(out, ",\"box\":{}", nums(rect)).unwrap();
            }
            if base.is_none_or(|b| b.boxes.as_ref().map(|x| &x.1) != Some(bounds)) {
                write!(out, ",\"bounds\":{}", nums(bounds)).unwrap();
            }
        }
        out.push('}');
        out
    }
}
