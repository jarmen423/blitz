//! known-future (wave 1a): `fshot serve`, one layout-engine process for a scene.
//!
//! The renderer's compiler ran `fshot` afresh for every question it had of a scene's page: five to seven `fshot
//! dump` runs and a `paintset` run a scene, each starting a process, a rasteriser and the system's fonts before it
//! read the page. `fshot serve` is one process that keeps all three and is asked questions.
//!
//! # The protocol
//!
//! Requests come on stdin and answers go to stdout, one JSON object on one line each, an answer for every request,
//! in order (nothing is answered out of turn, and nothing is said unasked). Nothing else is written to stdout: what
//! the libraries print goes to stderr. End of input ends the process with status 0, as does `shutdown`.
//!
//! A request is `{"id": <anything>, "cmd": "<command>", ...}`. Its answer starts `{"id": <the same>, "ok": true`
//! and goes on with the command's members, or is `{"id": ..., "ok": false, "error": {"code": "<word>", "message":
//! "<sentence>"}}`. A request that fails changes nothing (but `panic`, below). The codes: `bad_request` (not JSON,
//! no such command, a member missing or of the wrong kind), `no_document` (no page is open under that name),
//! `not_found` (no such element), `no_pic_style` (the page has no `<style id="fshot-pic">` to put CSS in),
//! `docroot` (the page is outside FSHOT_DOCROOT), `io` (a file could not be read or written), `too_large` (a
//! picture bigger than 16384 px a side or 2^28 px), `panic` (the layout engine failed on the request: the page it
//! was for is closed, the process goes on).
//!
//! The process holds pages by name: `"doc": "<name>"` in any request says which, and `"main"` is meant when none
//! is given. Pages do not see each other: each has its own DOM, styles, fetches and fonts it registered.
//!
//! ## Opening a page
//!
//! * `open` (also `load`): `{"cmd": "open", "path": "/abs/page.html"}` reads a page file as every other mode
//!   reads it (its `<base href>` or its own address is the base URL), or `{"cmd": "open", "html": "<markup>",
//!   "base_url": "file:///abs/dir/"}` takes the markup itself. `"viewport": [W, H]` is the frame it is laid out in
//!   (1920 x 1080 when not given; a page's own `<meta name="fshot-viewport">` wins, as everywhere). A page already
//!   open under the name is replaced. Answer: `"doc"`, `"viewport": [W, H]`, `"base_url"`, `"stats": {"nodes",
//!   "elements", "text", "styled", "stylesheets", "rules"}`, `"refused": [web addresses not fetched]`,
//!   `"outside": [file addresses outside FSHOT_DOCROOT]`.
//!   A command below that works on a page (`reset`, `dump`, `boxes`, `analyze`, `resolve`, `paint`, `frame`,
//!   `paintset`) may carry `"path"` or `"html"` itself: the page is then opened first, as by `open`, whose answer
//!   is the command's `"opened"` member. Should the command then fail, the page it opened is not kept: the one
//!   that was open under the name still is. No other command opens a page, and one that is not known opens none.
//!   A member that is `null` is a member that is not given, here as everywhere.
//! * `close`: forget the page (`"all": true`: every page). Answer: `"closed": [names]`.
//! * `reset`: put the page back as it was loaded, undoing a state `resolve` kept; `"reload": true` builds it
//!   again from its source instead. Answer: `"reset": true`.
//!
//! ## Asking
//!
//! * `dump`: the text `fshot dump` writes for the page, byte for byte: as the answer's `"dump"` member (that
//!   text is one JSON value), or written to `"out": "/abs/file.json"` (answer: `"out"`, `"bytes"`).
//! * `boxes`: the same for `fshot boxes` (`"boxes"`, or `"out"`).
//! * `analyze`: the page as data (analyze.rs says what each member is). Options: `"props": [names] | "all"` (the
//!   computed values to list; a default list when not given), `"used": [names] | false` (resolved values),
//!   `"sparse": false` (list values equal to the initial value too), `"vars": "own" | "all" | "none"`,
//!   `"rects": true | "full" | false`, `"text": false`, `"attrs": false`, `"hidden_styles": true`,
//!   `"rules": true` (adds `"sheets"`, the rules of the style sheets), `"paint": true` (adds `"paint"`, `"svg"`
//!   and `"svgparts"`: the paint order `dump` gives, by node id).
//! * `resolve`: what the page computes to under changes, without keeping them. `"states": [state, ...]`; each
//!   state is applied to the page as loaded, the styles and layout are worked out again, the report is taken and
//!   the page is put back. A state: `{"vars": {"--name": "value"}` (custom properties on the root element),
//!   `"set": [{<element>, "style": {"property": "value" | null}, "attrs": {"name": "value" | null}}]`, `"css":
//!   "text for <style id=fshot-pic>"}`. An element is `"i": "<data-i>"`, `"n": <node id>`, `"id": "<id>"` or
//!   `"root": true`. A style property is set as `element.style.setProperty()` sets it (a value may end in
//!   `!important`; `null` removes it); a value the style engine refuses changes nothing and is listed.
//!   What is reported: `"props"`, `"used"`, `"vars": ["--name"]` and `"rects": true` choose the values;
//!   `"nodes": "changed"` (the default: the elements any reported value changed on), `"all"`, or a list of
//!   elements; `"diff"` (true by default with `"changed"`) lists only the values that changed.
//!   Answer: `"props"`, `"used"`, `"vars"`, `"unsupported"`, `"states": [{"nodes": [...], "invalid": [...]}]`.
//!   `"keep": true` with one state leaves it applied, for `paint`, `frame`, `dump` or `analyze` of that state;
//!   the next `resolve`, `paintset`, `reset` or `open` undoes it.
//!
//! ## Painting (Skia, and nothing else: `serve` does not start with another rasteriser)
//!
//! * `paint`: `{"cmd": "paint", "w": W, "h": H, "out": "/abs/pic.png"}` writes what `fshot paint` writes for
//!   the page: a W x H picture on a transparent background, a PNG, or the raw RGBA bytes when `out` ends in `.rgba`.
//! * `frame`: `{"cmd": "frame", "out": "/abs/frame.png"}` writes what `fshot W H page.html frame.png` writes: the
//!   page on white, a PNG, W x H being the `"viewport"` the page was opened with (or the request's `"w"`, `"h"`).
//! * `paintset`: `{"cmd": "paintset", "jobs": "/abs/setjobs.txt"}` paints the pictures of a `fshot paintset` job
//!   file from the open page, or `"records": [{"out", "w", "h", "css", "attrs": [[data-i, name, value]]}]` gives
//!   them in the request. The page is put back afterwards. Answer: `"pictures": [{"out", "w", "h", "bytes"}]`.
//!
//! ## The rest
//!
//! * `ping`: `"pong": true, "protocol": 1, "backend": "skia"`.
//! * `describe`: the commands, and the default lists of `props` and `used`.
//! * `shutdown`: answer, then end.
//!
//! `"timing": true` in a request adds `"ms"` to its answer: wall-clock milliseconds by part. Answers are otherwise
//! the same for the same requests, whenever and however often they are made.
//!
//! # The replay log
//!
//! With `FSHOT_REPLAY_LOG=/abs/file.jsonl` every exchange is recorded there, a line of JSON each, written as it
//! happens: first `{"fshot": "serve", "protocol": 1, "backend": "skia", "net": "local" | "open", "docroot":
//! [folders] | null}`, then for each request `{"seq": N, "req": <the request>}` (`"req_text": "<the line>"` for a
//! line that was not JSON) and `{"seq": N, "res": <the answer>}`. The file is made anew by each process. No time,
//! process id or address of the moment is in it, so the same requests give the same file: a test, or a tool in
//! Python or JavaScript, answers a compiler's questions from the file without running the layout engine. A record
//! is one line to every reader of lines: a request is in the log to the letter, but that a carriage return between
//! two of its values is written as a space, and U+0085, U+2028 and U+2029 in a string as `\u0085`, `\u2028` and
//! `\u2029` (the same JSON; Python's `splitlines()` ends a line at each of them). Every
//! file an answer says was written is named with its size and, while a log is kept (or when a request says
//! `"digest": true`), its SHA-256 (`"sha256"`), so a replay knows the picture on disk is the one the answer meant.
//!
//! # What is kept between requests, and what is not
//!
//! Kept: the Skia rasteriser and its caches of typefaces and pictures; the system's fonts (each page starts from
//! a copy of one collection read once; `FSHOT_SERVE_FRESH_FONTS=1` reads them anew for every page, as a one-page
//! run does); the font database of SVG text; the runtime the fetches run on. Not kept: anything of a page. A
//! page's fonts, style sheets, pictures and fetch guard are its own, and die with it.
//!
//! The guards are those of every mode: FSHOT_NET (a page's web addresses are refused and listed, not fetched) and
//! FSHOT_DOCROOT (main.rs: a page opened by `path`, and every file read for a page, must be inside it).

