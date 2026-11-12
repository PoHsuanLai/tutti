//! The transitions of an Audio Unit: [`Loaded`] for [`AuLoaded`].
//!
//! `activate` is `AudioUnitInitialize` plus the configuration it needs (the
//! rate and block ceiling of the [`Prepare`], the render mode, the input render
//! callback); `deactivate` is `AudioUnitUninitialize`. A unit that refuses
//! `initialize` comes back whole in [`Refused`]; one that refuses the render
//! callback *and* the compensating uninitialize cannot be handed back, and
//! `Refused::plugin` is `None`.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use tutti_au_ffi::{RenderNotify, Renderer};
use tutti_plugin_types::{
    Loaded, MainThread, PluginError, Prepare, Refused, RestartRules, Stuck, sysex_assembler,
};
use tutti_rt::{ArcLend, edit_ring};

use crate::automation::{Stage, schedule_pending};
use crate::control::plugin_error;
use crate::controller::AuController;
use crate::error::AuError;
use crate::instance::AuLoaded;
use crate::processor::{AuProcessor, ProcessorParts, Rendering, UninitializeOnDrop};

/// Writes to `NonRealTime` parameters one block can hand the control side
/// before a tick drains them: one per parameter per block, so this is seconds
/// of every such parameter moving at once.
const DEFERRED_ROOM: usize = 1024;

impl Loaded for AuLoaded {
    type Processor = AuProcessor;
    type Controller = AuController;

    /// AU re-reads: latency and tail are plain property reads with no documented
    /// requirement that the unit be uninitialized, and the property listener
    /// fires *after* the AU has already changed the value. A rate or stream
    /// format change is the host's own reconfiguration, applied by `activate`.
    const RESTARTS: RestartRules = RestartRules::NONE;

    /// Initializes the unit so it can render.
    ///
    /// The stream is configured at `prepare`'s rate and block ceiling first
    /// (rolled back if the unit refuses), the render mode is written (the
    /// property is only honoured at `AudioUnitInitialize`), and the input render
    /// callback is installed ONCE, off the renderer's heap-pinned scratch, never
    /// per block on the render thread. The callback is gated on `has_input`, the
    /// AU's own answer to "is there an input bus": setting a render callback on
    /// the input scope of a unit that has no input element is a property error
    /// that failed every instrument's activation.
    ///
    /// # Errors
    /// [`Refused`] carries the unit back, as it was before the call, for a
    /// stream configuration or `AudioUnitInitialize` the unit refuses.
    #[allow(clippy::result_large_err)]
    fn activate(
        self,
        _main: &MainThread,
        prepare: Prepare,
    ) -> Result<(AuProcessor, AuController), Refused<Self>> {
        self.initialize(prepare).map_err(|(plugin, error)| Refused {
            plugin,
            reason: plugin_error(error),
        })
    }

    /// Tears down the render session: `AudioUnitUninitialize`, then the renderer
    /// (and the scratch its input callback reads) is dropped.
    ///
    /// ORDERING INVARIANT: the uninitialize runs before the renderer is dropped.
    /// Uninitialize is what stops the AU firing the input render callback, and
    /// the renderer withdraws the callback itself before freeing; the guard is
    /// disarmed only once the unit really is uninitialized.
    ///
    /// # Errors
    /// [`Stuck`] hands both halves back, as they were, for a pair from two
    /// activations or a unit that refuses to uninitialize (it stays
    /// initialized, and the guard retries on drop).
    fn deactivate(
        _main: &MainThread,
        mut processor: AuProcessor,
        controller: AuController,
    ) -> Result<Self, Stuck<AuProcessor, AuController>> {
        if !processor.is_of(controller.loaded.unit()) {
            return Err(Stuck {
                processor,
                controller,
                reason: PluginError::Other("the halves are of two activations".into()),
            });
        }
        if let Err(e) = processor.rendering.guard.unit.uninitialize() {
            return Err(Stuck {
                processor,
                controller,
                reason: plugin_error(e.into()),
            });
        }
        processor.rendering.guard.armed = false;
        drop(processor);
        Ok(controller.loaded)
    }
}

