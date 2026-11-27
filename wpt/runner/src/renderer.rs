//! The renderers compiled into the runner, one chosen at runtime with `--renderer`.

#[cfg(not(any(feature = "skia", feature = "cpu", feature = "tiny-skia", feature = "gpu")))]
compile_error!("enable at least one of the `skia`, `cpu`, `tiny-skia` and `gpu` features");

#[cfg(any(feature = "cpu", feature = "tiny-skia", feature = "gpu"))]
use anyrender::{AntiAliasing, ImageRenderer, PaintScene};
use blitz_dom::BaseDocument;
#[cfg(any(feature = "cpu", feature = "tiny-skia", feature = "gpu"))]
use blitz_dom::util::Color;
use blitz_paint::paint_scene;
#[cfg(any(feature = "cpu", feature = "tiny-skia", feature = "gpu"))]
use peniko::Fill;
#[cfg(any(feature = "cpu", feature = "tiny-skia", feature = "gpu"))]
use peniko::kurbo::Rect;

use crate::{HEIGHT, SCALE, WIDTH};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RendererKind {
    #[cfg(feature = "skia")]
    Skia,
    #[cfg(feature = "cpu")]
    VelloCpu,
    #[cfg(feature = "tiny-skia")]
    TinySkia,
    #[cfg(feature = "gpu")]
    Vello,
}

impl RendererKind {
    /// The compiled-in renderers. The first is the default.
    pub fn all() -> Vec<Self> {
        vec![
            #[cfg(feature = "skia")]
            Self::Skia,
            #[cfg(feature = "cpu")]
            Self::VelloCpu,
            #[cfg(feature = "tiny-skia")]
            Self::TinySkia,
            #[cfg(feature = "gpu")]
            Self::Vello,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "skia")]
            Self::Skia => "skia",
            #[cfg(feature = "cpu")]
            Self::VelloCpu => "vello-cpu",
            #[cfg(feature = "tiny-skia")]
            Self::TinySkia => "tiny-skia",
            #[cfg(feature = "gpu")]
            Self::Vello => "vello",
        }
    }
}

/// Splits `--renderer <name>` (or `--renderer=<name>`) out of the command line arguments.
///
/// Returns the selected renderer (the first compiled-in one if there is no `--renderer`) and the
/// remaining non-flag arguments.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> (RendererKind, Vec<String>) {
    let mut name = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--renderer" {
            name = args.next();
        } else if let Some(value) = arg.strip_prefix("--renderer=") {
            name = Some(value.to_string());
        } else if !arg.starts_with('-') {
            rest.push(arg);
        }
    }

    let all = RendererKind::all();
    let kind = match name {
        None => all[0],
        Some(name) => all
            .iter()
            .copied()
            .find(|kind| kind.name() == name)
            .unwrap_or_else(|| {
                let available: Vec<_> = all.iter().map(|kind| kind.name()).collect();
                eprintln!(
                    "Unknown renderer {name:?}. Available: {}",
                    available.join(", ")
                );
                std::process::exit(1);
            }),
    };
    (kind, rest)
}

// One per worker thread, so the size difference between variants doesn't matter.
#[allow(clippy::large_enum_variant)]
pub enum Renderer {
    #[cfg(feature = "skia")]
    Skia(SkiaRenderer),
    #[cfg(feature = "cpu")]
    VelloCpu(anyrender_vello_cpu::VelloCpuImageRenderer),
    #[cfg(feature = "tiny-skia")]
    TinySkia(anyrender_tiny_skia::TinySkiaImageRenderer),
    #[cfg(feature = "gpu")]
    Vello(anyrender_vello::VelloImageRenderer),
}

impl Renderer {
    pub fn new(kind: RendererKind, width: u32, height: u32) -> Self {
        match kind {
            #[cfg(feature = "skia")]
            RendererKind::Skia => Self::Skia(SkiaRenderer::new(width, height)),
            #[cfg(feature = "cpu")]
            RendererKind::VelloCpu => Self::VelloCpu(ImageRenderer::new(width, height)),
            #[cfg(feature = "tiny-skia")]
            RendererKind::TinySkia => Self::TinySkia(ImageRenderer::new(width, height)),
            #[cfg(feature = "gpu")]
            RendererKind::Vello => Self::Vello(ImageRenderer::new(width, height)),
        }
    }

