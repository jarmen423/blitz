//! Headless Blitz for the known-future renderer test. Three modes:
//!   fshot VW VH in1.html out1.png [in2.html out2.png ...]      full frames on white (fidelity test)
//!   fshot dump in.html out.json                                layout, styles and paint order at 1920x1080
//!   fshot paint in1.html:out1.png:W:H [...]                    lay out at 1920x1080, paint a W x H picture
//!                                                              on a transparent background
//!   fshot paintset base.html setjobs.txt                       load once, paint many pictures
//!
//! Rasteriser backends (default: Vello CPU):
//!   FSHOT_BACKEND=vello-cpu     (default) anyrender_vello_cpu
//!   FSHOT_BACKEND=skia          (or FSHOT_SKIA=1) anyrender_skia (CPU raster surface, Premul -> straight RGBA)
//!   FSHOT_BACKEND=vello-hybrid  (or FSHOT_HYBRID=1) anyrender_vello_hybrid (CPU strips + wgpu GPU raster)
//!   FSHOT_BACKEND=vello-gpu     (or FSHOT_GPU=1) anyrender_vello (full compute GPU Vello)
//! Pages are loaded from disk; their <base href> (which Blitz doesn't apply itself) becomes the base URL.
use anyrender::{AntiAliasing, ImageRenderer, PaintScene as _, render_to_buffer};
use anyrender_skia::{SkiaSceneCache, SkiaScenePainter};
use anyrender_vello_cpu::VelloCpuImageRenderer;
use anyrender_vello_hybrid::{
    ExternalTextures, ImageCache as HybridImageCache, ImageManager as HybridImageManager,
    VelloHybridRendererOptions, VelloHybridScenePainter,
};
use blitz_dom::{DocumentConfig, LocalName, NodeId, QualName, ns, util::Color};
use blitz_html::HtmlDocument;
use blitz_net::Provider;
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use peniko::Fill;
use peniko::kurbo::Rect;
use std::{fmt::Write as _, fs::File, io::BufWriter, sync::Arc, time::Instant};
use vello_gpu::{
    RenderOptions, RenderSize, RenderTarget, RenderTargetConfig, Renderer as VelloHybridRenderer,
    Resources as HybridResources, Scene as VelloHybridScene,
};
use wgpu_context::{BufferRenderer, BufferRendererConfig, DeviceHandle};

const VW: u32 = 1920;
const VH: u32 = 1080;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    VelloCpu,
    Skia,
    VelloHybrid,
    VelloGpu,
}

fn selected_backend() -> Backend {
    if let Ok(v) = std::env::var("FSHOT_BACKEND") {
        match v.trim().to_ascii_lowercase().as_str() {
            "" | "vello-cpu" | "vello_cpu" | "cpu" => return Backend::VelloCpu,
            "skia" => return Backend::Skia,
            "vello-hybrid" | "vello_hybrid" | "hybrid" => return Backend::VelloHybrid,
            "vello-gpu" | "vello_gpu" | "gpu" | "vello" => return Backend::VelloGpu,
            other => panic!(
                "unknown FSHOT_BACKEND={other:?}; expected vello-cpu, skia, vello-hybrid, or vello-gpu"
            ),
        }
    }
    if std::env::var_os("FSHOT_SKIA").is_some() {
        return Backend::Skia;
    }
    if std::env::var_os("FSHOT_HYBRID").is_some() {
        return Backend::VelloHybrid;
    }
    if std::env::var_os("FSHOT_GPU").is_some() {
        return Backend::VelloGpu;
    }
    Backend::VelloCpu
}

struct SkiaOffscreen {
    surface_props: skia_safe::SurfaceProps,
    cache: SkiaSceneCache,
}

