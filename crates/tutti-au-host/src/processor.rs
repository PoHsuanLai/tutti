//! [`AuProcessor`]: the realtime half of an initialized Audio Unit.
//!
//! It owns what only the render path touches: the renderer and the guard that
//! uninitializes the unit, the SysEx reassembly buffer, the transport the AU
//! pulls from, and a copy of the parameter ranges. The main thread's half,
//! [`AuController`](crate::AuController), holds the unit's loaded state; both
//! reach the one unit through AudioToolbox, which documents parameter access
//! as callable from any thread and serializes property access, and each holds
//! its own [`Unit`] handle so the instance outlives whichever drops last.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use tutti_au_ffi::{Element, Renderer, Scope, TransportState, Unit};
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    AudioBufferMut, PlayheadWatch, PluginError, PluginIo, PluginResult, ProcessContext,
    ProcessOutput, Processor, TransportInfo, is_usable,
};
use tutti_rt::Reader;
use tutti_ump::SysexAssembler;

use crate::error::{AuError, Result};
use crate::ranges::RangeTable;
use crate::types::K_AUDIO_UNIT_ERR_UNINITIALIZED;

/// Whether the AU raised `OutputIsSilence` for the block just rendered.
///
/// A bool would do, but the flag's meaning is the opposite of what a bare
/// `true` reads as at the call site: it says the output buffers hold *nothing
/// trustworthy*, not that they hold zeros. Naming the states keeps a caller
/// from emitting the scratch on the strength of "it rendered fine".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Silence {
    /// The AU declared the block silent; the scratch contents are stale and the
    /// destination must be zeroed instead of copied into.
    Declared,
    /// A normal block: emit the scratch.
    No,
}

/// What an initialized unit holds beside its loaded state.
///
/// Field order is drop order. The guard goes first: it uninitializes the unit
/// before the renderer (whose input callback the unit may still call while
/// initialized) is dropped.
pub(crate) struct Rendering {
    pub(crate) guard: UninitializeOnDrop,
    pub(crate) renderer: Renderer,
}

/// Uninitializes the unit when a [`Rendering`] is dropped without
/// [`Loaded::deactivate`](tutti_plugin_types::Loaded::deactivate) having done so.
pub(crate) struct UninitializeOnDrop {
    pub(crate) unit: Unit,
    pub(crate) armed: bool,
}

impl Drop for UninitializeOnDrop {
    fn drop(&mut self) {
        if self.armed {
            // Teardown: the status has nowhere to go.
            let _ = self.unit.uninitialize();
        }
    }
}

/// The realtime half of an activation: it renders blocks and nothing else.
///
/// `Send` to move to the realtime thread (its renderer, unit handle and ranges
/// reader are each `Send` at their own definition), not `Sync`.
pub struct AuProcessor {
    // Boxed so a `Stuck` carrying the Processor stays small, and declared first
    // so the unit is uninitialized before anything else here is dropped.
    pub(crate) rendering: Box<Rendering>,
    unit: Unit,
    /// Reassembles the SysEx7 packets of a block into whole messages, in a
    /// buffer reserved at activation and never grown, so the audio thread does
    /// not allocate for a long message.
    sysex: SysexAssembler<Box<[u8]>>,
    ranges: Reader<RangeTable>,
    /// The transport the AU pulls from, `None` for a unit that refused the host
    /// callbacks: nothing reads what would be written.
    transport: Option<Arc<TransportState>>,
    /// Derives the "transport changed" flag the AU's transport callback reports
    /// from consecutive blocks' snapshots.
    playhead: PlayheadWatch,
    receives_midi: bool,
    block_size: u32,
}

impl std::fmt::Debug for AuProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuProcessor")
            .field("block_size", &self.block_size)
            .finish_non_exhaustive()
    }
}

/// What activation hands [`AuProcessor::new`].
pub(crate) struct ProcessorParts {
    pub(crate) rendering: Box<Rendering>,
    pub(crate) unit: Unit,
    pub(crate) sysex: SysexAssembler<Box<[u8]>>,
    pub(crate) ranges: Reader<RangeTable>,
    pub(crate) transport: Option<Arc<TransportState>>,
    pub(crate) receives_midi: bool,
    pub(crate) block_size: u32,
}

