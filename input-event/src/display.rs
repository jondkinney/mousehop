//! Logical multi-display geometry.
//!
//! Display rectangles use integer compositor coordinates and half-open ranges:
//! a rectangle at `(x, y)` with size `(width, height)` contains coordinates in
//! `x..x + width` and `y..y + height`.  The projection methods return an
//! in-layout cursor coordinate, so the right and bottom edges use the final
//! contained coordinate rather than the exclusive rectangle end.

/// A side of a logical display layout.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DisplayEdge {
    /// The minimum x coordinate at each covered y coordinate.
    Left,
    /// The maximum contained x coordinate at each covered y coordinate.
    Right,
    /// The minimum y coordinate at each covered x coordinate.
    Top,
    /// The maximum contained y coordinate at each covered x coordinate.
    Bottom,
}

impl DisplayEdge {
    fn cross_coordinate(self, point: (i32, i32)) -> i32 {
        match self {
            Self::Left | Self::Right => point.1,
            Self::Top | Self::Bottom => point.0,
        }
    }

    fn point(self, coordinate: i32, cross_coordinate: i32) -> (i32, i32) {
        match self {
            Self::Left | Self::Right => (coordinate, cross_coordinate),
            Self::Top | Self::Bottom => (cross_coordinate, coordinate),
        }
    }
}

/// A valid half-open rectangle in logical display coordinates.
///
/// Use [`DisplayRect::new`] to construct a rectangle. Empty rectangles and
/// rectangles whose exclusive right or bottom coordinate cannot be represented
/// by `i32` are rejected.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DisplayRect {
    x: i32,
    y: i32,
    right: i32,
    bottom: i32,
}

impl DisplayRect {
    /// Constructs a non-empty rectangle, returning `None` for unusable or
    /// overflowing dimensions.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }

        let right = i64::from(x).checked_add(i64::from(width))?;
        let bottom = i64::from(y).checked_add(i64::from(height))?;
        Some(Self {
            x,
            y,
            right: i32::try_from(right).ok()?,
            bottom: i32::try_from(bottom).ok()?,
        })
    }

    /// Returns the rectangle's top-left coordinate.
    pub const fn origin(self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// Returns the rectangle's width and height.
    pub fn size(self) -> (u32, u32) {
        (
            u32::try_from(i64::from(self.right) - i64::from(self.x))
                .expect("validated display width must fit u32"),
            u32::try_from(i64::from(self.bottom) - i64::from(self.y))
                .expect("validated display height must fit u32"),
        )
    }

    /// Returns the inclusive left coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the inclusive top coordinate.
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the rectangle width.
    pub fn width(self) -> u32 {
        self.size().0
    }

    /// Returns the rectangle height.
    pub fn height(self) -> u32 {
        self.size().1
    }

    /// Returns the exclusive right coordinate.
    pub const fn right(self) -> i32 {
        self.right
    }

    /// Returns the exclusive bottom coordinate.
    pub const fn bottom(self) -> i32 {
        self.bottom
    }

    /// Returns whether the rectangle contains `point`.
    pub const fn contains(self, point: (i32, i32)) -> bool {
        point.0 >= self.x && point.0 < self.right && point.1 >= self.y && point.1 < self.bottom
    }

    /// Returns whether the rectangle contains a floating-point cursor point.
    pub fn contains_f64(self, point: (f64, f64)) -> bool {
        point.0 >= f64::from(self.x)
            && point.0 < f64::from(self.right)
            && point.1 >= f64::from(self.y)
            && point.1 < f64::from(self.bottom)
    }

    /// Clamps a floating-point point to the rectangle's contained pixel range.
    pub fn clamp_f64(self, point: (f64, f64)) -> (f64, f64) {
        (
            point.0.clamp(f64::from(self.x), f64::from(self.right - 1)),
            point.1.clamp(f64::from(self.y), f64::from(self.bottom - 1)),
        )
    }

    fn cross_range(self, edge: DisplayEdge) -> (i32, i32) {
        match edge {
            DisplayEdge::Left | DisplayEdge::Right => (self.y, self.bottom),
            DisplayEdge::Top | DisplayEdge::Bottom => (self.x, self.right),
        }
    }

    fn edge_coordinate(self, edge: DisplayEdge) -> i32 {
        match edge {
            DisplayEdge::Left => self.x,
            DisplayEdge::Right => self.right - 1,
            DisplayEdge::Top => self.y,
            DisplayEdge::Bottom => self.bottom - 1,
        }
    }
}

