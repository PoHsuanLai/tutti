//! An activation's two halves held together: [`Active`] is
//! `tutti_fixtures::Active` over `Vst2Loaded`, and this file is what VST2 adds
//! to it: loading and activating in one call, a rate or block change that must
//! be accepted, a block through the Processor, and its counters.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err, dead_code)]

use std::path::Path;
use tutti_fixtures::{Whole, main_thread};

use tutti_plugin_types::{MaxBlock, PluginError, Prepare, SampleFormat, SampleRate, Samples};
use tutti_vst2_host::{MidiEventVec, Vst2Error, Vst2Loaded, Vst2ProcessContext};

/// The two halves of one activation.
pub type Active = tutti_fixtures::Active<Whole<Vst2Loaded>>;

/// The `Prepare` a rate, block size and width stand for, in realtime mode.
pub fn prepare_for(rate: f64, frames: usize, double: bool) -> Prepare {
    let block = MaxBlock::new(Samples(frames)).expect("a block of at least one frame");
    Prepare::new(SampleRate(rate), block)
        .expect("a positive finite rate")
        .with_format(if double {
            SampleFormat::Float64
        } else {
            SampleFormat::Float32
        })
}

/// The width a plugin is told it will be processed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Precision {
    /// 32-bit float.
    #[default]
    Single,
    /// 64-bit float, for a plugin that declared it can.
    Double,
}

/// A load or an activation that did not take.
#[derive(Debug)]
pub enum LoadError {
    /// The binary did not load.
    Load(Vst2Error),
    /// The plugin loaded and refused to activate.
    Activate(PluginError),
}

/// What a suite calls on an [`Active`] besides the Controller: loading and
/// activating in one call, a rate or block change that must be accepted, a
/// block through the Processor, its counters, and the loaded plugin.
pub trait ActiveExt: Sized {
    /// Loads and activates at 32-bit, in realtime mode.
    fn load(path: impl AsRef<Path>, rate: f64, frames: usize) -> Result<Self, LoadError> {
        Self::load_with(path, prepare_for(rate, frames, false))
    }
    /// Loads and activates at `precision`.
    fn load_with_precision(
        path: impl AsRef<Path>,
        rate: f64,
        frames: usize,
        precision: Precision,
    ) -> Result<Self, LoadError> {
        Self::load_with(
            path,
            prepare_for(rate, frames, precision == Precision::Double),
        )
    }
    /// Loads and activates at `prepare`.
    fn load_with(path: impl AsRef<Path>, prepare: Prepare) -> Result<Self, LoadError>;
    /// A new rate, with the block size and width kept.
    fn set_sample_rate(&mut self, rate: f64);
    /// A new maximum block, with the rate and width kept.
    fn set_block_size(&mut self, frames: usize);
    /// [`Vst2Processor::process_f32`](tutti_vst2_host::Vst2Processor::process_f32).
    fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec;
    /// [`Vst2Processor::process_f64`](tutti_vst2_host::Vst2Processor::process_f64).
    fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec;
    /// [`Vst2Processor::dropped_midi_out`](tutti_vst2_host::Vst2Processor::dropped_midi_out).
    fn dropped_midi_out(&mut self) -> u64;
    /// [`Vst2Processor::dropped_sysex`](tutti_vst2_host::Vst2Processor::dropped_sysex).
    fn dropped_sysex(&mut self) -> u64;
    /// [`Vst2Processor::dropped_midi_in`](tutti_vst2_host::Vst2Processor::dropped_midi_in).
    fn dropped_midi_in(&mut self) -> u64;
    /// Whether the plugin was told 64-bit.
    fn is_double_precision(&mut self) -> bool;
    /// The loaded plugin under the Controller. A suite names it on purpose:
    /// a call through it skips what the Controller tells the Processor.
    fn loaded(&self) -> &Vst2Loaded;
    /// [`loaded`](Self::loaded), mutably.
    fn loaded_mut(&mut self) -> &mut Vst2Loaded;
}

impl ActiveExt for Active {
    fn load_with(path: impl AsRef<Path>, prepare: Prepare) -> Result<Self, LoadError> {
        let loaded = Vst2Loaded::load(&main_thread(), path).map_err(LoadError::Load)?;
        Self::activate(loaded, prepare).map_err(|(_, e)| LoadError::Activate(e))
    }

    fn set_sample_rate(&mut self, rate: f64) {
        let wanted = self
            .prepare()
            .with_rate(SampleRate(rate))
            .expect("a positive finite rate");
        self.reconfigure(wanted).expect("the rate is accepted");
    }

    fn set_block_size(&mut self, frames: usize) {
        let prepare = self.prepare();
        let wanted = Prepare::new(
            prepare.rate(),
            MaxBlock::new(Samples(frames)).expect("a block of at least one frame"),
        )
        .expect("the rate was already accepted")
        .with_format(prepare.format())
        .with_mode(prepare.mode());
        self.reconfigure(wanted).expect("the block is accepted");
    }

    fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.processor()
            .process_f32(inputs, outputs, num_samples, ctx)
    }

    fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.processor()
            .process_f64(inputs, outputs, num_samples, ctx)
    }

    fn dropped_midi_out(&mut self) -> u64 {
        self.processor().dropped_midi_out()
    }

    fn dropped_sysex(&mut self) -> u64 {
        self.processor().dropped_sysex()
    }

    fn dropped_midi_in(&mut self) -> u64 {
        self.processor().dropped_midi_in()
    }

    fn is_double_precision(&mut self) -> bool {
        self.processor().is_double_precision()
    }

    fn loaded(&self) -> &Vst2Loaded {
        self.controller().loaded()
    }

    fn loaded_mut(&mut self) -> &mut Vst2Loaded {
        self.controller_mut().loaded_mut()
    }
}
