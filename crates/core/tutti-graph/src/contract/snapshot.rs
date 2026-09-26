//! The fork's other contract: a forked unit renders a **snapshot** of its
//! controls, taken at fork time.
//!
//! `AudioUnit::forkable() == true` promises that `isolate` severs all
//! shared mutable state (see the trait). A `Param` cell the unit only
//! *reads* is shared mutable state too: a UI knob or an automation lane
//! writes it, so a clone still holding it follows every live move while an
//! offline export runs. [`IsolateRow::check`] pins the promise from the
//! outside, per control, and does not care how the unit stores it.
//!
//! For each control, on a freshly made unit:
//!
//! 1. fork it as [`Editor::fork`](crate::Editor::fork) does — `clone`, then
//!    `isolate` — and render a copy of the fork (the *before*);
//! 2. move the control on the **live** unit, through `&U` only: a write
//!    that has to go through a shared reference can only reach a shared
//!    cell, which is exactly what a UI thread holds;
//! 3. render the fork again, and require it **bit-identical** to *before*
//!    — the snapshot held;
//! 4. fork the live unit again and render that, and require it to
//!    **differ** from *before* — the control is audible under the row's
//!    stimulus, and a fork taken now sees the new value (`isolate` keeps
//!    current values, it does not reset them).
//!
//! Step 4 is what makes step 3 able to fail: a control the stimulus cannot
//! hear would pass step 3 whether or not `isolate` severed it, so a row
//! whose control is inaudible fails loudly instead of claiming coverage.
//!
//! Every render starts from `allocate()` and `reset()` — the fork's own
//! last step — then
//! the row's [`IsolateRow::excite`] (a note-on, say), then the row's
//! [`IsolateRow::input`] on every input, through `process` in blocks of
//! [`MAX_BUFFER_SIZE`] frames at [`SAMPLE_RATE`](super::SAMPLE_RATE).
//! Comparisons are within one run on one machine, so bit equality is sound
//! on every platform.
//!
//! ```
//! use tutti_graph::contract::IsolateRow;
//! # use tutti_node::buffer::{BufferMut, BufferRef};
//! # use tutti_node::{AudioUnit, SignalFrame};
//! # use tutti_types::{Amplitude, Param};
//! # #[derive(Clone)]
//! # struct Gain(Param<Amplitude>);
//! # impl Gain {
//! #     fn gain(&self) -> Param<Amplitude> { self.0.handle() }
//! # }
//! # impl AudioUnit for Gain {
//! #     fn isolate(&mut self) { self.0.detach(); }
//! #     fn tick(&mut self, i: &[f32], o: &mut [f32]) { o[0] = i[0] * self.0.load().get(); }
//! #     fn process(&mut self, size: usize, i: &BufferRef, o: &mut BufferMut) {
//! #         let g = self.0.load().get();
//! #         for n in 0..size { o.set_f32(0, n, i.at_f32(0, n) * g); }
//! #     }
//! #     fn inputs(&self) -> usize { 1 }
//! #     fn outputs(&self) -> usize { 1 }
//! #     fn route(&mut self, input: &SignalFrame, _: f64) -> SignalFrame { input.clone() }
//! #     fn get_id(&self) -> u64 { 0 }
//! #     fn as_any(&self) -> &dyn core::any::Any { self }
//! #     fn as_any_mut(&mut self) -> &mut dyn core::any::Any { self }
//! # }
//! IsolateRow::new("gain", || Gain(Param::new(Amplitude::new(0.5))))
//!     .control("gain", |g| g.gain().store(Amplitude::new(0.25)))
//!     .check();
//! ```

use tutti_node::buffer::BufferVec;
use tutti_node::{AudioUnit, MAX_BUFFER_SIZE};

use crate::controls::{ParamFork, ParamNode};

use super::SAMPLE_RATE;

