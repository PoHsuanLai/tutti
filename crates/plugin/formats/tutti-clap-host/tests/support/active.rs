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
    AudioBuffer, ClapController, ClapLoaded, ClapProcessContext, ClapProcessor, ClapSample,
    instance::ProcessOutputRef,
};
use tutti_plugin_types::{PluginError, Prepare, RenderMode, SampleFormat, SampleRate, Samples};

/// The two halves of one activation.
#[derive(Debug)]
pub struct Active<T: ClapSample> {
    pair: Option<(ClapProcessor<T>, ClapController)>,
    prepare: Prepare,
}

fn format_of<T: ClapSample>() -> SampleFormat {
    if T::requires_f64() {
        SampleFormat::Float64
    } else {
        SampleFormat::Float32
    }
}

fn prepare_for<T: ClapSample>(rate: f64, frames: u32) -> Prepare {
    Prepare {
        rate: SampleRate(rate),
        max_block: Samples(frames as usize),
        format: format_of::<T>(),
        mode: RenderMode::Realtime,
    }
}

/// Activation at a rate and block size, on a loaded plugin.
pub trait Activate {
    /// Activates as `T`; a refusal hands the plugin back with the reason.
    fn activate_at<T: ClapSample>(
        self,
        rate: f64,
        frames: u32,
    ) -> Result<Active<T>, (ClapLoaded, PluginError)>;
}

impl Activate for ClapLoaded {
    fn activate_at<T: ClapSample>(
        self,
        rate: f64,
        frames: u32,
    ) -> Result<Active<T>, (ClapLoaded, PluginError)> {
        let prepare = prepare_for::<T>(rate, frames);
        match self.activate_typed::<T>(prepare) {
            Ok(pair) => Ok(Active {
                pair: Some(pair),
                prepare,
            }),
            Err(refused) => Err((
                refused.plugin.expect("CLAP hands the plugin back"),
                refused.reason,
            )),
        }
    }
}

impl<T: ClapSample> Active<T> {
    fn pair(&self) -> &(ClapProcessor<T>, ClapController) {
        self.pair.as_ref().expect("an Active holds its pair")
    }

    fn pair_mut(&mut self) -> &mut (ClapProcessor<T>, ClapController) {
        self.pair.as_mut().expect("an Active holds its pair")
    }

    /// The realtime half.
    pub fn processor(&mut self) -> &mut ClapProcessor<T> {
        &mut self.pair_mut().0
    }

    /// One block through the Processor.
    pub fn process(
        &mut self,
        buffer: &mut AudioBuffer<T>,
        ctx: &ClapProcessContext<'_>,
    ) -> tutti_clap_host::Result<ProcessOutputRef<'_>> {
        self.pair_mut().0.process(buffer, ctx)
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
        ClapLoaded::deactivate_typed(processor, controller)
            .map_err(|(_, _, reason)| reason)
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
        let loaded = ClapLoaded::deactivate_typed(processor, controller)
            .map_err(|(_, _, reason)| reason)
            .expect("the halves are a pair");
        let wanted = prepare_for::<T>(rate, frames);
        match loaded.activate_typed::<T>(wanted) {
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

impl<T: ClapSample> Deref for Active<T> {
    type Target = ClapController;

    fn deref(&self) -> &ClapController {
        &self.pair().1
    }
}

impl<T: ClapSample> DerefMut for Active<T> {
    fn deref_mut(&mut self) -> &mut ClapController {
        &mut self.pair_mut().1
    }
}
