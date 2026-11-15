//! Headless Blitz for the known-future renderer test. Three modes:
//!   fshot VW VH in1.html out1.png [in2.html out2.png ...]      full frames on white (fidelity test)
//!   fshot dump in.html out.json                                layout, styles and paint order at 1920x1080
//!   fshot paint in1.html:out1.png:W:H [...]                    lay out at 1920x1080, paint a W x H picture
//!                                                              on a transparent background
//! Pages are loaded from disk; their <base href> (which Blitz doesn't apply itself) becomes the base URL.
use anyrender::{AntiAliasing, PaintScene as _, render_to_buffer};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{DocumentConfig, LocalName, NodeId, QualName, ns, util::Color};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;
use std::{fmt::Write as _, fs::File, io::BufWriter, sync::Arc, time::Instant};

const VW: u32 = 1920;
const VH: u32 = 1080;

fn load(input: &str, vw: u32, vh: u32) -> HtmlDocument {
    let path = std::fs::canonicalize(input).unwrap();
    let html = std::fs::read_to_string(&path).unwrap();
    let url = html
        .find("<base href=\"")
        .map(|i| {
            let r = &html[i + 12..];
            r[..r.find('"').unwrap()].to_string()
        })
        .unwrap_or_else(|| url::Url::from_file_path(&path).unwrap().to_string());
    let net = Arc::new(Provider::new(None));
    let mut document = HtmlDocument::from_html(
        &html,
        DocumentConfig {
            base_url: Some(url),
            net_provider: Some(Arc::clone(&net) as _),
            viewport: Some(Viewport::new(vw, vh, 1.0, ColorScheme::Light)),
            ..Default::default()
        },
    );
    // Wait until every <img> has its decoded image, not just until the downloads are done: images are decoded
    // after the fetch and arrive as messages, and painting earlier silently leaves them out.
    let img = LocalName::from("img");
    let src = LocalName::from("src");
    let t0 = Instant::now();
    loop {
        document.resolve(0.0);
        let doc = document.as_ref();
        let waiting = doc.tree().iter().any(|(_, n)| {
            n.data.is_element_with_tag_name(&img) && n.attr(src.clone()).is_some_and(|s| !s.is_empty())
                && n.element_data().is_none_or(|e| e.image_data().is_none())
        });
        if net.is_empty() && !doc.has_pending_critical_resources() && !waiting {
            break;
        }
        if t0.elapsed().as_secs() > 30 {
            eprintln!("warning: {input}: images still missing after 30 s");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    document.as_mut().resolve(0.0);
    document
}

fn render(document: &mut HtmlDocument, w: u32, h: u32, white: bool) -> Vec<u8> {
    render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| {
            if white {
                scene.fill(Fill::NonZero, Default::default(), Color::WHITE, Default::default(),
                           &Rect::new(0.0, 0.0, w as f64, h as f64), AntiAliasing::Enabled);
            }
            paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0);
        },
        w,
        h,
    )
}

fn write_png(path: &str, buf: &mut [u8], w: u32, h: u32) {
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(buf).unwrap();
    wr.finish().unwrap();
}

fn dump(input: &str, output: &str) {
    let mut document = load(input, VW, VH);
    *blitz_paint::PAINT_LOG.lock().unwrap() = Some(Vec::new());
    blitz_paint::SVG_LOG.lock().unwrap().clear();
    let _ = render(&mut document, 64, 64, false);
    let log = blitz_paint::PAINT_LOG.lock().unwrap().take().unwrap();
    let svg_log = std::mem::take(&mut *blitz_paint::SVG_LOG.lock().unwrap());
    let di = LocalName::from("data-i");
    let doc = document.as_ref();
    let mut out = String::from("{\"paint\":[");
    let mut first = true;
    for (id, m, w, h, bb) in &log {
        let Some(node) = doc.get_node(*id) else { continue };
        let Some(i) = node.attr(di.clone()) else { continue };
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},[{},{},{},{},{},{}],{},{},[{},{},{},{}]]", i, m[0], m[1], m[2], m[3], m[4], m[5], w, h, bb[0], bb[1], bb[2], bb[3]).unwrap();
    }
    out.push_str("],\"svg\":[");
    let mut first = true;
    for (id, bb) in &svg_log {
        let Some(node) = doc.get_node(*id) else { continue };
        let Some(i) = node.attr(di.clone()) else { continue };
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},[{},{},{},{}]]", i, bb[0], bb[1], bb[2], bb[3]).unwrap();
    }
    out.push_str("],\"styles\":{");
    let mut first = true;
    for (_, node) in doc.tree().iter() {
        let Some(i) = node.attr(di.clone()) else { continue };
        let Some(st) = node.primary_styles() else { continue };
        let size = node.final_layout().size;
        if !first {
            out.push(',');
        }
        first = false;
        write!(
            out,
            "\"{}\":{{\"opacity\":{},\"display\":\"{:?}\",\"visibility\":\"{:?}\",\"blend\":\"{:?}\",\"ox\":\"{:?}\",\"oy\":\"{:?}\",\"w\":{},\"h\":{}}}",
            i,
            st.get_effects().opacity,
            node.taffy_display(),
            st.get_inherited_box().visibility,
            st.get_effects().mix_blend_mode,
            st.get_box().overflow_x,
            st.get_box().overflow_y,
            size.width,
            size.height
        )
        .unwrap();
    }
    out.push_str("}}");
    std::fs::write(output, out).unwrap();
}