/// A non-overlapping portion of an exposed display-layout edge.
///
/// `start..end` is a half-open range on the cross axis: it is a y range for
/// [`DisplayEdge::Left`] and [`DisplayEdge::Right`], and an x range for
/// [`DisplayEdge::Top`] and [`DisplayEdge::Bottom`]. `coordinate` is the x or y
/// coordinate of the last in-layout pixel on that edge. `rect_index` is the
/// originating rectangle's input index in [`DisplayLayout::new`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EdgeSegment {
    /// The side represented by this segment.
    pub edge: DisplayEdge,
    /// The originating rectangle's input index.
    pub rect_index: usize,
    /// Inclusive cross-axis start.
    pub start: i32,
    /// Exclusive cross-axis end.
    pub end: i32,
    /// In-layout x (vertical sides) or y (horizontal sides).
    pub coordinate: i32,
}

impl EdgeSegment {
    /// Returns the segment length in logical coordinates.
    pub fn len(self) -> u32 {
        if self.is_empty() {
            0
        } else {
            // Every positive difference between two i32 values fits u32.
            (i64::from(self.end) - i64::from(self.start)) as u32
        }
    }

    /// Returns whether this segment has no cross-axis coordinates.
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }

    /// Returns the contour point at `cross_coordinate`, or `None` when the
    /// coordinate is outside this segment.
    pub const fn point_at(self, cross_coordinate: i32) -> Option<(i32, i32)> {
        if cross_coordinate < self.start || cross_coordinate >= self.end {
            return None;
        }
        Some(match self.edge {
            DisplayEdge::Left | DisplayEdge::Right => (self.coordinate, cross_coordinate),
            DisplayEdge::Top | DisplayEdge::Bottom => (cross_coordinate, self.coordinate),
        })
    }
}

/// A validated collection of logical display rectangles.
///
/// Input indices are retained when [`DisplayLayout::new`] filters an invalid
/// rectangle. Consequently, [`EdgeSegment::rect_index`] can be used to look up
/// the original compositor output without maintaining a second index map.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DisplayLayout {
    rects: Vec<(usize, DisplayRect)>,
}

impl DisplayLayout {
    /// Builds a layout from `(x, y, width, height)` tuples.
    ///
    /// Empty rectangles and rectangles with overflowing exclusive ends are
    /// discarded. An exposed segment retains the tuple's original input index.
    pub fn new<I>(rectangles: I) -> Self
    where
        I: IntoIterator<Item = (i32, i32, u32, u32)>,
    {
        Self {
            rects: rectangles
                .into_iter()
                .enumerate()
                .filter_map(|(index, (x, y, width, height))| {
                    DisplayRect::new(x, y, width, height).map(|rect| (index, rect))
                })
                .collect(),
        }
    }

    /// Builds a layout from already validated rectangles.
    pub fn from_rects<I>(rectangles: I) -> Self
    where
        I: IntoIterator<Item = DisplayRect>,
    {
        Self {
            rects: rectangles.into_iter().enumerate().collect(),
        }
    }

    /// Returns the number of usable rectangles.
    pub fn len(&self) -> usize {
        self.rects.len()
    }

