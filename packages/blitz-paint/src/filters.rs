use blitz_dom::util::ToColorColor as _;
use style::color::AbsoluteColor;
pub(crate) use style::computed_values::filter::single_value::T as StyloFilter;

use anyrender::filters::color_transformation::ColorMatrix;
use anyrender::filters::{
    Filter, FilterColorSpace, FilterEffect, FilterGraphNode, FilterId, FilterInput, FilterInputs,
    FilterSource,
};
use kurbo::Rect;
use style::values::computed::url::ComputedUrl;

/// `current_color` is the element's computed `color`: a `drop-shadow()` with no colour of its own, or with
/// `currentColor`, takes it (Filter Effects 1; known-future, wave 0: it was always black).
pub(crate) fn convert_filters(
    filters: &[StyloFilter],
    bounds: Rect,
    current_color: &AbsoluteColor,
) -> Option<Filter> {
    if filters.is_empty() {
        return None;
    }

    Some(Filter::linear_list(
        filters
            .iter()
            .filter_map(|filter| convert_single_filter(filter, current_color)),
        bounds,
    ))
}

pub(crate) fn convert_single_filter(
    filter: &StyloFilter,
    current_color: &AbsoluteColor,
) -> Option<FilterEffect> {
    Some(match filter {
        StyloFilter::Blur(radius) => FilterEffect::blur(radius.px()),
        StyloFilter::Brightness(amount) => FilterEffect::brightness(amount.0),
        StyloFilter::Contrast(amount) => FilterEffect::contrast(amount.0),
        StyloFilter::Grayscale(amount) => FilterEffect::grayscale(amount.0),
        StyloFilter::HueRotate(angle) => FilterEffect::hue_rotate(angle.radians()),
        StyloFilter::Invert(amount) => FilterEffect::invert(amount.0),
        StyloFilter::Opacity(amount) => FilterEffect::opacity(amount.0),
        StyloFilter::Saturate(amount) => FilterEffect::saturate(amount.0),
        StyloFilter::Sepia(amount) => FilterEffect::sepia(amount.0),
        StyloFilter::DropShadow(shadow) => FilterEffect::drop_shadow(
            shadow.horizontal.px(),
            shadow.vertical.px(),
            shadow.blur.px(),
            shadow
                .color
                .resolve_to_absolute(current_color)
                .as_color_color(),
        ),
        StyloFilter::Url(_) => return None,
    })
}

/// known-future (wave 2): a `filter` list in which `url(#id)` may name an inline SVG `<filter>`.
///
/// `svg_filter` gives the graph of such a reference, in the same space as `bounds` (`None` for a
/// reference to anything else, which filters nothing). The list is one graph: each function, and
/// each `<filter>`, takes what the one before it made as its source graphic. A list of functions
/// alone is the graph [`convert_filters`] makes.
pub(crate) fn convert_filter_list(
    filters: &[StyloFilter],
    bounds: Rect,
    current_color: &AbsoluteColor,
    svg_filter: &dyn Fn(&ComputedUrl) -> Option<std::sync::Arc<Filter>>,
) -> Option<Filter> {
    if !filters.iter().any(|f| matches!(f, StyloFilter::Url(_))) {
        return convert_filters(filters, bounds, current_color);
    }
    let mut graph: Option<Filter> = None;
    // What the list has made so far: the next filter's source graphic
    let mut source: Option<FilterId> = None;
    let mut rest = filters;
    while let Some(first) = rest.first() {
        if let StyloFilter::Url(url) = first {
            if let Some(svg) = svg_filter(url) {
                source = append_filter_graph(&mut graph, source, &svg).or(source);
            }
            rest = &rest[1..];
            continue;
        }
        // A run of filter functions: one region, grown by what the functions spread
        let len = rest
            .iter()
            .position(|f| matches!(f, StyloFilter::Url(_)))
            .unwrap_or(rest.len());
        let effects: Vec<FilterEffect> = rest[..len]
            .iter()
            .filter_map(|filter| convert_single_filter(filter, current_color))
            .collect();
        rest = &rest[len..];
        let region = Filter::region_for_bounds(bounds, effects.iter());
        let graph = graph.get_or_insert_with(|| Filter::new(region));
        graph.set_region(region);
        for effect in effects {
            let inputs = FilterInputs {
                primary: Some(source_input(source)),
                secondary: None,
            };
            source = Some(graph.add_with_color_space(effect, inputs, FilterColorSpace::Srgb));
        }
    }
    graph.filter(|graph| !graph.nodes().is_empty())
}

fn source_input(source: Option<FilterId>) -> FilterInput {
    match source {
        Some(id) => FilterInput::Result(id),
        None => FilterInput::Source(FilterSource::SourceGraphic),
    }
}

