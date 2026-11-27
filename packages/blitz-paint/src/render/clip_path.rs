use super::ElementCx;
use kurbo::{BezPath, Circle, Ellipse, Point, Rect, Shape, SvgArc, Vec2};
use style::values::computed::basic_shape::{BasicShape, ClipPath};
use style::values::computed::{Angle, CSSPixelLength, LengthPercentage};
use style::values::generics::basic_shape::{
    ArcSize, ArcSweep, AxisEndPoint, CommandEndPoint, ControlPoint, ControlReference,
    GenericBasicShape, GenericPathOrShapeFunction, GenericShapeCommand, GenericShapeRadius,
    ShapeBox, ShapeGeometryBox,
};
use style::values::generics::position::{GenericPosition, GenericPositionOrAuto};

impl ElementCx<'_, '_> {
    /// Compute the rectangle (in scaled pixels, relative to the border box origin) described by
    /// the CSS 2 `clip` property, if it applies. `clip` only applies to absolutely positioned
    /// elements; `auto` components resolve to the corresponding border box edge.
    pub(super) fn css_clip_rect(&self) -> Option<Rect> {
        use style::computed_values::position::T as Position;
        use style::values::computed::LengthOrAuto;
        use style::values::generics::ClipRectOrAuto;

        if !matches!(
            self.style.get_box().position,
            Position::Absolute | Position::Fixed
        ) {
            return None;
        }
        let ClipRectOrAuto::Rect(rect) = &self.style.get_effects().clip else {
            return None;
        };

        let border_box = self.frame.border_box;
        let resolve = |component: &LengthOrAuto, auto: f64| match component {
            LengthOrAuto::Auto => auto,
            LengthOrAuto::LengthPercentage(length) => length.px() as f64 * self.scale,
        };
        let x0 = resolve(&rect.left, 0.0);
        let y0 = resolve(&rect.top, 0.0);
        let x1 = resolve(&rect.right, border_box.width());
        let y1 = resolve(&rect.bottom, border_box.height());

        Some(Rect::new(x0, y0, x1.max(x0), y1.max(y0)))
    }

    /// Compute the clip-path BezPath (if any) for this element.
    /// Returns `None` if clip-path is `none` or unsupported.
    pub(super) fn clip_path_shape(&self) -> Option<BezPath> {
        match self.style.get_clip_path() {
            ClipPath::None => None,
            ClipPath::Url(_) => {
                // URL references (e.g. `clip-path: url(#myClip)`) are not yet supported
                None
            }
            ClipPath::Shape(basic_shape, geometry_box) => {
                let reference_box = self.resolve_geometry_box(geometry_box);
                self.basic_shape_to_path(basic_shape, reference_box)
            }
            ClipPath::Box(geometry_box) => {
                let reference_box = self.resolve_geometry_box(geometry_box);
                Some(Rect::from(reference_box).into_path(0.1))
            }
        }
    }

    /// Resolve a ShapeGeometryBox to a concrete rectangle (x, y, width, height) in scaled pixels
    ///
    /// For SVG elements without associated CSS layout box, the used value for content-box and padding-box is fill-box and for border-box and margin-box is stroke-box.
    /// For elements with associated CSS layout box, the used value for fill-box is content-box and for stroke-box and view-box is border-box.
    fn resolve_geometry_box(&self, geometry_box: &ShapeGeometryBox) -> ReferenceBox {
        match geometry_box {
            ShapeGeometryBox::ElementDependent
            | ShapeGeometryBox::StrokeBox
            | ShapeGeometryBox::ViewBox
            | ShapeGeometryBox::ShapeBox(ShapeBox::BorderBox) => ReferenceBox {
                x: 0.0,
                y: 0.0,
                width: self.frame.border_box.width() / self.scale,
                height: self.frame.border_box.height() / self.scale,
            },
            ShapeGeometryBox::ShapeBox(ShapeBox::PaddingBox) => {
                let padding_box = self.frame.padding_box();
                ReferenceBox {
                    x: self.frame.border_width.x0 / self.scale,
                    y: self.frame.border_width.y0 / self.scale,
                    width: padding_box.width() / self.scale,
                    height: padding_box.height() / self.scale,
                }
            }
            ShapeGeometryBox::FillBox | ShapeGeometryBox::ShapeBox(ShapeBox::ContentBox) => {
                let content_box = self.frame.content_box();
                ReferenceBox {
                    x: (self.frame.border_width.x0 + self.frame.padding_width.x0) / self.scale,
                    y: (self.frame.border_width.y0 + self.frame.padding_width.y0) / self.scale,
                    width: content_box.width() / self.scale,
                    height: content_box.height() / self.scale,
                }
            }
            ShapeGeometryBox::ShapeBox(ShapeBox::MarginBox) => {
                // Margin box is not tracked in CssBox, fall back to border box
                ReferenceBox {
                    x: 0.0,
                    y: 0.0,
                    width: self.frame.border_box.width() / self.scale,
                    height: self.frame.border_box.height() / self.scale,
                }
            }
        }
    }