use super::analyze::{self, AnalyzeOptions, Lookup, Props, Rects, Report, Stats, Taken, Vars};
use super::*;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead as _, Write as _};

const PROTOCOL: u32 = 1;
const DEFAULT_DOC: &str = "main";
const COMMANDS: &[&str] = &[
    "ping", "describe", "open", "load", "close", "reset", "dump", "boxes", "analyze", "resolve", "paint", "frame", "paintset", "shutdown",
];
/// A picture's side, and its area, at most (a request for more is a mistake, not a picture)
const MAX_SIDE: u32 = 16384;
const MAX_AREA: u64 = 1 << 28;

/// A request that was not carried out: a code for a program and a sentence for a person
struct Fail {
    code: &'static str,
    message: String,
}

fn fail<T>(code: &'static str, message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail { code, message: message.into() })
}

fn bad<T>(message: impl Into<String>) -> Result<T, Fail> {
    fail("bad_request", message)
}

/// Where a page came from, to load it again
struct Source {
    html: String,
    url: String,
    label: String,
    viewport: (u32, u32),
}

/// A page held open: its document, and what has been changed in it since it was loaded
struct Session {
    document: HtmlDocument,
    source: Source,
    refused: Vec<String>,
    outside: Vec<String>,
    /// the page's elements by data-i and id (node ids do not change while the page is open)
    lookup: Option<Lookup>,
    /// what `paintset` and a state's `"css"` change (`None` inside: the page has no <style id="fshot-pic">)
    pic: Option<Option<PaintsetPage>>,
    /// attributes changed since the page was as loaded, each with the value it had (`None`: it had none)
    originals: Vec<(NodeId, QualName, Option<String>)>,
    /// the text of <style id="fshot-pic"> as loaded, once it has been replaced
    pic_css: Option<String>,
}

impl Session {
    fn lookup(&mut self) -> &Lookup {
        if self.lookup.is_none() {
            self.lookup = Some(Lookup::of(self.document.as_ref()));
        }
        self.lookup.as_ref().unwrap()
    }

    fn pic(&mut self) -> Result<&PaintsetPage, Fail> {
        if self.pic.is_none() {
            self.pic = Some(PaintsetPage::of(self.document.as_ref()));
        }
        match self.pic.as_ref().unwrap() {
            Some(page) => Ok(page),
            None => fail("no_pic_style", "the page has no <style id=\"fshot-pic\">/**/</style> to put a picture's CSS in"),
        }
    }

    /// Remember what an attribute was before it is first changed
    fn remember(&mut self, id: NodeId, name: &str) -> QualName {
        let q = QualName::new(None, ns!(), LocalName::from(name));
        if !self.originals.iter().any(|(i, n, _)| *i == id && *n == q) {
            let was = self.document.as_ref().get_node(id).and_then(|n| n.attr(LocalName::from(name))).map(str::to_string);
            self.originals.push((id, q.clone(), was));
        }
        q
    }

    /// Set (or with `None` remove) an attribute, remembering what it was
    fn set_attr(&mut self, id: NodeId, name: &str, value: Option<&str>) {
        let q = self.remember(id, name);
        let mut m = self.document.as_mut().mutate();
        match value {
            Some(value) => m.set_attribute(id, q, value),
            None => m.clear_attribute(id, q),
        }
    }

    /// Replace the text of <style id="fshot-pic">, remembering what it was
    fn set_pic_css(&mut self, css: &str) -> Result<(), Fail> {
        let style_text = self.pic()?.style_text;
        if self.pic_css.is_none() {
            let was = self.document.as_ref().get_node(style_text).and_then(|n| n.text_data()).map(|t| t.content.clone());
            self.pic_css = Some(was.unwrap_or_default());
        }
        self.document.as_mut().mutate().set_node_text(style_text, css);
        Ok(())
    }

    fn changed(&self) -> bool {
        !self.originals.is_empty() || self.pic_css.is_some()
    }

    /// Put every changed attribute and the picture CSS back as loaded (the styles are not worked out again here)
    fn undo(&mut self) {
        let originals = std::mem::take(&mut self.originals);
        let css = self.pic_css.take();
        let style_text = self.pic.as_ref().and_then(|p| p.as_ref()).map(|p| p.style_text);
        let mut m = self.document.as_mut().mutate();
        for (id, q, was) in originals.into_iter().rev() {
            match was {
                Some(value) => m.set_attribute(id, q, &value),
                None => m.clear_attribute(id, q),
            }
        }
        if let (Some(css), Some(style_text)) = (css, style_text) {
            m.set_node_text(style_text, &css);
        }
    }

