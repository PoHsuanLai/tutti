//! An activation's two halves held together, for the suites that drive one
//! plugin from a single thread.
//!
//! The Processor and the Controller are separate values on purpose; most of these
//! suites want "the active plugin", so [`Active`] holds the pair, derefs to the
//! Controller, and forwards the Processor's calls. It does not deref to
//! `Vst2Loaded`: a call meant for the loaded plugin names [`Active::loaded`], so
//! a name both have (`Control::set_parameter`) cannot quietly skip what the
//! Controller tells the Processor. [`Active::reconfigure`] is the
//! deactivate/activate pair a rate or block-size change is.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err, dead_code)]

use std::ops::{Deref, DerefMut};
use std::path::Path;

use tutti_plugin_types::{
    Loaded, MaxBlock, PluginError, Prepare, SampleFormat, SampleRate, Samples,
};
use tutti_vst2_host::{
    MidiEventVec, Vst2Controller, Vst2Error, Vst2Loaded, Vst2ProcessContext, Vst2Processor,
};

/// The two halves of one activation.
#[derive(Debug)]
pub struct Active {
    pair: Option<(Vst2Processor, Vst2Controller)>,
    prepare: Prepare,
}

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

impl Active {
    /// Loads and activates at 32-bit, in realtime mode.
    pub fn load(path: impl AsRef<Path>, rate: f64, frames: usize) -> Result<Self, LoadError> {
        Self::load_with(path, prepare_for(rate, frames, false))
    }

    /// Loads and activates at `precision`.
    pub fn load_with_precision(
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
    pub fn load_with(path: impl AsRef<Path>, prepare: Prepare) -> Result<Self, LoadError> {
        let loaded = Vst2Loaded::load(&main_thread(), path).map_err(LoadError::Load)?;
        Self::activate(loaded, prepare).map_err(|(_, e)| LoadError::Activate(e))
    }

    /// Activates a loaded plugin; a refusal hands it back with the reason.
    pub fn activate(
        loaded: Vst2Loaded,
        prepare: Prepare,
    ) -> Result<Self, (Vst2Loaded, PluginError)> {
        match loaded.activate(&main_thread(), prepare) {
            Ok(pair) => Ok(Self {
                pair: Some(pair),
                prepare,
            }),
            Err(refused) => Err((
                refused.plugin.expect("VST2 hands the plugin back"),
                refused.reason,
            )),
        }
    }

    /// The configuration the plugin was last activated at.
    pub fn prepare(&self) -> Prepare {
        self.prepare
    }

    /// Rejoins the halves.
    pub fn deactivate(mut self) -> Vst2Loaded {
        let (processor, controller) = self.pair.take().expect("the pair is whole");
        match Vst2Loaded::deactivate(&main_thread(), processor, controller) {
            Ok(loaded) => loaded,
            Err(stuck) => panic!("the halves would not rejoin: {}", stuck.reason),
        }
    }

    /// Deactivates and activates again at `wanted`: a rate, block size or width
    /// change.
    pub fn reconfigure(&mut self, wanted: Prepare) -> Result<(), PluginError> {
        let (processor, controller) = self.pair.take().expect("the pair is whole");
        let loaded = match Vst2Loaded::deactivate(&main_thread(), processor, controller) {
            Ok(loaded) => loaded,
            Err(stuck) => panic!("the halves would not rejoin: {}", stuck.reason),
        };
        match loaded.activate(&main_thread(), wanted) {
            Ok(pair) => {
                self.pair = Some(pair);
                self.prepare = wanted;
                Ok(())
            }
            Err(refused) => {
                let loaded = refused.plugin.expect("VST2 hands the plugin back");
                let previous = self.prepare;
                let pair = loaded
                    .activate(&main_thread(), previous)
                    .map_err(|r| r.reason)
                    .expect("the previous configuration still activates");
                self.pair = Some(pair);
                Err(refused.reason)
            }
        }
    }

    /// A new rate, with the block size and width kept.
    pub fn set_sample_rate(&mut self, rate: f64) {
        let wanted = self
            .prepare
            .with_rate(SampleRate(rate))
            .expect("a positive finite rate");
        self.reconfigure(wanted).expect("the rate is accepted");
    }

    /// A new maximum block, with the rate and width kept.
    pub fn set_block_size(&mut self, frames: usize) {
        let wanted = Prepare::new(
            self.prepare.rate(),
            MaxBlock::new(Samples(frames)).expect("a block of at least one frame"),
        )
        .expect("the rate was already accepted")
        .with_format(self.prepare.format())
        .with_mode(self.prepare.mode());
        self.reconfigure(wanted).expect("the block is accepted");
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut Vst2Processor {
        &mut self.pair.as_mut().expect("the pair is whole").0
    }

    /// [`Vst2Processor::process_f32`].
    pub fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.processor()
            .process_f32(inputs, outputs, num_samples, ctx)
    }

    /// [`Vst2Processor::process_f64`].
    pub fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.processor()
            .process_f64(inputs, outputs, num_samples, ctx)
    }

    /// [`Vst2Processor::dropped_midi_out`].
    pub fn dropped_midi_out(&mut self) -> u64 {
        self.processor().dropped_midi_out()
    }

    /// [`Vst2Processor::dropped_sysex`].
    pub fn dropped_sysex(&mut self) -> u64 {
        self.processor().dropped_sysex()
    }

    /// [`Vst2Processor::dropped_midi_in`].
    pub fn dropped_midi_in(&mut self) -> u64 {
        self.processor().dropped_midi_in()
    }

    /// Whether the plugin was told 64-bit.
    pub fn is_double_precision(&mut self) -> bool {
        self.processor().is_double_precision()
    }
}

impl Active {
    /// The Controller, for the `Controller` trait the loaded plugin lacks.
    pub fn controller(&self) -> &Vst2Controller {
        &self.pair.as_ref().expect("the pair is whole").1
    }
}

impl Active {
    /// The loaded plugin under the Controller. A suite names it on purpose:
    /// a call through it skips what the Controller tells the Processor.
    pub fn loaded(&self) -> &Vst2Loaded {
        self.pair.as_ref().expect("the pair is whole").1.loaded()
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut Vst2Loaded {
        self.pair
            .as_mut()
            .expect("the pair is whole")
            .1
            .loaded_mut()
    }
}

impl Deref for Active {
    type Target = Vst2Controller;

    fn deref(&self) -> &Vst2Controller {
        &self.pair.as_ref().expect("the pair is whole").1
    }
}

impl DerefMut for Active {
    fn deref_mut(&mut self) -> &mut Vst2Controller {
        &mut self.pair.as_mut().expect("the pair is whole").1
    }
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}
