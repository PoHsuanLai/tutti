//! Progress reporting types and a stateful emitter helper.
//!
//! Public callers see only [`Phase`]: progress callbacks take the form
//! `Fn(Phase, f32)`. Internal stages use [`ProgressEmitter`] for sample-
//! driven phases and [`PhaseGuard`] for stages without a natural sample
//! count.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    Render,
    Process,
    Encode,
}

/// Stateful progress emitter for sample-driven stages (currently: rendering).
/// Emits at construction (`start`), at `interval`-sample boundaries via
/// `tick`, and once at completion via `finish`.
pub(crate) struct ProgressEmitter<'a> {
    on_progress: &'a (dyn Fn(Phase, f32) + Send + Sync),
    phase: Phase,
    interval: usize,
    next: usize,
    total: usize,
}

impl<'a> ProgressEmitter<'a> {
    pub fn new(
        on_progress: &'a (dyn Fn(Phase, f32) + Send + Sync),
        phase: Phase,
        total_samples: usize,
        sample_rate: f64,
    ) -> Self {
        let interval = (sample_rate * 0.5).max(1.0) as usize;
        Self {
            on_progress,
            phase,
            interval,
            next: interval,
            total: total_samples.max(1),
        }
    }

    pub fn start(&self) {
        (self.on_progress)(self.phase, 0.0);
    }

    pub fn tick(&mut self, samples_processed: usize) {
        if samples_processed >= self.next || samples_processed >= self.total {
            (self.on_progress)(self.phase, samples_processed as f32 / self.total as f32);
            self.next += self.interval;
        }
    }

    pub fn finish(&self) {
        (self.on_progress)(self.phase, 1.0);
    }
}

/// RAII bracket for stages without a natural sample count. Emits
/// `(phase, 0.0)` on construction, `(phase, 1.0)` on drop.
pub(crate) struct PhaseGuard<'a> {
    on_progress: &'a (dyn Fn(Phase, f32) + Send + Sync),
    phase: Phase,
}

impl<'a> PhaseGuard<'a> {
    pub fn new(on_progress: &'a (dyn Fn(Phase, f32) + Send + Sync), phase: Phase) -> Self {
        on_progress(phase, 0.0);
        Self { on_progress, phase }
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        (self.on_progress)(self.phase, 1.0);
    }
}