    /// The page as loaded again, styles and layout worked out
    fn restore(&mut self) {
        if self.changed() {
            self.undo();
            self.document.as_mut().resolve(0.0);
        }
    }

    /// Apply one state of a `resolve` request (not yet resolved). The declarations the style engine refused are
    /// returned as JSON records; a state that names an element the page has not changes nothing.
    fn apply_state(&mut self, state: &Value) -> Result<Vec<String>, Fail> {
        let Some(state) = state.as_object() else { return bad("a state is an object: {\"vars\": ..., \"set\": ..., \"css\": ...}") };
        if let Some(unknown) = state.keys().find(|k| !matches!(k.as_str(), "vars" | "set" | "css")) {
            return bad(format!("a state has \"vars\", \"set\" and \"css\"; \"{unknown}\" is none of them"));
        }
        // every element is found, and every member checked, before anything is changed
        let mut changes: Vec<(NodeId, Vec<(String, Option<String>)>, Vec<(String, Option<String>)>)> = Vec::new();
        let pairs = |what: &str, v: &Value| -> Result<Vec<(String, Option<String>)>, Fail> {
            let Some(o) = v.as_object() else { return bad(format!("\"{what}\" is an object of names and values")) };
            o.iter()
                .map(|(name, value)| match value {
                    Value::Null => Ok((name.clone(), None)),
                    Value::String(s) => Ok((name.clone(), Some(s.clone()))),
                    Value::Number(n) => Ok((name.clone(), Some(n.to_string()))),
                    _ => bad(format!("\"{what}\": the value of \"{name}\" is a string, a number or null")),
                })
                .collect()
        };
        if let Some(vars) = state.get("vars") {
            let vars = pairs("vars", vars)?;
            if let Some((name, _)) = vars.iter().find(|(name, _)| !name.starts_with("--")) {
                return bad(format!("\"vars\" sets custom properties: \"{name}\" does not start with \"--\""));
            }
            let Some(root) = self.document.as_ref().try_root_element().map(|n| n.id) else {
                return fail("not_found", "the page has no root element");
            };
            changes.push((root, vars, Vec::new()));
        }
        if let Some(set) = state.get("set") {
            let Some(set) = set.as_array() else { return bad("\"set\" is a list of {<element>, \"style\": ..., \"attrs\": ...}") };
            for entry in set {
                let id = {
                    let (document, lookup) = (&self.document, self.lookup.get_or_insert_with(|| Lookup::of(self.document.as_ref())));
                    lookup.find(document.as_ref(), entry).map_err(|m| Fail { code: "not_found", message: m })?
                };
                let style = entry.get("style").map(|v| pairs("style", v)).transpose()?.unwrap_or_default();
                let attrs = entry.get("attrs").map(|v| pairs("attrs", v)).transpose()?.unwrap_or_default();
                if attrs.iter().any(|(name, _)| name == "style") && !style.is_empty() {
                    return bad("an element's \"attrs\" sets its style attribute and its \"style\" sets properties in it: give one");
                }
                changes.push((id, style, attrs));
            }
        }
        let css = match state.get("css") {
            None => None,
            Some(Value::String(css)) => {
                self.pic()?;
                Some(css.as_str())
            }
            Some(_) => return bad("\"css\" is a string"),
        };

        let mut invalid = Vec::new();
        for (id, style, attrs) in changes {
            for (name, value) in &attrs {
                self.set_attr(id, name, value.as_deref());
            }
            if style.is_empty() {
                continue;
            }
            let doc = self.document.as_ref();
            let mut text = doc.get_node(id).and_then(|n| n.attr(LocalName::from("style"))).unwrap_or("").to_string();
            for (property, value) in &style {
                let next = match value.as_deref().map(str::trim) {
                    None | Some("") => doc.style_attr_remove_property(&text, property).map(|(text, _)| text),
                    Some(value) => {
                        let (value, important) = split_important(value);
                        doc.style_attr_set_property(&text, property, value, important)
                    }
                };
                match next {
                    Some(next) => text = next,
                    None => invalid.push(format!(
                        "{{\"n\":{},\"property\":{},\"value\":{}}}",
                        id.as_u64(),
                        json_string(property),
                        value.as_deref().map_or("null".to_string(), json_string)
                    )),
                }
            }
            self.set_attr(id, "style", Some(&text));
        }
        if let Some(css) = css {
            self.set_pic_css(css)?;
        }
        Ok(invalid)
    }
}

/// The system's fonts, read once: each page starts from a copy (`FSHOT_SERVE_FRESH_FONTS=1`: from its own reading
/// of them). Two collections, as a page either asks for Chrome's generic families or does not.
struct Fonts {
    fresh: bool,
    /// BLITZ_CHROME_GENERICS as the process was started with it: every page has them then, as in a one-page run
    always_chrome: bool,
    plain: Option<blitz_dom::FontContext>,
    chrome: Option<blitz_dom::FontContext>,
}

impl Fonts {
    fn new() -> Self {
        Fonts {
            fresh: std::env::var("FSHOT_SERVE_FRESH_FONTS").as_deref() == Ok("1"),
            always_chrome: std::env::var("BLITZ_CHROME_GENERICS").as_deref() == Ok("1"),
            plain: None,
            chrome: None,
        }
    }

    /// The font collection a page starts from: with Chrome's generic families when the page asks for them (or
    /// the process was started with BLITZ_CHROME_GENERICS=1), as a one-page run of it has them. Which it is, the
    /// page is told by the collection it is handed and never through the environment: that is the whole
    /// process's, the runtime's threads run beside this one, and it is not changed under them.
    fn for_page(&mut self, html: &str) -> blitz_dom::FontContext {
        let chrome = self.always_chrome || page_wants_chrome_generics(html);
        let read = || blitz_dom::BaseDocument::default_font_context_with_chrome_generics(chrome);
        if self.fresh {
            return read();
        }
        let slot = if chrome { &mut self.chrome } else { &mut self.plain };
        slot.get_or_insert_with(read).clone()
    }
}

/// A declaration's value without its `!important`, and whether it had one. CSS reads the word in any case and
/// with white space between the `!` and it: `!IMPORTANT` and `! important` are `!important`.
fn split_important(value: &str) -> (&str, bool) {
    const WORD: &str = "important";
    let space = |c: char| c.is_ascii_whitespace();
    let trimmed = value.trim_end_matches(space);
    if let Some(at) = trimmed.len().checked_sub(WORD.len()).filter(|at| trimmed.is_char_boundary(*at)) {
        if trimmed[at..].eq_ignore_ascii_case(WORD) {
            if let Some(rest) = trimmed[..at].trim_end_matches(space).strip_suffix('!') {
                return (rest.trim_end_matches(space), true);
            }
        }
    }
    (value, false)
}

