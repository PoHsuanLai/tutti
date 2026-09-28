//! The fork's other contract: a forked node renders a **snapshot** of its
//! controls, taken at fork time.
//!
//! A [`ParamNode`]'s fork is `fork_fresh`, which promises to detach every
//! cell a control writes. A `Param` cell the node only *reads* is shared
//! mutable state too: a UI knob or an automation lane writes it, so a copy
//! still holding it follows every live move while an offline export runs.
//! [`IsolateRow::check`] pins the promise from the outside, per
//! control, and does not care how the node stores it.
//!
//! For each control, on a freshly made node:
//!
//! 1. fork it as [`param_parts`](crate::param_parts) does
//!    ([`ParamFork::fork_node`]: `fork_fresh`, then the authored values)
//!    and render a copy of the fork (the *before*);
//! 2. move the control on the **live** node, through `&N` only: a write
//!    that has to go through a shared reference can only reach a shared
//!    cell, which is exactly what a UI thread holds;
//! 3. render the fork again, and require it **bit-identical** to *before*
//!    — the snapshot held;
//! 4. fork the live node again and render that, and require it to
//!    **differ** from *before* — the control is audible under the stimulus,
//!    and a fork taken now sees the new value (a fork keeps current values,
//!    it does not reset them).
//!
//! Step 4 is what makes step 3 able to fail: a control the stimulus cannot
//! hear would pass step 3 whether or not the fork severed it, so a row
//! whose control is inaudible fails loudly instead of claiming coverage.
//!
//! Every render is prepared at [`SAMPLE_RATE`](super::SAMPLE_RATE), reset,
//! and driven through [`drive_in`](super::drive_in) in blocks of [`BLOCK`]
//! frames. Comparisons are within one run on one machine, so bit equality
//! is sound on every platform.

use crate::controls::{ParamFork, ParamNode};

use super::SAMPLE_RATE;

/// Frames rendered per comparison unless [`IsolateRow::frames`] says
/// otherwise: long enough for a 100 ms release, a 1 s LFO cycle's first
/// quarter and several loud/quiet stimulus cycles.
pub const SNAPSHOT_FRAMES: usize = 16_384;

/// Frames per block of every render.
const BLOCK: usize = 64;

type Make<N> = Box<dyn Fn() -> N>;
type Write<N> = Box<dyn Fn(&N)>;

/// One [`ParamNode`] and the live controls a fork of it must not follow:
/// the four steps in the module docs (`src/contract/snapshot.rs`).
///
/// [`assert_param_fork`](super::assert_param_fork) checks the params a
/// node's [`ParamSet`](crate::ParamSet) addresses; this checks **every**
/// cell a control writes, addressed or not (a compressor's knee, a gate's
/// hold), from the outside: a cell `fork_fresh` forgot to detach fails
/// here with "a live move reached the fork".
///
/// ```
/// use tutti_graph::contract::IsolateRow;
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
/// IsolateRow::new("gain", || Gain(Param::new(Amplitude::new(0.5))))
///     .control("gain", |g| g.0.store(Amplitude::new(0.25)))
///     .check();
/// ```
pub struct IsolateRow<N> {
    name: String,
    make: Make<N>,
    frames: usize,
    controls: Vec<(String, Write<N>)>,
}

impl<N: ParamNode + Clone> IsolateRow<N> {
    /// A row for the node `make` builds, fresh for every control.
    pub fn new(name: &str, make: impl Fn() -> N + 'static) -> Self {
        Self {
            name: name.to_string(),
            make: Box::new(make),
            frames: SNAPSHOT_FRAMES,
            controls: Vec::new(),
        }
    }

    /// Renders `frames` frames per comparison instead of [`SNAPSHOT_FRAMES`].
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

    /// Runs every control through the four steps in the module docs.
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
        let mut node = super::prepared(node.clone(), SAMPLE_RATE, BLOCK);
        node.reset();
        let shape = node.shape();
        let (ins, outs) = (
            usize::from(shape.audio_in.count()),
            usize::from(shape.audio_out.count()),
        );
        let mut rendered = vec![Vec::with_capacity(self.frames); outs];
        let mut done = 0;
        while done < self.frames {
            let size = BLOCK.min(self.frames - done);
            let input: Vec<Vec<f32>> = (0..ins)
                .map(|ch| (0..size).map(|i| stimulus(ch, done + i)).collect())
                .collect();
            let refs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
            // `drive_in`, not `drive`: the block's length is the `Env`'s, so
            // a generator with no inputs (an LFO) renders too.
            let env = crate::node::Env {
                frame: tutti_types::Frame(done as u64),
                sample_rate: SAMPLE_RATE,
                block_len: tutti_types::Samples(size),
                transport: crate::node::Transport::default(),
                changes: crate::node::TransportChanges::NONE,
            };
            let block = super::drive_in(&mut node, &env, &refs, &[], &[]).audio;
            for (out, b) in rendered.iter_mut().zip(block) {
                out.extend(b);
            }
            done += size;
        }
        rendered
    }
}

/// The default stimulus on input `ch` at `frame`: a tone per channel under a
/// loud/quiet envelope (so a dynamics unit's threshold, attack and release
/// all act), plus a little broadband noise (so a filter's cutoff and Q
/// act on something at every frequency).
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

    fn row(detach_trim: bool) -> IsolateRow<TwoCells> {
        IsolateRow::new("two cells", move || TwoCells::new(detach_trim))
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
        IsolateRow::new("two cells", || TwoCells::new(true))
            .frames(256)
            .control("no-op", |_| {})
            .check();
    }
}
