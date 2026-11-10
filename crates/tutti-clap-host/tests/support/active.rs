//! An activation's two halves held together: [`Active`] is
//! `tutti_fixtures::Active`, and this file is what CLAP adds to it: the sample
//! width a suite names with a type, the plugin's processor counters, and the
//! rate-and-frames spellings of a reconfiguration.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err)]

use std::marker::PhantomData;

use tutti_clap_host::{
    AudioBuffer, ClapController, ClapLoaded, ClapProcessContext, ClapProcessor,
    instance::ProcessOutputRef,
};
use tutti_fixtures::{Activation, Whole};
use tutti_plugin_types::{
    AudioBufferMut, MainThread, MaxBlock, PluginError, Prepare, Sample, SampleFormat, SampleRate,
    Samples,
};

/// A sample width a suite names with a type (`Active<f32>`, `AudioBuffer32`):
/// test-side sugar for the processor's runtime `SampleFormat`. The host has no
/// sample type parameter; this only picks the format value and wraps a typed
/// buffer for the one `process`.
pub trait Width: Sample {
    /// The format an activation at this width negotiates.
    const FORMAT: SampleFormat;
    /// The buffer as the processor takes it.
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, Self>) -> AudioBufferMut<'t, 'd>;
}

impl Width for f32 {
    const FORMAT: SampleFormat = SampleFormat::Float32;
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, f32>) -> AudioBufferMut<'t, 'd> {
        AudioBufferMut::F32(buffer)
    }
}

impl Width for f64 {
    const FORMAT: SampleFormat = SampleFormat::Float64;
    fn wrap<'t, 'd: 't>(buffer: AudioBuffer<'t, 'd, f64>) -> AudioBufferMut<'t, 'd> {
        AudioBufferMut::F64(buffer)
    }
}

/// CLAP's activation at the width `T`.
#[derive(Debug)]
pub struct Clap<T>(PhantomData<fn() -> T>);

impl<T: Width> Activation for Clap<T> {
    type Loaded = ClapLoaded;
    type Processor = ClapProcessor;
    type Controller = ClapController;

    fn activate(
        main: &MainThread,
        loaded: ClapLoaded,
        prepare: Prepare,
    ) -> Result<(ClapProcessor, ClapController), (Option<ClapLoaded>, PluginError)> {
        Whole::<ClapLoaded>::activate(main, loaded, prepare)
    }

    fn deactivate(
        main: &MainThread,
        processor: ClapProcessor,
        controller: ClapController,
    ) -> Result<ClapLoaded, PluginError> {
        Whole::<ClapLoaded>::deactivate(main, processor, controller)
    }
}

/// The two halves of one activation, at the width `T`.
pub type Active<T> = tutti_fixtures::Active<Clap<T>>;

fn prepare_for<T: Width>(rate: f64, frames: u32) -> Prepare {
    let block = MaxBlock::new(Samples(frames as usize)).expect("a block of at least one frame");
    Prepare::new(SampleRate(rate), block)
        .expect("a positive finite rate")
        .with_format(T::FORMAT)
}

/// Activation at a rate and block size, on a loaded plugin.
pub trait Activate {
    /// Activates as `T`; a refusal hands the plugin back with the reason.
    fn activate_at<T: Width>(
        self,
        rate: f64,
        frames: u32,
    ) -> Result<Active<T>, (ClapLoaded, PluginError)>;
}

impl Activate for ClapLoaded {
    fn activate_at<T: Width>(
        self,
        rate: f64,
        frames: u32,
    ) -> Result<Active<T>, (ClapLoaded, PluginError)> {
        Active::activate(self, prepare_for::<T>(rate, frames))
            .map_err(|(plugin, reason)| (plugin.expect("CLAP hands the plugin back"), reason))
    }
}