/// Frames rendered per comparison unless [`IsolateRow::frames`] says
/// otherwise: long enough for a 100 ms release, a 1 s LFO cycle's first
/// quarter and several loud/quiet stimulus cycles.
pub const SNAPSHOT_FRAMES: usize = 16_384;

type Make<U> = Box<dyn Fn() -> U>;
type Poke<U> = Box<dyn Fn(&mut U)>;
type Write<U> = Box<dyn Fn(&U)>;

/// One unit and the live controls a fork of it must not follow. See the
/// module docs.
pub struct IsolateRow<U> {
    name: String,
    make: Make<U>,
    excite: Option<Poke<U>>,
    input: fn(usize, usize) -> f32,
    frames: usize,
    controls: Vec<(String, Write<U>)>,
}

impl<U: AudioUnit + Clone + 'static> IsolateRow<U> {
    /// A row for the unit `make` builds, fresh for every control. The
    /// harness sets its sample rate to [`SAMPLE_RATE`](super::SAMPLE_RATE).
    pub fn new(name: &str, make: impl Fn() -> U + 'static) -> Self {
        Self {
            name: name.to_string(),
            make: Box::new(make),
            excite: None,
            input: stimulus,
            frames: SNAPSHOT_FRAMES,
            controls: Vec::new(),
        }
    }

    /// Something to do to every rendered copy after its `reset` and before
    /// its first frame — a note-on for an instrument whose inbox `isolate`
    /// severs, so both forks have something to play.
    #[must_use]
    pub fn excite(mut self, excite: impl Fn(&mut U) + 'static) -> Self {
        self.excite = Some(Box::new(excite));
        self
    }

    /// Feed `input(channel, frame)` to the unit's inputs instead of the
    /// default stimulus (a tone under a loud/quiet envelope, plus noise) —
    /// for a unit whose inputs are not audio, such as a beat pair.
    #[must_use]
    pub fn input(mut self, input: fn(usize, usize) -> f32) -> Self {
        self.input = input;
        self
    }

    /// Render `frames` frames per comparison instead of [`SNAPSHOT_FRAMES`].
    #[must_use]
    pub fn frames(mut self, frames: usize) -> Self {
        self.frames = frames;
        self
    }

    /// A live control: `write` moves it on the live unit, through a shared
    /// reference (a handle's `store`, a `&self` setter). It must move it far
    /// enough to be heard under the stimulus — the check requires that.
    #[must_use]
    pub fn control(mut self, name: &str, write: impl Fn(&U) + 'static) -> Self {
        self.controls.push((name.to_string(), Box::new(write)));
        self
    }

    /// Run every control through the four steps in the module docs.
    ///
    /// # Panics
    ///
    /// When the unit says it is not forkable (a row for it proves nothing),
    /// when the row has no controls, when a live move reaches the fork, and
    /// when a move is inaudible.
    pub fn check(&self) {
        assert!(
            !self.controls.is_empty(),
            "{}: a row with no controls",
            self.name
        );
        for (control, write) in &self.controls {
            let mut live = (self.make)();
            live.set_sample_rate(SAMPLE_RATE);
            assert!(
                live.forkable(),
                "{}: forkable() is false, so no fork takes it — drop the row",
                self.name
            );
            let forked = fork(&live);
            let before = self.render(&forked);

            write(&live);

            let after = self.render(&forked);
            if let Some((ch, frame)) = first_difference(&before, &after) {
                panic!(
                    "{} / {control}: a live move reached the fork (channel {ch}, frame {frame}: \
                     {} before, {} after) — its `isolate` left the control's cell shared",
                    self.name, before[ch][frame], after[ch][frame]
                );
            }
            // A fork taken *after* the move.
            let moved = self.render(&fork(&live));
            assert!(
                first_difference(&before, &moved).is_some(),
                "{} / {control}: moving it did not change a fresh fork's output, so this row \
                 cannot tell a severed cell from a shared one — move it further, or excite \
                 the unit so the control is heard",
                self.name
            );
        }
    }

    /// A copy of `unit`, reset and excited, rendered for `self.frames`
    /// frames of the stimulus.
    fn render(&self, unit: &U) -> Vec<Vec<f32>> {
        let mut unit = unit.clone();
        // What a graph does before it runs a unit (`Legacy` at prepare):
        // a clone may leave its block scratch unsized for this hook.
        unit.allocate();
        unit.reset();
        if let Some(excite) = &self.excite {
            excite(&mut unit);
        }
        let (ins, outs) = (unit.inputs(), unit.outputs());
        let mut input = BufferVec::new(ins);
        let mut output = BufferVec::new(outs);
        let mut rendered = vec![Vec::with_capacity(self.frames); outs];
        let mut done = 0;
        while done < self.frames {
            let size = MAX_BUFFER_SIZE.min(self.frames - done);
            for ch in 0..ins {
                for i in 0..size {
                    input.set_f32(ch, i, (self.input)(ch, done + i));
                }
            }
            unit.process(size, &input.buffer_ref(), &mut output.buffer_mut());
            for (ch, out) in rendered.iter_mut().enumerate() {
                out.extend((0..size).map(|i| output.at_f32(ch, i)));
            }
            done += size;
        }
        rendered
    }
}