/// Load one scene page once, then paint many pictures from it: per picture, swap the CSS inside
/// <style id="fshot-pic"> and set a few attributes (SVG part poses), re-resolve, paint, restore.
/// Job file: records separated by a line "\x1e"; each record is "OUT W H", then "@ <data-i> <attr> <value>"
/// lines, then a line "---", then the picture's CSS.
fn paintset(base: &str, jobfile: &str) {
    let gpu = std::env::var_os("FSHOT_GPU").is_some();
    let t_all = Instant::now();
    let mut document = load(base, VW, VH);
    let t_first = t_all.elapsed().as_millis();
    let di = LocalName::from("data-i");
    let idn = LocalName::from("id");
    let (mut style_text, mut by_i): (Option<NodeId>, std::collections::HashMap<String, NodeId>) = (None, Default::default());
    {
        let doc = document.as_ref();
        for (_, node) in doc.tree().iter() {
            if let Some(i) = node.attr(di.clone()) {
                by_i.insert(i.to_string(), node.id);
            }
            if node.attr(idn.clone()) == Some("fshot-pic") {
                style_text = node.children.first().copied();
            }
        }
    }
    let style_text = style_text.expect("base page needs <style id=\"fshot-pic\">/**/</style>");
    let mut gpu_renderer: Option<anyrender_vello::VelloImageRenderer> = None;
    let (mut t_restyle, mut t_paint, mut t_write, mut n) = (0u128, 0u128, 0u128, 0);
    let jobs = std::fs::read_to_string(jobfile).unwrap();
    for rec in jobs.split("\u{1e}\n") {
        if rec.trim().is_empty() {
            continue;
        }
        let (head, css) = rec.split_once("\n---\n").unwrap();
        let mut lines = head.lines();
        let first: Vec<&str> = lines.next().unwrap().split(' ').collect();
        let (output, w, h) = (first[0], first[1].parse::<u32>().unwrap(), first[2].parse::<u32>().unwrap());
        let t0 = Instant::now();
        let mut restore: Vec<(NodeId, QualName, Option<String>)> = Vec::new();
        {
            let mut m = document.as_mut().mutate();
            m.set_node_text(style_text, css);
            for l in lines {
                let mut it = l.splitn(4, ' ');
                let (_, i, attr, value) = (it.next(), it.next().unwrap(), it.next().unwrap(), it.next().unwrap_or(""));
                let id = by_i[i];
                let q = QualName::new(None, ns!(), LocalName::from(attr));
                restore.push((id, q.clone(), None));
                m.set_attribute(id, q, value);
            }
        }
        // remember the originals (read after the fact from a fresh load would be costly; the base page has them)
        document.as_mut().resolve(0.0);
        t_restyle += t0.elapsed().as_millis();
        let t1 = Instant::now();
        let mut buf = if gpu {
            use anyrender::ImageRenderer;
            let r = gpu_renderer.get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(w, h));
            r.resize(w, h);
            let mut v = Vec::new();
            r.render_to_vec(|scene| paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0), &mut v);
            v
        } else {
            let mut b = render(&mut document, w, h, false);
            for px in b.chunks_exact_mut(4) {
                let a = px[3] as u32;
                if a > 0 && a < 255 {
                    for c in &mut px[..3] {
                        *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                    }
                }
            }
            b
        };
        t_paint += t1.elapsed().as_millis();
        let t2 = Instant::now();
        if output.ends_with(".rgba") {
            std::fs::write(output, &buf).unwrap();
        } else {
            write_png(output, &mut buf, w, h);
        }
        t_write += t2.elapsed().as_millis();
        // put the base page's attributes back (looks only ever set attributes the base page had or didn't have)
        if !restore.is_empty() {
            let mut m = document.as_mut().mutate();
            for (id, q, _) in restore {
                m.clear_attribute(id, q);
            }
        }
        n += 1;
    }
    println!("SUMMARY pictures {} first load {} ms, restyle+layout {} ms, paint {} ms, write {} ms, total {} ms",
             n, t_first, t_restyle, t_paint, t_write, t_all.elapsed().as_millis());
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter(); // blitz-net fetches assets on the runtime's threads
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "dump" => dump(&args[2], &args[3]),
        "paintset" => paintset(&args[2], &args[3]),
        "paint" => {
            // FSHOT_GPU=1: paint with Vello's GPU renderer (wgpu), one renderer reused for every picture
            let gpu = std::env::var_os("FSHOT_GPU").is_some();
            let mut gpu_renderer: Option<anyrender_vello::VelloImageRenderer> = None;
            let (mut t_load, mut t_paint, mut t_write) = (0u128, 0u128, 0u128);
            let t_all = Instant::now();
            for spec in &args[2..] {
                let t0 = Instant::now();
                let p: Vec<&str> = spec.rsplitn(3, ':').collect(); // H, W, "in:out"
                let (h, w) = (p[0].parse::<u32>().unwrap(), p[1].parse::<u32>().unwrap());
                let (input, output) = p[2].split_once(':').unwrap();
                let mut document = load(input, VW, VH);
                t_load += t0.elapsed().as_millis();
                let t1 = Instant::now();
                let mut buf = if gpu {
                    use anyrender::ImageRenderer;
                    let r = gpu_renderer.get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(w, h));
                    r.resize(w, h);
                    let mut v = Vec::new();
                    r.render_to_vec(|scene| paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0), &mut v);
                    v
                } else {
                    render(&mut document, w, h, false)
                };
                t_paint += t1.elapsed().as_millis();
                let t2 = Instant::now();
                if gpu {
                    // Vello's GPU output is already straight (separated) alpha
                    if output.ends_with(".rgba") {
                        std::fs::write(output, &buf).unwrap();
                    } else {
                        write_png(output, &mut buf, w, h);
                    }
                    t_write += t2.elapsed().as_millis();
                    println!("{output} {w}x{h} {} ms", t0.elapsed().as_millis());
                    continue;
                }
                // Vello's buffer is premultiplied; PNG stores straight alpha
                for px in buf.chunks_exact_mut(4) {
                    let a = px[3] as u32;
                    if a > 0 && a < 255 {
                        for c in &mut px[..3] {
                            *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
                        }
                    }
                }
                if output.ends_with(".rgba") {
                    std::fs::write(output, &buf).unwrap();
                } else {
                    write_png(output, &mut buf, w, h);
                }
                t_write += t2.elapsed().as_millis();
                println!("{output} {w}x{h} {} ms", t0.elapsed().as_millis());
            }
            println!("SUMMARY pictures {} load+style+layout {} ms, paint {} ms, write {} ms, total {} ms",
                     args.len() - 2, t_load, t_paint, t_write, t_all.elapsed().as_millis());
        }
        _ => {
            let width: u32 = args[1].parse().unwrap();
            let height: u32 = args[2].parse().unwrap();
            for pair in args[3..].chunks(2) {
                let t0 = Instant::now();
                let mut document = load(&pair[0], width, height);
                let t_layout = t0.elapsed().as_millis();
                let buf = render(&mut document, width, height, true);
                let t_paint = t0.elapsed().as_millis() - t_layout;
                write_png(&pair[1], &mut buf.clone(), width, height);
                println!("{} -> {}: load+style+layout {t_layout} ms, paint {t_paint} ms", pair[0], pair[1]);
            }
        }
    }
}
