//! Headless Blitz for the known-future renderer test. Its modes:
//!   fshot VW VH in1.html out1.png [in2.html out2.png ...]      full frames on white (fidelity test)
//!   fshot dump in.html out.json                                layout, styles and paint order at 1920x1080
//!   fshot paint in1.html:out1.png:W:H [...]                    lay out at 1920x1080, paint a W x H picture
//!                                                              on a transparent background
//!   fshot paintset base.html setjobs.txt                       load once, paint many pictures
//!   fshot boxes in1.html out1.json [in2.html out2.json ...]    each element's CSSOM boxes (offsetWidth & co.), no paint
//!   fshot serve                                                (wave 1a) one process for a scene: requests on stdin,
//!                                                              answers on stdout, a line of JSON each (serve.rs)
//! A page that holds `<meta name="fshot-viewport" content="WxH">` is laid out at W x H instead of 1920x1080 in
//! every mode (its frame size: `vw`, `vh`, the root's percentage sizes and media queries depend on it).
//!
//! Rasteriser (known-future, wave 0: Skia, and only Skia unless a developer asks otherwise):
//!   FSHOT_BACKEND=skia          (the default; or FSHOT_SKIA=1) anyrender_skia (CPU raster surface, Premul -> straight
//!                               RGBA). Every mode paints with it, `dump`'s paint-order pass too.
//!   Any other rasteriser is refused with exit status 2 and one line on stderr: the renderer's pictures, and its
//!   checks against Chrome, are Skia's. For development and benchmarks, FSHOT_ALLOW_NON_SKIA=1 lets these through:
//!   FSHOT_BACKEND=vello-cpu     anyrender_vello_cpu
//!   FSHOT_BACKEND=vello-hybrid  (or FSHOT_HYBRID=1) anyrender_vello_hybrid (CPU strips + wgpu GPU raster)
//!   FSHOT_BACKEND=vello-gpu     (or FSHOT_GPU=1) anyrender_vello (full compute GPU Vello)
//! Text and images (docs/FIDELITY.md in known-future):
//!   FSHOT_GEOMETRIC_PRECISION=auto (default; wave 2) text as Chrome on Linux lays out and draws it, by each
//!                               element's own `text-rendering`: under geometricPrecision (which HyperFrames'
//!                               compiler sets on every element) sub-pixel glyph positions, no hinting and Blink's
//!                               ascent/descent rule; otherwise Chrome's default there: each glyph's advance width a
//!                               whole pixel, each glyph drawn at a whole pixel, slight hinting.
//!                               1 = every element as under geometricPrecision (the default before wave 2);
//!                               0 = fshot's text before 2026-11-18 (hinted, sub-pixel positions)
//!   FSHOT_LCD=rgb|bgr           LCD text where it lands on opaque pixels (default off: grayscale everywhere)
//!   BLITZ_NO_CLIP_MASK=tiles|all (wave 2) where a mask with `mask-clip: no-clip` is applied: `tiles` (default) in the
//!                               raster tiles its box is in, as Chrome's software rasteriser does (what the box's
//!                               descendants paint in other tiles is left unmasked); `all`: everywhere
//!   FSHOT_TEXT_HINT=0|1         override the hinting; FSHOT_IMAGE_QUALITY=low|medium|high for image-rendering:auto
//! Pages are loaded from disk; their <base href> (which Blitz doesn't apply itself) becomes the base URL.
//! (wave 1a) No CSS transition runs: a page is painted as it stands, and when `paintset` or `serve` change its CSS or an
//! attribute, the changed values apply at once (a transition would be painted at its first instant: the old value).
//! What it fetches by itself (known-future, net round 1):
//!   FSHOT_NET=local             (default) `file:` and `data:` addresses only; every other address a page names (http,
//!                               https, ...) is refused, not fetched, and listed on stderr at the end of each page's load
//!                               as one line `FSHOT-REFUSED ["<url>", ...]` (the renderer fetches what a page may have
//!                               itself, by its rules, before the layout: known_future/weblocal.py)
//!   FSHOT_NET=open              blitz-net as it was: anything, over http too, from any host (a developer's own pages only)
//!   FSHOT_NET=none              (wave 1a) another name for `local`
//!   FSHOT_DOCROOT=DIR[:DIR...]  (wave 1a; unset: no such limit) every file read for a page, the page itself too, must be
//!                               inside one of these folders once its symbolic links and `..` are resolved: its style
//!                               sheets, pictures, fonts, and what an inline <svg>'s <image> names. A `file:` address
//!                               outside them is not read: the fetch fails, and the address is listed on stderr at the end
//!                               of the page's load as `FSHOT-DOCROOT-REFUSED ["<url>", ...]`. A second wall for a render
//!                               that runs with no sandbox around it; inside one, name what the sandbox shows.
//! (wave 0) What an inline <svg>'s <image href|xlink:href> names is resolved against the page's base URL and goes
//! through the same guard: `file:` and `data:` are read, anything else is refused and listed with FSHOT_NET=local.
//! An SVG that is a picture itself (<img src>, a CSS url()) loads no file of its own, as in a browser.
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
use style::values::computed::CSSPixelLength;
use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{fmt::Write as _, fs::File, io::BufWriter, sync::Arc, time::Instant};
use vello_gpu::{
    RenderOptions, RenderSize, RenderTarget, RenderTargetConfig, Renderer as VelloHybridRenderer,
    Resources as HybridResources, Scene as VelloHybridScene,
};
use wgpu_context::{BufferRenderer, BufferRendererConfig, DeviceHandle};

mod analyze;
mod serve;
mod sha256;

const VW: u32 = 1920;
const VH: u32 = 1080;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    VelloCpu,
    Skia,
    VelloHybrid,
    VelloGpu,
}

/// known-future (wave 0): stop with one line on stderr and exit status 2 (a wrong rasteriser is the caller's
/// mistake to see, not a panic's backtrace to read).
fn backend_error(message: String) -> ! {
    eprintln!("fshot: error: {message}");
    std::process::exit(2);
}

/// The rasteriser every picture of this run is painted with: Skia. Another one named in the environment is
/// refused, unless FSHOT_ALLOW_NON_SKIA=1 says a developer wants it (header).
fn selected_backend() -> Backend {
    const NAMES: &str = "skia (the default), or with FSHOT_ALLOW_NON_SKIA=1 vello-cpu, vello-hybrid, vello-gpu";
    let (backend, asked) = if let Ok(v) = std::env::var("FSHOT_BACKEND") {
        let backend = match v.trim().to_ascii_lowercase().as_str() {
            "" | "skia" => Backend::Skia,
            "vello-cpu" | "vello_cpu" | "cpu" => Backend::VelloCpu,
            "vello-hybrid" | "vello_hybrid" | "hybrid" => Backend::VelloHybrid,
            "vello-gpu" | "vello_gpu" | "gpu" | "vello" => Backend::VelloGpu,
            "tiny-skia" | "tiny_skia" | "tinyskia" => backend_error(format!(
                "FSHOT_BACKEND={v:?}: tiny-skia is not a rasteriser of fshot (it is not Skia); expected {NAMES}"
            )),
            _ => backend_error(format!("unknown FSHOT_BACKEND={v:?}; expected {NAMES}")),
        };
        (backend, format!("FSHOT_BACKEND={v}"))
    } else if std::env::var_os("FSHOT_SKIA").is_some() {
        (Backend::Skia, "FSHOT_SKIA".to_string())
    } else if std::env::var_os("FSHOT_HYBRID").is_some() {
        (Backend::VelloHybrid, "FSHOT_HYBRID".to_string())
    } else if std::env::var_os("FSHOT_GPU").is_some() {
        (Backend::VelloGpu, "FSHOT_GPU".to_string())
    } else {
        (Backend::Skia, String::new())
    };
    if backend != Backend::Skia && std::env::var("FSHOT_ALLOW_NON_SKIA").as_deref() != Ok("1") {
        backend_error(format!(
            "{asked} asks for the {backend:?} rasteriser, and fshot paints with Skia only (the renderer's pictures \
             and its checks against Chrome are Skia's). For development or a benchmark, set FSHOT_ALLOW_NON_SKIA=1 \
             as well; otherwise leave FSHOT_BACKEND unset or set it to skia."
        ));
    }
    backend
}

struct SkiaOffscreen {
    surface_props: skia_safe::SurfaceProps,
    cache: SkiaSceneCache,
    /// LCD text: Skia's raster pipeline draws it only into N32 (BGRA here) surfaces, so paint BGRA and swap back.
    lcd: bool,
    /// The same props without a pixel geometry (grayscale text), for the second paint of pictures whose text
    /// landed on pixels that are not opaque: Skia's LCD there is worse than grayscale (docs/FIDELITY.md in
    /// known-future), and Chrome draws such text grayscale too.
    gray_props: skia_safe::SurfaceProps,
}

