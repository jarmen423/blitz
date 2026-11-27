use crate::blink_stops;
use crate::color::{Color, ToColorColor};
use color::{ColorSpaceTag, DynamicColor, HueDirection};
use kurbo::{self, Affine, Point, Rect, Vec2};
use peniko::{self, ColorStop, Gradient, LinearGradientPosition, SweepGradientPosition};
use style::color::AbsoluteColor;
use style::color::ColorSpace as StyloColorSpace;
use style::color::mix::{ColorInterpolationMethod, HueInterpolationMethod};
use style::{
    OwnedSlice,
    values::{
        computed::{
            Angle, AngleOrPercentage, CSSPixelLength, Gradient as StyloGradient, LengthPercentage,
            LineDirection, Percentage,
        },
        generics::{
            NonNegative,
            color::GenericColor,
            image::{
                EndingShape, GenericCircle, GenericEllipse, GenericEndingShape, GenericGradient,
                GenericGradientItem, GradientFlags, ShapeExtent,
            },
            position::GenericPosition,
        },
        specified::{
            percentage::ToPercentage,
            position::{HorizontalPositionKeyword, VerticalPositionKeyword},
        },
    },
};

type GradientItem<T> = GenericGradientItem<GenericColor<Percentage>, T>;
type LinearGradient<'a> = (
    &'a LineDirection,
    &'a [GradientItem<LengthPercentage>],
    GradientFlags,
);
type RadialGradient<'a> = (
    &'a EndingShape<NonNegative<CSSPixelLength>, NonNegative<LengthPercentage>>,
    &'a GenericPosition<LengthPercentage, LengthPercentage>,
    &'a OwnedSlice<GenericGradientItem<GenericColor<Percentage>, LengthPercentage>>,
    GradientFlags,
);
type ConicGradient<'a> = (
    &'a Angle,
    &'a GenericPosition<LengthPercentage, LengthPercentage>,
    &'a OwnedSlice<GenericGradientItem<GenericColor<Percentage>, AngleOrPercentage>>,
    GradientFlags,
);

pub(crate) fn to_peniko_gradient(
    gradient: &StyloGradient,
    origin_rect: Rect,
    bounding_box: Rect,
    scale: f64,
    current_color: &AbsoluteColor,
) -> (peniko::Gradient, Option<Affine>) {
    // known-future (wave 0): the colour space the stops are interpolated in, and the way round the hue circle,
    // are the gradient's own (CSS Images 4: `linear-gradient(in oklch longer hue, ...)`; when none is written, Stylo
    // has already chosen oklab for a gradient with a stop in a modern colour syntax and srgb for the rest). Every
    // gradient was interpolated in sRGB.
    let method = match gradient {
        GenericGradient::Linear {
            color_interpolation_method,
            ..
        }
        | GenericGradient::Radial {
            color_interpolation_method,
            ..
        }
        | GenericGradient::Conic {
            color_interpolation_method,
            ..
        } => color_interpolation_method,
    };
    let (mut peniko_gradient, transform) = match gradient {
        // https://developer.mozilla.org/en-US/docs/Web/CSS/gradient/linear-gradient
        GenericGradient::Linear {
            direction,
            items,
            flags,
            // compat_mode,
            ..
        } => linear_gradient(
            (direction, items, *flags),
            origin_rect,
            bounding_box,
            scale,
            current_color,
            method,
        ),
        GenericGradient::Radial {
            shape,
            position,
            items,
            flags,
            // compat_mode,
            ..
        } => radial_gradient((shape, position, items, *flags), origin_rect, current_color, method),
        GenericGradient::Conic {
            angle,
            position,
            items,
            flags,
            ..
        } => conic_gradient((angle, position, items, *flags), origin_rect, current_color, method),
    };

    let (interpolation_cs, hue_direction) = to_peniko_interpolation(method);
    peniko_gradient.interpolation_cs = interpolation_cs;
    peniko_gradient.hue_direction = hue_direction;

    // known-future (wave 1a): a gradient with no extent is a solid colour, not a shader the rasteriser cannot
    // make (Skia gives none for a brush transform it cannot invert, and its painter stops the process on that:
    // `radial-gradient(circle 0px, red, blue)`, an ellipse `closest-side` at a corner of its box, a
    // `repeating-radial-gradient(circle, red 5px, blue 5px)` ended a scene's paint).
    if !is_drawable(&peniko_gradient, transform) {
        return (solid_gradient(&peniko_gradient), None);
    }

    (peniko_gradient, transform)
}

/// The largest coordinate, radius, angle or matrix coefficient a gradient is handed to the rasteriser with (NaN
/// and the infinities are not under it either)
const COORDINATE_LIMIT: f64 = 1e9;

/// Whether a rasteriser can make a shader of the gradient: stops at finite places, a shape with an extent, in
/// coordinates it can hold, a brush transform that can be inverted.
fn is_drawable(gradient: &peniko::Gradient, transform: Option<Affine>) -> bool {
    use peniko::GradientKind;
    let stops = !gradient.stops.is_empty() && gradient.stops.iter().all(|stop| stop.offset.is_finite());
    // (a place a rasteriser can hold: its coordinates are `f32`, and a line that ends 1e30 px away is none to it)
    let place = |p: Point| p.x.abs() < COORDINATE_LIMIT && p.y.abs() < COORDINATE_LIMIT;
    let size = |v: f32| (v as f64).abs() < COORDINATE_LIMIT;
    let shape = match gradient.kind {
        GradientKind::Linear(line) => place(line.start) && place(line.end) && line.start != line.end,
        GradientKind::Radial(radial) => {
            place(radial.start_center)
                && place(radial.end_center)
                && size(radial.start_radius)
                && size(radial.end_radius)
                && radial.start_radius >= 0.0
                && radial.end_radius > radial.start_radius
        }
        GradientKind::Sweep(sweep) => {
            place(sweep.center)
                && size(sweep.start_angle)
                && size(sweep.end_angle)
                && sweep.start_angle != sweep.end_angle
        }
    };
    // (Skia inverts the matrix in `f32`, and takes a determinant under about 1.5e-11 for none)
    let invertible = transform.is_none_or(|transform| {
        let determinant = transform.determinant();
        transform.as_coeffs().iter().all(|c| c.abs() < COORDINATE_LIMIT)
            && determinant.is_finite()
            && determinant.abs() > 1e-9
    });
    stops && shape && invertible
}

/// The solid colour a gradient with no extent is drawn as: the colour of its last stop (CSS Images 3, 3.2.2: a
/// circle of radius zero or an ellipse of no height is drawn "as if" tiny, so everything is past its last stop;
/// so is an ellipse of no width when its stops are percentages). A repeating gradient whose stops span nothing
/// is its last stop's colour too: the specification says the average of its colours, and Chrome 152 draws the
/// last (`repeating-linear-gradient(90deg, red 5px, blue 5px)` is blue there). No stops at all: transparent.
fn solid_gradient(gradient: &peniko::Gradient) -> peniko::Gradient {
    let color = gradient
        .stops
        .last()
        .map(|last| last.color.to_alpha_color::<color::Srgb>().components)
        .filter(|color| color.iter().all(|c| c.is_finite()))
        .unwrap_or([0.0; 4]);
    let color = DynamicColor::from_alpha_color(Color::new(color));
    // (any shape with an extent will do: every place on it is the one colour)
    let mut solid = peniko::Gradient::new_linear((0.0, 0.0), (1.0, 0.0));
    solid.stops.push(ColorStop { color, offset: 0.0 });
    solid.stops.push(ColorStop { color, offset: 1.0 });
    solid
}