impl SkiaOffscreen {
    fn new() -> Self {
        skia_safe::graphics::set_font_cache_count_limit(512);
        skia_safe::graphics::set_typeface_cache_count_limit(256);
        skia_safe::graphics::set_resource_cache_total_bytes_limit(256 * 1024 * 1024);
        let dflt = skia_safe::SurfaceProps::default();
        // Match Chromium's Linux Skia text contrast (0.2) and gamma (1.2), same as anyrender_skia::surface_props().
        let surface_props = skia_safe::SurfaceProps::new_with_text_properties(
            dflt.flags(),
            dflt.pixel_geometry(),
            0.2,
            1.2,
        );
        Self {
            surface_props,
            cache: SkiaSceneCache::new(),
        }
    }

    fn render(&mut self, document: &mut HtmlDocument, w: u32, h: u32, white: bool) -> Vec<u8> {
        let info = skia_safe::ImageInfo::new(
            (w as i32, h as i32),
            skia_safe::ColorType::RGBA8888,
            skia_safe::AlphaType::Premul,
            None,
        );
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        {
            let mut surface = skia_safe::surfaces::wrap_pixels(
                &info,
                &mut buf[..],
                Some((w as usize) * 4),
                Some(&self.surface_props),
            )
            .expect("skia wrap_pixels failed");
            surface.canvas().clear(if white {
                skia_safe::Color::WHITE
            } else {
                skia_safe::Color::TRANSPARENT
            });
            let mut painter = SkiaScenePainter::new(surface.canvas(), &mut self.cache);
            paint_scene(&mut painter, document.as_mut(), 1.0, w, h, 0, 0);
        }
        self.cache.next_gen();
        buf
    }
}

struct HybridOffscreen {
    device_handle: DeviceHandle,
    buffer_renderer: BufferRenderer,
    renderer: VelloHybridRenderer,
    resources: HybridResources,
    depth_view: wgpu::TextureView,
    texture_bindings: ExternalTextures,
    cached_images: HybridImageCache,
    scene: VelloHybridScene,
    options: VelloHybridRendererOptions,
    w: u32,
    h: u32,
}

