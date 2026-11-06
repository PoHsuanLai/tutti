//! An activation's two halves held together, for the suites that drive one
//! plugin from a single thread.
//!
//! The Processor and the Controller are separate values on purpose; most of
//! these suites want "the active plugin", so [`Active`] holds the pair, derefs
//! to the Controller (and so to `ClapLoaded`), and forwards the Processor's
//! calls. [`Active::reconfigure`] is the deactivate/activate pair a rate or
//! block-size change is.

// The error carries the whole plugin back, which is the point of it.
#![allow(clippy::result_large_err)]

use std::ops::{Deref, DerefMut};

use tutti_clap_host::{
    AudioBuffer, ClapController, ClapLoaded, ClapProcessContext, ClapProcessor,
    instance::ProcessOutputRef,
};
use tutti_plugin_types::{
    AudioBufferMut, Loaded, PluginError, Prepare, RenderMode, Sample, SampleFormat, SampleRate,
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

/// The two halves of one activation.
#[derive(Debug)]
pub struct Active<T: Width> {
    pair: Option<(ClapProcessor, ClapController)>,
    _width: std::marker::PhantomData<T>,
    prepare: Prepare,
}

fn prepare_for<T: Width>(rate: f64, frames: u32) -> Prepare {
    Prepare {
        rate: SampleRate(rate),
        max_block: Samples(frames as usize),
        format: T::FORMAT,
        mode: RenderMode::Realtime,
    }
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
        let prepare = prepare_for::<T>(rate, frames);
        match <ClapLoaded as Loaded>::activate(self, prepare) {
            Ok(pair) => Ok(Active {
                pair: Some(pair),
                _width: std::marker::PhantomData,
                prepare,
            }),
            Err(refused) => Err((
                refused.plugin.expect("CLAP hands the plugin back"),
                refused.reason,
            )),
        }
    }
}

impl<T: Width> Active<T> {
    fn pair(&self) -> &(ClapProcessor, ClapController) {
        self.pair.as_ref().expect("an Active holds its pair")
    }

    fn pair_mut(&mut self) -> &mut (ClapProcessor, ClapController) {
        self.pair.as_mut().expect("an Active holds its pair")
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut ClapProcessor {
        &mut self.pair_mut().0
    }

    /// One block through the Processor.
    pub fn process(
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
        self.pair_mut().0.process(&mut wrapped, ctx)
    }

    /// [`ClapProcessor::reset`].
    pub fn reset(&mut self) {
        self.pair_mut().0.reset();
    }

    /// [`ClapProcessor::stop`].
    pub fn stop(&mut self) {
        self.pair_mut().0.stop();
    }

    /// [`ClapProcessor::is_processing`].
    pub fn is_processing(&self) -> bool {
        self.pair().0.is_processing()
    }

    /// [`ClapProcessor::param_points_dropped`].
    pub fn param_points_dropped(&self) -> u64 {
        self.pair().0.param_points_dropped()
    }

    /// [`ClapProcessor::sysex_dropped`].
    pub fn sysex_dropped(&self) -> u64 {
        self.pair().0.sysex_dropped()
    }

    /// [`ClapProcessor::midi_dropped`].
    pub fn midi_dropped(&self) -> u64 {
        self.pair().0.midi_dropped()
    }

    /// [`ClapProcessor::output_param_points_dropped`].
    pub fn output_param_points_dropped(&self) -> u64 {
        self.pair().0.output_param_points_dropped()
    }

    /// [`ClapProcessor::output_events_refused`].
    pub fn output_events_refused(&self) -> u64 {
        self.pair().0.output_events_refused()
    }

    /// [`ClapProcessor::last_process_status`].
    pub fn last_process_status(&self) -> clap_sys::process::clap_process_status {
        self.pair().0.last_process_status()
    }

    /// [`ClapProcessor::is_tailing`].
    pub fn is_tailing(&self) -> bool {
        self.pair().0.is_tailing()
    }

    /// [`ClapProcessor::is_sleeping`].
    pub fn is_sleeping(&self) -> bool {
        self.pair().0.is_sleeping()
    }

    /// [`ClapProcessor::sends_midi_notes`].
    pub fn sends_midi_notes(&self) -> bool {
        self.pair().0.sends_midi_notes()
    }

    /// Ends the activation: the plugin comes back deactivated.
    pub fn deactivate(mut self) -> ClapLoaded {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        <ClapLoaded as Loaded>::deactivate(processor, controller)
            .map_err(|stuck| stuck.reason)
            .expect("the halves are a pair")
    }

    /// Deactivates and activates at `rate` and `frames`, as a rate change does.
    ///
    /// # Panics
    ///
    /// If the plugin refuses: a test of a refusal deactivates and activates by
    /// hand and looks at what the refusal hands back. (The rollback to the
    /// previous configuration is the server bridge's, tested there.)
    pub fn reconfigure(&mut self, rate: f64, frames: u32) -> Result<(), PluginError> {
        let (processor, controller) = self.pair.take().expect("an Active holds its pair");
        let loaded = <ClapLoaded as Loaded>::deactivate(processor, controller)
            .map_err(|stuck| stuck.reason)
            .expect("the halves are a pair");
        let wanted = prepare_for::<T>(rate, frames);
        match <ClapLoaded as Loaded>::activate(loaded, wanted) {
            Ok(pair) => {
                self.pair = Some(pair);
                self.prepare = wanted;
                Ok(())
            }
            Err(refused) => panic!("the plugin refused {wanted:?}: {}", refused.reason),
        }
    }

    /// Deactivates and activates at the configuration it has.
    pub fn reactivate(&mut self) -> Result<(), PluginError> {
        let Prepare {
            rate, max_block, ..
        } = self.prepare;
        self.reconfigure(rate.0, max_block.0 as u32)
    }
}

impl<T: Width> Deref for Active<T> {
    type Target = ClapController;

    fn deref(&self) -> &ClapController {
        &self.pair().1
    }
}

impl<T: Width> DerefMut for Active<T> {
    fn deref_mut(&mut self) -> &mut ClapController {
        &mut self.pair_mut().1
    }
}