/// The `color` crate's names for a CSS `<color-interpolation-method>`.
///
/// The XYZ spaces and linear Display P3 have no tag of their own in the rasterisers; they are linear-light spaces,
/// a linear map away from linear sRGB, so a straight line between two colours is the same line there (Chrome's
/// gradients do the same for `in xyz`).
fn to_peniko_interpolation(method: &ColorInterpolationMethod) -> (ColorSpaceTag, HueDirection) {
    let cs = match method.space {
        StyloColorSpace::Srgb => ColorSpaceTag::Srgb,
        StyloColorSpace::Hsl => ColorSpaceTag::Hsl,
        StyloColorSpace::Hwb => ColorSpaceTag::Hwb,
        StyloColorSpace::Lab => ColorSpaceTag::Lab,
        StyloColorSpace::Lch => ColorSpaceTag::Lch,
        StyloColorSpace::Oklab => ColorSpaceTag::Oklab,
        StyloColorSpace::Oklch => ColorSpaceTag::Oklch,
        StyloColorSpace::DisplayP3 => ColorSpaceTag::DisplayP3,
        StyloColorSpace::A98Rgb => ColorSpaceTag::A98Rgb,
        StyloColorSpace::ProphotoRgb => ColorSpaceTag::ProphotoRgb,
        StyloColorSpace::Rec2020 => ColorSpaceTag::Rec2020,
        StyloColorSpace::SrgbLinear
        | StyloColorSpace::DisplayP3Linear
        | StyloColorSpace::XyzD50
        | StyloColorSpace::XyzD65 => ColorSpaceTag::LinearSrgb,
    };
    let hue = match method.hue {
        HueInterpolationMethod::Shorter | HueInterpolationMethod::Specified => HueDirection::Shorter,
        HueInterpolationMethod::Longer => HueDirection::Longer,
        HueInterpolationMethod::Increasing => HueDirection::Increasing,
        HueInterpolationMethod::Decreasing => HueDirection::Decreasing,
    };
    (cs, hue)
}

fn linear_gradient(
    gradient: LinearGradient,
    rect: Rect,
    bounding_box: Rect,
    scale: f64,
    current_color: &AbsoluteColor,
    method: &ColorInterpolationMethod,
) -> (peniko::Gradient, Option<Affine>) {
    let (direction, items, flags) = gradient;

    // known-future (outside round 5): the gradient box is the tile (`rect`: the background positioning area, sized by
    // background-size), so an angled gradient line is centred on it. It was centred on the element's border box,
    // which put every tile of a `background-size` gradient out of phase by (box width / 2 - tile / 2) modulo the
    // tile (launch-claude-paper's tartan, 120px tiles in a 1346px box: 13px), and moved a bordered box's by its border.
    let _ = bounding_box;
    let center = rect.center();
    let (start, end) = match direction {
        LineDirection::Angle(angle) => {
            let angle = -angle.radians64() + std::f64::consts::PI;
            let offset_length =
                rect.width() / 2.0 * angle.sin().abs() + rect.height() / 2.0 * angle.cos().abs();
            let offset_vec = Vec2::new(angle.sin(), angle.cos()) * offset_length;
            (center - offset_vec, center + offset_vec)
        }
        LineDirection::Horizontal(horizontal) => {
            let start = Point::new(rect.x0, rect.y0 + rect.height() / 2.0);
            let end = Point::new(rect.x1, rect.y0 + rect.height() / 2.0);
            match horizontal {
                HorizontalPositionKeyword::Right => (start, end),
                HorizontalPositionKeyword::Left => (end, start),
            }
        }
        LineDirection::Vertical(vertical) => {
            let start = Point::new(rect.x0 + rect.width() / 2.0, rect.y0);
            let end = Point::new(rect.x0 + rect.width() / 2.0, rect.y1);
            match vertical {
                VerticalPositionKeyword::Top => (end, start),
                VerticalPositionKeyword::Bottom => (start, end),
            }
        }
        LineDirection::Corner(horizontal, vertical) => {
            let (start_x, end_x) = match horizontal {
                HorizontalPositionKeyword::Right => (rect.x0, rect.x1),
                HorizontalPositionKeyword::Left => (rect.x1, rect.x0),
            };
            let (start_y, end_y) = match vertical {
                VerticalPositionKeyword::Top => (rect.y1, rect.y0),
                VerticalPositionKeyword::Bottom => (rect.y0, rect.y1),
            };
            (Point::new(start_x, start_y), Point::new(end_x, end_y))
        }
    };

    let gradient_length = CSSPixelLength::new((start.distance(end) / scale) as f32);
    let repeating = flags.contains(GradientFlags::REPEATING);

    let mut gradient = peniko::Gradient::new_linear(start, end).with_extend(if repeating {
        peniko::Extend::Repeat
    } else {
        peniko::Extend::Pad
    });

    let (first_offset, last_offset) = resolve_length_color_stops(
        current_color,
        items,
        gradient_length,
        &mut gradient,
        repeating,
        method,
        false,
    );
    // (wave 1a: also when the stops of a gradient that does not repeat go before 0 % or past 100 %)
    if (repeating || (first_offset, last_offset) != (0.0, 1.0)) && gradient.stops.len() > 1 {
        gradient.kind = peniko::GradientKind::Linear(LinearGradientPosition {
            start: start + (end - start) * first_offset as f64,
            end: end + (start - end) * (1.0 - last_offset) as f64,
        });
    }

    (gradient, None)
}

fn radial_gradient(
    gradient: RadialGradient,
    rect: Rect,
    current_color: &AbsoluteColor,
    method: &ColorInterpolationMethod,
) -> (peniko::Gradient, Option<Affine>) {
    let (shape, position, items, flags) = gradient;
    let repeating = flags.contains(GradientFlags::REPEATING);

    let mut gradient = peniko::Gradient::new_radial((0.0, 0.0), 1.0).with_extend(if repeating {
        peniko::Extend::Repeat
    } else {
        peniko::Extend::Pad
    });

    let (width_px, height_px) = (
        resolve_gradient_length(&position.horizontal, rect.width()),
        resolve_gradient_length(&position.vertical, rect.height()),
    );

    // known-future (wave 2): how far the centre is from the nearer and the farther side of the box in each
    // direction. They are distances, so never negative, wherever the centre is: with the centre outside the box,
    // `closest-side` is how far the centre is from the box (Blink's `RadiusToSide` takes `fabs` of each). They
    // were `min(x, width - x)`, which is negative there: a circle `closest-side at -30px 50%` had a radius of
    // -30px, the same circle mirrored as long as its stops were percentages, and with stops in pixels a gradient
    // whose stops ran the wrong way from the centre (it came out as its last stop's colour, where Chrome draws
    // the first 60px of it).
    let sides = |centre: f64, size: f64| {
        let (a, b) = (centre.abs(), (size - centre).abs());
        (a.min(b), a.max(b))
    };
    let (near_x, far_x) = sides(width_px, rect.width());
    let (near_y, far_y) = sides(height_px, rect.height());

    let gradient_scale: Option<Vec2> = match shape {
        GenericEndingShape::Circle(circle) => {
            let scale = match circle {
                GenericCircle::Extent(extent) => match extent {
                    ShapeExtent::FarthestSide => far_x.max(far_y),
                    ShapeExtent::ClosestSide => near_x.min(near_y),
                    // known-future (wave 1b): a circle sized to a corner has the distance from its centre to
                    // that corner as its radius (CSS Images 3, 3.2.1). It was `(dx + dy) / sqrt(2)`, which is that
                    // distance only where dx = dy and less than it everywhere else (`circle at 38% 50%` in a box
                    // of 520 x 440: 383.5 px for 390.3 px)
                    ShapeExtent::FarthestCorner => far_x.hypot(far_y),
                    ShapeExtent::ClosestCorner => near_x.hypot(near_y),
                    _ => 0.0,
                },
                GenericCircle::Radius(radius) => radius.0.px() as f64,
            };
            Some(Vec2::new(scale, scale))
        }
        GenericEndingShape::Ellipse(ellipse) => match ellipse {
            GenericEllipse::Extent(extent) => match extent {
                ShapeExtent::FarthestCorner | ShapeExtent::FarthestSide => {
                    let mut scale = Vec2::new(far_x, far_y);
                    if *extent == ShapeExtent::FarthestCorner {
                        scale *= 2.0_f64.sqrt();
                    }
                    Some(scale)
                }
                ShapeExtent::ClosestCorner | ShapeExtent::ClosestSide => {
                    let mut scale = Vec2::new(near_x, near_y);
                    if *extent == ShapeExtent::ClosestCorner {
                        scale *= 2.0_f64.sqrt();
                    }
                    Some(scale)
                }
                _ => None,
            },
            GenericEllipse::Radii(x, y) => Some(Vec2::new(
                resolve_gradient_length(&x.0, rect.width()),
                resolve_gradient_length(&y.0, rect.height()),
            )),
        },
    };

    // known-future (wave 2): an ending shape with no size (a circle of radius zero, an ellipse of no width or no
    // height) is its last stop's colour everywhere, whatever the stops are, as Chrome draws it. Its stops are not
    // placed along it: a stop at a length is that length over the radius, which is no place when the radius is
    // zero, so a gradient with such stops (`circle 0px, red 20px, blue 60px`) was left with no stops at all and
    // drew nothing. The last stop alone is kept, and `to_peniko_gradient` makes the solid colour of it.
    if let Some(scale) = gradient_scale
        && !(scale.x > 0.0 && scale.y > 0.0)
    {
        if let Some(color) = items.iter().rev().find_map(|item| match item {
            GenericGradientItem::SimpleColorStop(color)
            | GenericGradientItem::ComplexColorStop { color, .. } => Some(color),
            GenericGradientItem::InterpolationHint(_) => None,
        }) {
            let color = color.resolve_to_absolute(current_color).as_dynamic_color();
            gradient.stops.push(ColorStop { color, offset: 1.0 });
        }
        return (gradient, Some(Affine::scale(0.0)));
    }

    let gradient_transform = {
        // If the gradient has no valid scale, we don't need to calculate the color stops
        if let Some(gradient_scale) = gradient_scale {
            let (first_offset, last_offset) = resolve_length_color_stops(
                current_color,
                items,
                CSSPixelLength::new(gradient_scale.x as f32),
                &mut gradient,
                repeating,
                method,
                true,
            );
            // (wave 1a: also when the stops of a gradient that does not repeat go past 100 %)
            let ranged =
                gradient.stops.len() >= 2 && (repeating || (first_offset, last_offset) != (0.0, 1.0));
            let scale = if ranged {
                (last_offset - first_offset) as f64
            } else {
                1.0
            };
            // known-future (wave 1a): a repeating gradient's first ring is at its first stop, not at the centre
            // (`repeating-radial-gradient(circle, red 10px, blue 30px)` was drawn 10 px out of phase): the
            // rings run from the first stop's radius, brought to a radius that is not negative by whole periods
            if repeating && ranged && first_offset != 0.0 && scale > 0.0 {
                let start = (first_offset as f64 / scale).rem_euclid(1.0);
                gradient.kind = peniko::GradientKind::Radial(peniko::RadialGradientPosition {
                    start_center: Point::ZERO,
                    start_radius: start as f32,
                    end_center: Point::ZERO,
                    end_radius: (start + 1.0) as f32,
                });
            }
            Some(
                Affine::scale_non_uniform(gradient_scale.x * scale, gradient_scale.y * scale)
                    .then_translate(get_translation(position, rect)),
            )
        } else {
            None
        }
    };

    (gradient, gradient_transform)
}