impl SkiaOffscreen {
    fn new() -> Self {
        skia_safe::graphics::set_font_cache_count_limit(512);
        skia_safe::graphics::set_typeface_cache_count_limit(256);
        skia_safe::graphics::set_resource_cache_total_bytes_limit(256 * 1024 * 1024);
        let dflt = skia_safe::SurfaceProps::default();
        // Match Chromium's Linux Skia text contrast (0.2) and gamma (1.2), same as anyrender_skia::surface_props().
        // FSHOT_LCD=rgb|bgr: LCD (sub-pixel) text, which Chrome draws on opaque layers (the real HyperFrames render
        // draws every element into its root layer, so its text is LCD; docs/FIDELITY.md). Skia only uses it where
        // the text is drawn axis-aligned outside an opacity layer.
        let geometry = match std::env::var("FSHOT_LCD").as_deref() {
            Ok("1") | Ok("rgb") => skia_safe::PixelGeometry::RGBH,
            Ok("bgr") => skia_safe::PixelGeometry::BGRH,
            _ => dflt.pixel_geometry(),
        };
        let surface_props = skia_safe::SurfaceProps::new_with_text_properties(
            dflt.flags(),
            geometry,
            0.2,
            1.2,
        );
        Self {
            surface_props,
            cache: SkiaSceneCache::new(),
            lcd: geometry != dflt.pixel_geometry(),
            gray_props: skia_safe::SurfaceProps::new_with_text_properties(dflt.flags(), dflt.pixel_geometry(), 0.2, 1.2),
        }
    }

    fn render(&mut self, document: &mut HtmlDocument, w: u32, h: u32, white: bool) -> Vec<u8> {
        let info = skia_safe::ImageInfo::new(
            (w as i32, h as i32),
            if self.lcd { skia_safe::ColorType::BGRA8888 } else { skia_safe::ColorType::RGBA8888 },
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
            if self.lcd {
                blitz_paint::record_glyph_rects(true);
            }
            let mut painter = SkiaScenePainter::new(surface.canvas(), &mut self.cache);
            paint_scene(&mut painter, document.as_mut(), 1.0, w, h, 0, 0);
        }
        if self.lcd {
            let rects: Vec<(usize, usize, usize, usize)> = blitz_paint::take_glyph_rects()
                .iter()
                .filter_map(|r| {
                    let (x0, y0) = (r.x0.floor().max(0.0) as usize, r.y0.floor().max(0.0) as usize);
                    let (x1, y1) = ((r.x1.ceil().max(0.0) as usize).min(w as usize), (r.y1.ceil().max(0.0) as usize).min(h as usize));
                    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
                })
                .collect();
            let row = (w as usize) * 4;
            let not_opaque = rects.iter().any(|&(x0, y0, x1, y1)| {
                (y0..y1).any(|y| buf[y * row + x0 * 4..y * row + x1 * 4].chunks_exact(4).any(|p| p[3] != 255))
            });
            if not_opaque {
                // paint again with grayscale text, and use it inside the text boxes wherever it is not opaque
                let gray_info = skia_safe::ImageInfo::new(
                    (w as i32, h as i32),
                    skia_safe::ColorType::BGRA8888,
                    skia_safe::AlphaType::Premul,
                    None,
                );
                let mut gray = vec![0u8; buf.len()];
                {
                    let mut surface = skia_safe::surfaces::wrap_pixels(&gray_info, &mut gray[..], Some(row), Some(&self.gray_props))
                        .expect("skia wrap_pixels failed");
                    surface.canvas().clear(if white { skia_safe::Color::WHITE } else { skia_safe::Color::TRANSPARENT });
                    let mut painter = SkiaScenePainter::new(surface.canvas(), &mut self.cache);
                    paint_scene(&mut painter, document.as_mut(), 1.0, w, h, 0, 0);
                }
                for &(x0, y0, x1, y1) in &rects {
                    for y in y0..y1 {
                        for x in x0..x1 {
                            let i = y * row + x * 4;
                            if gray[i + 3] != 255 {
                                buf[i..i + 4].copy_from_slice(&gray[i..i + 4]);
                            }
                        }
                    }
                }
            }
        }
        self.cache.next_gen();
        if self.lcd {
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
        }
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

/// known-future: the size a page asks to be laid out at, `<meta name="fshot-viewport" content="WxH">` in its head
/// (prepare writes it for a frame size other than 1920x1080).
fn page_viewport(html: &str) -> Option<(u32, u32)> {
    let head = &html[..html.find("</head>").unwrap_or(html.len())];
    let tail = &head[head.find("name=\"fshot-viewport\"")?..];
    let value = &tail[tail.find("content=\"")? + 9..];
    let (w, h) = value[..value.find('"')?].split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// known-future: a page that holds `<meta name="fshot-generics" content="chrome">` in its head resolves the generic
/// families as Chrome on Linux does (the Liberation fonts: blitz-dom's chrome_linux_generics, BLITZ_CHROME_GENERICS=1).
/// prepare writes it into pages read in HyperFrames' forms, where a family the page names but HyperFrames gives no face
/// falls back to them in Chrome (hf-test-chat's Montserrat, named only in a linked stylesheet).
fn page_wants_chrome_generics(html: &str) -> bool {
    let head = &html[..html.find("</head>").unwrap_or(html.len())];
    head.contains("<meta name=\"fshot-generics\" content=\"chrome\">")
}

/// known-future: the layout engine's own fetching (FSHOT_NET, header). With `local` (the default) it serves `file:` and
/// `data:` addresses through blitz-net and refuses every other one at once, without opening a connection: the handler
/// is told the fetch failed (so a stylesheet or a font no longer holds the layout up) and the address is recorded.
/// Every fetch that fails, refused or not, is recorded too, so the wait for pictures below does not wait 30 s for an
/// <img> whose file will never come.
struct FetchGuard {
    inner: Arc<Provider>,
    open: bool,
    refused: Arc<Mutex<Vec<String>>>,
    failed: Arc<Mutex<HashSet<String>>>,
    /// (wave 1a) FSHOT_DOCROOT: the folders a `file:` address must be inside, and the addresses that were not
    docroot: Option<Arc<Docroot>>,
    outside: Arc<Mutex<Vec<String>>>,
    /// (wave 1a) files being read under FSHOT_DOCROOT (the load waits for them as it waits for blitz-net's fetches)
    pending: Arc<AtomicUsize>,
}

/// FSHOT_NET: may the layout engine fetch from the web by itself (header)?
fn net_is_open() -> bool {
    match std::env::var("FSHOT_NET").as_deref() {
        Err(_) | Ok("") | Ok("local") | Ok("none") => false,
        Ok("open") => true,
        Ok(other) => {
            eprintln!("warning: FSHOT_NET={other:?} is not one of local, none, open: fetching only file: and data: addresses");
            false
        }
    }
}

impl FetchGuard {
    fn from_env(inner: Arc<Provider>) -> Self {
        FetchGuard {
            inner,
            open: net_is_open(),
            refused: Default::default(),
            failed: Default::default(),
            docroot: Docroot::from_env(),
            outside: Default::default(),
            pending: Default::default(),
        }
    }
}

/// known-future (wave 1a): FSHOT_DOCROOT, the folders every file read for a page must be inside (header). The
/// folders are named as PATH names them (`:` between them) and are resolved once, when first asked for.
struct Docroot {
    roots: Vec<PathBuf>,
}

/// Why a `file:` address was not read under FSHOT_DOCROOT
struct DocrootError {
    /// the address is outside every folder of FSHOT_DOCROOT (and is listed as refused), not merely unreadable
    outside: bool,
    message: String,
}

const OUTSIDE_DOCROOT: &str = "refused: the file is outside FSHOT_DOCROOT";

impl Docroot {
    fn from_env() -> Option<Arc<Docroot>> {
        static ROOTS: std::sync::OnceLock<Option<Arc<Docroot>>> = std::sync::OnceLock::new();
        ROOTS
            .get_or_init(|| {
                let value = std::env::var_os("FSHOT_DOCROOT").filter(|v| !v.is_empty())?;
                let mut roots: Vec<PathBuf> = Vec::new();
                for dir in std::env::split_paths(&value).filter(|d| !d.as_os_str().is_empty()) {
                    // (a folder that cannot be resolved would confine nothing while seeming to: that is an error)
                    match std::fs::canonicalize(&dir) {
                        Ok(real) if real.is_dir() => {
                            if !roots.contains(&real) {
                                roots.push(real);
                            }
                        }
                        Ok(real) => backend_error(format!("FSHOT_DOCROOT names {}, which is not a folder", real.display())),
                        Err(e) => backend_error(format!("FSHOT_DOCROOT names {}, which cannot be resolved: {e}", dir.display())),
                    }
                }
                if roots.is_empty() {
                    backend_error("FSHOT_DOCROOT names no folder".to_string());
                }
                Some(Arc::new(Docroot { roots }))
            })
            .clone()
    }

    fn contains(&self, real: &Path) -> bool {
        self.roots.iter().any(|root| real.starts_with(root))
    }

    /// The real path of `path` (symbolic links and `..` resolved) when it is inside a folder of FSHOT_DOCROOT.
    fn resolve(&self, path: &Path) -> Result<PathBuf, DocrootError> {
        match std::fs::canonicalize(path) {
            Ok(real) if self.contains(&real) => Ok(real),
            Ok(_) => Err(DocrootError { outside: true, message: OUTSIDE_DOCROOT.into() }),
            // no such file (or one that cannot be reached): outside when its name alone already is
            Err(e) => Err(DocrootError { outside: !self.contains(path), message: format!("io error: {e}") }),
        }
    }

    /// The bytes of the file a `file:` address names, read only when its real path is inside FSHOT_DOCROOT: the
    /// path is resolved, checked, opened, and the opened file is checked to be that very path and a regular file
    /// (so nothing that changes on disk between the check and the read is read in its place).
    fn read(&self, url: &url::Url) -> Result<Bytes, DocrootError> {
        let path = url.to_file_path().map_err(|()| DocrootError { outside: true, message: "not a file path".into() })?;
        self.read_path(&path)
    }

    /// `read` for a file named by its path: the page itself is read by this (`read_page`)
    fn read_path(&self, path: &Path) -> Result<Bytes, DocrootError> {
        use std::io::Read as _;
        let io = |e: std::io::Error| DocrootError { outside: false, message: format!("io error: {e}") };
        let real = self.resolve(path)?;
        let mut file = File::open(&real).map_err(io)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd as _;
            // (where /proc is not mounted there is nothing to compare with, and the check above stands alone)
            if let Ok(opened) = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
                if opened != real || !self.contains(&opened) {
                    return Err(DocrootError { outside: true, message: OUTSIDE_DOCROOT.into() });
                }
            }
        }
        if !file.metadata().map_err(io)?.is_file() {
            return Err(DocrootError { outside: false, message: "io error: not a regular file".into() });
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        Ok(Bytes::from(bytes))
    }
}

/// The page itself is a file read for the page: under FSHOT_DOCROOT it must be inside too.
fn docroot_allows_page(path: &Path) -> Result<(), String> {
    match Docroot::from_env() {
        Some(docroot) if !docroot.contains(path) => Err(outside_docroot(path)),
        _ => Ok(()),
    }
}

fn outside_docroot(path: &Path) -> String {
    format!("{} is outside FSHOT_DOCROOT", path.display())
}

/// Why a page file was not read
enum PageError {
    /// it is outside FSHOT_DOCROOT (the sentence says which file)
    Outside(String),
    Io(std::io::Error),
}

/// The text of the page file whose real path is `path`. Under FSHOT_DOCROOT it is read as every other file of a
/// page is (`Docroot::read_path`): the file that is opened is checked to be the one that was allowed, so a name
/// that is made to lead elsewhere between the check and the reading (`docroot_allows_page`, then this) is not
/// followed outside.
fn read_page(path: &Path) -> Result<String, PageError> {
    let Some(docroot) = Docroot::from_env() else { return std::fs::read_to_string(path).map_err(PageError::Io) };
    match docroot.read_path(path) {
        Ok(bytes) => String::from_utf8(bytes.into())
            .map_err(|_| PageError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, "stream did not contain valid UTF-8"))),
        Err(e) if e.outside => Err(PageError::Outside(outside_docroot(path))),
        Err(e) => Err(PageError::Io(std::io::Error::other(e.message.trim_start_matches("io error: ").to_string()))),
    }
}