    /// Convert a CSS basic-shape to a kurbo BezPath
    fn basic_shape_to_path(
        &self,
        shape: &BasicShape,
        reference_box: ReferenceBox,
    ) -> Option<BezPath> {
        let w = reference_box.width;
        let h = reference_box.height;
        let ox = reference_box.x;
        let oy = reference_box.y;

        match shape {
            GenericBasicShape::Circle(circle) => {
                let (cx, cy) = resolve_position(&circle.position, w, h, ox, oy);
                let r = resolve_circle_radius(&circle.radius, w, h, cx - ox, cy - oy);
                Some(Circle::new(Point::new(cx, cy), r).into_path(0.1))
            }
            GenericBasicShape::Ellipse(ellipse) => {
                let (cx, cy) = resolve_position(&ellipse.position, w, h, ox, oy);
                let rx = resolve_ellipse_radius(&ellipse.semiaxis_x, w, cx - ox);
                let ry = resolve_ellipse_radius(&ellipse.semiaxis_y, h, cy - oy);
                Some(Ellipse::new(Point::new(cx, cy), (rx, ry), 0.0).into_path(0.1))
            }
            GenericBasicShape::Polygon(polygon) => {
                let mut path = BezPath::new();
                let _fill = &polygon.fill;
                let coords = &polygon.coordinates;

                if coords.is_empty() {
                    return None;
                }

                for (i, coord) in coords.iter().enumerate() {
                    let x = ox + resolve_lp(&coord.0, w);
                    let y = oy + resolve_lp(&coord.1, h);
                    if i == 0 {
                        path.move_to(Point::new(x, y));
                    } else {
                        path.line_to(Point::new(x, y));
                    }
                }
                path.close_path();

                Some(path)
            }
            GenericBasicShape::Rect(inset_rect) => {
                // inset() function: inset(top right bottom left round border-radius)
                let top = resolve_lp(&inset_rect.rect.0, h);
                let right = resolve_lp(&inset_rect.rect.1, w);
                let bottom = resolve_lp(&inset_rect.rect.2, h);
                let left = resolve_lp(&inset_rect.rect.3, w);

                let x0 = ox + left;
                let y0 = oy + top;
                let x1 = ox + w - right;
                let y1 = oy + h - bottom;

                if x1 <= x0 || y1 <= y0 {
                    // known-future (outside round 4): an inset with no area clips everything away, as in
                    // Chrome (an empty path; render_element then draws nothing), not nothing at all
                    return Some(BezPath::new());
                }

                // known-future (outside round 4): the `round` radii of inset(), as border-radius
                // (each corner's horizontal radius, percentages of the inset rectangle's width),
                // scaled down together where they would overlap
                let (iw, ih) = (x1 - x0, y1 - y0);
                let rad = |c: &style::values::generics::border::GenericBorderCornerRadius<
                    style::values::generics::NonNegative<LengthPercentage>,
                >| c.0.width.0.resolve(CSSPixelLength::new(iw as f32)).px() as f64;
                let r = &inset_rect.round;
                let (tl, tr, br, bl) = (rad(&r.top_left), rad(&r.top_right), rad(&r.bottom_right), rad(&r.bottom_left));
                if tl > 0.0 || tr > 0.0 || br > 0.0 || bl > 0.0 {
                    let f = [iw / (tl + tr).max(1e-9), iw / (bl + br).max(1e-9), ih / (tl + bl).max(1e-9), ih / (tr + br).max(1e-9)]
                        .into_iter()
                        .fold(1.0f64, f64::min);
                    return Some(
                        kurbo::RoundedRect::new(x0, y0, x1, y1, (tl * f, tr * f, br * f, bl * f)).into_path(0.1),
                    );
                }
                Some(kurbo::Rect::new(x0, y0, x1, y1).into_path(0.1))
            }
            GenericBasicShape::PathOrShape(path_or_shape) => match path_or_shape {
                GenericPathOrShapeFunction::Path(path) => svg_path_to_bezpath(
                    path.commands(),
                    |v| *v as f64,
                    |v| *v as f64,
                    |v| *v as f64,
                )
                .map(|mut p| {
                    p.apply_affine(kurbo::Affine::translate((ox, oy)));
                    p
                }),
                GenericPathOrShapeFunction::Shape(shape) => svg_path_to_bezpath(
                    &shape.commands,
                    move |v: &LengthPercentage| {
                        v.resolve(CSSPixelLength::new(w as f32)).px() as f64
                    },
                    move |v: &LengthPercentage| {
                        v.resolve(CSSPixelLength::new(h as f32)).px() as f64
                    },
                    move |v: &Angle| v.degrees() as f64,
                )
                .map(|mut p| {
                    p.apply_affine(kurbo::Affine::translate((ox, oy)));
                    p
                }),
            },
        }
    }
}