fn conic_gradient(
    gradient: ConicGradient,
    rect: Rect,
    current_color: &AbsoluteColor,
    method: &ColorInterpolationMethod,
) -> (peniko::Gradient, Option<Affine>) {
    let (angle, position, items, flags) = gradient;

    let repeating = flags.contains(GradientFlags::REPEATING);
    let mut gradient = peniko::Gradient::new_sweep((0.0, 0.0), 0.0, std::f32::consts::PI * 2.0)
        .with_extend(if repeating {
            peniko::Extend::Repeat
        } else {
            peniko::Extend::Pad
        });

    let (first_offset, last_offset) = resolve_angle_color_stops(
        current_color,
        items,
        CSSPixelLength::new(1.0),
        &mut gradient,
        repeating,
        method,
    );
    // (wave 1a: also when the stops of a gradient that does not repeat go before 0 % or past 100 %)
    if (repeating || (first_offset, last_offset) != (0.0, 1.0)) && gradient.stops.len() >= 2 {
        gradient.kind = peniko::GradientKind::Sweep(SweepGradientPosition {
            center: Point::new(0.0, 0.0),
            start_angle: std::f32::consts::PI * 2.0 * first_offset,
            end_angle: std::f32::consts::PI * 2.0 * last_offset,
        });
    }

    let gradient_transform = Some(
        Affine::rotate(angle.radians() as f64 - std::f64::consts::PI / 2.0)
            .then_translate(get_translation(position, rect)),
    );

    (gradient, gradient_transform)
}

#[inline]
fn resolve_length_color_stops(
    current_color: &AbsoluteColor,
    items: &[GradientItem<LengthPercentage>],
    gradient_length: CSSPixelLength,
    gradient: &mut Gradient,
    repeating: bool,
    method: &ColorInterpolationMethod,
    clamp_at_zero: bool,
) -> (f32, f32) {
    resolve_color_stops(
        current_color,
        items,
        gradient_length,
        gradient,
        repeating,
        method,
        clamp_at_zero,
        |gradient_length: CSSPixelLength, position: &LengthPercentage| -> Option<f32> {
            position
                .to_percentage_of(gradient_length)
                .map(|percentage| percentage.to_percentage().unwrap_or(0.0))
        },
    )
}