struct WatchFailure {
    inner: Box<dyn NetHandler>,
    url: String,
    failed: Arc<Mutex<HashSet<String>>>,
}

impl NetHandler for WatchFailure {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        self.inner.bytes(resolved_url, bytes)
    }
    fn error(self: Box<Self>, url: String, error: String) {
        self.failed.lock().unwrap().insert(self.url.clone());
        self.inner.error(url, error)
    }
}

const REFUSED: &str = "refused: the layout engine fetches nothing by itself (FSHOT_NET=local)";

impl FetchGuard {
    /// (wave 1a) The folders a `file:` address is confined to, when FSHOT_DOCROOT names any: such an address is
    /// read here, by `Docroot::read`, and not by blitz-net.
    fn confined(&self, url: &url::Url) -> Option<&Arc<Docroot>> {
        self.docroot.as_ref().filter(|_| url.scheme() == "file")
    }

    /// Record a `file:` address that was not read under FSHOT_DOCROOT (never waited for; listed when it was outside)
    fn note_unread(failed: &Mutex<HashSet<String>>, outside: &Mutex<Vec<String>>, url: &str, error: &DocrootError) {
        failed.lock().unwrap().insert(url.to_string());
        if error.outside {
            let mut o = outside.lock().unwrap();
            if !o.iter().any(|u| u == url) {
                o.push(url.to_string());
            }
        }
    }

    /// May this address be fetched by the layout engine itself? `file:` and `data:` always; any other only with
    /// FSHOT_NET=open. One that may not is recorded (listed as refused, and never waited for).
    fn allows(&self, url: &url::Url) -> bool {
        if self.open || matches!(url.scheme(), "file" | "data") {
            return true;
        }
        let url = url.to_string();
        {
            let mut r = self.refused.lock().unwrap();
            if !r.contains(&url) {
                r.push(url.clone());
            }
        }
        self.failed.lock().unwrap().insert(url);
        false
    }
}

/// known-future (wave 0): the answer of a fetch made on the runtime's threads, handed to the thread that waits for it
struct SyncAnswer(std::sync::mpsc::Sender<Result<(String, Bytes), String>>);

impl NetHandler for SyncAnswer {
    fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
        let _ = self.0.send(Ok((resolved_url, bytes)));
    }
    fn error(self: Box<Self>, _url: String, error: String) {
        let _ = self.0.send(Err(error));
    }
}

impl NetProvider for FetchGuard {
    fn fetch(&self, doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url.to_string();
        if let Some(docroot) = self.confined(&request.url) {
            // (wave 1a) read on one of the runtime's threads, as blitz-net reads, and counted while it is read
            let (docroot, failed, outside, pending) =
                (Arc::clone(docroot), Arc::clone(&self.failed), Arc::clone(&self.outside), Arc::clone(&self.pending));
            pending.fetch_add(1, Ordering::SeqCst);
            tokio::task::spawn_blocking(move || {
                match docroot.read(&request.url) {
                    Ok(bytes) => handler.bytes(url, bytes),
                    Err(error) => {
                        Self::note_unread(&failed, &outside, &url, &error);
                        handler.error(url, error.message);
                    }
                }
                pending.fetch_sub(1, Ordering::SeqCst);
            });
            return;
        }
        if self.allows(&request.url) {
            let watched = Box::new(WatchFailure { inner: handler, url, failed: Arc::clone(&self.failed) });
            return self.inner.fetch(doc_id, request, watched);
        }
        handler.error(url, REFUSED.into());
    }

    /// known-future (wave 0): what an inline <svg>'s <image> names (blitz-dom asks for it while usvg builds the
    /// tree), by the same rule as every other fetch: with FSHOT_NET=local a `file:` or `data:` address is read here
    /// and now, and any other is refused and listed, with no connection opened. With FSHOT_NET=open an address that
    /// needs a connection is fetched on the runtime's threads and waited for (30 s at most; this is the main thread,
    /// which is none of them).
    fn fetch_sync(&self, doc_id: usize, request: Request) -> Option<Result<(String, Bytes), String>> {
        let url = request.url.to_string();
        if let Some(docroot) = self.confined(&request.url) {
            return Some(match docroot.read(&request.url) {
                Ok(bytes) => Ok((url, bytes)),
                Err(error) => {
                    Self::note_unread(&self.failed, &self.outside, &url, &error);
                    Err(error.message)
                }
            });
        }
        if !self.allows(&request.url) {
            return Some(Err(REFUSED.into()));
        }
        let result = match self.inner.fetch_sync(doc_id, request.clone()) {
            Some(result) => result,
            None => {
                let (tx, rx) = std::sync::mpsc::channel();
                self.inner.fetch(doc_id, request, Box::new(SyncAnswer(tx)));
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or_else(|_| Err("no answer in 30 s".into()))
            }
        };
        if result.is_err() {
            self.failed.lock().unwrap().insert(url);
        }
        Some(result)
    }
}