/// A line of JSON as one line to every reader of lines. A carriage return, U+0085, U+2028 and U+2029 end a line
/// to some of them (Python's `splitlines()`, and its files read as text). In JSON the first is white space between
/// two values and the others are found only inside a string, so a space and the escapes `\u0085`, `\u2028` and
/// `\u2029` say the same.
fn one_line(json: &str) -> std::borrow::Cow<'_, str> {
    if !json.contains(['\r', '\u{85}', '\u{2028}', '\u{2029}']) {
        return json.into();
    }
    let mut out = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        match c {
            '\r' => out.push(' '),
            '\u{85}' | '\u{2028}' | '\u{2029}' => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out.into()
}

/// FSHOT_REPLAY_LOG: every request and answer, a line each (the module documentation has the format)
struct ReplayLog {
    file: std::io::BufWriter<File>,
    seq: u64,
}

impl ReplayLog {
    fn from_env() -> Option<ReplayLog> {
        let path = std::env::var_os("FSHOT_REPLAY_LOG").filter(|p| !p.is_empty())?;
        let file = match File::create(&path) {
            Ok(file) => file,
            Err(e) => backend_error(format!("FSHOT_REPLAY_LOG names {}, which cannot be written: {e}", Path::new(&path).display())),
        };
        let docroot = match Docroot::from_env() {
            Some(d) => format!("[{}]", d.roots.iter().map(|r| json_string(&r.to_string_lossy())).collect::<Vec<_>>().join(",")),
            None => "null".to_string(),
        };
        let mut log = ReplayLog { file: std::io::BufWriter::new(file), seq: 0 };
        log.line(&format!(
            "{{\"fshot\":\"serve\",\"protocol\":{PROTOCOL},\"backend\":\"skia\",\"net\":\"{}\",\"docroot\":{docroot}}}",
            if net_is_open() { "open" } else { "local" }
        ));
        Some(log)
    }

    fn line(&mut self, line: &str) {
        let line = one_line(line);
        // (a log that cannot be written is said once on stderr and does not stop the scene)
        if writeln!(self.file, "{line}").and_then(|()| self.file.flush()).is_err() {
            eprintln!("warning: FSHOT_REPLAY_LOG could not be written to");
        }
    }

    fn request(&mut self, line: &str, is_json: bool) {
        self.seq += 1;
        let seq = self.seq;
        if is_json {
            self.line(&format!("{{\"seq\":{seq},\"req\":{}}}", line.trim()));
        } else {
            self.line(&format!("{{\"seq\":{seq},\"req_text\":{}}}", json_string(line.trim_end_matches(['\r', '\n']))));
        }
    }

    fn response(&mut self, line: &str) {
        let seq = self.seq;
        self.line(&format!("{{\"seq\":{seq},\"res\":{line}}}"));
    }
}

struct Server {
    painter: Painter,
    sessions: HashMap<String, Session>,
    fonts: Fonts,
    /// give the SHA-256 of every file written (a replay log is kept)
    digests: bool,
}

fn get_str<'a>(req: &'a Value, key: &str) -> Result<Option<&'a str>, Fail> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => bad(format!("\"{key}\" is a string")),
    }
}

fn get_bool(req: &Value, key: &str, default: bool) -> Result<bool, Fail> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => bad(format!("\"{key}\" is true or false")),
    }
}

fn get_u32(req: &Value, key: &str) -> Result<Option<u32>, Fail> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64().filter(|n| *n <= u32::MAX as u64) {
            Some(n) => Ok(Some(n as u32)),
            None => bad(format!("\"{key}\" is a whole number")),
        },
    }
}

fn names_of<'a>(req: &'a Value, key: &str) -> Result<Option<Vec<&'a str>>, Fail> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| v.as_str().ok_or(()))
            .collect::<Result<Vec<_>, ()>>()
            .map(Some)
            .or_else(|()| bad(format!("\"{key}\" is a list of names"))),
        Some(_) => bad(format!("\"{key}\" is a list of names")),
    }
}

fn picture_size(w: u32, h: u32) -> Result<(), Fail> {
    if w == 0 || h == 0 {
        return bad("a picture is at least 1 px a side");
    }
    if w > MAX_SIDE || h > MAX_SIDE || (w as u64) * (h as u64) > MAX_AREA {
        return fail("too_large", format!("a {w} x {h} picture is more than {MAX_SIDE} px a side or 2^28 px"));
    }
    Ok(())
}

fn ms(t: Instant) -> String {
    format!("{:.3}", t.elapsed().as_secs_f64() * 1000.0)
}

impl Server {
    fn doc_name(req: &Value) -> Result<&str, Fail> {
        Ok(get_str(req, "doc")?.unwrap_or(DEFAULT_DOC))
    }