/// A resolved reference box in scaled pixel coordinates
#[derive(Clone, Copy)]
struct ReferenceBox {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl From<ReferenceBox> for kurbo::Rect {
    fn from(reference_box: ReferenceBox) -> Self {
        kurbo::Rect::new(
            reference_box.x,
            reference_box.y,
            reference_box.x + reference_box.width,
            reference_box.y + reference_box.height,
        )
    }
}

/// Resolve a LengthPercentage value against a basis length (already scaled)
fn resolve_lp(lp: &LengthPercentage, basis: f64) -> f64 {
    let basis_css = CSSPixelLength::new(basis as f32);
    lp.resolve(basis_css).px() as f64
}

/// Resolve a position (or auto => center) to absolute coordinates
fn resolve_position(
    position: &GenericPositionOrAuto<style::values::computed::Position>,
    w: f64,
    h: f64,
    ox: f64,
    oy: f64,
) -> (f64, f64) {
    match position {
        GenericPositionOrAuto::Auto => (ox + w / 2.0, oy + h / 2.0),
        GenericPositionOrAuto::Position(pos) => {
            let x = ox + resolve_lp(&pos.horizontal, w);
            let y = oy + resolve_lp(&pos.vertical, h);
            (x, y)
        }
    }
}

/// The radius of `circle()` (CSS Shapes 1, 3.1) in a reference box `width` x `height`, the centre
/// at (`cx`, `cy`) from the box's origin.
///
/// known-future (wave 2): a percentage is a percentage of `sqrt(width^2 + height^2) / sqrt(2)`, not
/// of the width (`circle(42% at 50% 45%)` on a 170 x 340 box has a radius of 112.9px, not 71.4px).
/// The keywords measure distances, so they are never negative, whichever side of the box the centre
/// is on: with the centre outside the box, the closest side or corner is the near one, at the
/// centre's distance from the box (it was clamped to a radius of zero).
fn resolve_circle_radius(
    radius: &GenericShapeRadius<LengthPercentage>,
    width: f64,
    height: f64,
    cx: f64,
    cy: f64,
) -> f64 {
    let (near_x, far_x) = side_distances(width, cx);
    let (near_y, far_y) = side_distances(height, cy);
    match radius {
        GenericShapeRadius::Length(lp) => {
            resolve_lp(&lp.0, width.hypot(height) / std::f64::consts::SQRT_2).max(0.0)
        }
        GenericShapeRadius::ClosestSide => near_x.min(near_y),
        GenericShapeRadius::FarthestSide => far_x.max(far_y),
        GenericShapeRadius::ClosestCorner => near_x.hypot(near_y),
        GenericShapeRadius::FarthestCorner => far_x.hypot(far_y),
    }
}

/// One semi-axis of `ellipse()` (CSS Shapes 1, 3.1): `size` is the reference box's dimension in
/// that axis and `c` the centre's distance from the box's origin in it.
///
/// known-future (wave 2): `closest-side` and `farthest-side` look at the two sides of this axis
/// alone (they took the nearest of all four sides, as a circle does). An ellipse through a corner
/// keeps the proportions of the one through the sides, so each semi-axis is `sqrt(2)` times that
/// side's distance, as for a radial gradient's ending shape.
fn resolve_ellipse_radius(radius: &GenericShapeRadius<LengthPercentage>, size: f64, c: f64) -> f64 {
    let (near, far) = side_distances(size, c);
    match radius {
        GenericShapeRadius::Length(lp) => resolve_lp(&lp.0, size).max(0.0),
        GenericShapeRadius::ClosestSide => near,
        GenericShapeRadius::FarthestSide => far,
        GenericShapeRadius::ClosestCorner => near * std::f64::consts::SQRT_2,
        GenericShapeRadius::FarthestCorner => far * std::f64::consts::SQRT_2,
    }
}

/// The distances from a centre at `c` to the nearer and the farther of the two sides of a box that
/// runs from 0 to `size` in one axis.
fn side_distances(size: f64, c: f64) -> (f64, f64) {
    let (a, b) = (c.abs(), (size - c).abs());
    (a.min(b), a.max(b))
}

type GenericPathCommand<Angle, N> = GenericShapeCommand<Angle, N, GenericPosition<N, N>, N>;

/// Convert an SVG path() to a kurbo BezPath.
/// The returned path is in the path's own coordinate system (origin at 0,0).
/// The caller is responsible for translating it to the reference box origin.
fn svg_path_to_bezpath<Angle: Copy, N>(
    commands: &[GenericPathCommand<Angle, N>],
    resolve_x: impl Fn(&N) -> f64,
    resolve_y: impl Fn(&N) -> f64,
    resolve_angle: impl Fn(&Angle) -> f64,
) -> Option<BezPath> {
    if commands.is_empty() {
        return None;
    }

    let mut bez = BezPath::new();
    let mut cur = Point::ZERO;
    let mut subpath_start = cur;
    // Tracks the last control point for smooth continuation.
    let mut last_control: Option<Point> = None;

    for cmd in commands {
        match &cmd {
            GenericShapeCommand::Close => {
                bez.close_path();
                cur = subpath_start;
                last_control = None;
            }
            GenericShapeCommand::Move { point } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                bez.move_to(p);
                cur = p;
                subpath_start = p;
                last_control = None;
            }
            GenericShapeCommand::Line { point } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                bez.line_to(p);
                cur = p;
                last_control = None;
            }
            GenericShapeCommand::HLine { x } => {
                cur.x = resolve_axis_endpoint(x, cur.x, &resolve_x);
                bez.line_to(cur);
                last_control = None;
            }
            GenericShapeCommand::VLine { y } => {
                cur.y = resolve_axis_endpoint(y, cur.y, &resolve_y);
                bez.line_to(cur);
                last_control = None;
            }
            GenericShapeCommand::CubicCurve {
                point,
                control1,
                control2,
            } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                let c1 = resolve_control_point(control1, cur, p, &resolve_x, &resolve_y);
                let c2 = resolve_control_point(control2, cur, p, &resolve_x, &resolve_y);
                bez.curve_to(c1, c2, p);
                last_control = Some(c2);
                cur = p;
            }
            GenericShapeCommand::QuadCurve { point, control1 } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                let c1 = resolve_control_point(control1, cur, p, &resolve_x, &resolve_y);
                bez.quad_to(c1, p);
                last_control = Some(c1);
                cur = p;
            }
            GenericShapeCommand::SmoothCubic { point, control2 } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                let c2 = resolve_control_point(control2, cur, p, &resolve_x, &resolve_y);
                let c1 = reflect_point(last_control, cur);
                bez.curve_to(c1, c2, p);
                last_control = Some(c2);
                cur = p;
            }
            GenericShapeCommand::SmoothQuad { point } => {
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                let c1 = reflect_point(last_control, cur);
                bez.quad_to(c1, p);
                last_control = Some(c1);
                cur = p;
            }
            GenericShapeCommand::Arc {
                point,
                radii,
                arc_sweep,
                arc_size,
                rotate,
            } => {
                // SVG arc commands are complex; approximate as a line for now
                let p = resolve_endpoint(point, cur, &resolve_x, &resolve_y);
                let svg_arc = SvgArc {
                    from: cur,
                    to: p,
                    radii: Vec2 {
                        x: resolve_x(&radii.rx),
                        y: resolve_y(radii.ry.as_ref().unwrap_or(&radii.rx)),
                    },
                    x_rotation: resolve_angle(rotate),
                    large_arc: match arc_size {
                        ArcSize::Large => true,
                        ArcSize::Small => false,
                    },
                    // TODO: check mapping is correct
                    sweep: match arc_sweep {
                        ArcSweep::Ccw => true,
                        ArcSweep::Cw => false,
                    },
                };
                let arc = kurbo::Arc::from_svg_arc(&svg_arc)?;
                for el in arc.append_iter(0.1) {
                    bez.push(el);
                }
                last_control = None;
                cur = p;
            }
        }
    }

    Some(bez)
}