impl HybridOffscreen {
    fn new(w: u32, h: u32) -> Self {
        let options = VelloHybridRendererOptions::default();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::from_env().unwrap_or_default(),
            flags: wgpu::InstanceFlags::from_build_config().with_env(),
            backend_options: wgpu::BackendOptions::from_env_or_default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });
        let adapter = pollster::block_on(wgpu::util::initialize_adapter_from_env_or_default(
            &instance, None,
        ))
        .expect("No compatible wgpu adapter found for vello-hybrid");
        let wanted = wgpu::Features::CLEAR_TEXTURE | wgpu::Features::PIPELINE_CACHE;
        let required_features = wanted & adapter.features();
        let adapter_limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("fshot-vello-hybrid"),
            required_features,
            required_limits: adapter_limits.clone(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
        }))
        .expect("Failed to request wgpu device for vello-hybrid");
        let max_dim = adapter_limits.max_texture_dimension_2d.min(u16::MAX as u32) as u16;
        let mut cached_images = HybridImageCache::default();
        cached_images.set_max_image_size((max_dim, max_dim));

        let device_handle = DeviceHandle {
            instance,
            adapter,
            device,
            queue,
        };
        let buffer_renderer = BufferRenderer::new(
            BufferRendererConfig {
                width: w,
                height: h,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            },
            device_handle.clone(),
            0,
        );
        let (renderer, resources) = VelloHybridRenderer::new_with(
            buffer_renderer.device(),
            buffer_renderer.queue(),
            &RenderTargetConfig {
                format: wgpu::TextureFormat::Rgba8Unorm,
                width: w as u16,
                height: h as u16,
            },
            options.render_settings.clone(),
        );
        let depth_view = renderer.create_depth_texture_view(&RenderSize {
            width: w as u16,
            height: h as u16,
        });
        let scene =
            VelloHybridScene::new_with(w as u16, h as u16, options.render_settings.level());
        Self {
            device_handle,
            buffer_renderer,
            renderer,
            resources,
            depth_view,
            texture_bindings: ExternalTextures::default(),
            cached_images,
            scene,
            options,
            w,
            h,
        }
    }

    fn render(&mut self, document: &mut HtmlDocument, w: u32, h: u32, white: bool) -> Vec<u8> {
        if (w, h) != (self.w, self.h) {
            self.buffer_renderer.resize(w, h);
            self.depth_view = self.renderer.create_depth_texture_view(&RenderSize {
                width: w as u16,
                height: h as u16,
            });
            self.scene = VelloHybridScene::new_with(
                w as u16,
                h as u16,
                self.options.render_settings.level(),
            );
            self.w = w;
            self.h = h;
        } else {
            self.scene.reset();
        }

        let texture_view = self.buffer_renderer.target_texture_view();
        let mut encoder =
            self.device_handle
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("fshot-hybrid-render"),
                });
        {
            let clear = if white {
                wgpu::Color::WHITE
            } else {
                wgpu::Color::TRANSPARENT
            };
            let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("fshot-hybrid-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        {
            let image_manager = HybridImageManager::new(
                &mut self.resources,
                &self.device_handle.device,
                &self.device_handle.queue,
                &mut self.cached_images,
                &mut self.texture_bindings,
            );
            let mut painter =
                VelloHybridScenePainter::new(&mut self.scene, image_manager, &self.device_handle)
                    .with_glyph_caching(self.options.glyph_caching);
            if white {
                painter.fill(
                    Fill::NonZero,
                    Default::default(),
                    Color::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, w as f64, h as f64),
                    AntiAliasing::Enabled,
                );
            }
            paint_scene(&mut painter, document.as_mut(), 1.0, w, h, 0, 0);
        }

        self.renderer
            .render(
                &self.scene,
                &mut self.resources,
                &mut encoder,
                RenderTarget {
                    view: &texture_view,
                    depth_view: Some(&self.depth_view),
                },
                self.texture_bindings.bindings(),
                RenderOptions::default(),
            )
            .expect("vello-hybrid failed to render to texture");

        HybridImageManager::new(
            &mut self.resources,
            &self.device_handle.device,
            &self.device_handle.queue,
            &mut self.cached_images,
            &mut self.texture_bindings,
        )
        .finish_frame();

        self.device_handle.queue.submit([encoder.finish()]);
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        self.buffer_renderer.copy_texture_to_buffer(&mut buf);
        self.scene.reset();
        buf
    }
}