    /// Returns whether the layout contains no usable rectangles.
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Iterates over `(input_index, rectangle)` pairs.
    pub fn rectangles(&self) -> impl ExactSizeIterator<Item = (usize, DisplayRect)> + '_ {
        self.rects.iter().copied()
    }

    /// Returns the rectangle with an original input index.
    pub fn rectangle(&self, input_index: usize) -> Option<DisplayRect> {
        self.rects
            .iter()
            .find_map(|&(index, rect)| (index == input_index).then_some(rect))
    }

    /// Returns the half-open bounding union of all usable rectangles.
    pub fn bounds(&self) -> Option<DisplayRect> {
        let &(_, first) = self.rects.first()?;
        let (mut left, mut top) = first.origin();
        let mut right = first.right();
        let mut bottom = first.bottom();

        for &(_, rect) in &self.rects[1..] {
            left = left.min(rect.x());
            top = top.min(rect.y());
            right = right.max(rect.right());
            bottom = bottom.max(rect.bottom());
        }

        let width = u32::try_from(i64::from(right) - i64::from(left)).ok()?;
        let height = u32::try_from(i64::from(bottom) - i64::from(top)).ok()?;
        DisplayRect::new(left, top, width, height)
    }

    /// Returns the bounding union's top-left coordinate.
    pub fn origin(&self) -> Option<(i32, i32)> {
        self.bounds().map(DisplayRect::origin)
    }

    /// Returns the bounding union's width and height.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.bounds().map(DisplayRect::size)
    }

    /// Returns `point` unchanged when it lies on a display, otherwise snaps it
    /// to the closest contained point across every display rectangle.
    ///
    /// This mirrors compositors such as Hyprland when relative motion ends in
    /// a gap between outputs. Equal-distance ties prefer the lower original
    /// rectangle index. Non-finite points return `None`.
    pub fn clamp_to_nearest_display(&self, point: (f64, f64)) -> Option<(f64, f64)> {
        if !point.0.is_finite() || !point.1.is_finite() {
            return None;
        }
        if self.rectangles().any(|(_, rect)| rect.contains_f64(point)) {
            return Some(point);
        }
        self.rectangles()
            .map(|(index, rect)| {
                let clamped = rect.clamp_f64(point);
                let dx = point.0 - clamped.0;
                let dy = point.1 - clamped.1;
                (dx.mul_add(dx, dy * dy), index, clamped)
            })
            .min_by(|left, right| {
                left.0
                    .total_cmp(&right.0)
                    .then_with(|| left.1.cmp(&right.1))
            })
            .map(|(_, _, clamped)| clamped)
    }

    /// Enumerates the non-overlapping exposed segments on `edge`.
    ///
    /// At every cross-axis coordinate the contour is owned by the rectangle
    /// with the minimum edge coordinate for left/top or the maximum edge
    /// coordinate for right/bottom. Equal coordinates are resolved by the
    /// lowest original input index. Adjacent ranges remain separate when their
    /// owning rectangles differ, allowing callers to create per-output edge
    /// surfaces.
    pub fn exposed_segments(&self, edge: DisplayEdge) -> Vec<EdgeSegment> {
        let mut cuts = Vec::with_capacity(self.rects.len().saturating_mul(2));
        for &(_, rect) in &self.rects {
            let (start, end) = rect.cross_range(edge);
            cuts.push(start);
            cuts.push(end);
        }
        cuts.sort_unstable();
        cuts.dedup();

        let mut segments: Vec<EdgeSegment> = Vec::new();
        for cut_pair in cuts.windows(2) {
            let start = cut_pair[0];
            let end = cut_pair[1];
            let owner = self
                .rects
                .iter()
                .filter(|(_, rect)| {
                    let (rect_start, rect_end) = rect.cross_range(edge);
                    rect_start <= start && rect_end >= end
                })
                .min_by(|(left_index, left_rect), (right_index, right_rect)| {
                    let left_coordinate = left_rect.edge_coordinate(edge);
                    let right_coordinate = right_rect.edge_coordinate(edge);
                    let ordering = match edge {
                        DisplayEdge::Left | DisplayEdge::Top => {
                            left_coordinate.cmp(&right_coordinate)
                        }
                        DisplayEdge::Right | DisplayEdge::Bottom => {
                            right_coordinate.cmp(&left_coordinate)
                        }
                    };
                    ordering.then_with(|| left_index.cmp(right_index))
                });

            let Some(&(rect_index, rect)) = owner else {
                continue;
            };
            let segment = EdgeSegment {
                edge,
                rect_index,
                start,
                end,
                coordinate: rect.edge_coordinate(edge),
            };
            let extends_previous = segments.last().is_some_and(|previous| {
                previous.end == segment.start
                    && previous.rect_index == segment.rect_index
                    && previous.coordinate == segment.coordinate
            });
            if extends_previous {
                segments.last_mut().expect("previous segment").end = segment.end;
            } else {
                segments.push(segment);
            }
        }
        segments
    }

    /// Projects `desired` onto the actual exposed contour on `edge`.
    ///
    /// The desired point's on-axis coordinate is ignored. If its cross-axis
    /// coordinate lies in a layout gap, it is snapped to the nearest contained
    /// coordinate; equal-distance ties prefer the lower coordinate.
    pub fn project_point(&self, edge: DisplayEdge, desired: (i32, i32)) -> Option<(i32, i32)> {
        let desired_cross = edge.cross_coordinate(desired);
        let segments = self.exposed_segments(edge);
        let (segment, cross_coordinate) = nearest_segment(&segments, desired_cross)?;
        Some(edge.point(segment.coordinate, cross_coordinate))
    }

    /// Projects a normalized cross-axis fraction onto the exposed contour.
    ///
    /// The fraction is measured across the layout's bounding union, clamped to
    /// `0.0..=1.0`, then projected through any gap. `1.0` maps to the final
    /// contained coordinate. Non-finite fractions return `None`.
    pub fn project_fraction(&self, edge: DisplayEdge, fraction: f64) -> Option<(i32, i32)> {
        if !fraction.is_finite() {
            return None;
        }
        let bounds = self.bounds()?;
        let (cross_start, cross_length) = match edge {
            DisplayEdge::Left | DisplayEdge::Right => (bounds.y(), bounds.height()),
            DisplayEdge::Top | DisplayEdge::Bottom => (bounds.x(), bounds.width()),
        };
        let fraction = fraction.clamp(0.0, 1.0);
        let offset = if fraction == 1.0 {
            cross_length - 1
        } else {
            ((fraction * f64::from(cross_length)).floor() as u32).min(cross_length - 1)
        };
        let cross_coordinate = i32::try_from(i64::from(cross_start) + i64::from(offset)).ok()?;
        let desired = match edge {
            DisplayEdge::Left | DisplayEdge::Right => (0, cross_coordinate),
            DisplayEdge::Top | DisplayEdge::Bottom => (cross_coordinate, 0),
        };
        self.project_point(edge, desired)
    }
}