/// [`IsolateRow`] for a native [`ParamNode`]: the same four steps, with the
/// fork taken as [`param_parts`](crate::param_parts) takes it
/// ([`ParamFork::fork_node`]: `fork_fresh`, then the authored values) and
/// every render through [`drive`](super::drive), prepared at
/// [`SAMPLE_RATE`](super::SAMPLE_RATE), in blocks of [`MAX_BUFFER_SIZE`].
///
/// [`assert_param_fork`](super::assert_param_fork) checks the params a
/// node's [`ParamSet`](crate::ParamSet) addresses; this checks **every**
/// cell a control writes, addressed or not (a compressor's knee, a gate's
/// hold), from the outside: a cell `fork_fresh` forgot to detach fails
/// here with "a live move reached the fork".
///
/// ```
/// use tutti_graph::contract::NativeIsolateRow;
/// # use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
/// # use tutti_types::{Amplitude, ChannelLayout, Param, UnitParam};
/// # #[derive(Clone)]
/// # struct Gain(Param<Amplitude>);
/// # impl Node for Gain {
/// #     fn shape(&self) -> Shape { Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO) }
/// #     fn prepare(&mut self, _: &Prepare) {}
/// #     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
/// #         let g = self.0.load().get();
/// #         let (i, mut o) = io.split();
/// #         for (y, x) in o.get(0).iter_mut().zip(i.get(0)) { *y = x * g; }
/// #         Status::Modified
/// #     }
/// #     fn reset(&mut self) {}
/// # }
/// # impl ParamNode for Gain {
/// #     fn param_set(&self) -> ParamSet {
/// #         ParamSet::builder().param(UnitParam::Volume, self.0.as_atomic()).build()
/// #     }
/// #     fn fork_fresh(&self) -> Self { let mut f = self.clone(); f.0.detach(); f }
/// # }
/// NativeIsolateRow::new("gain", || Gain(Param::new(Amplitude::new(0.5))))
///     .control("gain", |g| g.0.store(Amplitude::new(0.25)))
///     .check();
/// ```
pub struct NativeIsolateRow<N> {
    name: String,
    make: Make<N>,
    frames: usize,
    controls: Vec<(String, Write<N>)>,
}

impl<N: ParamNode + Clone> NativeIsolateRow<N> {
    /// A row for the node `make` builds, fresh for every control.
    pub fn new(name: &str, make: impl Fn() -> N + 'static) -> Self {
        Self {
            name: name.to_string(),
            make: Box::new(make),
            frames: SNAPSHOT_FRAMES,
            controls: Vec::new(),
        }
    }