/// Append the primitives of `filter` to `graph`, with `source` (the result made so far, `None`
/// for the element itself) as its source graphic. Returns the id of its result in `graph`.
///
/// Every input is written out: `SourceGraphic` is `source`, a result is that primitive's new id,
/// and `SourceAlpha` is a primitive of its own that keeps the alpha of `source` and nothing else
/// (the rasteriser reads any source input as the source graphic, so a shadow made from
/// `in="SourceAlpha"` came out in the element's colours).
fn append_filter_graph(
    graph: &mut Option<Filter>,
    source: Option<FilterId>,
    filter: &Filter,
) -> Option<FilterId> {
    let first = filter.nodes().first()?;
    let graph = graph.get_or_insert_with(|| Filter::new(first.region));
    let mut ids: Vec<FilterId> = Vec::with_capacity(filter.nodes().len());
    let mut source_alpha: Option<FilterId> = None;
    for (index, node) in filter.nodes().iter().enumerate() {
        let mut map = |input: &Option<FilterInput>, graph: &mut Filter| -> FilterInput {
            match input {
                None => match index.checked_sub(1) {
                    Some(previous) => FilterInput::Result(ids[previous]),
                    None => source_input(source),
                },
                Some(FilterInput::Source(FilterSource::SourceAlpha)) => {
                    let id = *source_alpha.get_or_insert_with(|| {
                        graph.add_node(FilterGraphNode {
                            effect: FilterEffect::ColorMatrix(ColorMatrix::SOURCE_ALPHA),
                            inputs: FilterInputs::single(source_input(source)),
                            color_space: FilterColorSpace::Srgb,
                            subregion: node.region,
                            region: node.region,
                        })
                    });
                    FilterInput::Result(id)
                }
                Some(FilterInput::Source(_)) => source_input(source),
                Some(FilterInput::Result(id)) => match ids.get(id.0 as usize) {
                    Some(id) => FilterInput::Result(*id),
                    None => source_input(source),
                },
            }
        };
        let primary = map(&node.inputs.primary, graph);
        let secondary = map(&node.inputs.secondary, graph);
        let id = graph.add_node(FilterGraphNode {
            effect: node.effect.clone(),
            inputs: FilterInputs::dual(primary, secondary),
            color_space: node.color_space,
            subregion: node.subregion,
            region: node.region,
        });
        ids.push(id);
    }
    ids.get(filter.output().0 as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyrender::filters::composite::CompositeOperator;
    use kurbo::Vec2;

    fn svg_shadow() -> Filter {
        // <feGaussianBlur in="SourceAlpha"/><feOffset/><feComposite in="SourceGraphic" in2=(offset)/>
        let region = Rect::new(-10.0, -10.0, 110.0, 110.0);
        let mut filter = Filter::new(region);
        let blur = filter.add(
            FilterEffect::blur(4.0),
            FilterInputs::single(FilterInput::Source(FilterSource::SourceAlpha)),
        );
        let offset = filter.add(FilterEffect::Offset(Vec2::new(3.0, 3.0)), FilterInputs::NONE);
        assert_eq!((blur, offset), (FilterId(0), FilterId(1)));
        filter.add(
            FilterEffect::Composite(CompositeOperator::Over),
            FilterInputs::dual(
                FilterInput::Source(FilterSource::SourceGraphic),
                FilterInput::Result(offset),
            ),
        );
        filter
    }

    #[test]
    fn source_alpha_becomes_a_primitive_of_its_own() {
        let mut graph = None;
        let out = append_filter_graph(&mut graph, None, &svg_shadow()).unwrap();
        let graph = graph.unwrap();
        // alpha, blur, offset, composite
        assert_eq!(graph.nodes().len(), 4);
        assert_eq!(out, FilterId(3));
        assert_eq!(graph.output(), out);
        assert!(matches!(&graph.nodes()[0].effect, FilterEffect::ColorMatrix(m) if *m == ColorMatrix::SOURCE_ALPHA));
        assert_eq!(
            graph.nodes()[0].inputs.primary,
            Some(FilterInput::Source(FilterSource::SourceGraphic))
        );
        // the blur reads the alpha, the offset the blur, the composite the element over the offset
        assert_eq!(graph.nodes()[1].inputs.primary, Some(FilterInput::Result(FilterId(0))));
        assert_eq!(graph.nodes()[2].inputs.primary, Some(FilterInput::Result(FilterId(1))));
        assert_eq!(
            graph.nodes()[3].inputs,
            FilterInputs::dual(
                FilterInput::Source(FilterSource::SourceGraphic),
                FilterInput::Result(FilterId(2))
            )
        );
        // each primitive keeps its filter's region
        assert!(graph.nodes().iter().all(|n| n.region == Rect::new(-10.0, -10.0, 110.0, 110.0)));
    }

    #[test]
    fn a_filter_after_another_takes_its_result_as_the_source() {
        let mut graph = None;
        let first = append_filter_graph(&mut graph, None, &svg_shadow()).unwrap();
        let second = append_filter_graph(&mut graph, Some(first), &svg_shadow()).unwrap();
        let graph = graph.unwrap();
        assert_eq!(graph.nodes().len(), 8);
        assert_eq!(second, FilterId(7));
        // the second filter's alpha and its SourceGraphic are the first filter's result
        assert_eq!(graph.nodes()[4].inputs.primary, Some(FilterInput::Result(first)));
        assert_eq!(graph.nodes()[7].inputs.primary, Some(FilterInput::Result(first)));
        assert_eq!(graph.nodes()[7].inputs.secondary, Some(FilterInput::Result(FilterId(6))));
    }

    #[test]
    fn a_list_of_functions_alone_is_unchanged() {
        use style::values::generics::NonNegative;
        let black = AbsoluteColor::BLACK;
        let list = [StyloFilter::Brightness(NonNegative(0.5))];
        let bounds = Rect::new(0.0, 0.0, 100.0, 50.0);
        let plain = convert_filters(&list, bounds, &black);
        let with = convert_filter_list(&list, bounds, &black, &|_| unreachable!());
        assert_eq!(plain, with);
        assert_eq!(with.unwrap().nodes().len(), 1);
    }
}