impl AuLoaded {
    /// [`activate`](Loaded::activate) with the host's own error: the same
    /// transition, for a caller that wants the typed [`AuError`] (a test that
    /// tells `kAudioUnitErr_InvalidElement` from another refusal) rather than
    /// the shared vocabulary's.
    ///
    /// # Errors
    /// The unit back, as it was before the call, beside the error, for a stream
    /// configuration or `AudioUnitInitialize` the unit refuses. The unit is
    /// `None` only when the render callback was refused *and* the compensating
    /// uninitialize failed too.
    #[allow(clippy::type_complexity, clippy::result_large_err)]
    pub fn initialize(
        mut self,
        prepare: Prepare,
    ) -> Result<(AuProcessor, AuController), (Option<Self>, AuError)> {
        // The unit's frame count is a `u32`, the bound `MaxBlock` holds, and a
        // `MaxBlock` is never zero.
        let block_size = prepare.max_block().get_u32();
        if let Err(e) = self.apply_stream(prepare.rate().0, block_size) {
            return Err((Some(self), e));
        }

        // Written only when it changes, and before `initialize`: a unit that
        // sizes an oversampling or look-ahead buffer from the flag does so there.
        let offline = prepare.mode().is_offline();
        if offline != self.wrote_offline() {
            self.write_render_mode(offline);
        }

        if let Err(e) = self.unit().initialize() {
            return Err((Some(self), e.into()));
        }

        let unit = self.unit().clone();
        // The renderer owns the heap-pinned scratch the input callback reads,
        // and withdraws that callback before freeing it.
        let renderer = Renderer::new(&unit, self.config().channels, self.config().block_size);
        // The pre-render notify that schedules a block's automation at its
        // frames. A unit that refuses it still renders; its points are then
        // written at the block head.
        let stage = Stage::shared();
        let notify = {
            let stage = Arc::clone(&stage);
            let render_unit = unit.render_unit();
            RenderNotify::new(&unit, move |notification| {
                schedule_pending(&stage, &render_unit, &notification);
            })
            .ok()
        };
        let mut rendering = Box::new(Rendering {
            guard: UninitializeOnDrop {
                unit: unit.clone(),
                armed: true,
            },
            notify,
            renderer,
        });

        if self.config().channels.has_input
            && let Err(e) = rendering.renderer.install_input_callback()
        {
            // The AU *is* initialized at this point, so backing out has to undo
            // that too. The reported error is always `e`, the install failure: a
            // follow-on `AudioUnitUninitialize` complaint would only describe the
            // cleanup. If that cleanup also failed there is no `Loaded` unit to
            // hand back: dropping the armed guard retries it and disposes.
            let error: AuError = e.into();
            return Err(match unit.uninitialize() {
                Ok(()) => {
                    rendering.guard.armed = false;
                    drop(rendering);
                    (Some(self), error)
                }
                Err(_) => {
                    drop(rendering);
                    drop(self);
                    (None, error)
                }
            });
        }

        let ranges = Arc::new(ArcLend::new(self.range_snapshot()));
        let (deferred_in, deferred_out) = edit_ring(DEFERRED_ROOM);
        let processor = AuProcessor::new(ProcessorParts {
            rendering,
            unit,
            // Off the audio thread: reserve the SysEx buffer now, so
            // `send_midi` never grows one.
            sysex: sysex_assembler(),
            ranges: ranges.reader(),
            transport: self.share_transport(),
            receives_midi: self.au_type().receives_midi(),
            block_size: self.config().block_size,
            stage,
            deferred: deferred_in,
            listener: self.listener(),
        });
        Ok((processor, AuController::new(self, ranges, deferred_out)))
    }
}