/// Resolve a CommandEndPoint to an absolute Point.
/// `ToPosition` is absolute; `ByCoordinate` is relative to `cur`.
fn resolve_endpoint<N>(
    endpoint: &CommandEndPoint<GenericPosition<N, N>, N>,
    cur: Point,
    resolve_x: impl Fn(&N) -> f64,
    resolve_y: impl Fn(&N) -> f64,
) -> Point {
    match endpoint {
        CommandEndPoint::ToPosition(pos) => {
            Point::new(resolve_x(&pos.horizontal), resolve_y(&pos.vertical))
        }
        CommandEndPoint::ByCoordinate(coord) => {
            Point::new(cur.x + resolve_x(&coord.x), cur.y + resolve_y(&coord.y))
        }
    }
}

/// Resolve a ControlPoint to an absolute Point.
/// - `Absolute`: uses the position directly.
/// - `Relative`: the coordinate pair is offset from a base point determined by `ControlReference`:
///   - `Start` (default): relative to the command's starting point (`cur`).
///   - `End`: relative to the command's end point (`end`).
///   - `Origin`: relative to the reference box origin (0,0 in path coordinates).
fn resolve_control_point<N>(
    control_point: &ControlPoint<GenericPosition<N, N>, N>,
    cur: Point,
    end: Point,
    resolve_x: impl Fn(&N) -> f64,
    resolve_y: impl Fn(&N) -> f64,
) -> Point {
    match control_point {
        ControlPoint::Absolute(pos) => {
            Point::new(resolve_x(&pos.horizontal), resolve_y(&pos.vertical))
        }
        ControlPoint::Relative(rel) => {
            let base = match rel.reference {
                ControlReference::Start => cur,
                ControlReference::End => end,
                ControlReference::Origin => Point::ZERO,
            };
            Point::new(
                base.x + resolve_x(&rel.coord.x),
                base.y + resolve_y(&rel.coord.y),
            )
        }
    }
}