    /// Render `frames` frames per comparison instead of [`SNAPSHOT_FRAMES`].
    #[must_use]
    pub fn frames(mut self, frames: usize) -> Self {
        self.frames = frames;
        self
    }

    /// A live control: `write` moves it on the live node, through a shared
    /// reference, far enough to be heard under the stimulus.
    #[must_use]
    pub fn control(mut self, name: &str, write: impl Fn(&N) + 'static) -> Self {
        self.controls.push((name.to_string(), Box::new(write)));
        self
    }

    /// Run every control through [`IsolateRow`]'s four steps.
    ///
    /// # Panics
    ///
    /// When the row has no controls, when a live move reaches the fork, and
    /// when a move is inaudible.
    pub fn check(&self) {
        assert!(
            !self.controls.is_empty(),
            "{}: a row with no controls",
            self.name
        );
        for (control, write) in &self.controls {
            let live = (self.make)();
            let forked = ParamFork::new(&live).fork_node();
            let before = self.render(&forked);

            write(&live);

            let after = self.render(&forked);
            if let Some((ch, frame)) = first_difference(&before, &after) {
                panic!(
                    "{} / {control}: a live move reached the fork (channel {ch}, frame {frame}: \
                     {} before, {} after) — its `fork_fresh` left the control's cell shared",
                    self.name, before[ch][frame], after[ch][frame]
                );
            }
            // A fork taken *after* the move.
            let moved = self.render(&ParamFork::new(&live).fork_node());
            assert!(
                first_difference(&before, &moved).is_some(),
                "{} / {control}: moving it did not change a fresh fork's output, so this row \
                 cannot tell a severed cell from a shared one — move it further",
                self.name
            );
        }
    }

    /// A copy of `node`, prepared and reset, rendered for `self.frames`
    /// frames of the stimulus.
    fn render(&self, node: &N) -> Vec<Vec<f32>> {
        let mut node = super::prepared(node.clone(), SAMPLE_RATE, MAX_BUFFER_SIZE);
        node.reset();
        let shape = node.shape();
        let (ins, outs) = (
            usize::from(shape.audio_in.count()),
            usize::from(shape.audio_out.count()),
        );
        let mut rendered = vec![Vec::with_capacity(self.frames); outs];
        let mut done = 0;
        while done < self.frames {
            let size = MAX_BUFFER_SIZE.min(self.frames - done);
            let input: Vec<Vec<f32>> = (0..ins)
                .map(|ch| (0..size).map(|i| stimulus(ch, done + i)).collect())
                .collect();
            let refs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
            let block = super::drive(&mut node, SAMPLE_RATE, &refs, &[]);
            for (out, b) in rendered.iter_mut().zip(block) {
                out.extend(b);
            }
            done += size;
        }
        rendered
    }
}

/// `unit` forked as the graph forks it, before `rebind_offline` and
/// `reset` (the render resets).
fn fork<U: AudioUnit + Clone>(unit: &U) -> U {
    let mut fork = unit.clone();
    fork.isolate();
    fork
}

