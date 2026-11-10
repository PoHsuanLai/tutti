//! [`Blend`]: what a [`DesignNode`](crate::DesignNode) does with the input around
//! the realization, and its two kinds, [`DryWet`] and [`NoBlend`].

use tutti_graph::{Inputs, Io, Outputs, ParamSetBuilder};
use tutti_types::{MaxBlock, Mix, Param, Samples, UnitAddr, UnitParam};

use tutti_dsp::Ramp;

use crate::dry_align::DryAlign;
use crate::ramp::{LastGood, block_driven};

/// What a [`DesignNode`](crate::DesignNode) does with its input around the
/// realization: a node's dry path and the controls that mix it back.
///
/// A reverb used as an insert blends the dry input, aligned to the realization's
/// latency, against the wet signal ([`DryWet`]); a family that is only ever the
/// wet signal (a filter, a dynamics processor) has [`NoBlend`], no port and no
/// state. The node calls [`begin`](Self::begin) once a block has sound to render
/// (after the park check: an idle block reads nothing) and [`finish`](Self::finish)
/// with the rendered wet signal in the outputs.
pub trait Blend: Default + Clone + Send + 'static {
    /// The addresses of the params the blend puts first among the node's ports.
    const ADDRS: &'static [UnitAddr];

    /// Sizes the blend for a realization of `latency` frames over `inputs`
    /// channels and blocks up to `max_block`. Control side; allocates.
    fn prepare(&mut self, latency: Samples, inputs: usize, max_block: MaxBlock);

    /// Back to silence: nothing held, no mix remembered.
    fn clear(&mut self);

    /// Reads the controls of this block, once the block has sound to render.
    fn begin(&mut self, io: &Io<'_>);

    /// Mixes the blend into `outs`, which hold the wet signal for the first `size`
    /// frames, from `ins`. Audio thread: allocates nothing.
    fn finish(&mut self, ins: &Inputs<'_>, outs: &mut Outputs<'_, '_>, size: usize);

    /// Adds each cell to `set` under its address.
    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder;

    /// The base of the `k`th address: its cell's value.
    fn base(&self, k: usize) -> Option<f32>;

    /// Stops sharing the cells, keeping their values: a fork's own.
    fn detach(&mut self);
}

/// No blend: the node is the wet signal alone, with no port for it and no dry
/// path.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBlend;

impl Blend for NoBlend {
    const ADDRS: &'static [UnitAddr] = &[];

    fn prepare(&mut self, _: Samples, _: usize, _: MaxBlock) {}
    fn clear(&mut self) {}
    fn begin(&mut self, _: &Io<'_>) {}
    fn finish(&mut self, _: &Inputs<'_>, _: &mut Outputs<'_, '_>, _: usize) {}

    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        set
    }

    fn base(&self, _: usize) -> Option<f32> {
        None
    }

    fn detach(&mut self) {}
}

/// The dry input, aligned to the realization's latency (never its pre-delay:
/// pre-delay is part of the wet sound), blended against the wet signal by a
/// **mix** ([`UnitParam::Wet`], port 0, default 1: the wet signal alone).
#[derive(Debug)]
pub struct DryWet {
    mix: Param<Mix>,
    good: LastGood,
    dry: Vec<DryAlign>,
    dry_buf: Vec<Vec<f32>>,
    /// The mix the previous block ended on.
    last: Option<f32>,
    /// The mix this block starts and ends on.
    from: f32,
    to: f32,
}

impl DryWet {
    /// The shared cell of the mix: `0` is the dry input alone, `1` (the default)
    /// the room alone.
    #[must_use]
    pub fn mix(&self) -> Param<Mix> {
        self.mix.handle()
    }
}

impl Default for DryWet {
    fn default() -> Self {
        Self {
            mix: Param::new(Mix::WET),
            good: LastGood::fixed(Mix::WET.get()),
            dry: Vec::new(),
            dry_buf: Vec::new(),
            last: None,
            from: 1.0,
            to: 1.0,
        }
    }
}

/// A clone shares the mix and starts with its dry path empty.
impl Clone for DryWet {
    fn clone(&self) -> Self {
        let mut copy = Self {
            mix: self.mix.handle(),
            good: self.good,
            dry: self.dry.clone(),
            dry_buf: self.dry_buf.clone(),
            last: None,
            from: 1.0,
            to: 1.0,
        };
        copy.clear();
        copy
    }
}

impl Blend for DryWet {
    const ADDRS: &'static [UnitAddr] = &[UnitAddr::first(UnitParam::Wet)];

    fn prepare(&mut self, latency: Samples, inputs: usize, max_block: MaxBlock) {
        self.dry = (0..inputs).map(|_| DryAlign::new(latency.get())).collect();
        self.dry_buf = vec![vec![0.0; max_block.get()]; inputs];
        self.clear();
    }

    fn clear(&mut self) {
        for d in &mut self.dry {
            d.clear();
        }
        self.last = None;
    }

    /// The mix this block ends on, ramped from where the last one ended.
    fn begin(&mut self, io: &Io<'_>) {
        let to = Mix::new_clamped(
            self.good
                .read(block_driven(io, 0).unwrap_or_else(|| self.mix.load().get())),
        )
        .get();
        self.from = self.last.unwrap_or(to);
        self.to = to;
        self.last = Some(to);
    }

    fn finish(&mut self, ins: &Inputs<'_>, outs: &mut Outputs<'_, '_>, size: usize) {
        let wet_only = self.from == 1.0 && self.to == 1.0;
        let pass_through = self.dry.iter().all(DryAlign::is_pass_through);
        // The dry path, only when the mix is not all wet (or the dry delay holds
        // history a later mix would need).
        if !(wet_only && pass_through) {
            for (c, dry) in self.dry.iter_mut().enumerate() {
                for (b, &x) in self.dry_buf[c][..size].iter_mut().zip(&ins.get(c)[..size]) {
                    *b = dry.step(x);
                }
            }
        }
        if !wet_only {
            let inputs = self.dry.len();
            let ramp = Ramp::new(self.from, self.to, size);
            for (c, ch) in outs.iter_mut().enumerate() {
                let dry = &self.dry_buf[c % inputs.max(1)];
                for (i, (o, &d)) in ch[..size].iter_mut().zip(dry).enumerate() {
                    *o = Mix(ramp.at(i)).blend(d, *o);
                }
            }
        }
    }

    fn bind(&self, set: ParamSetBuilder) -> ParamSetBuilder {
        set.param(UnitParam::Wet, self.mix.as_atomic())
    }

    fn base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.mix.load().get())
    }

    fn detach(&mut self) {
        self.mix.detach();
    }
}