    fn session<'a>(sessions: &'a mut HashMap<String, Session>, req: &Value) -> Result<&'a mut Session, Fail> {
        let name = Self::doc_name(req)?;
        match sessions.get_mut(name) {
            Some(session) => Ok(session),
            None => fail("no_document", format!("no page is open as \"{name}\": open one first (or give \"path\" or \"html\" with the request)")),
        }
    }

    /// A file an answer says was written: its name, its size and (when digests are given) its SHA-256
    fn written(&self, req: &Value, out: &str, bytes: &[u8]) -> Result<String, Fail> {
        if let Err(e) = std::fs::write(out, bytes) {
            return fail("io", format!("{out} cannot be written: {e}"));
        }
        let mut members = format!("\"out\":{},\"bytes\":{}", json_string(out), bytes.len());
        if self.digests || get_bool(req, "digest", false)? {
            write!(members, ",\"sha256\":\"{}\"", sha256::hex(bytes)).unwrap();
        }
        Ok(members)
    }

    fn load_source(&mut self, source: Source) -> Session {
        let fonts = self.fonts.for_page(&source.html);
        let loaded = load_html(&source.html, source.url.clone(), &source.label, source.viewport.0, source.viewport.1, Some(fonts));
        loaded.report();
        Session {
            document: loaded.document,
            source,
            refused: loaded.refused,
            outside: loaded.outside,
            lookup: None,
            pic: None,
            originals: Vec::new(),
            pic_css: None,
        }
    }

    fn open(&mut self, req: &Value) -> Result<String, Fail> {
        let name = Self::doc_name(req)?.to_string();
        let viewport = match req.get("viewport") {
            None | Some(Value::Null) => (VW, VH),
            Some(v) => {
                let side = |k: usize| v.get(k).and_then(Value::as_u64).filter(|n| *n >= 1 && *n <= MAX_SIDE as u64).map(|n| n as u32);
                match (v.as_array().map(Vec::len), side(0), side(1)) {
                    (Some(2), Some(w), Some(h)) => (w, h),
                    _ => return bad(format!("\"viewport\" is [width, height], each from 1 to {MAX_SIDE}")),
                }
            }
        };
        let source = match (get_str(req, "path")?, get_str(req, "html")?) {
            (Some(_), Some(_)) => return bad("give \"path\" or \"html\", not both"),
            (None, None) => return bad("open needs \"path\" (a page file) or \"html\" (the markup itself)"),
            (Some(input), None) => {
                let path = match std::fs::canonicalize(input) {
                    Ok(path) => path,
                    Err(e) => return fail("io", format!("{input} cannot be read: {e}")),
                };
                if let Err(message) = docroot_allows_page(&path) {
                    return fail("docroot", message);
                }
                let html = match read_page(&path) {
                    Ok(html) => html,
                    Err(PageError::Outside(message)) => return fail("docroot", message),
                    Err(PageError::Io(e)) => return fail("io", format!("{input} cannot be read: {e}")),
                };
                let own = match url::Url::from_file_path(&path) {
                    Ok(url) => url.to_string(),
                    Err(()) => return fail("io", format!("{input} has no file: address")),
                };
                let url = page_base_url(&html, || own);
                Source { html, url, label: input.to_string(), viewport }
            }
            (None, Some(html)) => {
                let given = get_str(req, "base_url")?.unwrap_or("about:blank").to_string();
                let url = page_base_url(html, || given);
                Source { html: html.to_string(), url, label: "<markup>".to_string(), viewport }
            }
        };
        let t0 = Instant::now();
        // (the page that was open under the name is gone before the new one loads: nothing of it is there to meet)
        self.sessions.remove(&name);
        let session = self.load_source(source);
        let doc = session.document.as_ref();
        let vp = doc.viewport().window_size;
        let list = |urls: &[String]| urls.iter().map(|u| json_string(u)).collect::<Vec<_>>().join(",");
        let mut members = format!(
            "\"doc\":{},\"viewport\":[{},{}],\"base_url\":{},\"stats\":{},\"refused\":[{}],\"outside\":[{}]",
            json_string(&name),
            vp.0,
            vp.1,
            json_string(&session.source.url),
            Stats::of(doc).json(),
            list(&session.refused),
            list(&session.outside)
        );
        if get_bool(req, "timing", false)? {
            write!(members, ",\"ms\":{{\"load\":{}}}", ms(t0)).unwrap();
        }
        self.sessions.insert(name, session);
        Ok(members)
    }

    fn close(&mut self, req: &Value) -> Result<String, Fail> {
        let mut closed: Vec<String> = if get_bool(req, "all", false)? {
            self.sessions.drain().map(|(name, _)| name).collect()
        } else {
            let name = Self::doc_name(req)?;
            self.sessions.remove(name).map(|_| name.to_string()).into_iter().collect()
        };
        closed.sort();
        Ok(format!("\"closed\":[{}]", closed.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(",")))
    }

    fn reset(&mut self, req: &Value) -> Result<String, Fail> {
        if get_bool(req, "reload", false)? {
            let name = Self::doc_name(req)?.to_string();
            let Some(old) = self.sessions.remove(&name) else {
                return fail("no_document", format!("no page is open as \"{name}\""));
            };
            let session = self.load_source(old.source);
            self.sessions.insert(name, session);
        } else {
            Self::session(&mut self.sessions, req)?.restore();
        }
        Ok("\"reset\":true".to_string())
    }

    /// `dump` and `boxes`: the text of the cold mode, in the answer or in a file
    fn text_answer(&mut self, req: &Value, key: &str) -> Result<String, Fail> {
        let t0 = Instant::now();
        let session = Self::session(&mut self.sessions, req)?;
        let text = if key == "dump" { dump_json(&mut session.document, &mut self.painter) } else { boxes_json(session.document.as_ref()) };
        let mut members = match get_str(req, "out")? {
            Some(out) => self.written(req, out, text.as_bytes())?,
            None => format!("\"{key}\":{text}"),
        };
        if get_bool(req, "timing", false)? {
            write!(members, ",\"ms\":{{\"{key}\":{}}}", ms(t0)).unwrap();
        }
        Ok(members)
    }

    fn paint(&mut self, req: &Value, frame: bool) -> Result<String, Fail> {
        let Some(out) = get_str(req, "out")? else { return bad("\"out\" names the file the picture is written to") };
        let session = Self::session(&mut self.sessions, req)?;
        // (the frame the page was opened with: `fshot W H` paints W x H whatever size the page lays itself out at)
        let (w, h) = match (get_u32(req, "w")?, get_u32(req, "h")?) {
            (Some(w), Some(h)) => (w, h),
            (None, None) if frame => session.source.viewport,
            _ if frame => return bad("\"w\" and \"h\" go together (without them a frame is the size the page was opened with)"),
            _ => return bad("\"w\" and \"h\" are the picture's size"),
        };
        picture_size(w, h)?;
        let t0 = Instant::now();
        let buf = if frame { self.painter.frame(&mut session.document, w, h) } else { self.painter.picture(&mut session.document, w, h) };
        let t_paint = ms(t0);
        let t1 = Instant::now();
        // (a frame is always a PNG, as `fshot W H` writes it; a picture is raw RGBA under a `.rgba` name, as `fshot paint`)
        let members = if !frame && out.ends_with(".rgba") { self.written(req, out, &buf)? } else { self.written(req, out, &png_bytes(&buf, w, h))? };
        let mut members = format!("{members},\"w\":{w},\"h\":{h}");
        if get_bool(req, "timing", false)? {
            write!(members, ",\"ms\":{{\"paint\":{t_paint},\"write\":{}}}", ms(t1)).unwrap();
        }
        Ok(members)
    }

    fn paintset(&mut self, req: &Value) -> Result<String, Fail> {
        let jobs_text;
        let records_json;
        let records: Vec<PaintsetRecord> = match (get_str(req, "jobs")?, req.get("records")) {
            (Some(_), Some(_)) => return bad("give \"jobs\" (a job file) or \"records\", not both"),
            (Some(path), None) => {
                jobs_text = match std::fs::read_to_string(path) {
                    Ok(text) => text,
                    Err(e) => return fail("io", format!("{path} cannot be read: {e}")),
                };
                paintset_records(&jobs_text).collect::<Result<Vec<_>, String>>().map_err(|m| Fail { code: "bad_request", message: format!("{path}: {m}") })?
            }
            (None, Some(Value::Array(list))) => {
                records_json = list;
                let mut records = Vec::new();
                for rec in records_json {
                    let (Some(out), Some(w), Some(h)) = (get_str(rec, "out")?, get_u32(rec, "w")?, get_u32(rec, "h")?) else {
                        return bad("a record has \"out\", \"w\" and \"h\"");
                    };
                    let mut attrs = Vec::new();
                    for attr in rec.get("attrs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
                        match (attr.get(0).and_then(Value::as_str), attr.get(1).and_then(Value::as_str), attr.get(2).and_then(Value::as_str)) {
                            (Some(i), Some(name), Some(value)) => attrs.push((i, name, value)),
                            _ => return bad("a record's \"attrs\" is a list of [data-i, name, value], each a string"),
                        }
                    }
                    records.push(PaintsetRecord { output: out, w, h, attrs, css: get_str(rec, "css")?.unwrap_or("/**/") });
                }
                records
            }
            _ => return bad("paintset needs \"jobs\" (a job file) or \"records\" (a list)"),
        };
        for rec in &records {
            picture_size(rec.w, rec.h)?;
        }
        let session = Self::session(&mut self.sessions, req)?;
        // from the page as loaded, as the cold mode starts from it
        session.restore();
        let page_ids: HashMap<String, NodeId> = session.pic()?.by_i.clone();
        // (every record is checked against the page before the first picture is painted)
        for rec in &records {
            if let Some((i, _, _)) = rec.attrs.iter().find(|(i, _, _)| !page_ids.contains_key(*i)) {
                return fail("not_found", format!("the page has no element with data-i=\"{i}\""));
            }
        }
        let timing = get_bool(req, "timing", false)?;
        let (mut t_restyle, mut t_paint, mut t_write) = (0f64, 0f64, 0f64);
        let mut pictures = Vec::new();
        let mut failure = None;
        for rec in &records {
            let t0 = Instant::now();
            // what the cold mode changes, changed as it changes it, with what was there remembered for the end
            for (i, attr, _) in &rec.attrs {
                session.remember(page_ids[*i], attr);
            }
            session.set_pic_css(rec.css)?;
            let page = session.pic.as_ref().and_then(|p| p.as_ref()).unwrap();
            let restore = match page.apply(&mut session.document, rec) {
                Ok(restore) => restore,
                Err(message) => {
                    failure = Some(Fail { code: "not_found", message });
                    break;
                }
            };
            session.document.as_mut().resolve(0.0);
            t_restyle += t0.elapsed().as_secs_f64();
            let t1 = Instant::now();
            let buf = self.painter.picture(&mut session.document, rec.w, rec.h);
            t_paint += t1.elapsed().as_secs_f64();
            let t2 = Instant::now();
            let written = if rec.output.ends_with(".rgba") {
                Self::written_with(self.digests || get_bool(req, "digest", false)?, rec.output, &buf)
            } else {
                Self::written_with(self.digests || get_bool(req, "digest", false)?, rec.output, &png_bytes(&buf, rec.w, rec.h))
            };
            t_write += t2.elapsed().as_secs_f64();
            PaintsetPage::clear(&mut session.document, restore);
            match written {
                Ok(members) => pictures.push(format!("{{{members},\"w\":{},\"h\":{}}}", rec.w, rec.h)),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        // the page as loaded again, whatever happened
        session.restore();
        if let Some(e) = failure {
            return Err(e);
        }
        let mut members = format!("\"pictures\":[{}]", pictures.join(","));
        if timing {
            write!(members, ",\"ms\":{{\"restyle\":{:.3},\"paint\":{:.3},\"write\":{:.3}}}", t_restyle * 1000.0, t_paint * 1000.0, t_write * 1000.0).unwrap();
        }
        Ok(members)
    }

    fn written_with(digest: bool, out: &str, bytes: &[u8]) -> Result<String, Fail> {
        if let Err(e) = std::fs::write(out, bytes) {
            return fail("io", format!("{out} cannot be written: {e}"));
        }
        let mut members = format!("\"out\":{},\"bytes\":{}", json_string(out), bytes.len());
        if digest {
            write!(members, ",\"sha256\":\"{}\"", sha256::hex(bytes)).unwrap();
        }
        Ok(members)
    }

    /// The `props` and `used` lists of a request (`"all"`: every longhand; not given: the default list)
    fn props_of(req: &Value, key: &str, default: &[&str]) -> Result<Props, Fail> {
        match req.get(key) {
            None | Some(Value::Null) | Some(Value::Bool(true)) => Ok(Props::parse(default.iter().copied())),
            Some(Value::Bool(false)) => Ok(Props::parse([])),
            Some(Value::String(s)) if s == "all" => Ok(Props::all()),
            Some(_) => Ok(Props::parse(names_of(req, key)?.unwrap_or_default())),
        }
    }

    fn analyze(&mut self, req: &Value) -> Result<String, Fail> {
        let options = AnalyzeOptions {
            props: Self::props_of(req, "props", analyze::DEFAULT_PROPS)?,
            used: Self::props_of(req, "used", analyze::DEFAULT_USED)?,
            sparse: get_bool(req, "sparse", true)?,
            vars: match get_str(req, "vars")?.unwrap_or("own") {
                "own" => Vars::Own,
                "all" => Vars::All,
                "none" => Vars::None,
                _ => return bad("\"vars\" is \"own\", \"all\" or \"none\""),
            },
            rects: match req.get("rects") {
                None | Some(Value::Null) | Some(Value::Bool(true)) => Rects::Basic,
                Some(Value::Bool(false)) => Rects::None,
                Some(Value::String(s)) if s == "full" => Rects::Full,
                Some(_) => return bad("\"rects\" is true, false or \"full\""),
            },
            text: get_bool(req, "text", true)?,
            attrs: get_bool(req, "attrs", true)?,
            hidden_styles: get_bool(req, "hidden_styles", false)?,
        };
        let (rules, paint, timing) = (get_bool(req, "rules", false)?, get_bool(req, "paint", false)?, get_bool(req, "timing", false)?);
        let t0 = Instant::now();
        let session = Self::session(&mut self.sessions, req)?;
        let initial = session.document.as_mut().stylist_device().default_computed_values_arc().clone();
        let mut members = analyze::analyze_members(session.document.as_ref(), &initial, &options);
        if rules {
            write!(members, ",\"sheets\":{}", analyze::sheets_json(session.document.as_ref())).unwrap();
        }
        if paint {
            let order = paint_order(&mut session.document, &mut self.painter);
            let doc = session.document.as_ref();
            let known = |id: &NodeId| doc.get_node(*id).is_some_and(|n| n.is_element());
            let paint: Vec<String> = order
                .log
                .iter()
                .filter(|(id, ..)| known(id))
                .map(|(id, m, w, h, bb)| format!("[{},{},{},{},{}]", id.as_u64(), analyze::nums(m), analyze::num(*w), analyze::num(*h), analyze::nums(bb)))
                .collect();
            let svg: Vec<String> = order.svg.iter().filter(|(id, _)| known(id)).map(|(id, bb)| format!("[{},{}]", id.as_u64(), analyze::nums(bb))).collect();
            let parts: Vec<String> = order
                .svg_parts
                .iter()
                .filter(|(id, ..)| known(id))
                .map(|(id, part, bb)| format!("[{},{},{}]", id.as_u64(), json_string(part), analyze::nums(bb)))
                .collect();
            write!(members, ",\"paint\":[{}],\"svg\":[{}],\"svgparts\":[{}]", paint.join(","), svg.join(","), parts.join(",")).unwrap();
        }
        if timing {
            write!(members, ",\"ms\":{{\"analyze\":{}}}", ms(t0)).unwrap();
        }
        Ok(members)
    }

    fn resolve(&mut self, req: &Value) -> Result<String, Fail> {
        let Some(states) = req.get("states").and_then(Value::as_array) else { return bad("resolve needs \"states\": a list of states") };
        let keep = get_bool(req, "keep", false)?;
        if keep && states.len() != 1 {
            return bad("\"keep\": true keeps one state: give exactly one");
        }
        let report = Report {
            props: Self::props_of(req, "props", analyze::DEFAULT_PROPS)?,
            used: Self::props_of(req, "used", analyze::DEFAULT_USED)?,
            vars: names_of(req, "vars")?.unwrap_or_default().into_iter().map(str::to_string).collect(),
            rects: get_bool(req, "rects", true)?,
        };
        if let Some(name) = report.vars.iter().find(|name| !name.starts_with("--")) {
            return bad(format!("\"vars\" lists custom properties: \"{name}\" does not start with \"--\""));
        }
        enum Nodes {
            Changed,
            All,
            List(Vec<NodeId>),
        }
        let timing = get_bool(req, "timing", false)?;
        let session = Self::session(&mut self.sessions, req)?;
        // from the page as loaded: a state kept by an earlier request is undone
        session.restore();
        let nodes = match req.get("nodes") {
            None | Some(Value::Null) => Nodes::Changed,
            Some(Value::String(s)) if s == "changed" => Nodes::Changed,
            Some(Value::String(s)) if s == "all" => Nodes::All,
            Some(Value::Array(list)) => {
                session.lookup();
                let (doc, lookup) = (session.document.as_ref(), session.lookup.as_ref().unwrap());
                Nodes::List(list.iter().map(|t| lookup.find(doc, t)).collect::<Result<_, _>>().map_err(|m| Fail { code: "not_found", message: m })?)
            }
            Some(_) => return bad("\"nodes\" is \"changed\", \"all\" or a list of elements"),
        };
        let diff = get_bool(req, "diff", matches!(nodes, Nodes::Changed))?;
        let t_base = Instant::now();
        let base: HashMap<NodeId, Taken> = if diff || matches!(nodes, Nodes::Changed) {
            report.all(session.document.as_ref(), None).into_iter().collect()
        } else {
            HashMap::new()
        };
        let ms_base = ms(t_base);
        let mut out_states = Vec::new();
        let mut failure = None;
        for (k, state) in states.iter().enumerate() {
            let t0 = Instant::now();
            let invalid = match session.apply_state(state) {
                Ok(invalid) => invalid,
                Err(e) => {
                    failure = Some(Fail { code: e.code, message: format!("state {k}: {}", e.message) });
                    break;
                }
            };
            session.document.as_mut().resolve(0.0);
            let ms_resolve = ms(t0);
            let t1 = Instant::now();
            let doc = session.document.as_ref();
            let before = |id: &NodeId| if diff { base.get(id).map(|(_, state)| state) } else { None };
            let listed: Vec<String> = match &nodes {
                Nodes::Changed => report
                    .all(doc, Some(&base))
                    .iter()
                    .filter(|(id, (_, s))| base.get(id).map(|(_, b)| b) != Some(s))
                    .map(|(id, (_, s))| report.json(doc, *id, s, before(id)))
                    .collect(),
                Nodes::All => report.all(doc, Some(&base)).iter().map(|(id, (_, s))| report.json(doc, *id, s, before(id))).collect(),
                Nodes::List(ids) => ids.iter().filter_map(|id| report.element(doc, *id, base.get(id)).map(|(_, s)| report.json(doc, *id, &s, before(id)))).collect(),
            };
            let mut rec = format!("{{\"nodes\":[{}],\"invalid\":[{}]", listed.join(","), invalid.join(","));
            if timing {
                write!(rec, ",\"ms\":{{\"resolve\":{ms_resolve},\"report\":{}}}", ms(t1)).unwrap();
            }
            rec.push('}');
            out_states.push(rec);
            if !keep {
                session.undo();
            }
        }
        let t_end = Instant::now();
        if failure.is_some() {
            session.undo();
        }
        if !keep || failure.is_some() {
            session.document.as_mut().resolve(0.0);
        }
        if let Some(e) = failure {
            return Err(e);
        }
        let list = |names: &[String]| names.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(",");
        let mut unsupported = report.props.unsupported.clone();
        unsupported.extend(report.used.unsupported.iter().cloned());
        let mut members = format!(
            "\"props\":[{}],\"used\":[{}],\"vars\":[{}],\"unsupported\":[{}],\"states\":[{}]",
            list(&report.props.names),
            list(&report.used.names),
            list(&report.vars),
            list(&unsupported),
            out_states.join(",")
        );
        if keep {
            members.push_str(",\"kept\":true");
        }
        if timing {
            write!(members, ",\"ms\":{{\"baseline\":{ms_base},\"restore\":{}}}", ms(t_end)).unwrap();
        }
        Ok(members)
    }

    fn describe() -> String {
        let list = |names: &[&str]| names.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(",");
        let props = Props::parse(analyze::DEFAULT_PROPS.iter().copied());
        format!(
            "\"protocol\":{PROTOCOL},\"backend\":\"skia\",\"commands\":[{}],\"props\":[{}],\"used\":[{}],\"unsupported\":[{}],\"viewport\":[{VW},{VH}],\"max_side\":{MAX_SIDE}",
            list(COMMANDS),
            props.names.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(","),
            list(analyze::DEFAULT_USED),
            props.unsupported.iter().map(|n| json_string(n)).collect::<Vec<_>>().join(","),
        )
    }

    /// Carry a command out: the members of its answer
    fn command(&mut self, cmd: &str, req: &Value) -> Result<String, Fail> {
        match cmd {
            "ping" => Ok(format!("\"pong\":true,\"protocol\":{PROTOCOL},\"backend\":\"skia\"")),
            "describe" => Ok(Self::describe()),
            "open" | "load" => self.open(req),
            "close" => self.close(req),
            "reset" => self.reset(req),
            "dump" => self.text_answer(req, "dump"),
            "boxes" => self.text_answer(req, "boxes"),
            "analyze" => self.analyze(req),
            "resolve" => self.resolve(req),
            "paint" => self.paint(req, false),
            "frame" => self.paint(req, true),
            "paintset" => self.paintset(req),
            "shutdown" => Ok("\"shutdown\":true".to_string()),
            other => bad(format!("no command \"{other}\": the commands are {}", COMMANDS.join(", "))),
        }
    }

    /// Carry a request out: the members of its answer, and whether the process ends after it
    fn dispatch(&mut self, req: &Value) -> Result<(String, bool), Fail> {
        if !req.is_object() {
            return bad("a request is a JSON object: {\"id\": ..., \"cmd\": \"...\", ...}");
        }
        let Some(cmd) = get_str(req, "cmd")? else { return bad("a request names its command: \"cmd\"") };
        // A command that works on a page may open that page itself, and only such a command: one that is not
        // known, or that works on no page, opens nothing (a member that is null is not given).
        let given = |key: &str| !matches!(req.get(key), None | Some(Value::Null));
        let opens = matches!(cmd, "reset" | "dump" | "boxes" | "analyze" | "resolve" | "paint" | "frame" | "paintset") && (given("path") || given("html"));
        if !opens {
            return self.command(cmd, req).map(|members| (members, cmd == "shutdown"));
        }
        // A request that fails changes nothing: the page that was open under the name is set aside while the new
        // one is opened and the command carried out, and is the open one again should either of them fail.
        let name = Self::doc_name(req)?.to_string();
        let was = self.sessions.remove(&name);
        let result = self.open(req).and_then(|opened| self.command(cmd, req).map(|members| format!("{members},\"opened\":{{{opened}}}")));
        if result.is_err() {
            self.sessions.remove(&name);
            if let Some(was) = was {
                self.sessions.insert(name, was);
            }
        }
        result.map(|members| (members, false))
    }

    /// One line in, one line out: the answer, and whether the process ends after it
    fn handle(&mut self, line: &str, log: &mut Option<ReplayLog>) -> (String, bool) {
        let parsed: Result<Value, _> = serde_json::from_str(line);
        if let Some(log) = log {
            log.request(line, parsed.is_ok());
        }
        let (id, result) = match &parsed {
            Err(e) => ("null".to_string(), bad(format!("the request is not JSON: {e}"))),
            Ok(req) => {
                let id = req.get("id").map_or("null".to_string(), Value::to_string);
                // a failure of the layout engine on one page is that page's: it is closed, and the process goes on
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(req)));
                match outcome {
                    Ok(result) => (id, result),
                    Err(panic) => {
                        let name = req.get("doc").and_then(Value::as_str).unwrap_or(DEFAULT_DOC);
                        self.sessions.remove(name);
                        // (the painter may have been stopped half-way through a picture: a new one)
                        self.painter = Painter::new(Backend::Skia);
                        let what = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned());
                        (id, fail("panic", format!("the layout engine failed on this request ({}); the page \"{name}\" is closed", what.unwrap_or_default())))
                    }
                }
            }
        };
        match result {
            Ok((members, stop)) if members.is_empty() => (format!("{{\"id\":{id},\"ok\":true}}"), stop),
            Ok((members, stop)) => (format!("{{\"id\":{id},\"ok\":true,{members}}}"), stop),
            Err(e) => (
                format!("{{\"id\":{id},\"ok\":false,\"error\":{{\"code\":\"{}\",\"message\":{}}}}}", e.code, json_string(&e.message)),
                false,
            ),
        }
    }
}