/// The default stimulus on input `ch` at `frame`: a tone per channel under a
/// loud/quiet envelope (so a dynamics unit's threshold, attack and release
/// all act), plus a little broadband noise (so a filter's cutoff and Q
/// act on something at every frequency). A control input a unit exposes
/// as a port reads it too; rows use constructors without such ports.
fn stimulus(ch: usize, frame: usize) -> f32 {
    let envelope = if (frame / 1024).is_multiple_of(2) {
        0.9
    } else {
        0.05
    };
    let hz = 110.0 * (ch + 2) as f32;
    let phase = frame as f32 * hz / SAMPLE_RATE.get() as f32;
    let tone = (core::f32::consts::TAU * phase).sin();
    // A fixed LCG on the frame and channel: the same noise every render.
    let seed = (frame as u32)
        .wrapping_mul(1_664_525)
        .wrapping_add(1_013_904_223 ^ (ch as u32).wrapping_mul(0x9E37_79B9));
    let noise = (seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
    envelope * (0.8 * tone + 0.2 * noise)
}

fn first_difference(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<(usize, usize)> {
    a.iter().zip(b).enumerate().find_map(|(ch, (x, y))| {
        x.iter()
            .zip(y)
            .position(|(p, q)| p.to_bits() != q.to_bits())
            .map(|frame| (ch, frame))
    })
}

/// [`IsolateRow`] with one control, `write_live`, for a unit `make` builds.
///
/// # Panics
///
/// As [`IsolateRow::check`].
pub fn assert_isolate_snapshots<U: AudioUnit + Clone + 'static>(
    make: impl Fn() -> U + 'static,
    write_live: impl Fn(&U) + 'static,
) {
    IsolateRow::new(core::any::type_name::<U>(), make)
        .control("write_live", write_live)
        .check();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cx, Io, Node, ParamSet, Prepare, Shape, Status};
    use tutti_types::{Amplitude, ChannelLayout, Param, UnitParam};

    /// A gain with two cells: `addressed` in its `ParamSet`, `trim` not.
    /// `detach_trim: false` is a `fork_fresh` that forgot the unaddressed
    /// one.
    #[derive(Clone)]
    struct TwoCells {
        addressed: Param<Amplitude>,
        trim: Param<Amplitude>,
        detach_trim: bool,
    }

    impl TwoCells {
        fn new(detach_trim: bool) -> Self {
            Self {
                addressed: Param::new(Amplitude::new(0.5)),
                trim: Param::new(Amplitude::new(0.5)),
                detach_trim,
            }
        }
    }

    impl Node for TwoCells {
        fn shape(&self) -> Shape {
            Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
        }

        fn prepare(&mut self, _: &Prepare) {}

        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            let g = self.addressed.load().get() * self.trim.load().get();
            let (i, mut o) = io.split();
            for (y, x) in o.get(0).iter_mut().zip(i.get(0)) {
                *y = x * g;
            }
            Status::Modified
        }

        fn reset(&mut self) {}
    }

    impl ParamNode for TwoCells {
        fn param_set(&self) -> ParamSet {
            ParamSet::builder()
                .param(UnitParam::Volume, self.addressed.as_atomic())
                .build()
        }

        fn fork_fresh(&self) -> Self {
            let mut f = self.clone();
            f.addressed.detach();
            if self.detach_trim {
                f.trim.detach();
            }
            f
        }
    }

    fn row(detach_trim: bool) -> NativeIsolateRow<TwoCells> {
        NativeIsolateRow::new("two cells", move || TwoCells::new(detach_trim))
            .frames(256)
            .control("addressed", |n| n.addressed.store(Amplitude::new(0.1)))
            .control("trim", |n| n.trim.store(Amplitude::new(0.1)))
    }

    /// Every cell detached: the snapshot holds for both controls.
    #[test]
    fn a_node_that_detaches_every_cell_passes() {
        row(true).check();
    }

    /// The cell `assert_param_fork` cannot see — one the `ParamSet` does
    /// not address — is caught here.
    ///
    /// Mutation (run): drop the "a live move reached the fork" check in
    /// `check` → this passes → fails.
    #[test]
    #[should_panic(expected = "trim: a live move reached the fork")]
    fn an_unaddressed_cell_left_shared_fails() {
        row(false).check();
    }

    /// A control the render cannot hear fails loudly, rather than passing
    /// the snapshot step for nothing.
    ///
    /// Mutation (run): drop the fresh-fork `assert!` in `check` → this
    /// passes → fails.
    #[test]
    #[should_panic(expected = "moving it did not change a fresh fork's output")]
    fn an_inaudible_control_fails() {
        NativeIsolateRow::new("two cells", || TwoCells::new(true))
            .frames(256)
            .control("no-op", |_| {})
            .check();
    }
}