fn json_string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A page laid out, and what its load was refused: the web addresses the layout engine did not fetch by itself
/// (FSHOT_NET) and the `file:` addresses outside FSHOT_DOCROOT.
struct Loaded {
    document: HtmlDocument,
    refused: Vec<String>,
    outside: Vec<String>,
}

impl Loaded {
    /// The lines a load ends with on stderr (header): the renderer reads `FSHOT-REFUSED` to fetch what a page may have.
    fn report(&self) {
        for (tag, list) in [("FSHOT-REFUSED", &self.refused), ("FSHOT-DOCROOT-REFUSED", &self.outside)] {
            if !list.is_empty() {
                let list: Vec<String> = list.iter().map(|u| json_string(u)).collect();
                eprintln!("{tag} [{}]", list.join(","));
            }
        }
    }
}

/// The base URL of a page: its `<base href>` (which Blitz does not apply itself), or the address it has by itself
fn page_base_url(html: &str, own: impl FnOnce() -> String) -> String {
    html.find("<base href=\"")
        .and_then(|i| {
            let r = &html[i + 12..];
            r.find('"').map(|j| r[..j].to_string())
        })
        .unwrap_or_else(own)
}

fn load(input: &str, vw: u32, vh: u32) -> HtmlDocument {
    let path = std::fs::canonicalize(input).unwrap();
    if let Err(message) = docroot_allows_page(&path) {
        backend_error(message);
    }
    let html = match read_page(&path) {
        Ok(html) => html,
        Err(PageError::Outside(message)) => backend_error(message),
        Err(PageError::Io(e)) => panic!("{input} cannot be read: {e}"),
    };
    // A page that asks for Chrome's generic families is handed a font collection that has them (any other makes
    // its own, which has them with BLITZ_CHROME_GENERICS=1). It is not asked for through the environment: the
    // runtime's threads are running by now, and the environment is not changed under them (`serve`: serve.rs).
    let fonts = page_wants_chrome_generics(&html).then(|| blitz_dom::BaseDocument::default_font_context_with_chrome_generics(true));
    let url = page_base_url(&html, || url::Url::from_file_path(&path).unwrap().to_string());
    let loaded = load_html(&html, url, input, vw, vh, fonts);
    loaded.report();
    loaded.document
}

