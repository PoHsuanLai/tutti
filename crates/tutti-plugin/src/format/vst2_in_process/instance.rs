//! The plugin both halves of the in-process node share: the two halves of one
//! activation behind the one lock the node and its handle take.
//!
//! The audio thread reaches the Processor through the lock's `try_lock`; the
//! control side reaches the Controller (and, through it, the loaded plugin)
//! through the lock itself. A rate change is a rejoin and an activation, which
//! needs both halves, so it happens only on the control side, under the lock.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use tutti_plugin_types::MainThread;

use tutti_plugin_types::{Loaded, MaxBlock, Prepare, RenderMode, Request, SampleRate, Samples};
use tutti_vst2_host::{
    ChannelLayout, MidiEventVec, ParameterChange, PluginInfo, Vst2Controller, Vst2Loaded,
    Vst2ProcessContext, Vst2Processor,
};

use crate::error::LoadStage;
use crate::protocol::PluginTail;

/// What the plugin changed about itself since the last
/// [`Instance::poll_changes`]. VST2 has no notification for any of it except
/// `audioMasterIOChanged`, so every field is a re-read the host did.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Changes {
    /// The new latency, when it moved.
    pub(super) latency: Option<Samples>,
    /// The new tail, when it moved.
    pub(super) tail: Option<PluginTail>,
    /// The plugin reported `audioMasterIOChanged`: its pin counts or latency
    /// may differ.
    pub(super) io: bool,
}

/// The figures the host last read of the plugin, to tell a move from a re-read.
struct Declared {
    latency: Samples,
    tail: PluginTail,
}

/// A VST2 plugin, activated.
pub(super) struct Instance {
    /// `None` only while a reconfiguration holds the halves, or after the
    /// plugin was lost (see `lost`).
    halves: Option<(Vst2Processor, Vst2Controller)>,
    /// The plugin, suspended, when no activation would take it again. Every
    /// block then renders silence; the control side is told by
    /// [`is_lost`](Self::is_lost).
    lost: Option<Vst2Loaded>,
    /// Nothing, for a block a lost plugin did not render.
    no_events: MidiEventVec,
    /// What the plugin runs at. Its `mode` follows the live render mode
    /// ([`set_offline_render`](Self::set_offline_render)), so a rate change
    /// re-activates in the mode the plugin was told.
    prepare: Prepare,
    declared: Declared,
    /// A rate change happened since the last [`poll_changes`](Self::poll_changes):
    /// the plugin may have recomputed its latency or tail, and VST2 will not say.
    reread: bool,
}

/// What is done to a plugin that is open and not yet activated.
pub(super) type Restore<'a> = &'a mut dyn FnMut(&mut Vst2Loaded) -> Result<(), String>;

impl Instance {
    /// Loads and activates the plugin at `sample_rate` and blocks of up to
    /// `block_size`, 32-bit, in realtime mode. Main thread.
    ///
    /// # Errors
    ///
    /// The stage that failed and the reason.
    ///
    /// `restore` runs on the plugin **before it is activated**: a plugin may take
    /// its mode from the state at activation, and the activation then is the
    /// state's. Its outcome is returned beside the instance; a refusal does not
    /// fail the load (a plugin at its defaults beats none).
    pub(super) fn load(
        main: &MainThread,
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        restore: Option<Restore<'_>>,
    ) -> Result<(Self, Option<Result<(), String>>), (LoadStage, String)> {
        let mut loaded = Vst2Loaded::load(main, path)
            .map_err(|e| (LoadStage::Opening, format!("vst2-host load failed: {e}")))?;
        let restored = restore.map(|restore| restore(&mut loaded));
        let prepare = MaxBlock::new(Samples(block_size))
            .and_then(|block| Prepare::new(SampleRate(sample_rate), block))
            .ok_or_else(|| {
                (
                    LoadStage::Activation,
                    format!(
                        "vst2-host activation failed: the plugin cannot run at {sample_rate} Hz in blocks of {block_size} frames"
                    ),
                )
            })?;
        match loaded.activate(main, prepare) {
            Ok(halves) => {
                let declared = Declared {
                    latency: halves.1.loaded().latency_samples(),
                    tail: halves.1.loaded().tail(main),
                };
                Ok((
                    Self {
                        halves: Some(halves),
                        lost: None,
                        no_events: MidiEventVec::new(),
                        prepare,
                        declared,
                        reread: false,
                    },
                    restored,
                ))
            }
            Err(refused) => Err((
                LoadStage::Activation,
                format!("vst2-host activation failed: {}", refused.reason),
            )),
        }
    }

    /// The Controller. Panics on a lost plugin: the control side asks
    /// [`is_lost`](Self::is_lost) first (it cannot happen unless a rate
    /// rollback fails twice, which `set_sample_rate`'s up-front check rules out).
    pub(super) fn controller(&self) -> &Vst2Controller {
        &self.halves.as_ref().expect("the plugin is not lost").1
    }