    /// Renders `document` on a white background into `buffer`.
    pub fn render_to_vec(&mut self, document: &mut BaseDocument, buffer: &mut Vec<u8>) {
        match self {
            #[cfg(feature = "skia")]
            Self::Skia(renderer) => renderer.render_to_vec(document, buffer),
            #[cfg(feature = "cpu")]
            Self::VelloCpu(renderer) => {
                renderer.render_to_vec(|scene| draw(scene, document), buffer)
            }
            #[cfg(feature = "tiny-skia")]
            Self::TinySkia(renderer) => {
                renderer.render_to_vec(|scene| draw(scene, document), buffer)
            }
            #[cfg(feature = "gpu")]
            Self::Vello(renderer) => renderer.render_to_vec(|scene| draw(scene, document), buffer),
        }
    }
}

/// known-future (wave 1a): Skia on the CPU, set up as `apps/fshot` sets it up for the frames it paints (its
/// `SkiaOffscreen`): a premultiplied RGBA8888 raster surface over the buffer, Chromium's Linux text contrast
/// (0.2) and gamma (1.2), cleared to white, painted through `anyrender_skia`'s scene painter with one scene
/// cache kept between pictures. What passes here is drawn the same way in the known-future renderer.
#[cfg(feature = "skia")]
pub struct SkiaRenderer {
    info: skia_safe::ImageInfo,
    props: skia_safe::SurfaceProps,
    cache: anyrender_skia::SkiaSceneCache,
}

// SAFETY: the runner keeps one renderer per worker thread (a `ThreadLocal`, which asks for `Send` because it
// drops its values on whichever thread drops it). A renderer is made by the worker that uses it and is never
// used by another; all that happens on another thread is its drop, once every worker has finished. What keeps
// `SkiaSceneCache` from being `Send` is its handle to Skia's font manager, whose reference count is atomic and
// which is not tied to the thread that made it.
#[cfg(feature = "skia")]
unsafe impl Send for SkiaRenderer {}

#[cfg(feature = "skia")]
impl SkiaRenderer {
    fn new(width: u32, height: u32) -> Self {
        skia_safe::graphics::set_font_cache_count_limit(512);
        skia_safe::graphics::set_typeface_cache_count_limit(256);
        skia_safe::graphics::set_resource_cache_total_bytes_limit(256 * 1024 * 1024);
        let default = skia_safe::SurfaceProps::default();
        Self {
            info: skia_safe::ImageInfo::new(
                (width as i32, height as i32),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Premul,
                None,
            ),
            props: skia_safe::SurfaceProps::new_with_text_properties(
                default.flags(),
                default.pixel_geometry(),
                0.2,
                1.2,
            ),
            cache: anyrender_skia::SkiaSceneCache::new(),
        }
    }

    /// Renders `document` on a white background into `buffer` (opaque, so premultiplied is straight RGBA).
    fn render_to_vec(&mut self, document: &mut BaseDocument, buffer: &mut Vec<u8>) {
        let row = self.info.width() as usize * 4;
        buffer.clear();
        buffer.resize(row * self.info.height() as usize, 0);
        {
            let mut surface =
                skia_safe::surfaces::wrap_pixels(&self.info, &mut buffer[..], Some(row), Some(&self.props))
                    .expect("skia wrap_pixels failed");
            surface.canvas().clear(skia_safe::Color::WHITE);
            let mut painter = anyrender_skia::SkiaScenePainter::new(surface.canvas(), &mut self.cache);
            paint_scene(&mut painter, document, SCALE, WIDTH, HEIGHT, 0, 0);
        }
        self.cache.next_gen();
    }
}

#[cfg(any(feature = "cpu", feature = "tiny-skia", feature = "gpu"))]
fn draw(scene: &mut impl PaintScene, document: &mut BaseDocument) {
    scene.reset();
    scene.fill(
        Fill::NonZero,
        Default::default(),
        Color::WHITE,
        Default::default(),
        &Rect::new(0.0, 0.0, WIDTH as f64, HEIGHT as f64),
        AntiAliasing::Enabled,
    );
    paint_scene(scene, document, SCALE, WIDTH, HEIGHT, 0, 0);
}