#[inline]
fn resolve_color_stops<T>(
    current_color: &AbsoluteColor,
    items: &[GradientItem<T>],
    gradient_length: CSSPixelLength,
    gradient: &mut Gradient,
    repeating: bool,
    method: &ColorInterpolationMethod,
    clamp_at_zero: bool,
    item_resolver: impl Fn(CSSPixelLength, &T) -> Option<f32>,
) -> (f32, f32) {
    let mut hint: Option<f32> = None;

    // known-future (wave 1a): in a polar interpolation space (hsl, hwb, lch, oklch) Skia is given the stops Blink
    // gives it, not the colours as written: each converted into the interpolation space and back by Blink's
    // arithmetic, and one with a missing or powerless component split in two, a colour per neighbour
    // (blink_stops.rs: which way round two hues 180 degrees apart go, and what a grey stop does to the hue, are
    // decided there in Chrome). A rectangular space's stops are the colours as written, as before.
    let prepared = blink_stops::polar_space(method.space).map(|space| {
        let colors: Vec<AbsoluteColor> = items
            .iter()
            .filter_map(|item| match item {
                GenericGradientItem::SimpleColorStop(color)
                | GenericGradientItem::ComplexColorStop { color, .. } => {
                    Some(color.resolve_to_absolute(current_color))
                }
                GenericGradientItem::InterpolationHint(_) => None,
            })
            .collect();
        blink_stops::prepare_stops(&colors, space)
    });
    let srgb = |c: [f32; 4]| DynamicColor::from_alpha_color(Color::new(c));
    // (`longer hue`: Blink adds no stop of its own at 0 or 1, which would be a whole turn of the hue circle
    // between the added stop and the first one; Skia extends the end stops itself)
    let pads = !(prepared.is_some() && matches!(method.hue, HueInterpolationMethod::Longer));
    let mut color_index = 0usize;

    for (idx, item) in items.iter().enumerate() {
        let stylo_color = match item {
            GenericGradientItem::SimpleColorStop(color)
            | GenericGradientItem::ComplexColorStop { color, .. } => color,
            GenericGradientItem::InterpolationHint(position) => {
                hint = item_resolver(gradient_length, position);
                continue;
            }
        };
        let (color, second) = match &prepared {
            Some((stops, _)) => (
                srgb(stops[color_index].first),
                stops[color_index].second.map(srgb),
            ),
            None => (
                stylo_color
                    .resolve_to_absolute(current_color)
                    .as_dynamic_color(),
                None,
            ),
        };
        color_index += 1;
        let offset = match item {
            GenericGradientItem::ComplexColorStop { position, .. } => {
                match item_resolver(gradient_length, position) {
                    Some(offset) => offset,
                    None => continue,
                }
            }
            _ => {
                let step = 1.0 / (items.len() as f32 - 1.0);
                step * idx as f32
            }
        };

        // (wave 1a: a first stop before 0 % is before the stop this would add, not after it)
        if idx == 0 && !repeating && offset > 0.0 && pads {
            // (Blink pads with the first stop as it was written, not as resolved against its neighbour)
            let color = prepared.as_ref().map_or(color, |(_, front)| srgb(*front));
            gradient
                .stops
                .push(peniko::ColorStop { color, offset: 0.0 });
        }

        match hint {
            None => gradient.stops.push(peniko::ColorStop { color, offset }),
            Some(hint) => {
                let &last_stop = gradient.stops.last().unwrap();

                if hint <= last_stop.offset {
                    // Upstream code has a bug here, so we're going to do something different
                    match gradient.stops.len() {
                        0 => (),
                        1 => {
                            gradient.stops.pop();
                        }
                        _ => {
                            let prev_stop = gradient.stops[gradient.stops.len() - 2];
                            if prev_stop.offset == hint {
                                gradient.stops.pop();
                            }
                        }
                    }
                    gradient.stops.push(peniko::ColorStop {
                        color,
                        offset: hint,
                    });
                } else if hint >= offset {
                    gradient.stops.push(peniko::ColorStop {
                        color: last_stop.color,
                        offset: hint,
                    });
                    gradient.stops.push(peniko::ColorStop {
                        color,
                        offset: last_stop.offset,
                    });
                } else if hint == (last_stop.offset + offset) / 2.0 {
                    gradient.stops.push(peniko::ColorStop { color, offset });
                } else {
                    let mid_point = (hint - last_stop.offset) / (offset - last_stop.offset);
                    let mut interpolate_stop = |cur_offset: f32| {
                        let relative_offset =
                            (cur_offset - last_stop.offset) / (offset - last_stop.offset);
                        let multiplier = relative_offset.powf(0.5f32.log(mid_point));
                        let [last_r, last_g, last_b, last_a] = last_stop.color.components;
                        let [r, g, b, a] = color.components;

                        let color = Color::new([
                            (last_r + multiplier * (r - last_r)),
                            (last_g + multiplier * (g - last_g)),
                            (last_b + multiplier * (b - last_b)),
                            (last_a + multiplier * (a - last_a)),
                        ]);
                        gradient.stops.push(peniko::ColorStop {
                            color: DynamicColor::from_alpha_color(color),
                            offset: cur_offset,
                        });
                    };
                    if mid_point > 0.5 {
                        for i in 0..7 {
                            interpolate_stop(
                                last_stop.offset
                                    + (hint - last_stop.offset) * (7.0 + i as f32) / 13.0,
                            );
                        }
                        interpolate_stop(hint + (offset - hint) / 3.0);
                        interpolate_stop(hint + (offset - hint) * 2.0 / 3.0);
                    } else {
                        interpolate_stop(last_stop.offset + (hint - last_stop.offset) / 3.0);
                        interpolate_stop(last_stop.offset + (hint - last_stop.offset) * 2.0 / 3.0);
                        for i in 0..7 {
                            interpolate_stop(hint + (offset - hint) * (i as f32) / 13.0);
                        }
                    }
                    gradient.stops.push(peniko::ColorStop { color, offset });
                }
            }
        }
        // (the stop's second colour, at the same place: what follows starts from it)
        if let (Some(color), Some(offset)) = (second, gradient.stops.last().map(|s| s.offset)) {
            gradient.stops.push(peniko::ColorStop { color, offset });
        }
    }

    // known-future (wave 1a): a repeating gradient whose one period is a hundred boxes long or more
    // (`repeating-linear-gradient(red, blue calc(infinity * 1px))`), and starts at or before the box and ends at
    // or after it, shows one period of itself and no second: it is drawn as the gradient that does not repeat,
    // whose far stops are cut back below (a gradient line a rasteriser has no coordinates for is no gradient)
    let mut repeating = repeating;
    if repeating && gradient.stops.len() > 1 {
        let first_offset = gradient.stops.first().unwrap().offset;
        let last_offset = gradient.stops.last().unwrap().offset;
        if !(last_offset - first_offset < FAR_STOP) && first_offset <= 0.0 && last_offset >= 1.0 {
            repeating = false;
            gradient.extend = peniko::Extend::Pad;
        }
    }

    // Post-process the stops for repeating gradients
    if repeating && gradient.stops.len() > 1 {
        let first_offset = gradient.stops.first().unwrap().offset;
        let last_offset = gradient.stops.last().unwrap().offset;
        if first_offset != 0.0 || last_offset != 1.0 {
            let scale_inv = 1e-7_f32.max(1.0 / (last_offset - first_offset));
            for stop in &mut *gradient.stops {
                stop.offset = (stop.offset - first_offset) * scale_inv;
            }
        }
        (first_offset, last_offset)
    } else {
        let lo = if clamp_at_zero { 0.0 } else { -FAR_STOP };
        clip_stops(gradient, lo, 1.0 + FAR_STOP, to_peniko_interpolation(method));
        // Ensure that the gradient ends at offset 1.0
        if gradient.stops.len() > 1 && pads {
            let last_stop = &gradient.stops.last().unwrap();
            if last_stop.offset < 1.0 {
                let last_stop = ColorStop {
                    offset: 1.0,
                    ..(**last_stop)
                };
                gradient.stops.push(last_stop);
            }
        }
        // known-future (wave 1a): stops before 0 % or past 100 % (`linear-gradient(90deg, red, blue 200%)`,
        // `red -50%`, a length longer than the gradient line). A rasteriser takes stops from 0 to 1 along its
        // gradient line and pins the rest to those ends, so such a gradient was squeezed into its box (red to
        // blue across it, where a browser shows red to half-way purple). As for a repeating gradient, the stops
        // are spread over 0 to 1 and the caller moves the ends of the gradient line to where the first and the
        // last of them are.
        let lo = gradient.stops.iter().map(|s| s.offset).fold(0.0f32, f32::min);
        let hi = gradient.stops.iter().map(|s| s.offset).fold(1.0f32, f32::max);
        if gradient.stops.len() > 1 && (lo < 0.0 || hi > 1.0) && lo.is_finite() && hi.is_finite() {
            for stop in &mut *gradient.stops {
                stop.offset = (stop.offset - lo) / (hi - lo);
            }
            (lo, hi)
        } else {
            (0.0, 1.0)
        }
    }
}

/// How far outside its gradient line a stop is left where it is, in lengths of the line. A stop farther out
/// (`red calc(infinity * 1px)`, a length a million times the box) is cut back to here: spread over 0 to 1 with the
/// rest, it would leave the stops inside the box no room in an `f32`, and the gradient line's ends no place in
/// the rasteriser's coordinates. What is seen of a colour change that takes a hundred boxes is under a level.
const FAR_STOP: f32 = 100.0;

/// known-future (wave 1a): cut a gradient's stops off before `lo` and after `hi` (in lengths of the gradient
/// line): the gradient starts, or ends, there with the colour it has reached by then (Blink's
/// `ClampNegativeOffsets`, at both ends). A radial gradient has no radius under zero, so its `lo` is 0; for
/// the rest the limits are far outside the box (`FAR_STOP`). The colour at a cut is the gradient's own there:
/// interpolated in its colour space, premultiplied, its hues the way round its hue method says (so a gradient
/// whose two stops are both a million boxes away still shows, in its box, the mix those two make there).
/// One limit of that, said once: under `longer hue` the part that is left is again taken the longer way round.
fn clip_stops(gradient: &mut Gradient, lo: f32, hi: f32, (space, hue): (ColorSpaceTag, HueDirection)) {
    let outside = |stop: &ColorStop| stop.offset < lo || stop.offset > hi;
    if !gradient.stops.iter().any(outside) {
        return;
    }
    let mut stops: Vec<ColorStop> = gradient.stops.iter().copied().collect();
    // the colour of the gradient at `at`, between two stops that are either side of it
    let mixed = |before: &ColorStop, after: &ColorStop, at: f32| -> ColorStop {
        let t = if before.offset.is_infinite() {
            1.0
        } else if after.offset.is_infinite() {
            0.0
        } else {
            (at - before.offset) / (after.offset - before.offset)
        };
        let color = before.color.interpolate(after.color, space, hue).eval(t);
        ColorStop {
            color: DynamicColor::from_alpha_color(color.to_alpha_color::<color::Srgb>()),
            offset: at,
        }
    };
    // the start
    if stops.first().is_some_and(|first| first.offset < lo) {
        stops = match stops.iter().position(|stop| stop.offset >= lo) {
            // every stop is before the start: all there is to see is past the last of them
            None => {
                let last = *stops.last().unwrap();
                vec![ColorStop { offset: lo, ..last }, ColorStop { offset: lo + 1.0, ..last }]
            }
            Some(i) => {
                let mut kept = Vec::with_capacity(stops.len());
                if stops[i].offset > lo {
                    kept.push(mixed(&stops[i - 1], &stops[i], lo));
                }
                kept.extend_from_slice(&stops[i..]);
                kept
            }
        };
    }
    // the end
    if stops.last().is_some_and(|last| last.offset > hi) {
        stops = match stops.iter().rposition(|stop| stop.offset <= hi) {
            // every stop is after the end: all there is to see is before the first of them
            None => {
                let first = stops[0];
                vec![ColorStop { offset: hi - 1.0, ..first }, ColorStop { offset: hi, ..first }]
            }
            Some(j) => {
                let mut kept = stops[..=j].to_vec();
                if stops[j].offset < hi {
                    kept.push(mixed(&stops[j], &stops[j + 1], hi));
                }
                kept
            }
        };
    }
    gradient.stops.clear();
    for stop in stops {
        gradient.stops.push(stop);
    }
}