/// Lay a page out: `html` at the base URL `url`, in a `vw` x `vh` frame unless it names its own. `font_ctx` is the
/// font collection it starts from (`serve` hands every page a copy of one made once); with none the document
/// makes its own, as every one-page run does.
fn load_html(html: &str, url: String, input: &str, vw: u32, vh: u32, font_ctx: Option<blitz_dom::FontContext>) -> Loaded {
    let (vw, vh) = page_viewport(html).unwrap_or((vw, vh));
    let base = url::Url::parse(&url).ok();
    let net = Arc::new(Provider::new(None));
    let guard = Arc::new(FetchGuard::from_env(Arc::clone(&net)));
    let mut document = HtmlDocument::from_html(
        html,
        DocumentConfig {
            base_url: Some(url),
            net_provider: Some(Arc::clone(&guard) as _),
            viewport: Some(Viewport::new(vw, vh, 1.0, ColorScheme::Light)),
            font_ctx,
            ..Default::default()
        },
    );
    // (wave 1a) fshot paints a page as it stands, and after `paintset` or `serve` change its CSS, as it then
    // stands: no CSS transition is started, which would hold every changed value at what it was before
    document.as_mut().set_css_transitions_enabled(false);
    // Wait until every <img> has its decoded image, not just until the downloads are done: images are decoded
    // after the fetch and arrive as messages, and painting earlier silently leaves them out.
    let img = LocalName::from("img");
    let src = LocalName::from("src");
    let t0 = Instant::now();
    loop {
        document.resolve(0.0);
        let doc = document.as_ref();
        let failed = guard.failed.lock().unwrap().clone();
        let waiting = doc.tree().iter().any(|(_, n)| {
            n.data.is_element_with_tag_name(&img)
                && n.attr(src.clone()).is_some_and(|s| !s.is_empty())
                && n.element_data().is_none_or(|e| e.image_data().is_none())
                // (a picture whose fetch was refused or failed will not come: not waited for)
                && !n.attr(src.clone()).is_some_and(|s| {
                    base.as_ref().and_then(|b| b.join(s.trim()).ok()).is_some_and(|u| failed.contains(u.as_str()))
                })
        });
        if net.is_empty() && guard.pending.load(Ordering::SeqCst) == 0 && !doc.has_pending_critical_resources() && !waiting {
            break;
        }
        if t0.elapsed().as_secs() > 30 {
            eprintln!("warning: {input}: images still missing after 30 s");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    document.as_mut().resolve(0.0);
    let refused = guard.refused.lock().unwrap().clone();
    let outside = guard.outside.lock().unwrap().clone();
    Loaded { document, refused, outside }
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

/// The bytes `write_png` puts in its file (the same encoder, asked the same way: `serve` writes them itself, to
/// answer with their size and digest and to say so, not stop, when the file cannot be written)
fn png_bytes(buf: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(BufWriter::new(&mut out), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(buf).unwrap();
        wr.finish().unwrap();
    }
    out
}

/// A picture's file: the raw RGBA bytes when its name ends in `.rgba`, a PNG otherwise (`paint` and `paintset`)
fn write_picture(output: &str, buf: &mut [u8], w: u32, h: u32) {
    if output.ends_with(".rgba") {
        std::fs::write(output, &*buf).unwrap();
    } else {
        write_png(output, buf, w, h);
    }
}

/// The rasteriser of a run and what it keeps between pictures (one per process: every picture of a run, and
/// every page `serve` holds, is painted by the same one).
struct Painter {
    backend: Backend,
    gpu: Option<anyrender_vello::VelloImageRenderer>,
    skia: Option<SkiaOffscreen>,
    hybrid: Option<HybridOffscreen>,
}

impl Painter {
    fn new(backend: Backend) -> Self {
        Painter { backend, gpu: None, skia: None, hybrid: None }
    }

    /// A `w` x `h` picture of the page on a transparent background, straight (not premultiplied) RGBA: what
    /// `paint` and `paintset` write.
    fn picture(&mut self, document: &mut HtmlDocument, w: u32, h: u32) -> Vec<u8> {
        match self.backend {
            Backend::VelloGpu => {
                let r = self.gpu.get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(w, h));
                r.resize(w, h);
                let mut v = Vec::new();
                r.render_to_vec(|scene| paint_scene(scene, document.as_mut(), 1.0, w, h, 0, 0), &mut v);
                v
            }
            Backend::Skia => {
                let r = self.skia.get_or_insert_with(SkiaOffscreen::new);
                let mut b = r.render(document, w, h, false);
                unpremultiply(&mut b);
                b
            }
            Backend::VelloHybrid => {
                let r = self.hybrid.get_or_insert_with(|| HybridOffscreen::new(w, h));
                let mut b = r.render(document, w, h, false);
                unpremultiply(&mut b);
                b
            }
            Backend::VelloCpu => {
                let mut b = render(document, w, h, false);
                unpremultiply(&mut b);
                b
            }
        }
    }

    /// A `width` x `height` frame of the page on white: what `fshot W H in.html out.png` writes.
    fn frame(&mut self, document: &mut HtmlDocument, width: u32, height: u32) -> Vec<u8> {
        match self.backend {
            Backend::VelloGpu => {
                let r = self.gpu.get_or_insert_with(|| anyrender_vello::VelloImageRenderer::new(width, height));
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
            Backend::Skia => self.skia.get_or_insert_with(SkiaOffscreen::new).render(document, width, height, true),
            Backend::VelloHybrid => {
                self.hybrid.get_or_insert_with(|| HybridOffscreen::new(width, height)).render(document, width, height, true)
            }
            Backend::VelloCpu => render(document, width, height, true),
        }
    }

    /// The pass `dump` paints to learn the paint order (64 x 64, thrown away). (wave 0) Skia's too, as every
    /// picture is; another rasteriser only for a developer.
    fn paint_order_pass(&mut self, document: &mut HtmlDocument) {
        let _ = match self.backend {
            Backend::Skia => self.skia.get_or_insert_with(SkiaOffscreen::new).render(document, 64, 64, false),
            _ => render(document, 64, 64, false),
        };
    }
}

fn dump(input: &str, output: &str) {
    let mut document = load(input, VW, VH);
    let out = dump_json(&mut document, &mut Painter::new(selected_backend()));
    std::fs::write(output, out).unwrap();
}

/// What the painter logged in one paint-order pass over a page (blitz-paint's PAINT_LOG, SVG_LOG and SVG_PARTS):
/// every element in paint order with its transform and boxes, each inline SVG's drawn bounds, and the object
/// bounding box of each SVG part with an id.
struct PaintOrder {
    log: Vec<(NodeId, [f64; 6], f64, f64, [f64; 4])>,
    svg: Vec<(NodeId, [f64; 4])>,
    svg_parts: Vec<(NodeId, String, [f64; 4])>,
}

/// blitz-paint's logs, on for one paint-order pass, and off again when this goes, however it goes. `serve` catches
/// a pass that fails and carries on: a log left on would have every later picture painted without the culling of
/// what is off-screen and logged for nobody. (A lock the failure poisoned is taken, and mended, all the same.)
struct PaintLog;

impl PaintLog {
    fn on() -> PaintLog {
        // (made first: should what follows fail, the logs are put off again)
        let on = PaintLog;
        *blitz_paint::PAINT_LOG.lock().unwrap() = Some(Vec::new());
        blitz_paint::SVG_LOG.lock().unwrap().clear();
        blitz_paint::SVG_PARTS.lock().unwrap().clear();
        on
    }
}

impl Drop for PaintLog {
    fn drop(&mut self) {
        fn put<T>(log: &Mutex<T>, value: T) {
            *log.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = value;
            log.clear_poison();
        }
        put(&blitz_paint::PAINT_LOG, None);
        put(&blitz_paint::SVG_LOG, Vec::new());
        put(&blitz_paint::SVG_PARTS, Vec::new());
    }
}

fn paint_order(document: &mut HtmlDocument, painter: &mut Painter) -> PaintOrder {
    let _log = PaintLog::on();
    painter.paint_order_pass(document);
    PaintOrder {
        log: blitz_paint::PAINT_LOG.lock().unwrap().take().unwrap(),
        svg: std::mem::take(&mut *blitz_paint::SVG_LOG.lock().unwrap()),
        svg_parts: std::mem::take(&mut *blitz_paint::SVG_PARTS.lock().unwrap()),
    }
}

/// The text `fshot dump` writes for a laid-out page (the same bytes from `serve`'s `dump`: it is this function)
fn dump_json(document: &mut HtmlDocument, painter: &mut Painter) -> String {
    let PaintOrder { log, svg: svg_log, svg_parts } = paint_order(document, painter);
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
    // known-future: the object bounding box of each SVG part with an id, as [svg's data-i, id, [x0, y0, x1, y1]]
    out.push_str("],\"svgparts\":[");
    let mut first = true;
    for (id, part, bb) in svg_parts {
        let Some(i) = doc.get_node(id).and_then(|node| node.attr(di.clone())) else {
            continue;
        };
        if !first {
            out.push(',');
        }
        first = false;
        let part: String = part.chars().filter(|c| *c != '"' && *c != '\\' && !c.is_control()).collect();
        write!(out, "[{},\"{}\",[{},{},{},{}]]", i, part, bb[0], bb[1], bb[2], bb[3]).unwrap();
    }
    // known-future: each <video>'s computed object-fit and object-position (the renderer places the decoded frames
    // itself), as [data-i, fit, [x px, x %, y px, y %]]: a position is px + % of (box - object). Only <video> elements
    // are listed, so a page without one dumps as before.
    out.push_str("],\"video\":[");
    let mut first = true;
    let video = LocalName::from("video");
    for (_, node) in doc.tree().iter() {
        let (Some(i), Some(el)) = (node.attr(di.clone()), node.element_data()) else {
            continue;
        };
        if el.name.local != video {
            continue;
        }
        let Some(st) = node.primary_styles() else {
            continue;
        };
        let pos = st.get_position().object_position.clone();
        let part = |lp: &style::values::computed::LengthPercentage| {
            let z = lp.resolve(CSSPixelLength::new(0.0)).px();
            (z, lp.resolve(CSSPixelLength::new(100.0)).px() - z)
        };
        let (hx, hp) = part(&pos.horizontal);
        let (vy, vp) = part(&pos.vertical);
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},\"{:?}\",[{},{},{},{}]]", i, st.get_position().object_fit, hx, hp, vy, vp).unwrap();
    }
    // known-future: elements marked data-kf-place (the timed children of a HyperFrames root composition, which HyperFrames'
    // runtime positions and sizes when the film loads), as [data-i, position, content width, content height]; -1 for a
    // box with display:none
    out.push_str("],\"place\":[");
    let mut first = true;
    let place = LocalName::from("data-kf-place");
    for (_, node) in doc.tree().iter() {
        let (Some(i), Some(_)) = (node.attr(di.clone()), node.attr(place.clone())) else {
            continue;
        };
        let Some(st) = node.primary_styles() else {
            continue;
        };
        let l = node.final_layout();
        let none = format!("{:?}", node.taffy_display()) == "None";
        let cw = if none { -1.0 } else { l.size.width - l.padding.left - l.padding.right - l.border.left - l.border.right };
        let ch = if none { -1.0 } else { l.size.height - l.padding.top - l.padding.bottom - l.border.top - l.border.bottom };
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},\"{:?}\",{},{}]", i, st.get_box().position, cw, ch).unwrap();
    }
    // known-future: the computed colours of elements marked data-kf-cs (a GSAP tween of color, backgroundColor or
    // borderColor starts from them), as [data-i, color, background-color, border-top-color, clip-path, transform,
    // transform-origin, display, translate, rotate, scale, width, height, top, right, bottom, left], each colour [r, g, b, a] 0-255, the rest as CSS text (a layer's own clip-path is drawn by the
    // compositor; GSAP starts from the transform)
    // known-future (outside round 5): for elements marked data-kf-cq (a GSAP x or y given in container units, which
    // GSAP keeps and Chrome resolves against the element's query container), the content box those units resolve
    // against, as [data-i, width, height]: for the width the nearest ancestor with inline-size or size containment, for
    // the height the nearest with size containment (stylo's ContainerSizeQuery), the viewport when there is none.
    out.push_str("],\"cq\":[");
    let mut first = true;
    let cq_mark = LocalName::from("data-kf-cq");
    let vp = doc.viewport().window_size;
    for (_, node) in doc.tree().iter() {
        let (Some(i), Some(_)) = (node.attr(di.clone()), node.attr(cq_mark.clone())) else {
            continue;
        };
        let (mut w, mut h): (Option<f32>, Option<f32>) = (None, None);
        let mut p = node.parent;
        while let (Some(pid), true) = (p, w.is_none() || h.is_none()) {
            let Some(pn) = doc.get_node(pid) else { break };
            if let Some(st) = pn.primary_styles() {
                use style::values::specified::box_::ContainerType;
                let ct = st.get_box().container_type;
                let l = pn.final_layout();
                if w.is_none() && ct.intersects(ContainerType::INLINE_SIZE | ContainerType::SIZE) {
                    w = Some(l.size.width - l.padding.left - l.padding.right - l.border.left - l.border.right);
                }
                if h.is_none() && ct.contains(ContainerType::SIZE) {
                    h = Some(l.size.height - l.padding.top - l.padding.bottom - l.border.top - l.border.bottom);
                }
            }
            p = pn.parent;
        }
        if !first {
            out.push(',');
        }
        first = false;
        write!(out, "[{},{},{}]", i, w.unwrap_or(vp.0 as f32), h.unwrap_or(vp.1 as f32)).unwrap();
    }
    out.push_str("],\"cs\":[");
    let mut first = true;
    let cs_mark = LocalName::from("data-kf-cs");
    for (_, node) in doc.tree().iter() {
        let (Some(i), Some(_)) = (node.attr(di.clone()), node.attr(cs_mark.clone())) else {
            continue;
        };
        let Some(st) = node.primary_styles() else {
            continue;
        };
        let current = st.get_inherited_text().color;
        let rgba = |c: style::color::AbsoluteColor| {
            let c = c.to_color_space(style::color::ColorSpace::Srgb);
            let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as i32;
            format!("[{},{},{},{}]", q(c.components.0), q(c.components.1), q(c.components.2), q(c.alpha))
        };
        if !first {
            out.push(',');
        }
        first = false;
        let esc = |t: String| -> String {
            t.chars().flat_map(|c| match c {
                '"' => vec!['\\', '"'],
                '\\' => vec!['\\', '\\'],
                c if c.is_control() => vec![' '],
                c => vec![c],
            }).collect()
        };
        // (outside round 4) and its computed width and height, as CSS text: GSAP starts a width or height tween from
        // the element's value (a bar grown from a stylesheet's `width: 0` was drawn whole from its first frame)
        // (outside round 7) and its computed top, right, bottom and left, as CSS text (a percentage stays one): GSAP starts
        // a tween of them from the element's value (launch-claude-paper's composers, from top: 50% to 87%, jumped to 87%)
        let (clip, transform, origin, display, translate, rotate, scale, width, height, top, right, bottom, left) = {
            use style_traits::ToCss;
            (
                st.get_svg().clip_path.to_css_string(),
                st.get_box().transform.to_css_string(),
                st.get_box().transform_origin.to_css_string(),
                st.get_box().display.to_css_string(),
                st.get_box().translate.to_css_string(),
                st.get_box().rotate.to_css_string(),
                st.get_box().scale.to_css_string(),
                st.get_position().width.to_css_string(),
                st.get_position().height.to_css_string(),
                st.get_position().top.to_css_string(),
                st.get_position().right.to_css_string(),
                st.get_position().bottom.to_css_string(),
                st.get_position().left.to_css_string(),
            )
        };
        write!(
            out,
            "[{},{},{},{},\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\",\"{}\"]",
            i,
            rgba(current),
            rgba(st.get_background().background_color.resolve_to_absolute(&current)),
            rgba(st.get_border().border_top_color.resolve_to_absolute(&current)),
            esc(clip),
            esc(transform),
            esc(origin),
            esc(display),
            esc(translate),
            esc(rotate),
            esc(scale),
            esc(width),
            esc(height),
            esc(top),
            esc(right),
            esc(bottom),
            esc(left)
        )
        .unwrap();
    }
    // known-future (wave 0): every element whose computed mask-image has a layer that is not `none`, as
    // [data-i, the computed value as CSS text]. The cascade is Stylo's, so a style attribute's `mask-image: none`,
    // `!important`, @layer, @supports and @media decide here as they do in a browser: an element a stylesheet rule
    // gives a mask and its own style attribute takes it from again is not listed (prepare read the rules alone: in 17
    // of jensen-yes' 63 scenes it took an element for masked that was not).
    out.push_str("],\"mask\":[");
    let mut first = true;
    for (_, node) in doc.tree().iter() {
        let Some(i) = node.attr(di.clone()) else {
            continue;
        };
        let Some(st) = node.primary_styles() else {
            continue;
        };
        let mask = &st.get_svg().mask_image;
        if mask.0.iter().all(|image| matches!(image, style::values::generics::image::GenericImage::None)) {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        use style_traits::ToCss;
        write!(out, "[{},{}]", i, json_string(&mask.to_css_string())).unwrap();
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
    out
}

/// known-future (outside round 6): `fshot boxes in1.html out1.json [in2.html out2.json ...]` lays each page out (no
/// painting) and writes, for every element that has a `data-i`, the boxes CSSOM View's measurements are read from: the
/// timeline reader answers a script's `offsetWidth`, `getBoundingClientRect()` & co. while the page builds from them
/// (known_future/reader/measure.mjs). One record per element:
///   [data-i, flags, [x, y, w, h], offsetParent, [offsetLeft, offsetTop, offsetWidth, offsetHeight],
///    [clientLeft, clientTop, clientWidth, clientHeight], [scrollWidth, scrollHeight], [x0, y0, x1, y1],
///    [padding l t r b], [border l t r b], [margin l t r b]]
/// [x, y, w, h] is the border box before transforms, document-relative, as getClientBoundingRect has it (a non-atomic
/// inline: the union of its line fragments); [x0, y0, x1, y1] its bounds under the CSS transforms of it and of its
/// containing blocks (getBoundingClientRect); offsetParent the data-i of the offset parent, -1 for none, -2 for one
/// without a data-i. The values are unrounded CSS px; the reader rounds them as Chrome does. Flags: 1 not rendered
/// (display: none, on it or above it), 2 a non-atomic inline, 4 a CSS transform of its own, 8 box-sizing: border-box,
/// 16 position: fixed. Pages are laid out one after another in one process at their own viewport size.
fn boxes(pairs: &[String]) {
    for pair in pairs.chunks(2) {
        let (input, output) = (&pair[0], &pair[1]);
        let document = load(input, VW, VH);
        std::fs::write(output, boxes_json(document.as_ref())).unwrap();
    }
}

/// The boxes of one element that has computed styles, as `fshot boxes` lists them (the comment on `boxes` says
/// what each is): `serve`'s `analyze` and `resolve` give the same numbers under their names.
pub(crate) struct ElementBoxes {
    /// 1 not rendered, 2 a non-atomic inline, 4 a CSS transform of its own, 8 box-sizing: border-box, 16 position: fixed
    pub(crate) flags: u32,
    /// the border box before transforms, document-relative: x, y, width, height
    pub(crate) rect: [f64; 4],
    /// its bounds under the CSS transforms of it and of its containing blocks: x0, y0, x1, y1
    pub(crate) bounds: [f64; 4],
    /// offsetLeft, offsetTop, offsetWidth, offsetHeight
    pub(crate) offset: [f64; 4],
    /// the offset parent (none for a fixed box, one that is not rendered, or one that has none)
    pub(crate) offset_parent: Option<NodeId>,
    /// clientLeft, clientTop, clientWidth, clientHeight
    pub(crate) client: [f64; 4],
    /// scrollWidth, scrollHeight
    pub(crate) scroll: [f64; 2],
    /// left, top, right, bottom
    pub(crate) padding: [f64; 4],
    pub(crate) border: [f64; 4],
    pub(crate) margin: [f64; 4],
}

/// The affine from the page origin to a node's border box: each containing block's position, scroll and CSS
/// transform, as the painter applies them (blitz-paint's node_transform), with the unrounded layout
fn page_transform(doc: &blitz_dom::BaseDocument, id: NodeId, depth: u32) -> peniko::kurbo::Affine {
    use peniko::kurbo::{Affine, Vec2};
    let Some(node) = doc.get_node(id) else { return Affine::IDENTITY };
    let l = node.unrounded_layout().location;
    // (wave 2) a sticky box is painted where its sticky offset puts it
    let sticky = node.sticky_offset.get();
    let own = node.transform().as_deref().copied().unwrap_or_default();
    let local = Affine::translate(Vec2::new((l.x + sticky.x) as f64, (l.y + sticky.y) as f64)) * own;
    match node.containing_block() {
        Some(p) if depth < 4096 => {
            let (sx, sy) = doc.get_node(p).map(|n| (n.scroll_offset().x, n.scroll_offset().y)).unwrap_or((0.0, 0.0));
            page_transform(doc, p, depth + 1) * Affine::translate(Vec2::new(-sx, -sy)) * local
        }
        _ => local,
    }
}

pub(crate) fn element_boxes(doc: &blitz_dom::BaseDocument, id: NodeId, node: &blitz_dom::Node, st: &style::properties::ComputedValues) -> ElementBoxes {
    let fin = |v: f64| if v.is_finite() { v } else { 0.0 };
    let mut flags = 0u32;
    let mut p = Some(id);
    while let Some(q) = p {
        let Some(n) = doc.get_node(q) else { break };
        if n.element_data().is_some() && format!("{:?}", n.taffy_display()) == "None" {
            flags |= 1;
            break;
        }
        p = n.parent;
    }
    let inline = node.is_non_atomic_inline();
    if inline {
        flags |= 2;
    }
    if node.transform().is_some() && !inline {
        flags |= 4;
    }
    if format!("{:?}", st.get_position().box_sizing) == "BorderBox" {
        flags |= 8;
    }
    if format!("{:?}", st.get_box().position) == "Fixed" {
        flags |= 16;
    }
    let l = *node.unrounded_layout();
    let (rect, bounds, off, op) = if flags & 1 != 0 {
        ([0.0; 4], [0.0; 4], [0.0; 4], None)
    } else {
        let r = doc.get_client_bounding_rect(id).unwrap_or(blitz_dom::BoundingRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        let o = doc.offset_rect(id).unwrap_or(blitz_dom::BoundingRect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        let op = match node.offset_parent() {
            _ if flags & 16 != 0 => None,
            Some(pn) => Some(pn.id),
            None => None,
        };
        let b = if inline {
            // the fragments are relative to the inline root's border box, under its transforms
            match (node.inline_fragment_boxes(), node.inline_root_ancestor()) {
                (Some(fr), Some(root)) => {
                    let m = page_transform(doc, root.id, 0);
                    let mut u: Option<Rect> = None;
                    for f in fr {
                        let q = m.transform_rect_bbox(Rect::new(f.left as f64, f.top as f64, f.right as f64, f.bottom as f64));
                        u = Some(u.map_or(q, |a| a.union(q)));
                    }
                    u.map_or([0.0; 4], |u| [u.x0, u.y0, u.x1, u.y1])
                }
                _ => [r.x, r.y, r.x + r.width, r.y + r.height],
            }
        } else {
            let q = page_transform(doc, id, 0).transform_rect_bbox(Rect::new(0.0, 0.0, l.size.width as f64, l.size.height as f64));
            [q.x0, q.y0, q.x1, q.y1]
        };
        ([r.x, r.y, r.width, r.height], b, [o.x, o.y, o.width, o.height], op)
    };
    let none = flags & 1 != 0;
    let z = |v: f32| if none { 0.0 } else { v as f64 };
    ElementBoxes {
        flags,
        rect: rect.map(fin),
        bounds: bounds.map(fin),
        offset: off.map(fin),
        offset_parent: op,
        client: [
            z(l.border.left),
            z(l.border.top),
            if none || inline { 0.0 } else { node.client_width() as f64 },
            if none || inline { 0.0 } else { node.client_height() as f64 },
        ],
        scroll: [
            if none || inline { 0.0 } else { node.scroll_width() as f64 },
            if none || inline { 0.0 } else { node.scroll_height() as f64 },
        ],
        padding: [z(l.padding.left), z(l.padding.top), z(l.padding.right), z(l.padding.bottom)],
        border: [z(l.border.left), z(l.border.top), z(l.border.right), z(l.border.bottom)],
        margin: [z(l.margin.left), z(l.margin.top), z(l.margin.right), z(l.margin.bottom)],
    }
}

/// The text `fshot boxes` writes for a laid-out page (the same bytes from `serve`'s `boxes`)
fn boxes_json(doc: &blitz_dom::BaseDocument) -> String {
    let di = LocalName::from("data-i");
    let index = |id: NodeId| -> Option<String> { doc.get_node(id).and_then(|n| n.attr(di.clone()).map(|s| s.to_string())) };
    let mut out = String::from("{\"boxes\":[");
    let mut first = true;
    for (id, node) in doc.tree().iter() {
        let Some(i) = node.attr(di.clone()) else { continue };
        let Some(st) = node.primary_styles() else { continue };
        let b = element_boxes(doc, id, node, &st);
        // (the data-i of the offset parent, -1 for none, -2 for one without a data-i)
        let op = match b.offset_parent {
            Some(pid) => index(pid).and_then(|s| s.parse::<i64>().ok()).unwrap_or(-2),
            None => -1,
        };
        if !first {
            out.push(',');
        }
        first = false;
        write!(
            out,
            "[{},{},[{},{},{},{}],{},[{},{},{},{}],[{},{},{},{}],[{},{}],[{},{},{},{}],[{},{},{},{}],[{},{},{},{}],[{},{},{},{}]]",
            i, b.flags,
            b.rect[0], b.rect[1], b.rect[2], b.rect[3],
            op,
            b.offset[0], b.offset[1], b.offset[2], b.offset[3],
            b.client[0], b.client[1], b.client[2], b.client[3],
            b.scroll[0], b.scroll[1],
            b.bounds[0], b.bounds[1], b.bounds[2], b.bounds[3],
            b.padding[0], b.padding[1], b.padding[2], b.padding[3],
            b.border[0], b.border[1], b.border[2], b.border[3],
            b.margin[0], b.margin[1], b.margin[2], b.margin[3]
        )
        .unwrap();
    }
    let vp = doc.viewport().window_size;
    write!(out, "],\"vp\":[{},{}]}}", vp.0, vp.1).unwrap();
    out
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
    let page = PaintsetPage::of(document.as_ref()).expect("base page needs <style id=\"fshot-pic\">/**/</style>");
    let mut painter = Painter::new(backend);
    let (mut t_restyle, mut t_paint, mut t_write, mut n) = (0u128, 0u128, 0u128, 0);
    let jobs = std::fs::read_to_string(jobfile).unwrap();
    for rec in paintset_records(&jobs) {
        let rec = rec.unwrap();
        let t0 = Instant::now();
        let restore = page.apply(&mut document, &rec).unwrap();
        document.as_mut().resolve(0.0);
        t_restyle += t0.elapsed().as_millis();
        let t1 = Instant::now();
        let mut buf = painter.picture(&mut document, rec.w, rec.h);
        t_paint += t1.elapsed().as_millis();
        let t2 = Instant::now();
        write_picture(rec.output, &mut buf, rec.w, rec.h);
        t_write += t2.elapsed().as_millis();
        PaintsetPage::clear(&mut document, restore);
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

/// One picture of a `paintset` job file: where it goes, its size, the attributes it sets and its CSS
struct PaintsetRecord<'a> {
    output: &'a str,
    w: u32,
    h: u32,
    /// (data-i, attribute, value)
    attrs: Vec<(&'a str, &'a str, &'a str)>,
    css: &'a str,
}

/// The records of a `paintset` job file, in order (the format is on `paintset`); one that is not in that format
/// is an `Err` that says what is missing.
fn paintset_records(jobs: &str) -> impl Iterator<Item = Result<PaintsetRecord<'_>, String>> {
    jobs.split("\u{1e}\n").filter(|rec| !rec.trim().is_empty()).map(|rec| {
        let (head, css) = rec.split_once("\n---\n").ok_or("a record has no line \"---\" before its CSS")?;
        let mut lines = head.lines();
        let first: Vec<&str> = lines.next().ok_or("a record has no \"OUT W H\" line")?.split(' ').collect();
        let size = |k: usize| first.get(k).and_then(|v| v.parse::<u32>().ok()).ok_or_else(|| format!("\"{}\" is not \"OUT W H\"", first.join(" ")));
        let (w, h) = (size(1)?, size(2)?);
        let mut attrs = Vec::new();
        for l in lines {
            let mut it = l.splitn(4, ' ');
            let (_, i, attr, value) = (
                it.next(),
                it.next().ok_or_else(|| format!("\"{l}\" is not \"@ <data-i> <attr> <value>\""))?,
                it.next().ok_or_else(|| format!("\"{l}\" is not \"@ <data-i> <attr> <value>\""))?,
                it.next().unwrap_or(""),
            );
            attrs.push((i, attr, value));
        }
        Ok(PaintsetRecord { output: first[0], w, h, attrs, css })
    })
}

/// What `paintset` changes in its base page: the text of <style id="fshot-pic"> and the elements by `data-i`
struct PaintsetPage {
    style_text: NodeId,
    by_i: std::collections::HashMap<String, NodeId>,
}

impl PaintsetPage {
    /// `None` for a page without <style id="fshot-pic"> (with a text in it: `/**/`)
    fn of(doc: &blitz_dom::BaseDocument) -> Option<Self> {
        let di = LocalName::from("data-i");
        let idn = LocalName::from("id");
        let (mut style_text, mut by_i): (Option<NodeId>, std::collections::HashMap<String, NodeId>) = (None, Default::default());
        for (_, node) in doc.tree().iter() {
            if let Some(i) = node.attr(di.clone()) {
                by_i.insert(i.to_string(), node.id);
            }
            if node.attr(idn.clone()) == Some("fshot-pic") {
                style_text = node.children.first().copied();
            }
        }
        Some(PaintsetPage { style_text: style_text?, by_i })
    }

    /// Put a picture's CSS and attributes into the page (not yet resolved); the attributes set are returned, to
    /// be cleared after the picture. Nothing is changed when the record names an element the page has not.
    fn apply(&self, document: &mut HtmlDocument, rec: &PaintsetRecord) -> Result<Vec<(NodeId, QualName)>, String> {
        let mut set: Vec<(NodeId, QualName, &str)> = Vec::new();
        for (i, attr, value) in &rec.attrs {
            let id = *self.by_i.get(*i).ok_or_else(|| format!("the page has no element with data-i=\"{i}\""))?;
            set.push((id, QualName::new(None, ns!(), LocalName::from(*attr)), value));
        }
        let mut m = document.as_mut().mutate();
        m.set_node_text(self.style_text, rec.css);
        let mut restore = Vec::new();
        for (id, q, value) in set {
            restore.push((id, q.clone()));
            m.set_attribute(id, q, value);
        }
        Ok(restore)
    }

    /// After a picture: the attributes it set are removed (the CSS stays until the next picture replaces it)
    fn clear(document: &mut HtmlDocument, restore: Vec<(NodeId, QualName)>) {
        if !restore.is_empty() {
            let mut m = document.as_mut().mutate();
            for (id, q) in restore {
                m.clear_attribute(id, q);
            }
        }
    }
}

fn main() {
    // fshot draws HyperFrames pages, whose compiler sets `text-rendering: geometricPrecision` on every element: that
    // is the default here (FSHOT_GEOMETRIC_PRECISION=0 gives the text of fshot before 2026-11-18). The real render
    // also draws text LCD on opaque layers; FSHOT_LCD=rgb does that too, but the colour fringes cost about 5 s of
    // NVENC time on a 16.5-minute episode, so it is off by default (docs/FIDELITY.md in known-future).
    for (k, v) in [("FSHOT_GEOMETRIC_PRECISION", "auto")] {
        if std::env::var_os(k).is_none() {
            #[allow(unused_unsafe)]
            unsafe {
                std::env::set_var(k, v)
            };
        }
    }
    // (wave 0) the rasteriser is settled before anything is read: a run that names another one stops here, whatever
    // its mode (header)
    let _ = selected_backend();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _rt = rt.enter(); // blitz-net fetches assets on the runtime's threads
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "dump" => dump(&args[2], &args[3]),
        "boxes" => boxes(&args[2..]),
        "paintset" => paintset(&args[2], &args[3]),
        "serve" => serve::serve(),
        "paint" => {
            let mut painter = Painter::new(selected_backend());
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
                let mut buf = painter.picture(&mut document, w, h);
                t_paint += t1.elapsed().as_millis();
                let t2 = Instant::now();
                write_picture(output, &mut buf, w, h);
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
            let mut painter = Painter::new(selected_backend());
            let width: u32 = args[1].parse().unwrap();
            let height: u32 = args[2].parse().unwrap();
            for pair in args[3..].chunks(2) {
                let t0 = Instant::now();
                let mut document = load(&pair[0], width, height);
                let t_layout = t0.elapsed().as_millis();
                let mut buf = painter.frame(&mut document, width, height);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pages_own_frame_size_and_base_url() {
        let page = "<html><head><meta name=\"fshot-viewport\" content=\"800x600\"><base href=\"file:///proj/\"></head><body></body></html>";
        assert_eq!(page_viewport(page), Some((800, 600)));
        assert_eq!(page_base_url(page, || "file:///own.html".into()), "file:///proj/");
        let plain = "<html><head></head><body><meta name=\"fshot-viewport\" content=\"1x1\"></body></html>";
        assert_eq!(page_viewport(plain), None, "only a <meta> of the head counts");
        assert_eq!(page_base_url(plain, || "file:///own.html".into()), "file:///own.html");
        assert_eq!(page_viewport("<head><meta name=\"fshot-viewport\" content=\"0x600\"></head>"), None);
        assert!(page_wants_chrome_generics("<head><meta name=\"fshot-generics\" content=\"chrome\"></head>"));
        assert!(!page_wants_chrome_generics(plain));
    }

    #[test]
    fn paintset_job_files() {
        let jobs = "a.png 320 200\n---\n/**/\n\u{1e}\nb.rgba 64 32\n@ 13 transform translate(6 4)\n@ 12 fill\n---\n.x{opacity:.5}\n.y{}\n";
        let recs: Vec<PaintsetRecord> = paintset_records(jobs).collect::<Result<_, _>>().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!((recs[0].output, recs[0].w, recs[0].h, recs[0].attrs.len(), recs[0].css), ("a.png", 320, 200, 0, "/**/\n"));
        assert_eq!((recs[1].output, recs[1].w, recs[1].h), ("b.rgba", 64, 32));
        assert_eq!(recs[1].attrs, vec![("13", "transform", "translate(6 4)"), ("12", "fill", "")]);
        assert_eq!(recs[1].css, ".x{opacity:.5}\n.y{}\n");
        // a record that is not in the format is said to be, not a panic
        for bad in ["a.png 320 200\n/**/\n", "a.png 320\n---\n/**/\n", "a.png x 200\n---\n/**/\n", "a.png 1 1\n@ 13\n---\n/**/\n"] {
            assert!(paintset_records(bad).next().unwrap().is_err(), "{bad:?}");
        }
        assert_eq!(paintset_records("\n  \n").count(), 0);
    }

    #[test]
    fn json_strings_are_json() {
        assert_eq!(json_string("a\"b\\c\n\u{1}\u{2028}d"), "\"a\\\"b\\\\c\\u000a\\u0001\\u2028d\"");
        assert_eq!(analyze::num(1.5), "1.5");
        assert_eq!(analyze::num(f64::NAN), "null");
        assert_eq!(analyze::num(f64::INFINITY), "null");
        assert_eq!(analyze::nums(&[0.0, -2.25, 1920.0]), "[0,-2.25,1920]");
    }

    #[test]
    fn the_properties_a_request_names() {
        blitz_dom::BaseDocument::init_style_prefs();
        let props = analyze::Props::parse(["opacity", " Mask-Image ", "overflow", "--tint", "opacity", "no-such-property", "--Tint"]);
        assert_eq!(props.names, ["opacity", "mask-image", "overflow", "--tint", "--Tint"]);
        assert_eq!(props.unsupported, ["no-such-property"]);
        // every property of the default lists is one the style engine has, but those said not to be
        let defaults = analyze::Props::parse(analyze::DEFAULT_PROPS.iter().copied());
        assert_eq!(defaults.unsupported, ["content-visibility", "transform-box", "offset-distance"]);
        for name in ["mask-image", "clip-path", "filter", "backdrop-filter", "opacity", "mix-blend-mode", "background-blend-mode", "transform",
            "transform-origin", "perspective", "transform-style", "backface-visibility", "overflow-x", "overflow-y", "visibility", "display", "position", "z-index"]
        {
            assert!(defaults.names.iter().any(|n| n == name), "{name}");
        }
        assert!(analyze::Props::parse(analyze::DEFAULT_USED.iter().copied()).unsupported.is_empty());
        assert!(analyze::Props::all().len() > 150);
        // the shorthands are the values the layout has a say in: read again on an element whose style did not change
        let mixed = analyze::Props::parse(["opacity", "margin", "--tint", "grid-template", "margin-left", "inset", "padding"]);
        assert_eq!(mixed.laid_out, [1, 3, 5, 6]);
        assert!(defaults.laid_out.is_empty() && analyze::Props::all().laid_out.is_empty(), "no shorthand is asked for unasked");
    }

    #[test]
    fn the_paint_log_is_off_after_a_pass_that_fails() {
        let failed = std::panic::catch_unwind(|| {
            let _log = PaintLog::on();
            assert!(blitz_paint::PAINT_LOG.lock().unwrap().is_some());
            blitz_paint::SVG_LOG.lock().unwrap().push((NodeId::from_u64(1), [0.0; 4]));
            // (the pass fails while it writes to the log, which poisons the log's lock)
            let _held = blitz_paint::PAINT_LOG.lock().unwrap();
            panic!("a pass that fails");
        });
        assert!(failed.is_err());
        assert!(!blitz_paint::PAINT_LOG.is_poisoned());
        assert!(blitz_paint::PAINT_LOG.lock().unwrap().is_none());
        assert!(blitz_paint::SVG_LOG.lock().unwrap().is_empty());
        // and a pass after it logs as the first one did
        drop(PaintLog::on());
        assert!(blitz_paint::PAINT_LOG.lock().unwrap().is_none());
    }

    #[test]
    fn a_folder_of_fshot_docroot() {
        let dir = std::env::temp_dir().join(format!("fshot-docroot-test-{}", std::process::id()));
        let (root, outside) = (dir.join("root"), dir.join("outside"));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("in.txt"), b"inside").unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link_out.txt")).unwrap();
            std::os::unix::fs::symlink(root.join("in.txt"), root.join("link_in.txt")).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("dir_out")).unwrap();
        }
        let docroot = Docroot { roots: vec![std::fs::canonicalize(&root).unwrap()] };
        let url = |p: &Path| url::Url::from_file_path(p).unwrap();
        assert_eq!(&docroot.read(&url(&root.join("in.txt"))).ok().unwrap()[..], b"inside");
        assert_eq!(&docroot.read(&url(&root.join("sub/../in.txt"))).ok().unwrap()[..], b"inside");
        #[cfg(unix)]
        assert_eq!(&docroot.read(&url(&root.join("link_in.txt"))).ok().unwrap()[..], b"inside");
        let refused = |p: PathBuf| match docroot.read(&url(&p)) {
            Ok(_) => panic!("{} was read", p.display()),
            Err(e) => e.outside,
        };
        assert!(refused(outside.join("secret.txt")));
        assert!(refused(root.join("../outside/secret.txt")));
        #[cfg(unix)]
        {
            assert!(refused(root.join("link_out.txt")));
            assert!(refused(root.join("dir_out/secret.txt")));
        }
        assert!(refused(outside.join("nothing.txt")), "a file that is not there, outside: refused as outside");
        assert!(!refused(root.join("nothing.txt")), "a file that is not there, inside: not there");
        assert!(!refused(root.join("sub")), "a folder is no file");
        assert!(docroot.read(&url::Url::parse("data:text/plain,x").unwrap()).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