impl AuProcessor {
    pub(crate) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            rendering,
            unit,
            sysex,
            ranges,
            transport,
            receives_midi,
            block_size,
        } = parts;
        Self {
            rendering,
            unit,
            sysex,
            ranges,
            transport,
            playhead: PlayheadWatch::new(),
            receives_midi,
            block_size,
        }
    }

    /// Whether this Processor and `unit` are of one activation.
    pub(crate) fn is_of(&self, unit: &Unit) -> bool {
        self.unit.same_unit(unit)
    }

    /// Test-only: how many times the input render callback has been installed
    /// on this activation.
    #[cfg(feature = "test-support")]
    pub fn input_callback_installs(&self) -> u32 {
        self.rendering.renderer.input_callback_installs()
    }

    /// Clears the AU's internal audio state (reverb tails, delay lines, filter
    /// memory) without disturbing its parameters.
    ///
    /// A host must call this on every discontinuity in the timeline, and the
    /// symptom of not calling it is stale audio arriving where none belongs:
    /// jumping from bar 60 to bar 1 smears the reverb tail of bar 60 across the
    /// downbeat. Parameters are deliberately untouched.
    ///
    /// # The render clock restarts too
    ///
    /// `AudioUnitReset` flushes the AU's signal history; the render cursor this
    /// host stamps each block's `mSampleTime` from is *this crate's* state, and
    /// AudioToolbox has no idea it exists. Leaving it running after a flush hands
    /// the AU a timestamp saying the block after a locate follows contiguously
    /// from the block before it, the opposite of what the flush announced. Only
    /// on success: a refused flush leaves the AU holding the history it had, so
    /// restarting the clock beside it would describe neither state. The push
    /// path's cursor is a separate object the host owns
    /// ([`PushScratch::reset_position`](crate::offline::PushScratch::reset_position)).
    ///
    /// A SysEx open across the reset is dropped, as in every format.
    ///
    /// # Errors
    /// [`AuError::OsStatus`] with the AU's own status. Propagated rather than
    /// absorbed: a host that jumps the playhead and silently fails to flush
    /// produces audible garbage on the next block.
    pub fn flush(&mut self) -> Result<()> {
        // Global scope / element 0 is the documented address for a whole-unit
        // reset; per-bus reset is not a thing AUv2 offers.
        self.unit.reset(Scope::GLOBAL, Element::ZERO)?;
        self.rendering.renderer.reset_position();
        self.sysex.reset();
        // A new timeline breaks the previous block's prediction of the playhead.
        self.playhead.reset();
        Ok(())
    }

    /// Delivers a block of UMP MIDI events to an instrument / music-effect AU as
    /// legacy `MusicDeviceMIDIEvent` calls.
    ///
    /// Each channel-voice event goes out as the MIDI 1.0 messages it stands for
    /// (`midi1`: a MIDI 1.0 one as it came, a MIDI 2.0 one narrowed, a Note On
    /// never at velocity 0, and a banked Program Change as CC 0, CC 32, then the
    /// program). One with no MIDI 1.0 form (per-note controllers, RPN/NRPN,
    /// per-note management) sends nothing. SysEx goes through `MusicDeviceSysEx`
    /// once reassembled across packets in the pre-sized buffer; a message past
    /// its capacity is dropped whole and counted
    /// ([`sysex_dropped`](Self::sysex_dropped)). A run left open at the end of a
    /// block continues in the next.
    pub fn send_midi(&mut self, events: &[UmpAt]) {
        use tutti_midi_protocol::midi1;
        use tutti_ump::{Message, SysexStep, decode};

        self.sysex.begin_block();

        let unit = &self.unit;
        for &(offset, ev) in events {
            match decode(ev) {
                Ok(message @ (Message::Midi1(..) | Message::Midi2(..))) => {
                    for bytes in midi1(message) {
                        let [status, d1, d2] = bytes.padded();
                        let _ = unit.midi_event(status, d1, d2, offset.get());
                    }
                }
                // SysEx has no 3-byte form; it goes out through a different AU
                // entry point entirely. Anything else (system, stream, utility)
                // has no AUv2 delivery and the assembler passes it by.
                _ => {
                    if let SysexStep::Complete(message) = self.sysex.push(ev) {
                        // `MusicDeviceSysEx` takes the complete message, `0xF0`
                        // and `0xF7` included (`MusicDevice.h`).
                        let _ = unit.sysex(self.sysex.message(message).bytes());
                    }
                }
            }
        }
    }

    /// How many SysEx messages were dropped whole for want of room, or because
    /// the packets that completed them never arrived.
    pub fn sysex_dropped(&self) -> u64 {
        self.sysex.dropped()
    }

    /// Renders `num_frames` through the AU: `input` staged into the render
    /// scratch, `output` filled from it.
    ///
    /// The AU renders in f32 either way. [`render_f64`](Self::render_f64) is the
    /// same with a narrowing and widening round trip through the one scratch.
    ///
    /// # Errors
    /// [`AuError::InvalidBuffer`] if `num_frames` exceeds the configured block
    /// size, or [`AuError::RenderFailed`] if `AudioUnitRender` fails.
    pub fn render_f32(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> Result<()> {
        self.rendering.renderer.stage_input(input, num_frames);
        if self.render_frames(num_frames)? == Silence::Declared {
            let n = num_frames as usize;
            for dst in output.iter_mut() {
                let len = n.min(dst.len());
                dst[..len].fill(0.0);
            }
        } else {
            self.rendering.renderer.emit_output(output, num_frames);
        }
        Ok(())
    }

    /// [`render_f32`](Self::render_f32) for an f64 caller. AUv2 has no native f64
    /// render entry point, so an f64 pipeline always costs a narrowing round
    /// trip through this AU; it is made here because the render scratch is
    /// already owned here, so the round trip costs no allocation.
    ///
    /// # Errors
    /// As [`render_f32`](Self::render_f32).
    pub fn render_f64(
        &mut self,
        input: &[&[f64]],
        output: &mut [&mut [f64]],
        num_frames: u32,
    ) -> Result<()> {
        self.rendering.renderer.stage_input_f64(input, num_frames);
        if self.render_frames(num_frames)? == Silence::Declared {
            let n = num_frames as usize;
            for dst in output.iter_mut() {
                let len = n.min(dst.len());
                dst[..len].fill(0.0);
            }
        } else {
            self.rendering.renderer.emit_output_f64(output, num_frames);
        }
        Ok(())
    }

    /// The width-agnostic half of a render: bounds-check, `AudioUnitRender`, and
    /// the silence-flag read.
    ///
    /// Split out so the f32 and f64 entry points cannot drift on any of it:
    /// notably the `LastRenderError` enrichment and the `OutputIsSilence`
    /// handling. Allocation-free on the success path.
    fn render_frames(&mut self, num_frames: u32) -> Result<Silence> {
        if num_frames > self.block_size {
            return Err(AuError::InvalidBuffer(format!(
                "num_frames ({num_frames}) > block_size ({})",
                self.block_size
            )));
        }
        // The render callback is installed ONCE at activation off the renderer's
        // stable heap scratch, deliberately NOT here: re-installing per block is
        // an `AudioUnitSetProperty` on the render thread every call. On failure
        // the renderer attaches the AU's last render error (an advisory
        // global-scope read, made only when the render itself failed).
        let silent = self.rendering.renderer.render(num_frames)?;
        // When the AU declares the block silent its output buffers are not
        // guaranteed to be zeroed, so the caller forces the destination to
        // silence rather than emit stale scratch.
        Ok(if silent {
            Silence::Declared
        } else {
            Silence::No
        })
    }

    /// Publishes this block's transport for the AU to pull during its render:
    /// the snapshot, whether it is a start/stop/locate since the last block, and
    /// the frames to the next whole beat. Allocation-free and lock-free. The
    /// distance to the next beat is stored before the snapshot, whose publication
    /// is what makes the callbacks start answering.
    fn publish_transport(&mut self, transport: &TransportInfo, frames: usize, rate: f64) {
        let Some(state) = &self.transport else {
            return;
        };
        let mut snapshot = *transport;
        // A snapshot without its own rate (a hand-built one) borrows the block's,
        // so the beat distance can still be derived.
        if !is_usable(snapshot.sample_rate) || snapshot.sample_rate <= 0.0 {
            snapshot.sample_rate = rate;
        }
        let frames = u32::try_from(frames).unwrap_or(u32::MAX);
        let changed = self.playhead.observe(&snapshot, frames);
        state.set_samples_to_next_beat(snapshot.samples_to_next_beat().unwrap_or(0));
        state.set_transport(&snapshot, changed);
    }

    /// A block with no transport: the AU's callbacks answer "cannot provide"
    /// until the next block that has one, which is then a start.
    fn withdraw_transport(&mut self) {
        self.playhead.reset();
        if let Some(state) = &self.transport {
            state.withdraw();
        }
    }

    /// One block, with the buffers, the context and the output taken apart.
    /// [`Processor::process`] is this with a [`PluginIo`]; a caller whose channel
    /// tables have distinct lifetimes, as `AudioBufferMut` allows, calls this one.
    ///
    /// # Errors
    /// [`PluginError::Block`] when the AU failed the block.
    pub fn render(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        // Automation arrives normalized `0..=1`, but `AudioUnitSetParameter`
        // takes NATIVE PLAIN UNITS. Writing the normalized value straight
        // through set Apple AUDelay's Lowpass Cutoff (declared `[10, 22050]` Hz)
        // to 1 Hz at full scale. Denormalize against the range the AU itself
        // declared; a parameter missing from the table is skipped rather than
        // written blind: a guessed range would be the same class of bug.
        if let Some(changes) = ctx.param_changes {
            let table = self.ranges.lend();
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    let id = queue.param_id.get();
                    if let Some(bounds) = table.bounds(id) {
                        let _ = crate::listener::set_parameter_notifying(
                            &self.unit,
                            id,
                            crate::listener::EventAddress::GLOBAL,
                            bounds.to_plain(point.value.get()),
                            0,
                        );
                    }
                }
            }
        }

        // The AU pulls transport during its render, so this block's snapshot
        // goes in first. A block with no transport withdraws the last one.
        match ctx.transport {
            Some(transport) => {
                self.publish_transport(transport, buffer.num_samples(), buffer.sample_rate());
            }
            None => self.withdraw_transport(),
        }

        // Deliver MIDI to instrument / music-effect AUs before rendering, so
        // note-ons scheduled this block sound in it. Plain effects don't consume
        // MIDI: skip the decode for them.
        if !ctx.midi_events.is_empty() && self.receives_midi {
            self.send_midi(ctx.midi_events);
        }

        let rendered = match buffer {
            AudioBufferMut::F32(buf) => {
                self.render_f32(buf.inputs, buf.outputs, buf.num_samples as u32)
            }
            AudioBufferMut::F64(buf) => {
                self.render_f64(buf.inputs, buf.outputs, buf.num_samples as u32)
            }
        };
        // AUv2 reports no per-block MIDI, parameter or note-expression output
        // through this path, so `out` is left as the caller cleared it.
        let _ = out;
        rendered.map_err(|e| PluginError::Block(block_reason(&e)))
    }
}

/// Why a block failed, as a constant: the audio thread builds this for every
/// errored block, so it must not allocate (`AuError`'s own text is formatted).
fn block_reason(error: &AuError) -> &'static str {
    match error {
        AuError::InvalidBuffer(_) => "the block is longer than the activated block size",
        AuError::RenderFailed { code, .. } if *code == K_AUDIO_UNIT_ERR_UNINITIALIZED => {
            "the unit is no longer initialized"
        }
        AuError::RenderFailed { .. } => "AudioUnitRender failed",
        _ => "the unit failed the block",
    }
}

impl Processor for AuProcessor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    /// Nothing to end: AudioUnitRender has no processing state to stop, and the
    /// next block renders as it would have.
    fn stop(&mut self) {}

    fn reset(&mut self) {
        // A refusal leaves the AU holding the history it had and the cursor
        // where it was (see `flush`); `Processor::reset` has nowhere to put it.
        let _ = self.flush();
    }
}
