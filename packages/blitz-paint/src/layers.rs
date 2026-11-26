use anyrender::{Filter, PaintScene};
use kurbo::{Affine, Shape};
use peniko::{BlendMode, Fill};
use std::sync::OnceLock;
use std::{cell::Cell, sync::Arc};

/// known-future (wave 0): how many layers (clips, opacity and blend groups, filters) one scene may push.
///
/// It was 1024, counted over the whole scene and not by depth, and every layer past it was left out without a
/// word: the 1026th element of a page with `overflow: hidden`, an opacity or a `clip-path` was painted unclipped
/// and opaque. Skia has no such limit, so the ceiling is now far past any page a renderer is given (about a
/// million), and a scene that does reach it says so on stderr, once when the first layer is refused and once with
/// the count when the scene is done. `BLITZ_LAYER_LIMIT` sets another ceiling (a test of the message, or a
/// rasteriser that needs one).
const DEFAULT_LAYER_LIMIT: u32 = 1 << 20;

fn layer_limit() -> u32 {
    static LIMIT: OnceLock<u32> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("BLITZ_LAYER_LIMIT")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_LAYER_LIMIT)
    })
}

pub(crate) struct LayerManager {
    layers_used: Cell<u32>,
    layer_depth: Cell<u32>,
    layers_wanted: Cell<u32>,
    /// Layers asked for past the limit: not pushed, and reported (never silently)
    layers_refused: Cell<u32>,
    limit: u32,

    #[allow(unused)] // Only used for debugging. Enabled as required.
    layer_depth_used: Cell<u32>,
}

impl Default for LayerManager {
    fn default() -> Self {
        Self {
            layers_used: Cell::new(0),
            layer_depth: Cell::new(0),
            layers_wanted: Cell::new(0),
            layers_refused: Cell::new(0),
            limit: layer_limit(),
            layer_depth_used: Cell::new(0),
        }
    }
}

impl Drop for LayerManager {
    fn drop(&mut self) {
        let refused = self.layers_refused.get();
        if refused > 0 {
            eprintln!(
                "blitz-paint: error: {refused} of the {} layers of this scene were not applied (limit {}): \
                 what they clip, fade, blend or filter is painted without it",
                self.layers_wanted.get(),
                self.limit
            );
        }
    }
}

impl LayerManager {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn maybe_with_layer<S: PaintScene, F: FnOnce(&mut S)>(
        &self,
        scene: &mut S,
        condition: bool,
        blend_mode: BlendMode,
        opacity: f32,
        transform: Affine,
        shape: &impl Shape,
        filter: Option<Arc<Filter>>,
        backdrop_filter: Option<Arc<Filter>>,
        paint_layer: F,
    ) {
        let layer_used = self.maybe_push_layer(
            scene,
            condition,
            blend_mode,
            opacity,
            transform,
            shape,
            filter,
            backdrop_filter,
        );
        paint_layer(scene);
        self.maybe_pop_layer(scene, layer_used);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn maybe_push_layer(
        &self,
        scene: &mut impl PaintScene,
        condition: bool,
        blend_mode: BlendMode,
        opacity: f32,
        transform: Affine,
        shape: &impl Shape,
        filter: Option<Arc<Filter>>,
        backdrop_filter: Option<Arc<Filter>>,
    ) -> bool {
        if !condition {
            return false;
        }
        self.layers_wanted.update(|x| x + 1);

        // The ceiling (above): a layer past it is not pushed, and that is said, not hidden
        if self.layers_used.get() >= self.limit {
            if self.layers_refused.get() == 0 {
                eprintln!(
                    "blitz-paint: error: this scene asks for more than {} layers (clips, opacity and blend groups, \
                     filters); the ones past the limit are not applied (BLITZ_LAYER_LIMIT sets the limit)",
                    self.limit
                );
            }
            self.layers_refused.update(|x| x + 1);
            return false;
        }

        // Actually push the layer
        if blend_mode == BlendMode::default()
            && opacity == 1.0
            && filter.is_none()
            && backdrop_filter.is_none()
        {
            scene.push_clip_layer(Fill::NonZero, transform, shape);
        } else {
            scene.push_layer(
                Fill::NonZero,
                blend_mode,
                opacity,
                transform,
                shape,
                filter,
                backdrop_filter,
            );
        };

        // Update accounting
        self.layers_used.update(|x| x + 1);
        self.layer_depth.update(|x| x + 1);
        self.layer_depth_used
            .update(|x| x.max(self.layer_depth.get()));

        true
    }

    pub(crate) fn maybe_pop_layer(&self, scene: &mut impl PaintScene, condition: bool) {
        if condition {
            scene.pop_layer();
            self.layer_depth.update(|x| x - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyrender::{Scene, recording::RenderCommand};
    use kurbo::Rect;
    use peniko::{Compose, Mix};

    #[test]
    fn blend_layers_are_not_replaced_by_clips() {
        for blend in [
            Mix::Multiply.into(),
            BlendMode::new(Mix::Normal, Compose::PlusLighter),
        ] {
            let manager = LayerManager::default();
            let mut scene = Scene::new();
            manager.maybe_with_layer(
                &mut scene,
                true,
                blend,
                1.0,
                Affine::IDENTITY,
                &Rect::ZERO,
                None,
                None,
                |_| {},
            );
            assert!(
                matches!(&scene.commands[0], RenderCommand::PushLayer(layer) if layer.blend == blend)
            );
            assert!(matches!(scene.commands[1], RenderCommand::PopLayer));
        }
    }

    #[test]
    fn normal_layers_clip_unless_isolation_is_forced() {
        for (opacity, isolated) in [(1.0, false), (1.0 - f32::EPSILON, true)] {
            let manager = LayerManager::default();
            let mut scene = Scene::new();
            manager.maybe_with_layer(
                &mut scene,
                true,
                BlendMode::default(),
                opacity,
                Affine::IDENTITY,
                &Rect::ZERO,
                None,
                None,
                |_| {},
            );
            assert_eq!(
                matches!(scene.commands[0], RenderCommand::PushLayer(_)),
                isolated
            );
            assert!(matches!(scene.commands[1], RenderCommand::PopLayer));
        }
    }

    #[test]
    fn disabled_or_exhausted_layers_do_not_pop() {
        let manager = LayerManager::default();
        let mut scene = Scene::new();
        for condition in [false, true] {
            manager.layers_used.set(manager.limit);
            manager.maybe_with_layer(
                &mut scene,
                condition,
                Mix::Multiply.into(),
                1.0,
                Affine::IDENTITY,
                &Rect::ZERO,
                None,
                None,
                |_| {},
            );
        }
        assert!(scene.commands.is_empty());
        assert_eq!(manager.layers_refused.get(), 1);
    }

    #[test]
    fn a_scene_of_many_layers_keeps_every_one() {
        // more than the old ceiling of 1024, counted over the scene and not by depth
        let manager = LayerManager::default();
        assert!(manager.limit >= 16384, "the ceiling is {}", manager.limit);
        let mut scene = Scene::new();
        for _ in 0..5000 {
            manager.maybe_with_layer(
                &mut scene,
                true,
                BlendMode::default(),
                0.5,
                Affine::IDENTITY,
                &Rect::ZERO,
                None,
                None,
                |_| {},
            );
        }
        assert_eq!(scene.commands.len(), 10000);
        assert_eq!(manager.layers_refused.get(), 0);
    }
}