fn unpremultiply(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

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
            n.data.is_element_with_tag_name(&img)
                && n.attr(src.clone()).is_some_and(|s| !s.is_empty())
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
                scene.fill(
                    Fill::NonZero,
                    Default::default(),
                    Color::WHITE,
                    Default::default(),
                    &Rect::new(0.0, 0.0, w as f64, h as f64),
                    AntiAliasing::Enabled,
                );
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
        let Some(node) = doc.get_node(*id) else {
            continue;
        };
        let Some(i) = node.attr(di.clone()) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        write!(
            out,
            "[{},[{},{},{},{},{},{}],{},{},[{},{},{},{}]]",
            i, m[0], m[1], m[2], m[3], m[4], m[5], w, h, bb[0], bb[1], bb[2], bb[3]
        )
        .unwrap();
    }
    out.push_str("],\"svg\":[");
    let mut first = true;
    for (id, bb) in &svg_log {
        let Some(node) = doc.get_node(*id) else {
            continue;
        };
        let Some(i) = node.attr(di.clone()) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},[{},{},{},{}]]", i, bb[0], bb[1], bb[2], bb[3]).unwrap();
    }
    out.push_str("],\"styles\":{");
    let mut first = true;
    for (_, node) in doc.tree().iter() {
        let Some(i) = node.attr(di.clone()) else {
            continue;
        };
        let Some(st) = node.primary_styles() else {
            continue;
        };
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
    let backend = selected_backend();
    let t_all = Instant::now();
    let mut document = load(base, VW, VH);
    let t_first = t_all.elapsed().as_millis();
    let di = LocalName::from("data-i");
    let idn = LocalName::from("id");
    let (mut style_text, mut by_i): (Option<NodeId>, std::collections::HashMap<String, NodeId>) =
        (None, Default::default());
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
    let mut skia_renderer: Option<SkiaOffscreen> = None;
    let mut hybrid_renderer: Option<HybridOffscreen> = None;
    let (mut t_restyle, mut t_paint, mut t_write, mut n) = (0u128, 0u128, 0u128, 0);
    let jobs = std::fs::read_to_string(jobfile).unwrap();
    for rec in jobs.split("\u{1e}\n") {
        if rec.trim().is_empty() {
            continue;
        }
        let (head, css) = rec.split_once("\n---\n").unwrap();
        let mut lines = head.lines();
        let first: Vec<&str> = lines.next().unwrap().split(' ').collect();
        let (output, w, h) = (
            first[0],
            first[1].parse::<u32>().unwrap(),
            first[2].parse::<u32>().unwrap(),
        );
        let t0 = Instant::now();
        let mut restore: Vec<(NodeId, QualName, Option<String>)> = Vec::new();
        {
            let mut m = document.as_mut().mutate();
            m.set_node_text(style_text, css);
            for l in lines {
                let mut it = l.splitn(4, ' ');
                let (_, i, attr, value) = (
                    it.next(),
                    it.next().unwrap(),
                    it.next().unwrap(),
                    it.next().unwrap_or(""),
                );
                let id = by_i[i];
                let q = QualName::new(None, ns!(), LocalName::from(attr));
                restore.push((id, q.clone(), None));
                m.set_attribute(id, q, value);
            }
        }
        document.as_mut().resolve(0.0);
        t_restyle += t0.elapsed().as_millis();
        let t1 = Instant::now();
        let mut buf = match backend {
            Backend::VelloGpu => {
                let r = gpu_renderer
                    .get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(w, h));
                r.resize(w, h);
                let mut v = Vec::new();
                r.render_to_vec(
                    |scene| paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0),
                    &mut v,
                );
                v
            }
            Backend::Skia => {
                let r = skia_renderer.get_or_insert_with(SkiaOffscreen::new);
                let mut b = r.render(&mut document, w, h, false);
                unpremultiply(&mut b);
                b
            }
            Backend::VelloHybrid => {
                let r = hybrid_renderer.get_or_insert_with(|| HybridOffscreen::new(w, h));
                let mut b = r.render(&mut document, w, h, false);
                unpremultiply(&mut b);
                b
            }
            Backend::VelloCpu => {
                let mut b = render(&mut document, w, h, false);
                unpremultiply(&mut b);
                b
            }
        };
        t_paint += t1.elapsed().as_millis();
        let t2 = Instant::now();
        if output.ends_with(".rgba") {
            std::fs::write(output, &buf).unwrap();
        } else {
            write_png(output, &mut buf, w, h);
        }
        t_write += t2.elapsed().as_millis();
        if !restore.is_empty() {
            let mut m = document.as_mut().mutate();
            for (id, q, _) in restore {
                m.clear_attribute(id, q);
            }
        }
        n += 1;
    }
    println!(
        "SUMMARY pictures {} first load {} ms, restyle+layout {} ms, paint {} ms, write {} ms, total {} ms",
        n,
        t_first,
        t_restyle,
        t_paint,
        t_write,
        t_all.elapsed().as_millis()
    );
}

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter(); // blitz-net fetches assets on the runtime's threads
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "dump" => dump(&args[2], &args[3]),
        "paintset" => paintset(&args[2], &args[3]),
        "paint" => {
            let backend = selected_backend();
            let mut gpu_renderer: Option<anyrender_vello::VelloImageRenderer> = None;
            let mut skia_renderer: Option<SkiaOffscreen> = None;
            let mut hybrid_renderer: Option<HybridOffscreen> = None;
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
                let mut buf = match backend {
                    Backend::VelloGpu => {
                        let r = gpu_renderer
                            .get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(w, h));
                        r.resize(w, h);
                        let mut v = Vec::new();
                        r.render_to_vec(
                            |scene| paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0),
                            &mut v,
                        );
                        v
                    }
                    Backend::Skia => {
                        let r = skia_renderer.get_or_insert_with(SkiaOffscreen::new);
                        let mut b = r.render(&mut document, w, h, false);
                        unpremultiply(&mut b);
                        b
                    }
                    Backend::VelloHybrid => {
                        let r = hybrid_renderer.get_or_insert_with(|| HybridOffscreen::new(w, h));
                        let mut b = r.render(&mut document, w, h, false);
                        unpremultiply(&mut b);
                        b
                    }
                    Backend::VelloCpu => {
                        let mut b = render(&mut document, w, h, false);
                        unpremultiply(&mut b);
                        b
                    }
                };
                t_paint += t1.elapsed().as_millis();
                let t2 = Instant::now();
                if output.ends_with(".rgba") {
                    std::fs::write(output, &buf).unwrap();
                } else {
                    write_png(output, &mut buf, w, h);
                }
                t_write += t2.elapsed().as_millis();
                println!("{output} {w}x{h} {} ms", t0.elapsed().as_millis());
            }
            println!(
                "SUMMARY pictures {} load+style+layout {} ms, paint {} ms, write {} ms, total {} ms",
                args.len() - 2,
                t_load,
                t_paint,
                t_write,
                t_all.elapsed().as_millis()
            );
        }
        _ => {
            let backend = selected_backend();
            let width: u32 = args[1].parse().unwrap();
            let height: u32 = args[2].parse().unwrap();
            let mut gpu_renderer: Option<anyrender_vello::VelloImageRenderer> = None;
            let mut skia_renderer: Option<SkiaOffscreen> = None;
            let mut hybrid_renderer: Option<HybridOffscreen> = None;
            for pair in args[3..].chunks(2) {
                let t0 = Instant::now();
                let mut document = load(&pair[0], width, height);
                let t_layout = t0.elapsed().as_millis();
                let mut buf = match backend {
                    Backend::VelloGpu => {
                        let r = gpu_renderer.get_or_insert_with(|| {
                            anyrender_vello::VelloImageRenderer::new(width, height)
                        });
                        r.resize(width, height);
                        let mut v = Vec::new();
                        r.render_to_vec(
                            |scene| {
                                scene.fill(
                                    Fill::NonZero,
                                    Default::default(),
                                    Color::WHITE,
                                    Default::default(),
                                    &Rect::new(0.0, 0.0, width as f64, height as f64),
                                    AntiAliasing::Enabled,
                                );
                                paint_scene(scene, document.as_mut(), 1.0, width, height, 0, 0);
                            },
                            &mut v,
                        );
                        v
                    }
                    Backend::Skia => {
                        let r = skia_renderer.get_or_insert_with(SkiaOffscreen::new);
                        r.render(&mut document, width, height, true)
                    }
                    Backend::VelloHybrid => {
                        let r =
                            hybrid_renderer.get_or_insert_with(|| HybridOffscreen::new(width, height));
                        r.render(&mut document, width, height, true)
                    }
                    Backend::VelloCpu => render(&mut document, width, height, true),
                };
                let t_paint = t0.elapsed().as_millis() - t_layout;
                write_png(&pair[1], &mut buf, width, height);
                println!(
                    "{} -> {}: load+style+layout {t_layout} ms, paint {t_paint} ms",
                    pair[0], pair[1]
                );
            }
        }
    }
}