/// Reflect a previous control point around the current point.
/// If there is no previous control point (previous command wasn't the matching curve type),
/// returns the current point itself.
fn reflect_point(last_control: Option<Point>, cur: Point) -> Point {
    match last_control {
        Some(ctrl) => Point::new(2.0 * cur.x - ctrl.x, 2.0 * cur.y - ctrl.y),
        None => cur,
    }
}

/// Resolve an AxisEndPoint to an absolute value.
/// `ToPosition` is absolute; `ByCoordinate` is relative to `cur_val`.
fn resolve_axis_endpoint<N>(
    endpoint: &AxisEndPoint<N, N>,
    cur_val: f64,
    resolve: impl Fn(&N) -> f64,
) -> f64 {
    match endpoint {
        AxisEndPoint::ToPosition(pos) => resolve(pos),
        AxisEndPoint::ByCoordinate(val) => cur_val + resolve(val),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use style::values::computed::Percentage;
    use style::values::generics::NonNegative;

    type Radius = GenericShapeRadius<LengthPercentage>;

    fn px(v: f32) -> Radius {
        GenericShapeRadius::Length(NonNegative(LengthPercentage::new_length(CSSPixelLength::new(v))))
    }

    fn pct(v: f32) -> Radius {
        GenericShapeRadius::Length(NonNegative(LengthPercentage::new_percent(Percentage(v))))
    }

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-3, "{a} is not {b}");
    }

    #[test]
    fn a_circle_percentage_is_of_the_normalised_diagonal() {
        // circle(42% at 50% 45%) on 170 x 340: 0.42 * sqrt((170^2 + 340^2) / 2)
        close(resolve_circle_radius(&pct(0.42), 170.0, 340.0, 85.0, 153.0), 112.8933);
        // a square box: the percentage is of its side
        close(resolve_circle_radius(&pct(0.5), 200.0, 200.0, 100.0, 100.0), 100.0);
        close(resolve_circle_radius(&px(30.0), 170.0, 340.0, 85.0, 153.0), 30.0);
    }

    #[test]
    fn circle_keywords_inside_the_box() {
        let (w, h, cx, cy) = (200.0, 100.0, 50.0, 30.0);
        close(resolve_circle_radius(&Radius::ClosestSide, w, h, cx, cy), 30.0);
        close(resolve_circle_radius(&Radius::FarthestSide, w, h, cx, cy), 150.0);
        close(resolve_circle_radius(&Radius::ClosestCorner, w, h, cx, cy), 50f64.hypot(30.0));
        close(resolve_circle_radius(&Radius::FarthestCorner, w, h, cx, cy), 150f64.hypot(70.0));
    }

    #[test]
    fn circle_keywords_with_the_centre_outside_the_box() {
        // the centre 40px left of and 20px above a 200 x 100 box
        let (w, h, cx, cy) = (200.0, 100.0, -40.0, -20.0);
        // N3: the closest corner is the box's own, 40 x 20 away: not a radius of zero
        close(resolve_circle_radius(&Radius::ClosestCorner, w, h, cx, cy), 40f64.hypot(20.0));
        close(resolve_circle_radius(&Radius::ClosestSide, w, h, cx, cy), 20.0);
        close(resolve_circle_radius(&Radius::FarthestSide, w, h, cx, cy), 240.0);
        close(resolve_circle_radius(&Radius::FarthestCorner, w, h, cx, cy), 240f64.hypot(120.0));
        // past the far edges too
        close(resolve_circle_radius(&Radius::ClosestCorner, w, h, 230.0, 130.0), 30f64.hypot(30.0));
    }

    #[test]
    fn an_ellipse_axis_looks_at_its_own_sides() {
        // ellipse(48% 30% at 50% 50%) on 170 x 340
        close(resolve_ellipse_radius(&pct(0.48), 170.0, 85.0), 81.6);
        close(resolve_ellipse_radius(&pct(0.30), 340.0, 170.0), 102.0);
        // closest-side in x is the nearer of left and right, whatever the centre's height
        close(resolve_ellipse_radius(&Radius::ClosestSide, 200.0, 50.0), 50.0);
        close(resolve_ellipse_radius(&Radius::FarthestSide, 200.0, 50.0), 150.0);
        close(resolve_ellipse_radius(&Radius::ClosestSide, 200.0, -40.0), 40.0);
        close(resolve_ellipse_radius(&Radius::ClosestCorner, 200.0, 50.0), 50.0 * std::f64::consts::SQRT_2);
        close(resolve_ellipse_radius(&Radius::FarthestCorner, 200.0, 50.0), 150.0 * std::f64::consts::SQRT_2);
    }
}
