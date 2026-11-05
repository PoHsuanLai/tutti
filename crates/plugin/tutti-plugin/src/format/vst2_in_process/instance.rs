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
    /// `None` only while a reconfiguration holds the halves.
    halves: Option<(Vst2Processor, Vst2Controller)>,
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

    fn controller(&self) -> &Vst2Controller {
        &self.halves.as_ref().expect("the halves are whole").1
    }

    fn processor(&mut self) -> &mut Vst2Processor {
        &mut self.halves.as_mut().expect("the halves are whole").0
    }

    /// One block through the plugin: [`Vst2Processor::process_f32`].
    pub(super) fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.processor()
            .process_f32(inputs, outputs, num_samples, ctx)
    }

    /// The block's parameter changes, written before it renders:
    /// [`Vst2Processor::write_automation`]. Returns how many were refused.
    pub(super) fn write_automation(&mut self, changes: &crate::protocol::ParameterChanges) -> u32 {
        self.processor().write_automation(changes)
    }

    /// SysEx messages dropped whole: [`Vst2Processor::dropped_sysex`].
    #[cfg(test)]
    pub(super) fn dropped_sysex(&mut self) -> u64 {
        self.processor().dropped_sysex()
    }

    /// Tells the plugin its rate: a rejoin and an activation at the new
    /// [`Prepare`], which is how VST2 takes one (`effSetSampleRate` between
    /// `effMainsChanged` brackets). Main thread, under the lock.
    ///
    /// A refusal is rolled back: the plugin is activated again at the rate it
    /// already accepted. The figures the plugin recomputes for the new rate are
    /// read by the next [`poll_changes`](Self::poll_changes).
    pub(super) fn set_sample_rate(&mut self, rate: f64) {
        let mut wanted = self.prepare;
        wanted.rate = SampleRate(rate);
        let (processor, controller) = self.halves.take().expect("the halves are whole");
        let loaded = match Vst2Loaded::deactivate(processor, controller) {
            Ok(loaded) => loaded,
            Err(stuck) => {
                self.halves = Some((stuck.processor, stuck.controller));
                return;
            }
        };
        match loaded.activate(wanted) {
            Ok(halves) => {
                self.halves = Some(halves);
                self.prepare = wanted;
            }
            Err(refused) => {
                let plugin = refused.plugin.expect("VST2 hands the plugin back");
                match plugin.activate(self.prepare) {
                    Ok(halves) => self.halves = Some(halves),
                    Err(refused) => {
                        panic!(
                            "the plugin would not run again at the rate it ran at: {}",
                            refused.reason
                        )
                    }
                }
            }
        }
        self.reread = true;
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
        &mut self.halves.as_mut().expect("the halves are whole").1
    }
}