/// What a suite calls on an [`Active`] besides the Controller: a block through
/// the Processor at the width `T`, its counters, and the loaded plugin.
pub trait ActiveExt<T: Width> {
    /// One block through the Processor.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        ctx: &ClapProcessContext<'_>,
    ) -> tutti_clap_host::Result<ProcessOutputRef<'_>>;
    /// [`ClapProcessor::reset`].
    fn reset(&mut self);
    /// [`ClapProcessor::stop`].
    fn stop(&mut self);
    /// [`ClapProcessor::is_processing`].
    fn is_processing(&self) -> bool;
    /// [`ClapProcessor::param_points_dropped`].
    fn param_points_dropped(&self) -> u64;
    /// [`ClapProcessor::sysex_dropped`].
    fn sysex_dropped(&self) -> u64;
    /// [`ClapProcessor::midi_dropped`].
    fn midi_dropped(&self) -> u64;
    /// [`ClapProcessor::output_param_points_dropped`].
    fn output_param_points_dropped(&self) -> u64;
    /// [`ClapProcessor::output_events_refused`].
    fn output_events_refused(&self) -> u64;
    /// [`ClapProcessor::last_process_status`].
    fn last_process_status(&self) -> clap_sys::process::clap_process_status;
    /// [`ClapProcessor::is_tailing`].
    fn is_tailing(&self) -> bool;
    /// [`ClapProcessor::is_sleeping`].
    fn is_sleeping(&self) -> bool;
    /// [`ClapProcessor::sends_midi_notes`].
    fn sends_midi_notes(&self) -> bool;
    /// Deactivates and activates at `rate` and `frames`, as a rate change does.
    ///
    /// # Panics
    ///
    /// If the plugin refuses: a test of a refusal deactivates and activates by
    /// hand and looks at what the refusal hands back.
    fn reconfigure_at(&mut self, rate: f64, frames: u32) -> Result<(), PluginError>;
    /// Deactivates and activates at the configuration it has.
    fn reactivate(&mut self) -> Result<(), PluginError>;
    /// The loaded plugin under the Controller. A suite names it on purpose:
    /// a call through it skips what the Controller tells the Processor.
    fn loaded(&self) -> &ClapLoaded;
    /// [`loaded`](Self::loaded), mutably.
    fn loaded_mut(&mut self) -> &mut ClapLoaded;
}

impl<T: Width> ActiveExt<T> for Active<T> {
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        ctx: &ClapProcessContext<'_>,
    ) -> tutti_clap_host::Result<ProcessOutputRef<'_>> {
        let mut wrapped = T::wrap(AudioBuffer {
            inputs: buffer.inputs,
            outputs: &mut *buffer.outputs,
            num_samples: buffer.num_samples,
            sample_rate: buffer.sample_rate,
        });
        self.processor().process(&mut wrapped, ctx)
    }

    fn reset(&mut self) {
        self.processor().reset();
    }

    fn stop(&mut self) {
        self.processor().stop();
    }

    fn is_processing(&self) -> bool {
        self.processor_shared().is_processing()
    }

    fn param_points_dropped(&self) -> u64 {
        self.processor_shared().param_points_dropped()
    }

    fn sysex_dropped(&self) -> u64 {
        self.processor_shared().sysex_dropped()
    }

    fn midi_dropped(&self) -> u64 {
        self.processor_shared().midi_dropped()
    }

    fn output_param_points_dropped(&self) -> u64 {
        self.processor_shared().output_param_points_dropped()
    }

    fn output_events_refused(&self) -> u64 {
        self.processor_shared().output_events_refused()
    }

    fn last_process_status(&self) -> clap_sys::process::clap_process_status {
        self.processor_shared().last_process_status()
    }

    fn is_tailing(&self) -> bool {
        self.processor_shared().is_tailing()
    }

    fn is_sleeping(&self) -> bool {
        self.processor_shared().is_sleeping()
    }

    fn sends_midi_notes(&self) -> bool {
        self.processor_shared().sends_midi_notes()
    }

    fn reconfigure_at(&mut self, rate: f64, frames: u32) -> Result<(), PluginError> {
        let wanted = prepare_for::<T>(rate, frames);
        self.reconfigure(wanted).inspect_err(|reason| {
            panic!("the plugin refused {wanted:?}: {reason}");
        })
    }

    fn reactivate(&mut self) -> Result<(), PluginError> {
        self.reconfigure(self.prepare())
    }

    fn loaded(&self) -> &ClapLoaded {
        self.controller().loaded()
    }

    fn loaded_mut(&mut self) -> &mut ClapLoaded {
        self.controller_mut().loaded_mut()
    }
}

/// One silent stereo block of `frames` frames through `inst` at 48 kHz, which
/// must process.
pub fn drive_block(inst: &mut Active<f32>, frames: usize, ctx: &ClapProcessContext<'_>) {
    let mut out_l = vec![0.0f32; frames];
    let mut out_r = vec![0.0f32; frames];
    let in_l = vec![0.0f32; frames];
    let in_r = vec![0.0f32; frames];
    let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
    let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
    let mut buffer = AudioBuffer {
        inputs: ins,
        outputs: outs,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    inst.process(&mut buffer, ctx).expect("process succeeds");
}