fn nearest_segment(segments: &[EdgeSegment], desired: i32) -> Option<(EdgeSegment, i32)> {
    segments
        .iter()
        .copied()
        .filter_map(|segment| {
            if segment.is_empty() {
                return None;
            }
            let cross_coordinate = desired.clamp(segment.start, segment.end - 1);
            let distance = (i64::from(desired) - i64::from(cross_coordinate)).abs();
            Some((distance, cross_coordinate, segment))
        })
        .min_by_key(|&(distance, cross_coordinate, segment)| {
            (distance, cross_coordinate, segment.rect_index)
        })
        .map(|(_, cross_coordinate, segment)| (segment, cross_coordinate))
}

#[cfg(test)]
mod tests {
    use super::{DisplayEdge, DisplayLayout, DisplayRect, EdgeSegment};

    const LINUX_RECTS: [(i32, i32, u32, u32); 3] = [
        (-1024, 0, 1024, 600),  // DVI-I-1
        (0, 0, 3072, 1728),     // DP-5
        (836, 1728, 1280, 360), // DP-6
    ];

    const MAC_RECTS: [(i32, i32, u32, u32); 2] = [
        (0, 0, 3072, 1728),     // main
        (-1728, 0, 1728, 1117), // built-in
    ];

    fn segment(
        edge: DisplayEdge,
        rect_index: usize,
        start: i32,
        end: i32,
        coordinate: i32,
    ) -> EdgeSegment {
        EdgeSegment {
            edge,
            rect_index,
            start,
            end,
            coordinate,
        }
    }