#[inline]
fn resolve_angle_color_stops(
    current_color: &AbsoluteColor,
    items: &[GradientItem<AngleOrPercentage>],
    gradient_length: CSSPixelLength,
    gradient: &mut Gradient,
    repeating: bool,
    method: &ColorInterpolationMethod,
) -> (f32, f32) {
    resolve_color_stops(
        current_color,
        items,
        gradient_length,
        gradient,
        repeating,
        method,
        false,
        |_gradient_length: CSSPixelLength, position: &AngleOrPercentage| -> Option<f32> {
            match position {
                AngleOrPercentage::Angle(angle) => {
                    Some(angle.radians() / (std::f64::consts::PI * 2.0) as f32)
                }
                AngleOrPercentage::Percentage(percentage) => {
                    Some(percentage.to_percentage().unwrap_or(0.0))
                }
            }
        },
    )
}

#[inline]
fn resolve_gradient_length(value: &LengthPercentage, basis: f64) -> f64 {
    // Percentage resolution can overflow even though computed lengths are finite.
    value
        .resolve(CSSPixelLength::new(basis as f32))
        .px()
        .clamp(f32::MIN, f32::MAX) as f64
}

#[inline]
fn get_translation(
    position: &GenericPosition<LengthPercentage, LengthPercentage>,
    rect: Rect,
) -> Vec2 {
    Vec2::new(
        rect.x0 + resolve_gradient_length(&position.horizontal, rect.width()),
        rect.y0 + resolve_gradient_length(&position.vertical, rect.height()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_method_is_the_gradients_own() {
        let m = |space, hue| to_peniko_interpolation(&ColorInterpolationMethod { space, hue });
        assert_eq!(
            m(StyloColorSpace::Srgb, HueInterpolationMethod::Shorter),
            (ColorSpaceTag::Srgb, HueDirection::Shorter)
        );
        assert_eq!(
            m(StyloColorSpace::Oklch, HueInterpolationMethod::Longer),
            (ColorSpaceTag::Oklch, HueDirection::Longer)
        );
        assert_eq!(
            m(StyloColorSpace::Oklab, HueInterpolationMethod::Shorter).0,
            ColorSpaceTag::Oklab
        );
        assert_eq!(
            m(StyloColorSpace::Hsl, HueInterpolationMethod::Increasing),
            (ColorSpaceTag::Hsl, HueDirection::Increasing)
        );
        assert_eq!(
            m(StyloColorSpace::Hwb, HueInterpolationMethod::Decreasing),
            (ColorSpaceTag::Hwb, HueDirection::Decreasing)
        );
        // linear-light spaces with no tag of their own interpolate as linear sRGB does
        assert_eq!(
            m(StyloColorSpace::XyzD65, HueInterpolationMethod::Shorter).0,
            ColorSpaceTag::LinearSrgb
        );
    }

    fn stops_of(method: ColorInterpolationMethod, colors: &[(AbsoluteColor, Option<f32>)]) -> Vec<(f32, [f32; 4])> {
        stops_and_range(method, colors, false, false).0
    }

    /// the stops as the rasteriser gets them, and where on the gradient line the first and the last of them are
    #[allow(clippy::type_complexity)]
    fn stops_and_range(
        method: ColorInterpolationMethod,
        colors: &[(AbsoluteColor, Option<f32>)],
        repeating: bool,
        radial: bool,
    ) -> (Vec<(f32, [f32; 4])>, (f32, f32)) {
        let items: Vec<GradientItem<LengthPercentage>> = colors
            .iter()
            .map(|(c, at)| match at {
                None => GenericGradientItem::SimpleColorStop(GenericColor::Absolute(*c)),
                Some(p) => GenericGradientItem::ComplexColorStop {
                    color: GenericColor::Absolute(*c),
                    position: LengthPercentage::new_percent(Percentage(*p)),
                },
            })
            .collect();
        let mut gradient = peniko::Gradient::new_linear((0.0, 0.0), (100.0, 0.0));
        let range = resolve_length_color_stops(&AbsoluteColor::BLACK, &items, CSSPixelLength::new(100.0), &mut gradient, repeating, &method, radial);
        let stops = gradient.stops.iter().map(|s| (s.offset, s.color.to_alpha_color::<color::Srgb>().components)).collect();
        (stops, range)
    }

    /// known-future (wave 1a): stops before 0 % or past 100 % are where they are, not pinned to the ends of the box
    #[test]
    fn stops_outside_the_gradient_line() {
        let srgb = ColorInterpolationMethod { space: StyloColorSpace::Srgb, hue: HueInterpolationMethod::Shorter };
        let (red, blue) = (AbsoluteColor::srgb_legacy(255, 0, 0, 1.0), AbsoluteColor::srgb_legacy(0, 0, 255, 1.0));
        let offsets = |stops: &[(f32, [f32; 4])]| stops.iter().map(|s| s.0).collect::<Vec<_>>();
        // inside 0 to 100 %: as it ever was
        let (stops, range) = stops_and_range(srgb, &[(red, None), (blue, None)], false, false);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (0.0, 1.0)));
        let (stops, range) = stops_and_range(srgb, &[(red, Some(0.2)), (blue, Some(0.8))], false, false);
        assert_eq!((offsets(&stops), range), (vec![0.0, 0.2, 0.8, 1.0], (0.0, 1.0)));
        // red, blue 200%: the line runs to 200 %, with the stops at its ends
        let (stops, range) = stops_and_range(srgb, &[(red, None), (blue, Some(2.0))], false, false);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (0.0, 2.0)));
        // red -50%, blue 150%: no stop is added at 0 before a stop that is before 0
        let (stops, range) = stops_and_range(srgb, &[(red, Some(-0.5)), (blue, Some(1.5))], false, false);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (-0.5, 1.5)));
        assert_eq!(stops[0].1, [1.0, 0.0, 0.0, 1.0]);
        // red -100%, blue 100%: only the start is outside
        let (stops, range) = stops_and_range(srgb, &[(red, Some(-1.0)), (blue, Some(1.0))], false, false);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (-1.0, 1.0)));
        // red 20%, blue 300%: the stop added at 0 stays at the start of the line
        let (stops, range) = stops_and_range(srgb, &[(red, Some(0.2)), (blue, Some(3.0))], false, false);
        assert_eq!(range, (0.0, 3.0));
        assert!((stops[1].0 - 0.2 / 3.0).abs() < 1e-6 && stops[0].0 == 0.0 && stops[2].0 == 1.0);
        // a radial gradient starts at its centre with the colour it has reached there
        let (stops, range) = stops_and_range(srgb, &[(red, Some(-1.0)), (blue, Some(1.0))], false, true);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (0.0, 1.0)));
        assert_eq!(stops[0].1, [0.5, 0.0, 0.5, 1.0]);
        let (stops, range) = stops_and_range(srgb, &[(red, Some(-2.0)), (blue, Some(-1.0))], false, true);
        assert_eq!((stops, range), (vec![(0.0, [0.0, 0.0, 1.0, 1.0]), (1.0, [0.0, 0.0, 1.0, 1.0])], (0.0, 1.0)));
        let (stops, range) = stops_and_range(srgb, &[(red, Some(-0.5)), (blue, Some(3.0))], false, true);
        assert_eq!((offsets(&stops), range), (vec![0.0, 1.0], (0.0, 3.0)));
        assert!((stops[0].1[0] - (1.0 - 0.5 / 3.5)).abs() < 1e-6, "{:?}", stops[0].1);
        // a stop far outside the box is cut back, with the colour the gradient has there: lime 100%, red at infinity
        // (Stylo keeps the largest length it has for it) stays lime to the eye, and the line stays a line
        let lime = AbsoluteColor::srgb_legacy(0, 255, 0, 1.0);
        for far in [f32::INFINITY, f32::MAX, 1e30, 5000.0] {
            let (stops, range) = stops_and_range(srgb, &[(lime, Some(1.0)), (red, Some(far))], false, false);
            assert_eq!(range, (0.0, 1.0 + FAR_STOP), "{far}");
            assert!(stops.iter().all(|s| s.0.is_finite() && (0.0..=1.0).contains(&s.0)), "{far}: {stops:?}");
            let last = stops.last().unwrap().1;
            assert!(last[1] > 0.97 && last[0] < 0.03, "{far}: the colour at the cut is {last:?}");
        }
        let (stops, range) = stops_and_range(srgb, &[(red, Some(f32::NEG_INFINITY)), (lime, Some(0.0)), (blue, Some(f32::INFINITY))], false, false);
        assert_eq!(range, (-FAR_STOP, 1.0 + FAR_STOP));
        assert_eq!(stops.len(), 3);
        assert!(stops.iter().all(|s| s.0.is_finite() && s.1 == [0.0, 1.0, 0.0, 1.0]), "{stops:?}");
        // two stops both far away, in oklab: what is seen in the box is the mix oklab makes there, not sRGB's
        let oklab = ColorInterpolationMethod { space: StyloColorSpace::Oklab, hue: HueInterpolationMethod::Shorter };
        let (stops, range) = stops_and_range(oklab, &[(red, Some(-50_000.0)), (blue, Some(50_001.0))], false, false);
        assert_eq!((stops.len(), range), (2, (-FAR_STOP, 1.0 + FAR_STOP)));
        let seen = |stops: &[(f32, [f32; 4])]| {
            let (a, b) = (DynamicColor::from_alpha_color(Color::new(stops[0].1)), DynamicColor::from_alpha_color(Color::new(stops[1].1)));
            a.interpolate(b, ColorSpaceTag::Oklab, HueDirection::Shorter).eval(0.5).to_alpha_color::<color::Srgb>().components
        };
        let whole = DynamicColor::from_alpha_color(Color::new([1.0, 0.0, 0.0, 1.0]))
            .interpolate(DynamicColor::from_alpha_color(Color::new([0.0, 0.0, 1.0, 1.0])), ColorSpaceTag::Oklab, HueDirection::Shorter)
            .eval(0.5)
            .to_alpha_color::<color::Srgb>()
            .components;
        for (got, want) in seen(&stops).iter().zip(whole) {
            assert!((got - want).abs() < 2e-3, "{:?} / {whole:?}", seen(&stops));
        }
        assert!((whole[0] - 0.5).abs() > 0.02, "oklab's half-way between red and blue is sRGB's: the test shows nothing");
        // every stop beyond a limit: the colour that is seen
        let (stops, _) = stops_and_range(srgb, &[(red, Some(500.0)), (blue, Some(600.0))], false, false);
        assert!(stops.iter().all(|s| s.1 == [1.0, 0.0, 0.0, 1.0]), "{stops:?}");
        let (stops, _) = stops_and_range(srgb, &[(red, Some(-600.0)), (blue, Some(-500.0))], false, false);
        assert!(stops.iter().all(|s| s.1 == [0.0, 0.0, 1.0, 1.0]), "{stops:?}");
        // a repeating gradient with a period no box will see the end of is one period, not repeating
        let (stops, range) = stops_and_range(srgb, &[(red, None), (blue, Some(f32::INFINITY))], true, false);
        assert_eq!(range, (0.0, 1.0 + FAR_STOP));
        assert!(stops.iter().all(|s| s.0.is_finite()) && stops[0].1 == [1.0, 0.0, 0.0, 1.0], "{stops:?}");
        // repeating gradients are as they were: spread over their own first and last stop
        let (stops, range) = stops_and_range(srgb, &[(red, Some(0.1)), (blue, Some(0.3))], true, false);
        assert_eq!(offsets(&stops), vec![0.0, 1.0]);
        assert!((range.0 - 0.1).abs() < 1e-6 && (range.1 - 0.3).abs() < 1e-6);
    }

    /// known-future (wave 1a): in a polar space the stops are Blink's (blink_stops.rs); elsewhere the colours as written
    #[test]
    fn polar_gradients_get_blinks_stops() {
        let oklch = |l: f32, c: f32, h: Option<f32>| AbsoluteColor::new(StyloColorSpace::Oklch, l, c, h, 1.0f32);
        let white = AbsoluteColor::srgb_legacy(255, 255, 255, 1.0);
        let in_oklch = ColorInterpolationMethod { space: StyloColorSpace::Oklch, hue: HueInterpolationMethod::Shorter };
        let in_srgb = ColorInterpolationMethod { space: StyloColorSpace::Srgb, hue: HueInterpolationMethod::Shorter };
        let in_oklab = ColorInterpolationMethod { space: StyloColorSpace::Oklab, hue: HueInterpolationMethod::Shorter };

        // a stop between two others whose hue is missing is two stops at its place, one per neighbour
        let three = [(oklch(0.7, 0.2, Some(30.0)), None), (oklch(0.5, 0.1, None), None), (oklch(0.7, 0.2, Some(250.0)), None)];
        let s = stops_of(in_oklch, &three);
        assert_eq!(s.iter().map(|s| s.0).collect::<Vec<_>>(), [0.0, 0.5, 0.5, 1.0]);
        assert_ne!(s[1].1, s[2].1);
        // ... and one colour, hue 0 as written, in a rectangular space
        assert_eq!(stops_of(in_oklab, &three).len(), 3);

        // white keeps its place but is no longer exactly white in oklch (Blink's conversion: blink_stops.rs)
        let to_white = [(oklch(0.7, 0.2, Some(30.0)), None), (white, None)];
        let s = stops_of(in_oklch, &to_white);
        assert_eq!(s.len(), 2);
        assert!(s[1].1 != [1.0, 1.0, 1.0, 1.0] && s[1].1.iter().all(|c| (c - 1.0).abs() < 1e-3), "{:?}", s[1].1);
        // ... and is exactly white where nothing is prepared
        assert_eq!(stops_of(in_srgb, &to_white)[1].1, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(stops_of(in_oklab, &to_white)[1].1, [1.0, 1.0, 1.0, 1.0]);
    }

    /// known-future (wave 1a): a gradient whose stops do not reach 0 and 1 is extended by a stop at each end, but not
    /// under `longer hue` in a polar space, where two equal colours are a whole turn of the hue circle apart
    #[test]
    fn longer_hue_adds_no_stops_at_the_ends() {
        let hsl = |h: f32| {
            let mut c = AbsoluteColor::new(StyloColorSpace::Hsl, h, 90.0f32, 50.0f32, 1.0f32);
            c.flags.insert(style::color::ColorFlags::IS_LEGACY_SRGB);
            c
        };
        let stops = [(hsl(20.0), Some(0.2)), (hsl(120.0), Some(0.8))];
        let offsets = |hue| {
            let method = ColorInterpolationMethod { space: StyloColorSpace::Hsl, hue };
            stops_of(method, &stops).iter().map(|s| s.0).collect::<Vec<_>>()
        };
        assert_eq!(offsets(HueInterpolationMethod::Shorter), [0.0, 0.2, 0.8, 1.0]);
        assert_eq!(offsets(HueInterpolationMethod::Longer), [0.2, 0.8]);
        // in a rectangular space the hue method is nothing, and the ends are extended as ever
        let method = ColorInterpolationMethod { space: StyloColorSpace::Srgb, hue: HueInterpolationMethod::Longer };
        assert_eq!(stops_of(method, &stops).len(), 4);
    }

    /// known-future (wave 1a): what the rasteriser cannot make a shader of is drawn as a solid colour
    #[test]
    fn a_gradient_with_no_extent_is_a_solid_colour() {
        let stop = |rgba: [f32; 4], offset: f32| ColorStop { color: DynamicColor::from_alpha_color(Color::new(rgba)), offset };
        let (red, blue) = ([1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        let colour = |g: &peniko::Gradient| g.stops.iter().map(|s| s.color.to_alpha_color::<color::Srgb>().components).collect::<Vec<_>>();
        let mut radial = peniko::Gradient::new_radial((0.0, 0.0), 1.0);
        radial.stops.push(stop(red, 0.0));
        radial.stops.push(stop(blue, 1.0));
        // drawable as it is, and under a transform that keeps an extent in both directions
        assert!(is_drawable(&radial, None));
        assert!(is_drawable(&radial, Some(Affine::scale_non_uniform(50.0, 20.0).then_translate(Vec2::new(10.0, 10.0)))));
        // a circle of radius zero, an ellipse with no width or no height: a transform that cannot be inverted
        for (sx, sy) in [(0.0, 0.0), (0.0, 20.0), (50.0, 0.0), (f64::NAN, 20.0), (f64::INFINITY, 20.0), (1e-7, 1e-7)] {
            assert!(!is_drawable(&radial, Some(Affine::scale_non_uniform(sx, sy))), "{sx} x {sy}");
        }
        assert_eq!(colour(&solid_gradient(&radial)), [blue, blue], "the last stop's colour");
        // stops at no place (a gradient line of length zero divides by it)
        let mut nowhere = radial.clone();
        nowhere.stops[1].offset = f32::NAN;
        assert!(!is_drawable(&nowhere, None));
        nowhere.stops[1].offset = f32::INFINITY;
        assert!(!is_drawable(&nowhere, None));
        // a line the rasteriser has no coordinates for
        let mut far = peniko::Gradient::new_linear((0.0, 0.0), (1e30, 0.0));
        far.stops.push(stop(red, 0.0));
        far.stops.push(stop(blue, 1.0));
        assert!(!is_drawable(&far, None));
        assert!(!is_drawable(&radial, Some(Affine::scale(1e12))));
        // a line, and a sweep, with no extent
        let mut line = peniko::Gradient::new_linear((5.0, 5.0), (5.0, 5.0));
        line.stops.push(stop(red, 0.0));
        line.stops.push(stop(blue, 1.0));
        assert!(!is_drawable(&line, None));
        let mut sweep = peniko::Gradient::new_sweep((0.0, 0.0), 1.0, 1.0);
        sweep.stops.push(stop(red, 0.0));
        sweep.stops.push(stop(blue, 1.0));
        assert!(!is_drawable(&sweep, None));
        // repeating: the last stop's colour too (as Chrome draws it)
        let repeating = line.clone().with_extend(peniko::Extend::Repeat);
        assert_eq!(colour(&solid_gradient(&repeating)), [blue, blue]);
        // two radii that are the same radius
        let mut ring = radial.clone();
        ring.kind = peniko::GradientKind::Radial(peniko::RadialGradientPosition {
            start_center: Point::ZERO,
            start_radius: 0.5,
            end_center: Point::ZERO,
            end_radius: 0.5,
        });
        assert!(!is_drawable(&ring, None));
        // no stops at all: transparent, and still something to draw
        let empty = peniko::Gradient::new_radial((0.0, 0.0), 1.0);
        assert!(!is_drawable(&empty, None));
        let solid = solid_gradient(&empty);
        assert_eq!(colour(&solid), [[0.0; 4]; 2]);
        assert!(is_drawable(&solid, None));
        assert!(is_drawable(&solid_gradient(&radial), None));
    }

    #[test]
    fn gradient_percentage_overflow_is_clamped() {
        for percentage in [f32::MAX, f32::MIN] {
            let value = LengthPercentage::new_percent(Percentage(percentage));
            assert_eq!(resolve_gradient_length(&value, 300.0), percentage as f64);
        }
    }

    #[test]
    fn finite_gradient_lengths_are_preserved() {
        let length = LengthPercentage::new_length(CSSPixelLength::new(-12.0));
        let percentage = LengthPercentage::new_percent(Percentage(-0.5));
        assert_eq!(resolve_gradient_length(&length, 300.0), -12.0);
        assert_eq!(resolve_gradient_length(&percentage, 300.0), -150.0);
    }

    /// the radii of a radial gradient's ending shape and its centre, as `radial_gradient` draws it: `black, transparent`
    /// with the shape sized by `extent`, centred `at` (fractions of the box) in a box of `size` with its corner at 0, 0
    fn ending_shape(extent: ShapeExtent, ellipse: bool, at: (f32, f32), size: (f64, f64)) -> ((f64, f64), (f64, f64)) {
        let shape: EndingShape<NonNegative<CSSPixelLength>, NonNegative<LengthPercentage>> = if ellipse {
            GenericEndingShape::Ellipse(GenericEllipse::Extent(extent))
        } else {
            GenericEndingShape::Circle(GenericCircle::Extent(extent))
        };
        let position = GenericPosition {
            horizontal: LengthPercentage::new_percent(Percentage(at.0)),
            vertical: LengthPercentage::new_percent(Percentage(at.1)),
        };
        let stop = |c| GenericGradientItem::SimpleColorStop(GenericColor::Absolute(c));
        let items: OwnedSlice<GradientItem<LengthPercentage>> =
            vec![stop(AbsoluteColor::BLACK), stop(AbsoluteColor::srgb_legacy(0, 0, 0, 0.0))].into();
        let srgb = ColorInterpolationMethod { space: StyloColorSpace::Srgb, hue: HueInterpolationMethod::Shorter };
        let rect = Rect::new(0.0, 0.0, size.0, size.1);
        let (_, transform) =
            radial_gradient((&shape, &position, &items, GradientFlags::empty()), rect, &AbsoluteColor::BLACK, &srgb);
        // the gradient is a circle of radius 1 at 0, 0, scaled to the radii and moved to the centre
        let [rx, skew_y, skew_x, ry, cx, cy] = transform.expect("a shape sized by a keyword has a size").as_coeffs();
        assert_eq!((skew_y, skew_x), (0.0, 0.0));
        ((rx, ry), (cx, cy))
    }

    /// what the radius of a circle sized to a corner was before wave 1b
    fn corner_radius_before(dx: f64, dy: f64) -> f64 {
        (dx + dy) * 0.5_f64.sqrt()
    }

    /// known-future (wave 1b): a circle sized to a corner passes through that corner: its radius is the distance from
    /// its centre to it, `hypot(dx, dy)` (CSS Images 3, 3.2.1). It was `(dx + dy) / sqrt(2)`: the same where dx = dy,
    /// too small everywhere else
    #[test]
    fn a_circle_to_a_corner_reaches_the_corner() {
        let radius = |extent, at, size| {
            let ((rx, ry), _) = ending_shape(extent, false, at, size);
            assert_eq!(rx, ry, "a circle has one radius");
            rx
        };
        let (farthest, closest) = (ShapeExtent::FarthestCorner, ShapeExtent::ClosestCorner);
        let near = |got: f64, want: f64| (got - want).abs() < 1e-3;

        // the centre of a square: every corner is as far, and dx = dy, the one case that was right
        for extent in [farthest, closest] {
            let r = radius(extent, (0.5, 0.5), (200.0, 200.0));
            assert_eq!(r, 100.0_f64.hypot(100.0));
            assert!(near(r, corner_radius_before(100.0, 100.0)), "{r}");
        }

        // the centre of a box that is not square: every corner is as far, 3-4-5 away
        for extent in [farthest, closest] {
            let r = radius(extent, (0.5, 0.5), (400.0, 300.0));
            assert_eq!(r, 250.0);
            assert!(r - corner_radius_before(200.0, 150.0) > 2.5, "it was 247.49");
        }

        // off the centre, in every quarter of a box of 120 x 160: the closest corner is 30 across and 40 down (or up),
        // the farthest 90 and 120
        for at in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
            assert_eq!(radius(closest, at, (120.0, 160.0)), 50.0, "{at:?}");
            assert_eq!(radius(farthest, at, (120.0, 160.0)), 150.0, "{at:?}");
        }
        assert!(near(corner_radius_before(30.0, 40.0), 49.497) && near(corner_radius_before(90.0, 120.0), 148.492));

        // `radial-gradient(circle at 38% 50%, ...)` in a box of 520 x 440 (the mask of Tier-0 page t0_16): the
        // farthest corner is 322.4 across and 220 down, 390.31 px away; the circle was 383.53 px, so a stop at 32 %
        // was drawn more than 2 px inside its place
        let r = radius(farthest, (0.38, 0.5), (520.0, 440.0));
        assert!(near(r, 322.4_f64.hypot(220.0)) && near(r, 390.310), "{r}");
        let before = corner_radius_before(322.4, 220.0);
        assert!(near(before, 383.535) && (r - before) * 0.32 > 2.0, "{before}");
        let r = radius(closest, (0.38, 0.5), (520.0, 440.0));
        assert!(near(r, 197.6_f64.hypot(220.0)) && near(r, 295.712), "{r}");

        // a centre outside the box: the closest corner is still the closest, the distances are distances
        assert_eq!(radius(closest, (-0.25, 0.5), (120.0, 160.0)), 30.0_f64.hypot(80.0));
        assert_eq!(radius(farthest, (-0.25, 0.5), (120.0, 160.0)), 170.0);

        // a circle sized to a side is as it was: it meets the closest side, or the farthest
        assert_eq!(radius(ShapeExtent::ClosestSide, (0.25, 0.25), (120.0, 160.0)), 30.0);
        assert_eq!(radius(ShapeExtent::FarthestSide, (0.25, 0.25), (120.0, 160.0)), 120.0);
        // and the centre is where it was put
        assert_eq!(ending_shape(farthest, false, (0.25, 0.75), (120.0, 160.0)).1, (30.0, 120.0));
    }

    /// known-future (wave 2, the audit's N2): with the centre outside the box, the distances to its sides are
    /// still distances. `closest-side` and `closest-corner` had radii under zero there.
    #[test]
    fn a_shape_sized_from_outside_the_box_has_radii_that_are_not_negative() {
        let circle = |extent, at, size| ending_shape(extent, false, at, size).0.0;
        let ellipse = |extent, at, size| ending_shape(extent, true, at, size).0;
        let size = (120.0, 160.0);
        // the centre 30px left of the box, half way down: the closest side (the left one) is 30px away
        assert_eq!(circle(ShapeExtent::ClosestSide, (-0.25, 0.5), size), 30.0, "it was -30");
        assert_eq!(circle(ShapeExtent::FarthestSide, (-0.25, 0.5), size), 150.0);
        // past the right edge and under the bottom one
        assert_eq!(circle(ShapeExtent::ClosestSide, (1.25, 1.125), size), 20.0);
        assert_eq!(circle(ShapeExtent::ClosestCorner, (1.25, 1.125), size), 30.0_f64.hypot(20.0));
        // an ellipse meets the closest side in each direction; to the corner it is sqrt(2) times that
        assert_eq!(ellipse(ShapeExtent::ClosestSide, (-0.25, -0.125), size), (30.0, 20.0), "it was -30 x -20");
        let (rx, ry) = ellipse(ShapeExtent::ClosestCorner, (-0.25, -0.125), size);
        assert!((rx - 30.0 * 2.0_f64.sqrt()).abs() < 1e-9 && (ry - 20.0 * 2.0_f64.sqrt()).abs() < 1e-9, "{rx} x {ry}");
        assert_eq!(ellipse(ShapeExtent::FarthestSide, (-0.25, -0.125), size), (150.0, 180.0));
        // inside the box nothing has changed
        assert_eq!(circle(ShapeExtent::ClosestSide, (0.25, 0.25), size), 30.0);
        assert_eq!(ellipse(ShapeExtent::ClosestSide, (0.25, 0.25), size), (30.0, 40.0));
    }

    /// known-future (wave 2, the audit's N1): an ending shape with no size is the last stop's colour, whatever
    /// its stops are. With stops at lengths it had no stops left, and nothing was drawn.
    #[test]
    fn a_shape_with_no_size_is_its_last_stops_colour_whatever_the_stops() {
        let srgb = ColorInterpolationMethod { space: StyloColorSpace::Srgb, hue: HueInterpolationMethod::Shorter };
        let (red, blue) = (AbsoluteColor::srgb_legacy(255, 0, 0, 1.0), AbsoluteColor::srgb_legacy(0, 0, 255, 1.0));
        let at = |x: f32, y: f32| GenericPosition {
            horizontal: LengthPercentage::new_percent(Percentage(x)),
            vertical: LengthPercentage::new_percent(Percentage(y)),
        };
        let px = |v: f32| LengthPercentage::new_length(CSSPixelLength::new(v));
        let at_length = |c: AbsoluteColor, v: f32| GenericGradientItem::ComplexColorStop {
            color: GenericColor::Absolute(c),
            position: px(v),
        };
        let plain = |c: AbsoluteColor| GenericGradientItem::SimpleColorStop(GenericColor::Absolute(c));
        type Shape = EndingShape<NonNegative<CSSPixelLength>, NonNegative<LengthPercentage>>;
        let circle = |r: f32| -> Shape { GenericEndingShape::Circle(GenericCircle::Radius(NonNegative(CSSPixelLength::new(r)))) };
        let ellipse = |rx: f32, ry: f32| -> Shape {
            GenericEndingShape::Ellipse(GenericEllipse::Radii(NonNegative(px(rx)), NonNegative(px(ry))))
        };
        let closest_side: Shape = GenericEndingShape::Circle(GenericCircle::Extent(ShapeExtent::ClosestSide));
        let rect = Rect::new(0.0, 0.0, 170.0, 120.0);
        let blue_rgba = [0.0, 0.0, 1.0, 1.0];
        let drawn = |shape: &Shape, position: &GenericPosition<LengthPercentage, LengthPercentage>, items: Vec<GradientItem<LengthPercentage>>, flags| {
            let items: OwnedSlice<GradientItem<LengthPercentage>> = items.into();
            let (gradient, transform) = radial_gradient((shape, position, &items, flags), rect, &AbsoluteColor::BLACK, &srgb);
            // what `to_peniko_gradient` does with it
            assert!(!is_drawable(&gradient, transform), "a shape with no size is no shader");
            solid_gradient(&gradient).stops.iter().map(|s| s.color.to_alpha_color::<color::Srgb>().components).collect::<Vec<_>>()
        };
        let none = GradientFlags::empty();
        // `circle 0px, red 20px, blue 60px`: blue, as Chrome draws it (it was transparent: no stop was kept)
        assert_eq!(drawn(&circle(0.0), &at(0.5, 0.5), vec![at_length(red, 20.0), at_length(blue, 60.0)], none), [blue_rgba; 2]);
        // the same with no places for the stops, as before
        assert_eq!(drawn(&circle(0.0), &at(0.5, 0.5), vec![plain(red), plain(blue)], none), [blue_rgba; 2]);
        // a circle that meets the side its centre is on
        assert_eq!(drawn(&closest_side, &at(0.0, 0.5), vec![at_length(red, 20.0), at_length(blue, 60.0)], none), [blue_rgba; 2]);
        // an ellipse of no width, and one of no height
        assert_eq!(drawn(&ellipse(0.0, 40.0), &at(0.5, 0.5), vec![at_length(red, 10.0), at_length(blue, 60.0)], none), [blue_rgba; 2]);
        assert_eq!(drawn(&ellipse(40.0, 0.0), &at(0.5, 0.5), vec![at_length(red, 10.0), at_length(blue, 60.0)], none), [blue_rgba; 2]);
        // repeating: the last stop's colour too
        assert_eq!(
            drawn(&circle(0.0), &at(0.5, 0.5), vec![at_length(red, 0.0), at_length(blue, 20.0)], GradientFlags::REPEATING),
            [blue_rgba; 2]
        );
        // a circle with a radius keeps its stops
        let items: OwnedSlice<GradientItem<LengthPercentage>> = vec![at_length(red, 20.0), at_length(blue, 60.0)].into();
        let (gradient, transform) = radial_gradient((&circle(40.0), &at(0.5, 0.5), &items, none), rect, &AbsoluteColor::BLACK, &srgb);
        assert!(is_drawable(&gradient, transform) && gradient.stops.len() >= 2);
    }

    /// an ellipse sized to a corner was already as CSS Images 3 (3.2.1) has it: it passes through the corner, with the
    /// ratio of its radii the one it has sized to the sides (the distances to the closest, or the farthest, side in
    /// each direction); both together are those distances times sqrt(2)
    #[test]
    fn an_ellipse_to_a_corner_passes_through_it() {
        let radii = |extent, at, size| ending_shape(extent, true, at, size).0;
        let pairs = [
            (ShapeExtent::ClosestCorner, ShapeExtent::ClosestSide, (30.0, 40.0)),
            (ShapeExtent::FarthestCorner, ShapeExtent::FarthestSide, (90.0, 120.0)),
        ];
        for at in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
            for (corner, side, (dx, dy)) in pairs {
                // sized to the sides, it meets the side in each direction
                assert_eq!(radii(side, at, (120.0, 160.0)), (dx, dy), "{at:?}");
                let (rx, ry) = radii(corner, at, (120.0, 160.0));
                // the corner (dx, dy from the centre) is on the ellipse ...
                assert!(((dx / rx).powi(2) + (dy / ry).powi(2) - 1.0).abs() < 1e-12, "{at:?}: {rx} x {ry}");
                // ... and the ellipse has the shape it has sized to the sides
                assert!((rx / ry - dx / dy).abs() < 1e-12, "{at:?}: {rx} x {ry}");
                assert!((rx - dx * 2.0_f64.sqrt()).abs() < 1e-9 && (ry - dy * 2.0_f64.sqrt()).abs() < 1e-9);
            }
        }
        // in the centre of a square the ellipse to a corner is the circle to a corner
        let (rx, ry) = radii(ShapeExtent::FarthestCorner, (0.5, 0.5), (200.0, 200.0));
        assert!((rx - 100.0_f64.hypot(100.0)).abs() < 1e-9 && rx == ry, "{rx} x {ry}");
    }
}