    /// [`controller`](Self::controller), mutably.
    pub(super) fn controller_mut(&mut self) -> &mut Vst2Controller {
        &mut self.halves.as_mut().expect("the plugin is not lost").1
    }

    /// Whether the plugin could not be activated again and renders silence.
    #[must_use]
    pub(super) fn is_lost(&self) -> bool {
        self.halves.is_none()
    }

    /// The latency and tail as the control side last read them
    /// ([`poll_changes`](Self::poll_changes)). Dispatches nothing, so the graph
    /// compile may call it.
    pub(super) fn figures(&self) -> (Samples, PluginTail) {
        (self.declared.latency, self.declared.tail)
    }

    /// Tells the plugin whether it renders offline, and keeps the mode a rate
    /// change activates it in.
    pub(super) fn set_offline_render(&mut self, offline: bool) {
        self.prepare = self.prepare.with_mode(if offline {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        });
        if let Some((_, controller)) = &self.halves {
            controller.loaded().set_offline_render(offline);
        } else if let Some(plugin) = &self.lost {
            plugin.set_offline_render(offline);
        }
    }

    /// One block through the plugin: [`Vst2Processor::process_f32`].
    pub(super) fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        match self.halves.as_mut() {
            Some((processor, _)) => processor.process_f32(inputs, outputs, num_samples, ctx),
            None => {
                for out in outputs.iter_mut() {
                    let n = num_samples.min(out.len());
                    out[..n].fill(0.0);
                }
                &self.no_events
            }
        }
    }

    /// The block's parameter changes, written before it renders:
    /// [`Vst2Processor::write_automation`]. Returns how many were refused.
    pub(super) fn write_automation(&mut self, changes: &crate::protocol::ParameterChanges) -> u32 {
        match self.halves.as_mut() {
            Some((processor, _)) => processor.write_automation(changes),
            None => 0,
        }
    }

    /// SysEx messages dropped whole: [`Vst2Processor::dropped_sysex`].
    #[cfg(test)]
    pub(super) fn dropped_sysex(&mut self) -> u64 {
        self.halves
            .as_mut()
            .map_or(0, |(processor, _)| processor.dropped_sysex())
    }

    /// Tells the plugin its rate: a rejoin and an activation at the new
    /// [`Prepare`], which is how VST2 takes one (`effSetSampleRate` between
    /// `effMainsChanged` brackets). Main thread, under the lock.
    ///
    /// A refusal is rolled back: the plugin is activated again at the rate it
    /// already accepted. The figures the plugin recomputes for the new rate are
    /// read by the next [`poll_changes`](Self::poll_changes).
    pub(super) fn set_sample_rate(&mut self, main: &MainThread, rate: f64) {
        // Before the plugin is taken apart: a refusal after the rejoin would
        // leave a plugin to put back.
        let Some(wanted) = self.prepare.with_rate(SampleRate(rate)) else {
            return;
        };
        let Some((processor, controller)) = self.halves.take() else {
            return;
        };
        let loaded = match Vst2Loaded::deactivate(main, processor, controller) {
            Ok(loaded) => loaded,
            Err(stuck) => {
                self.halves = Some((stuck.processor, stuck.controller));
                return;
            }
        };
        let halves = match loaded.activate(main, wanted) {
            Ok(halves) => {
                self.prepare = wanted;
                Some(halves)
            }
            Err(refused) => match refused.plugin {
                Some(plugin) => match plugin.activate(main, self.prepare) {
                    Ok(halves) => Some(halves),
                    Err(refused) => {
                        self.lost = refused.plugin;
                        None
                    }
                },
                None => None,
            },
        };
        self.halves = halves;
        self.reread = true;
    }

    /// Takes the plugin out of its activation, as a failed rollback would.
    #[cfg(test)]
    pub(super) fn lose(&mut self) {
        let (processor, controller) = self.halves.take().expect("the plugin is not lost");
        let Ok(loaded) = Vst2Loaded::deactivate(
            &MainThread::claim().expect("one test, one process"),
            processor,
            controller,
        ) else {
            panic!("the halves are a pair");
        };
        self.lost = Some(loaded);
    }

    /// What the plugin changed about itself since the last call, and the moment
    /// to act on it.
    ///
    /// Re-reads on `audioMasterIOChanged` (the plugin's own signal: its pin
    /// counts and latency may both have moved), and reports what the host's own
    /// re-reads found after a rate change. Main thread: the re-read dispatches
    /// `effGetTailSize`. An empty result is the common case and costs one atomic
    /// swap.
    pub(super) fn poll_changes(&mut self, main: &MainThread) -> Changes {
        let mut asked = Vec::new();
        self.controller().loaded().take_requests(main, &mut asked);
        let io = asked.contains(&Request::IoChanged);
        let mut changes = Changes {
            io,
            ..Changes::default()
        };
        if io || std::mem::take(&mut self.reread) {
            let latency = self.controller().loaded().latency_samples();
            if latency != self.declared.latency {
                self.declared.latency = latency;
                changes.latency = Some(latency);
            }
            let tail = self.controller().loaded().tail(main);
            if tail != self.declared.tail {
                self.declared.tail = tail;
                changes.tail = Some(tail);
            }
        }
        changes
    }

    /// The pin counts the plugin declares now.
    pub(super) fn pins(&self) -> (ChannelLayout, ChannelLayout) {
        self.controller().loaded().pins()
    }

    /// The plugin's identity and declared capabilities.
    pub(super) fn metadata(&self) -> &PluginInfo {
        self.controller().loaded().metadata()
    }

    /// The plugin's parameter edits since the last call.
    pub(super) fn drain_param_changes(&self) -> Vec<ParameterChange> {
        self.controller().loaded().drain_param_changes()
    }
}