    fn brute_contour(layout: &DisplayLayout, edge: DisplayEdge, cross: i32) -> Option<i32> {
        let coordinates = layout.rectangles().filter_map(|(_, rect)| {
            let covered = match edge {
                DisplayEdge::Left | DisplayEdge::Right => {
                    cross >= rect.y() && cross < rect.bottom()
                }
                DisplayEdge::Top | DisplayEdge::Bottom => cross >= rect.x() && cross < rect.right(),
            };
            covered.then(|| match edge {
                DisplayEdge::Left => rect.x(),
                DisplayEdge::Right => rect.right() - 1,
                DisplayEdge::Top => rect.y(),
                DisplayEdge::Bottom => rect.bottom() - 1,
            })
        });
        match edge {
            DisplayEdge::Left | DisplayEdge::Top => coordinates.min(),
            DisplayEdge::Right | DisplayEdge::Bottom => coordinates.max(),
        }
    }

    fn assert_every_cross_coordinate(layout: &DisplayLayout, edge: DisplayEdge) {
        let bounds = layout.bounds().unwrap();
        let (start, end) = match edge {
            DisplayEdge::Left | DisplayEdge::Right => (bounds.y(), bounds.bottom()),
            DisplayEdge::Top | DisplayEdge::Bottom => (bounds.x(), bounds.right()),
        };
        for cross in start..end {
            let desired = match edge {
                DisplayEdge::Left | DisplayEdge::Right => (123, cross),
                DisplayEdge::Top | DisplayEdge::Bottom => (cross, 123),
            };
            let projected = layout.project_point(edge, desired).unwrap();
            let projected_coordinate = match edge {
                DisplayEdge::Left | DisplayEdge::Right => projected.0,
                DisplayEdge::Top | DisplayEdge::Bottom => projected.1,
            };
            assert_eq!(
                projected_coordinate,
                brute_contour(layout, edge, cross).unwrap(),
                "wrong {edge:?} contour at cross coordinate {cross}"
            );
        }
    }

    #[test]
    fn rectangle_construction_is_half_open_and_overflow_safe() {
        let rect = DisplayRect::new(-10, -20, 30, 40).unwrap();
        assert_eq!(rect.origin(), (-10, -20));
        assert_eq!(rect.size(), (30, 40));
        assert_eq!((rect.right(), rect.bottom()), (20, 20));
        assert!(rect.contains((-10, -20)));
        assert!(rect.contains((19, 19)));
        assert!(!rect.contains((20, 19)));
        assert!(!rect.contains((19, 20)));

        assert_eq!(DisplayRect::new(0, 0, 0, 1), None);
        assert_eq!(DisplayRect::new(0, 0, 1, 0), None);
        assert_eq!(DisplayRect::new(i32::MAX, 0, 1, 1), None);
        assert_eq!(DisplayRect::new(0, i32::MAX, 1, 1), None);
        assert!(DisplayRect::new(i32::MAX - 1, i32::MAX - 1, 1, 1).is_some());

        let widest = DisplayRect::new(i32::MIN, 0, u32::MAX, 1).unwrap();
        assert_eq!(widest.right(), i32::MAX);
        assert_eq!(widest.width(), u32::MAX);
    }

