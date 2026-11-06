//! The plugin both halves of the in-process node share: the two halves of one
//! activation behind the one lock the node and its handle take.
//!
//! The audio thread reaches the Processor through the lock's `try_lock`; the
//! control side reaches the Controller (and, through it, the loaded plugin)
//! through the lock itself. A rate change is a rejoin and an activation, which
//! needs both halves, so it happens only on the control side, under the lock.

use std::ops::{Deref, DerefMut};
use std::path::Path;

use tutti_plugin_types::{Loaded, Prepare, RenderMode, Request, SampleFormat, SampleRate, Samples};
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

impl Instance {
    /// Loads and activates the plugin at `sample_rate` and blocks of up to
    /// `block_size`, 32-bit, in realtime mode. Main thread.
    ///
    /// # Errors
    ///
    /// The stage that failed and the reason.
    pub(super) fn load(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<Self, (LoadStage, String)> {
        let loaded = Vst2Loaded::load(path)
            .map_err(|e| (LoadStage::Opening, format!("vst2-host load failed: {e}")))?;
        let prepare = Prepare {
            rate: SampleRate(sample_rate),
            max_block: Samples(block_size),
            format: SampleFormat::Float32,
            mode: RenderMode::Realtime,
        };
        match loaded.activate(prepare) {
            Ok(halves) => {
                let declared = Declared {
                    latency: halves.1.latency_samples(),
                    tail: halves.1.tail(),
                };
                Ok(Self {
                    halves: Some(halves),
                    lost: None,
                    no_events: MidiEventVec::new(),
                    prepare,
                    declared,
                    reread: false,
                })
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
    fn controller(&self) -> &Vst2Controller {
        &self.halves.as_ref().expect("the plugin is not lost").1
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
        self.prepare.mode = if offline {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        };
        if let Some((_, controller)) = &self.halves {
            controller.set_offline_render(offline);
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
    pub(super) fn set_sample_rate(&mut self, rate: f64) {
        // The same test `activate` makes, before the plugin is taken apart: a
        // refusal after the rejoin would leave a plugin to put back.
        if !(rate.is_finite() && rate > 0.0) {
            return;
        }
        let Some((processor, controller)) = self.halves.take() else {
            return;
        };
        let mut wanted = self.prepare;
        wanted.rate = SampleRate(rate);
        let loaded = match Vst2Loaded::deactivate(processor, controller) {
            Ok(loaded) => loaded,
            Err(stuck) => {
                self.halves = Some((stuck.processor, stuck.controller));
                return;
            }
        };
        let halves = match loaded.activate(wanted) {
            Ok(halves) => {
                self.prepare = wanted;
                Some(halves)
            }
            Err(refused) => match refused.plugin {
                Some(plugin) => match plugin.activate(self.prepare) {
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
        let Ok(loaded) = Vst2Loaded::deactivate(processor, controller) else {
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
    pub(super) fn poll_changes(&mut self) -> Changes {
        let mut asked = Vec::new();
        self.controller().take_requests(&mut asked);
        let io = asked.contains(&Request::IoChanged);
        let mut changes = Changes {
            io,
            ..Changes::default()
        };
        if io || std::mem::take(&mut self.reread) {
            let latency = self.controller().latency_samples();
            if latency != self.declared.latency {
                self.declared.latency = latency;
                changes.latency = Some(latency);
            }
            let tail = self.controller().tail();
            if tail != self.declared.tail {
                self.declared.tail = tail;
                changes.tail = Some(tail);
            }
        }
        changes
    }

    /// The pin counts the plugin declares now.
    pub(super) fn pins(&self) -> (ChannelLayout, ChannelLayout) {
        self.controller().pins()
    }

    /// The plugin's identity and declared capabilities.
    pub(super) fn metadata(&self) -> &PluginInfo {
        self.controller().metadata()
    }

    /// The plugin's parameter edits since the last call.
    pub(super) fn drain_param_changes(&self) -> Vec<ParameterChange> {
        self.controller().drain_param_changes()
    }
}

impl Deref for Instance {
    type Target = Vst2Controller;

    fn deref(&self) -> &Vst2Controller {
        self.controller()
    }
}

impl DerefMut for Instance {
    fn deref_mut(&mut self) -> &mut Vst2Controller {
        &mut self.halves.as_mut().expect("the plugin is not lost").1
    }
}

#[cfg(test)]
#[path = "../../../tests/support/probe_path.rs"]
mod probe_path;

#[cfg(test)]
mod tests {
    use super::probe_path;
    use super::*;

    /// **A plugin that is lost renders silence, and the audio thread does not
    /// panic**: the block's outputs are zeroed, no event comes out, and the
    /// automation is refused without a plugin to take it.
    ///
    /// Mutation (run): `process_f32`'s `None` arm panicking
    /// (`unreachable!`) -> the block panics -> fails. Mutation (run): the arm
    /// returning without zeroing -> the outputs keep 1.0 -> fails.
    #[test]
    fn a_lost_plugin_renders_silence_without_panicking() {
        let mut instance = Instance::load(probe_path::probe_path(), 48_000.0, 64).expect("loads");
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