/// Answers go to the stream that was stdout when the process started, and stdout itself becomes stderr: a line
/// some library prints is then no part of the protocol.
#[cfg(unix)]
fn protocol_output() -> File {
    use std::os::fd::FromRawFd as _;
    // SAFETY: dup and dup2 on the process's own standard streams; the new descriptor is owned by the File.
    unsafe {
        let fd = libc::dup(1);
        if fd < 0 {
            backend_error("fshot serve: stdout cannot be duplicated".to_string());
        }
        libc::dup2(2, 1);
        File::from_raw_fd(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn important_in_any_case_and_spacing() {
        for (value, want) in [
            ("1px !important", ("1px", true)),
            ("1px!important", ("1px", true)),
            ("1px !IMPORTANT", ("1px", true)),
            ("1px ! important", ("1px", true)),
            ("1px\t!\n Important \t", ("1px", true)),
            ("!important", ("", true)),
            ("1px", ("1px", false)),
            ("1px important", ("1px important", false)),
            ("1px !importan", ("1px !importan", false)),
            ("url(a.png?!important)", ("url(a.png?!important)", false)),
            ("\u{e9}", ("\u{e9}", false)),
            ("", ("", false)),
        ] {
            assert_eq!(split_important(value), want, "{value:?}");
        }
    }

    #[test]
    fn a_log_record_is_one_line() {
        let plain = "{\"id\":1,\"cmd\":\"ping\"}";
        assert!(matches!(one_line(plain), std::borrow::Cow::Borrowed(s) if s == plain), "a request is logged to the letter");
        let raw = "{\"id\":1,\r\"cmd\":\"open\",\r\n \"html\":\"a\u{85}b\u{2028}c\u{2029}d\"}";
        let line = one_line(raw);
        assert_eq!(line, "{\"id\":1, \"cmd\":\"open\", \n \"html\":\"a\\u0085b\\u2028c\\u2029d\"}");
        // (the same JSON)
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), serde_json::from_str::<Value>(raw).unwrap());
    }
}

pub(crate) fn serve() {
    // (header of main.rs) another rasteriser is refused before anything is read, here even for a developer:
    // what `serve` answers is what the renderer draws with
    if selected_backend() != Backend::Skia {
        backend_error("fshot serve paints with Skia only; leave FSHOT_BACKEND unset or set it to skia".to_string());
    }
    let _ = Docroot::from_env();
    // (which properties the style engine knows is asked before the first page is opened: `describe`)
    blitz_dom::BaseDocument::init_style_prefs();
    let mut log = ReplayLog::from_env();
    let mut server = Server { painter: Painter::new(Backend::Skia), sessions: HashMap::new(), fonts: Fonts::new(), digests: log.is_some() };
    let mut out = std::io::LineWriter::new(protocol_output());
    let mut stdin = std::io::stdin().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdin.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        if text.trim().is_empty() {
            continue;
        }
        let (answer, stop) = server.handle(&text, &mut log);
        if writeln!(out, "{answer}").and_then(|()| out.flush()).is_err() {
            // nobody is reading any more
            break;
        }
        if let Some(log) = &mut log {
            log.response(&answer);
        }
        if stop {
            break;
        }
    }
}