/// The loaded plugin, for the format's own surface. The Controller (which
/// `Vst2Controller` no longer derefs from) is [`Instance::controller`] and
/// [`Instance::controller_mut`], for what is the `Control` trait's.
impl Deref for Instance {
    type Target = Vst2Loaded;

    fn deref(&self) -> &Vst2Loaded {
        self.controller().loaded()
    }
}

impl DerefMut for Instance {
    fn deref_mut(&mut self) -> &mut Vst2Loaded {
        self.controller_mut().loaded_mut()
    }
}

#[cfg(test)]
#[path = "../../../tests/support/probe_path.rs"]
mod probe_path;

#[cfg(test)]
mod tests {

    /// This process's main thread. Each test is its own process under nextest, so
    /// the claim cannot be refused.
    fn main_thread() -> tutti_plugin_types::MainThread {
        tutti_plugin_types::MainThread::claim().expect("one test, one process")
    }

    use super::probe_path;
    use super::*;

    /// **A load at no rate or no block is refused** at the activation stage:
    /// zero, negative and non-finite rates, and a block of no frames.
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every
    /// rate, and `MaxBlock::new` accepting zero, each -> the matching case loads
    /// -> fails.
    #[test]
    fn a_load_at_no_rate_or_no_block_is_refused() {
        for (rate, block) in [
            (0.0, 64),
            (-48_000.0, 64),
            (f64::NAN, 64),
            (f64::INFINITY, 64),
            (f64::NEG_INFINITY, 64),
            (48_000.0, 0),
        ] {
            match Instance::load(&main_thread(), probe_path::probe_path(), rate, block, None) {
                Err((LoadStage::Activation, reason)) => {
                    assert!(reason.contains("cannot run at"), "{reason}");
                }
                Err((stage, reason)) => panic!("{rate} Hz, {block}: {stage:?} {reason}"),
                Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
            }
        }
    }

    /// **A rate that is no rate leaves the instance as it was**: the plugin is
    /// not taken apart (its halves stay, it is not lost) and the configuration is
    /// the one it had.
    ///
    /// Mutation (run): `valid_rate` in `tutti-plugin-types` accepting every
    /// rate -> the zero rate is activated and kept as the configuration ->
    /// `prepare` changes -> fails.
    #[test]
    fn a_rate_that_is_no_rate_leaves_the_instance_alone() {
        let mut instance =
            Instance::load(&main_thread(), probe_path::probe_path(), 48_000.0, 64, None)
                .expect("loads")
                .0;
        let before = instance.prepare;
        for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            instance.set_sample_rate(&main_thread(), rate);
            assert_eq!(instance.prepare, before, "{rate}");
            assert!(instance.halves.is_some(), "{rate}");
            assert!(!instance.is_lost(), "{rate}");
        }
        instance.set_sample_rate(&main_thread(), 96_000.0);
        assert_eq!(instance.prepare.rate().0, 96_000.0);
    }

    /// **A plugin that is lost renders silence, and the audio thread does not
    /// panic**: the block's outputs are zeroed, no event comes out, and the
    /// automation is refused without a plugin to take it.
    ///
    /// Mutation (run): `process_f32`'s `None` arm panicking
    /// (`unreachable!`) -> the block panics -> fails. Mutation (run): the arm
    /// returning without zeroing -> the outputs keep 1.0 -> fails.
    #[test]
    fn a_lost_plugin_renders_silence_without_panicking() {
        let mut instance =
            Instance::load(&main_thread(), probe_path::probe_path(), 48_000.0, 64, None)
                .expect("loads")
                .0;
        assert!(!instance.is_lost());
        instance.lose();
        assert!(instance.is_lost());

        let inputs = [[0.0f32; 64], [0.0; 64]];
        let inputs: Vec<&[f32]> = inputs.iter().map(|c| &c[..]).collect();
        let mut out = [[1.0f32; 64], [1.0; 64]];
        let mut outputs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[..]).collect();
        let ctx = Vst2ProcessContext::new(48_000.0);
        let events = instance.process_f32(&inputs, &mut outputs, 64, &ctx);
        assert!(events.is_empty());
        assert!(out.iter().flatten().all(|&x| x == 0.0), "silence");
        assert_eq!(
            instance.write_automation(&crate::protocol::ParameterChanges::default()),
            0
        );
    }
}
