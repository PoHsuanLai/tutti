use tutti_types::Beat;

use crate::Law;

/// A breakpoint of a [`Trajectory`]: a value at a beat, and the
/// [`Law`] of the piece that starts there and runs to the next breakpoint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Breakpoint {
    /// Where the breakpoint is.
    pub time: Beat,
    /// The value there. Unit-erased: the trajectory's lane knows its target's
    /// unit.
    pub value: f32,
    /// How the value moves from here to the next breakpoint.
    pub curve: Law,
}

impl Breakpoint {
    /// A breakpoint whose piece is [`Law::Linear`].
    #[must_use]
    pub const fn new(time: Beat, value: f32) -> Self {
        Self::with_curve(time, value, Law::Linear)
    }

    /// A breakpoint whose piece follows `curve`.
    #[must_use]
    pub const fn with_curve(time: Beat, value: f32, curve: Law) -> Self {
        Self { time, value, curve }
    }
}

/// Breakpoints in time order, read at a beat: the least a lane or a recorder
/// needs of automation.
///
/// Before the first breakpoint and from the last on, the value is that breakpoint's.
/// Between two breakpoints it is the first's value moving to the second's along the
/// first's [`Law`]. A trajectory may carry a range its values are clamped to.
/// It has no value (`None`) while it has no breakpoint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Trajectory {
    breakpoints: Vec<Breakpoint>,
    range: Option<(f32, f32)>,
}

impl Trajectory {
    /// An empty trajectory with no range.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same trajectory with every value it reads clamped to `[min, max]`.
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

    /// The breakpoints, in time order.
    #[must_use]
    pub fn breakpoints(&self) -> &[Breakpoint] {
        &self.breakpoints
    }

    /// How many breakpoints there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.breakpoints.len()
    }

    /// Whether there is no breakpoint.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.breakpoints.is_empty()
    }

    /// Inserts `breakpoint` in time order; it replaces a breakpoint at the same time.
    pub fn add_breakpoint(&mut self, breakpoint: Breakpoint) -> &mut Self {
        let at = breakpoint.time.get();
        match self
            .breakpoints
            .binary_search_by(|p| p.time.get().total_cmp(&at))
        {
            Ok(i) => self.breakpoints[i] = breakpoint,
            Err(i) => self.breakpoints.insert(i, breakpoint),
        }
        self
    }

    /// The value at `beat`, or `None` for an empty trajectory. Allocation-free:
    /// a lane reads it on the audio thread.
    #[must_use]
    #[inline]
    pub fn value_at(&self, beat: Beat) -> Option<f32> {
        let (first, last) = (self.breakpoints.first()?, self.breakpoints.last()?);
        let time = beat.get();
        if time <= first.time.get() {
            return Some(self.clamped(first.value));
        }
        if time >= last.time.get() {
            return Some(self.clamped(last.value));
        }
        // `first.time < time < last.time`: the piece is the one before the
        // first breakpoint after `time`; a breakpoint exactly at `time` starts its own.
        let next = self.breakpoints.partition_point(|p| p.time.get() <= time);
        let (p, q) = (self.breakpoints[next - 1], self.breakpoints[next]);
        let t = ((time - p.time.get()) / (q.time.get() - p.time.get())) as f32;
        Some(self.clamped(p.curve.interpolate(p.value, q.value, t)))
    }

    /// Drops every breakpoint whose value its neighbours' piece already gives to
    /// within `tolerance`, in value space; the first and last stay.
    pub fn simplify(&mut self, tolerance: f32) -> &mut Self {
        if self.breakpoints.len() <= 2 {
            return self;
        }
        let last = self.breakpoints.len() - 1;
        let mut kept = vec![self.breakpoints[0]];
        for i in 1..last {
            let (prev, curr, next) = (
                self.breakpoints[i - 1],
                self.breakpoints[i],
                self.breakpoints[i + 1],
            );
            let span = next.time.get() - prev.time.get();
            let t = ((curr.time.get() - prev.time.get()) / span) as f32;
            if (curr.value - prev.curve.interpolate(prev.value, next.value, t)).abs() > tolerance {
                kept.push(curr);
            }
        }
        kept.push(self.breakpoints[last]);
        self.breakpoints = kept;
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
