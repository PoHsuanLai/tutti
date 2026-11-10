use tutti_types::Beat;

use crate::Law;

/// A breakpoint of an [`AutomationEnvelope`]: a value at a beat, and the
/// [`Law`] of the piece that starts there and runs to the next point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutomationPoint {
    /// Where the point is.
    pub time: Beat,
    /// The value there. Unit-erased: the envelope's lane knows its target's
    /// unit.
    pub value: f32,
    /// How the value moves from here to the next point.
    pub curve: Law,
}

impl AutomationPoint {
    /// A point whose piece is [`Law::Linear`].
    #[must_use]
    pub const fn new(time: Beat, value: f32) -> Self {
        Self::with_curve(time, value, Law::Linear)
    }

    /// A point whose piece follows `curve`.
    #[must_use]
    pub const fn with_curve(time: Beat, value: f32, curve: Law) -> Self {
        Self { time, value, curve }
    }
}

/// Breakpoints in time order, read at a beat: the least a lane or a recorder
/// needs of automation.
///
/// Before the first point and from the last on, the value is that point's.
/// Between two points it is the first's value moving to the second's along the
/// first's [`Law`]. An envelope may carry a range its values are clamped to.
/// It has no value (`None`) while it has no point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutomationEnvelope {
    points: Vec<AutomationPoint>,
    range: Option<(f32, f32)>,
}

impl AutomationEnvelope {
    /// An empty envelope with no range.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same envelope with every value it reads clamped to `[min, max]`.
    #[must_use]
    pub fn with_range(mut self, min: f32, max: f32) -> Self {
        self.range = Some((min, max));
        self
    }

    /// The range values are clamped to, if there is one.
    #[must_use]
    pub fn range(&self) -> Option<(f32, f32)> {
        self.range
    }

    /// The points, in time order.
    #[must_use]
    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    /// How many points there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Whether there is no point.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Inserts `point` in time order; it replaces a point at the same time.
    pub fn add_point(&mut self, point: AutomationPoint) -> &mut Self {
        let at = point.time.get();
        match self
            .points
            .binary_search_by(|p| p.time.get().total_cmp(&at))
        {
            Ok(i) => self.points[i] = point,
            Err(i) => self.points.insert(i, point),
        }
        self
    }

    /// The value at `beat`, or `None` for an empty envelope. Allocation-free:
    /// a lane reads it on the audio thread.
    #[must_use]
    #[inline]
    pub fn value_at(&self, beat: Beat) -> Option<f32> {
        let (first, last) = (self.points.first()?, self.points.last()?);
        let time = beat.get();
        if time <= first.time.get() {
            return Some(self.clamped(first.value));
        }
        if time >= last.time.get() {
            return Some(self.clamped(last.value));
        }
        // `first.time < time < last.time`: the piece is the one before the
        // first point after `time`; a point exactly at `time` starts its own.
        let next = self.points.partition_point(|p| p.time.get() <= time);
        let (p, q) = (self.points[next - 1], self.points[next]);
        let t = ((time - p.time.get()) / (q.time.get() - p.time.get())) as f32;
        Some(self.clamped(p.curve.interpolate(p.value, q.value, t)))
    }

    /// Drops every point whose value its neighbours' piece already gives to
    /// within `tolerance`, in value space; the first and last stay.
    pub fn simplify(&mut self, tolerance: f32) -> &mut Self {
        if self.points.len() <= 2 {
            return self;
        }
        let last = self.points.len() - 1;
        let mut kept = vec![self.points[0]];
        for i in 1..last {
            let (prev, curr, next) = (self.points[i - 1], self.points[i], self.points[i + 1]);
            let span = next.time.get() - prev.time.get();
            let t = ((curr.time.get() - prev.time.get()) / span) as f32;
            if (curr.value - prev.curve.interpolate(prev.value, next.value, t)).abs() > tolerance {
                kept.push(curr);
            }
        }
        kept.push(self.points[last]);
        self.points = kept;
        self
    }

    fn clamped(&self, value: f32) -> f32 {
        match self.range {
            Some((min, max)) => value.max(min).min(max),
            None => value,
        }
    }
}

#[cfg(test)]
mod tests;