    #[test]
    fn layout_filters_unusable_rectangles_and_retains_input_indices() {
        let layout = DisplayLayout::new([
            (0, 0, 0, 10),
            (-10, -20, 30, 40),
            (i32::MAX, 0, 1, 1),
            (50, 50, 10, 0),
        ]);
        assert_eq!(layout.len(), 1);
        assert!(!layout.is_empty());
        assert_eq!(layout.rectangle(0), None);
        assert_eq!(layout.rectangle(1), DisplayRect::new(-10, -20, 30, 40));
        assert_eq!(layout.rectangle(2), None);
        assert_eq!(
            layout.rectangles().collect::<Vec<_>>(),
            vec![(1, layout.rectangle(1).unwrap())]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Left),
            vec![segment(DisplayEdge::Left, 1, -20, 20, -10)]
        );
        assert_eq!(DisplayLayout::new([]).bounds(), None);
    }

    #[test]
    fn linux_layout_has_exact_bounds_and_exposed_segments() {
        let layout = DisplayLayout::new(LINUX_RECTS);
        assert_eq!(layout.origin(), Some((-1024, 0)));
        assert_eq!(layout.size(), Some((4096, 2088)));
        assert_eq!(layout.bounds(), DisplayRect::new(-1024, 0, 4096, 2088));
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Left),
            vec![
                segment(DisplayEdge::Left, 0, 0, 600, -1024),
                segment(DisplayEdge::Left, 1, 600, 1728, 0),
                segment(DisplayEdge::Left, 2, 1728, 2088, 836),
            ]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Right),
            vec![
                segment(DisplayEdge::Right, 1, 0, 1728, 3071),
                segment(DisplayEdge::Right, 2, 1728, 2088, 2115),
            ]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Top),
            vec![
                segment(DisplayEdge::Top, 0, -1024, 0, 0),
                segment(DisplayEdge::Top, 1, 0, 3072, 0),
            ]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Bottom),
            vec![
                segment(DisplayEdge::Bottom, 0, -1024, 0, 599),
                segment(DisplayEdge::Bottom, 1, 0, 836, 1727),
                segment(DisplayEdge::Bottom, 2, 836, 2116, 2087),
                segment(DisplayEdge::Bottom, 1, 2116, 3072, 1727),
            ]
        );
    }

    #[test]
    fn linux_projection_follows_every_side_and_owner_boundary() {
        let layout = DisplayLayout::new(LINUX_RECTS);
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (42, 100)),
            Some((-1024, 100))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (42, 600)),
            Some((0, 600))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Right, (42, 1727)),
            Some((3071, 1727))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Right, (42, 1728)),
            Some((2115, 1728))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Top, (-500, 42)),
            Some((-500, 0))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Top, (1000, 42)),
            Some((1000, 0))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (500, 42)),
            Some((500, 1727))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (1000, 42)),
            Some((1000, 2087))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (2500, 42)),
            Some((2500, 1727))
        );

        for edge in [
            DisplayEdge::Left,
            DisplayEdge::Right,
            DisplayEdge::Top,
            DisplayEdge::Bottom,
        ] {
            assert_every_cross_coordinate(&layout, edge);
        }
    }

    #[test]
    fn linux_normalized_projection_uses_union_then_contour() {
        let layout = DisplayLayout::new(LINUX_RECTS);
        assert_eq!(
            layout.project_fraction(DisplayEdge::Left, 0.0),
            Some((-1024, 0))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Left, 0.5),
            Some((0, 1044))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Right, 1.0),
            Some((2115, 2087))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Top, 0.0),
            Some((-1024, 0))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Top, 1.0),
            Some((3071, 0))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Bottom, 0.5),
            Some((1024, 2087))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Bottom, -2.0),
            Some((-1024, 599))
        );
        assert_eq!(
            layout.project_fraction(DisplayEdge::Bottom, 2.0),
            Some((3071, 1727))
        );
        assert_eq!(layout.project_fraction(DisplayEdge::Left, f64::NAN), None);
        assert_eq!(
            layout.project_fraction(DisplayEdge::Left, f64::INFINITY),
            None
        );
    }

    #[test]
    fn mac_layout_has_exact_contour_and_owner_boundaries() {
        let layout = DisplayLayout::new(MAC_RECTS);
        assert_eq!(layout.origin(), Some((-1728, 0)));
        assert_eq!(layout.size(), Some((4800, 1728)));
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Left),
            vec![
                segment(DisplayEdge::Left, 1, 0, 1117, -1728),
                segment(DisplayEdge::Left, 0, 1117, 1728, 0),
            ]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Right),
            vec![segment(DisplayEdge::Right, 0, 0, 1728, 3071)]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Top),
            vec![
                segment(DisplayEdge::Top, 1, -1728, 0, 0),
                segment(DisplayEdge::Top, 0, 0, 3072, 0),
            ]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Bottom),
            vec![
                segment(DisplayEdge::Bottom, 1, -1728, 0, 1116),
                segment(DisplayEdge::Bottom, 0, 0, 3072, 1727),
            ]
        );

        assert_eq!(
            layout.project_point(DisplayEdge::Left, (42, 1116)),
            Some((-1728, 1116))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (42, 1117)),
            Some((0, 1117))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Right, (42, 1117)),
            Some((3071, 1117))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Top, (-1000, 42)),
            Some((-1000, 0))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (-1000, 42)),
            Some((-1000, 1116))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (1000, 42)),
            Some((1000, 1727))
        );

        for edge in [
            DisplayEdge::Left,
            DisplayEdge::Right,
            DisplayEdge::Top,
            DisplayEdge::Bottom,
        ] {
            assert_every_cross_coordinate(&layout, edge);
        }
    }

    #[test]
    fn gaps_snap_to_nearest_covered_coordinate_with_lower_tie_break() {
        let layout = DisplayLayout::new([(0, 0, 10, 10), (19, 19, 11, 11)]);
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (999, 14)),
            Some((0, 9))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (999, 15)),
            Some((19, 19))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Right, (999, 14)),
            Some((9, 9))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Top, (14, 999)),
            Some((9, 0))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Bottom, (15, 999)),
            Some((19, 29))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Left, (999, i32::MIN)),
            Some((0, 0))
        );
        assert_eq!(
            layout.project_point(DisplayEdge::Right, (999, i32::MAX)),
            Some((29, 29))
        );
    }

    #[test]
    fn invalid_motion_endpoint_snaps_to_nearest_monitor_not_current_monitor() {
        let layout = DisplayLayout::new(LINUX_RECTS);
        assert_eq!(
            layout.clamp_to_nearest_display((2200.0, 1900.0)),
            Some((2115.0, 1900.0))
        );
        assert_eq!(
            layout.clamp_to_nearest_display((1000.25, 500.5)),
            Some((1000.25, 500.5))
        );
        assert_eq!(layout.clamp_to_nearest_display((f64::NAN, 0.0)), None);
    }

    #[test]
    fn overlapping_rectangles_choose_outermost_edge_and_stable_owner() {
        let layout = DisplayLayout::new([(0, 0, 100, 100), (20, 20, 20, 20), (0, 20, 100, 20)]);
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Left),
            vec![segment(DisplayEdge::Left, 0, 0, 100, 0)]
        );
        assert_eq!(
            layout.exposed_segments(DisplayEdge::Bottom),
            vec![segment(DisplayEdge::Bottom, 0, 0, 100, 99)]
        );
    }

    #[test]
    fn segment_point_and_validated_layout_constructor_are_consistent() {
        let rect = DisplayRect::new(-5, -10, 15, 20).unwrap();
        let layout = DisplayLayout::from_rects([rect]);
        let segment = layout.exposed_segments(DisplayEdge::Right)[0];
        assert_eq!(segment.len(), 20);
        assert!(!segment.is_empty());
        assert_eq!(segment.point_at(-10), Some((9, -10)));
        assert_eq!(segment.point_at(9), Some((9, 9)));
        assert_eq!(segment.point_at(10), None);
    }
}
