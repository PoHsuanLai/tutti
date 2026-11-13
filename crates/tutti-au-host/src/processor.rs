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

use tutti_au_ffi::{
    AuParameterListener, Element, EventAddress, EventList, Pushed, RenderNotify, Renderer, Scope,
    TransportState, Unit, Wire,
};
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    AudioBufferMut, ParamMapping, PlayheadWatch, PluginError, PluginIo, PluginResult,
    ProcessContext, ProcessOutput, Processor, TransportInfo, is_usable,
};
use tutti_rt::Reader;
use tutti_ump::SysexAssembler;

use crate::automation::SharedStage;
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
    /// The pre-render notify that schedules a block's automation at its frames
    /// ([`crate::automation`]); `None` when the unit refused it, and a block's
    /// points are then written at its head. Dropped after the guard, before
    /// the renderer: it removes itself from the unit.
    pub(crate) notify: Option<RenderNotify>,
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
    /// How `send_midi` delivers: UMP through `MusicDeviceMIDIEventList` for a
    /// unit that takes MIDI 2.0, legacy bytes for every other.
    midi: MidiDelivery,
    block_size: u32,
    /// The block's staged automation, which the render notify schedules.
    stage: SharedStage,
}

/// What `send_midi` delivers through: the protocol the unit takes, the list
/// built once per activation and the refusals counted.
struct MidiDelivery {
    wire: Option<Wire>,
    list: EventList,
    groups_dropped: u64,
    unsendable_dropped: u64,
}

/// Stages a block's events into `list` and hands it to `send` whenever it is
/// full or cannot take the next packet (another protocol, an earlier time), and
/// once at the end. The list is empty on entry and on return.
fn stage_ump(list: &mut EventList, events: &[UmpAt], mut send: impl FnMut(&EventList)) {
    list.clear();
    for &(offset, ev) in events {
        // Types with a Group and a musical meaning: System, channel voice,
        // SysEx7 and Data 128. Utility, Flex and Stream are not sent.
        if !matches!(ev.message_type(), 0x1..=0x5) {
            continue;
        }
        let wire = Wire::of_message_type(ev.message_type());
        if list.push(wire, offset.get(), ev.words()) == Pushed::Refused {
            send(list);
            list.clear();
            // An empty list takes any one packet.
            let _ = list.push(wire, offset.get(), ev.words());
        }
    }
    if !list.is_empty() {
        send(list);
    }
    list.clear();
}

/// Words of the event list reserved at activation: a few hundred packets, and a
/// block with more is sent as several lists.
const EVENT_LIST_WORDS: usize = 1024;

impl MidiDelivery {
    fn new(wire: Option<Wire>) -> Self {
        Self {
            wire,
            // Reserved only for a unit that is sent one.
            list: EventList::with_capacity(if wire == Some(Wire::Midi2) {
                EVENT_LIST_WORDS
            } else {
                0
            }),
            groups_dropped: 0,
            unsendable_dropped: 0,
        }
    }
}

/// Writes `value` (plain units) to `id` now, telling every listener but the
/// host's own.
pub(crate) fn write_now(
    listener: &Option<Arc<AuParameterListener>>,
    unit: &Unit,
    id: u32,
    value: f32,
) -> tutti_au_ffi::Result<()> {
    match listener {
        Some(listener) => listener.set_parameter(id, EventAddress::GLOBAL, value, 0),
        None => crate::listener::set_parameter_notifying(unit, id, EventAddress::GLOBAL, value, 0),
    }
}

/// Tells every listener but the host's own that `id` changed, writing nothing.
pub(crate) fn notify_now(
    listener: &Option<Arc<AuParameterListener>>,
    unit: &Unit,
    id: u32,
) -> tutti_au_ffi::Result<()> {
    match listener {
        Some(listener) => listener.notify_parameter(id, EventAddress::GLOBAL),
        None => crate::listener::notify_parameter(unit, id, EventAddress::GLOBAL),
    }
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
    pub(crate) midi_protocol: Option<Wire>,
    pub(crate) block_size: u32,
    pub(crate) stage: SharedStage,
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
            midi_protocol,
            block_size,
            stage,
        } = parts;
        Self {
            rendering,
            unit,
            sysex,
            ranges,
            transport,
            playhead: PlayheadWatch::new(),
            receives_midi,
            midi: MidiDelivery::new(midi_protocol),
            block_size,
            stage,
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

    /// Delivers a block of UMP MIDI events to an instrument / music-effect AU,
    /// by the protocol the unit takes ([`midi_protocol`](Self::midi_protocol)).
    ///
    /// # A unit that takes MIDI 2.0: the packets as they are
    ///
    /// Every packet of a channel voice, System, SysEx7 or Data 128 message goes
    /// to `MusicDeviceMIDIEventList` unchanged, its Group in its first word,
    /// stamped with its frame offset (the list's base offset is 0, so a
    /// packet's timestamp is its offset: `MusicDevice.h`). A SysEx needs no
    /// reassembly and no longer lands ahead of the notes that precede it. A list
    /// holds one protocol, so a run of MIDI 1.0 channel voice packets and a run
    /// of MIDI 2.0 ones go as separate calls in order, and a list is sent when
    /// it is full or its time would run back. The list is built in a buffer
    /// reserved at activation. Utility (JR included), Flex Data and UMP Stream
    /// messages are the endpoint's, not music, and are not sent. What the unit
    /// does with a Group is the unit's: AUv2 has no cable or port to route one
    /// by, so tutti sends the Group it was given and does not drop another.
    ///
    /// # Every other unit: legacy MIDI 1.0 bytes
    ///
    /// A unit that reports MIDI 1.0, or does not say (macOS before 12), is sent
    /// `MusicDeviceMIDIEvent` calls, as before: the framework would narrow a
    /// UMP list to the same bytes, and the narrowing here is tutti's own
    /// (`midi1`: a Note On never at velocity 0, a banked Program Change as CC 0,
    /// CC 32, then the program), SysEx7 is reassembled once for
    /// `MusicDeviceSysEx`, and the path works on every macOS. A MIDI 1.0 call
    /// has no Group, so only Group 0's channels are played: a channel voice
    /// message of another Group is dropped and counted
    /// ([`groups_dropped`](Self::groups_dropped)), never heard on Group 0's
    /// channels (M2-104 section 2.1.2), as VST2 does. A message with no MIDI 1.0
    /// form (per-note controllers, per-note management) is dropped and counted
    /// ([`unsendable_dropped`](Self::unsendable_dropped)). SysEx goes through
    /// `MusicDeviceSysEx` once reassembled in the pre-sized buffer; a message
    /// past its capacity is dropped whole and counted
    /// ([`sysex_dropped`](Self::sysex_dropped)). A run left open at the end of a
    /// block continues in the next.
    pub fn send_midi(&mut self, events: &[UmpAt]) {
        match self.midi.wire {
            Some(Wire::Midi2) => self.send_ump(events),
            _ => self.send_legacy(events),
        }
    }

    /// The protocol the unit asked for, read at activation: `Some(Midi2)` takes
    /// UMP, anything else the legacy calls.
    pub fn midi_protocol(&self) -> Option<Wire> {
        self.midi.wire
    }

    fn send_ump(&mut self, events: &[UmpAt]) {
        let unit = &self.unit;
        stage_ump(&mut self.midi.list, events, |list| {
            // The unit's status has nowhere to go on the audio thread.
            let _ = unit.midi_event_list(list);
        });
    }

    fn send_legacy(&mut self, events: &[UmpAt]) {
        use tutti_midi_protocol::midi1;
        use tutti_ump::{Message, SysexStep};

        self.sysex.begin_block();

        let unit = &self.unit;
        for &(offset, ev) in events {
            match ev.decode() {
                Ok(message @ (Message::Midi1(address, _) | Message::Midi2(address, _))) => {
                    if address.group.get() != 0 {
                        self.midi.groups_dropped += 1;
                        continue;
                    }
                    let mut sent = false;
                    for bytes in midi1(message) {
                        sent = true;
                        let [status, d1, d2] = bytes.padded();
                        let _ = unit.midi_event(status, d1, d2, offset.get());
                    }
                    if !sent {
                        self.midi.unsendable_dropped += 1;
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

    /// Channel voice messages dropped since activation because they were of a
    /// Group other than 0 on the legacy path (see [`send_midi`](Self::send_midi)).
    /// Zero on a unit that takes MIDI 2.0, which is sent the Group.
    pub fn groups_dropped(&self) -> u64 {
        self.midi.groups_dropped
    }

    /// Channel voice messages dropped since activation because MIDI 1.0 has no
    /// form for them (per-note controllers, per-note management) on the legacy
    /// path. Zero on a unit that takes MIDI 2.0.
    pub fn unsendable_dropped(&self) -> u64 {
        self.midi.unsendable_dropped
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
        // Automation arrives normalized `0..=1`, but the AU takes NATIVE PLAIN
        // UNITS. Writing the normalized value straight through set Apple
        // AUDelay's Lowpass Cutoff (declared `[10, 22050]` Hz) to 1 Hz at full
        // scale. Denormalize against the range the AU itself declared; a
        // parameter missing from the table is skipped rather than written
        // blind: a guessed range would be the same class of bug.
        //
        // Each point is staged as an immediate event at its offset, and the
        // render notify schedules them from inside `AudioUnitRender`, the one
        // place `AudioUnitScheduleParameters` is valid. A parameter the AU
        // flagged `NonRealTime` is not written here at all: its latest value
        // goes to the control side, which applies it between blocks.
        self.stage.borrow_mut().begin_block();
        if let Some(changes) = ctx.param_changes {
            let table = self.ranges.lend();
            let scheduled = self.rendering.notify.is_some();
            for queue in &changes.queues {
                let id = queue.param_id.get();
                let (Some(bounds), Some(slot), Some(last)) =
                    (table.bounds(id), table.slot(id), queue.points.last())
                else {
                    continue;
                };
                let plain = bounds.plain(last.value) as f32;
                if table.is_non_realtime(id) {
                    // The latest value, marked for the tick: a parameter moved
                    // in many blocks is written once, and nothing is dropped.
                    table.marks().defer(slot, plain);
                } else if !(scheduled && self.stage.borrow_mut().stage(queue, bounds)) {
                    // No notify, or no room left in the stage: the block's last
                    // point at its head, as a host that cannot place points.
                    // `AudioUnitSetParameter` applies it without notifying
                    // anyone; the control tick tells the listeners.
                    let _ = crate::parameters::set(&self.unit, id, plain);
                    table.marks().tell(slot);
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
        // The scheduled events do not reach the unit's listeners (the plugin's
        // editor among them): only `AUParameterSet` and
        // `AUParameterListenerNotify` do (`AudioUnitUtilities.h`), and both
        // may take locks and dispatch to other queues, so neither is real-time
        // safe and neither runs here. Each staged parameter is marked in the
        // table's `Marks`, and `AuController::apply_deferred` (the tick)
        // notifies the listeners then. It notifies and writes nothing, since a
        // later block may have moved the parameter on and a write of this
        // block's end would undo it; the listeners read the unit's value.
        // Only staged parameters are here: a `NonRealTime` one was marked for a
        // write above (made on the tick), and one set at the head (no notify,
        // or no room) was marked there, so no parameter is told twice for a
        // block. The table is lent again rather than held across the render.
        let stage = self.stage.borrow();
        if !stage.ends().is_empty() {
            let table = self.ranges.lend();
            for &(id, _) in stage.ends() {
                if let Some(slot) = table.slot(id) {
                    table.marks().tell(slot);
                }
            }
        }
        drop(stage);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::find_component;
    use crate::instance::AuLoaded;
    use crate::types::{AudioComponentDescription, K_AUDIO_UNIT_TYPE_EFFECT};
    use tutti_plugin_types::{
        AudioBuffer, Loaded, MainThread, MaxBlock, ParamId, ParameterChanges, Prepare, SampleRate,
    };

    fn delay() -> (AuProcessor, crate::controller::AuController) {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = find_component(&desc).expect("AUDelay ships with macOS");
        let au = AuLoaded::new(comp, 48_000.0, 512).unwrap();
        let prepare = Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(tutti_types::Samples(512)),
        )
        .unwrap();
        au.activate(&MainThread::claim().unwrap(), prepare)
            .unwrap_or_else(|e| panic!("AUDelay activates: {}", e.reason))
    }

    /// A block's automation points are staged and then handed to the unit from
    /// its pre-render notify: after the render nothing is left staged, though
    /// both points were (the stage's ends say so). A host that wrote the last
    /// point at the head would stage nothing, and one whose notify did not run
    /// would leave both events behind (`AUComponent.h`: the events are
    /// "scheduled as a part of the pre-render notification callback").
    #[test]
    fn the_block_points_are_scheduled_by_the_pre_render_notify() {
        let (mut processor, _controller) = delay();
        let mut changes = ParameterChanges::reserved(1, 4);
        assert!(changes.add_change(ParamId::from(0), 3, 0.25));
        assert!(changes.add_change(ParamId::from(0), 25, 0.75));
        let ctx = ProcessContext::new().params(&changes);

        let input = vec![vec![0.0f32; 64]; 2];
        let mut output = vec![vec![0.0f32; 64]; 2];
        let inputs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut out = ProcessOutput::default();
        let mut buffer = AudioBufferMut::F32(AudioBuffer {
            inputs: &inputs,
            outputs: &mut outputs,
            num_samples: 64,
            sample_rate: 48_000.0,
        });
        processor.render(&mut buffer, &ctx, &mut out).unwrap();

        let stage = processor.stage.borrow();
        assert_eq!(
            stage.ends(),
            [(0, 75.0)],
            "the parameter was staged, ending on the last point's plain value"
        );
        assert_eq!(stage.pending(), 0, "the notify handed the events over");
        let notify = processor
            .rendering
            .notify
            .as_ref()
            .expect("AUDelay takes it");
        assert_eq!(notify.deliveries(), 2, "one pre and one post per render");
    }
}

#[cfg(test)]
mod ump_staging {
    use super::*;
    use tutti_graph::Offset;
    use tutti_plugin_types::Samples;
    use tutti_ump::{Message, MidiChannel, MidiGroup, Ump};

    fn at(frame: u32, ump: Ump) -> UmpAt {
        (
            Offset::new(frame as usize, Samples(4096)).expect("inside the block"),
            ump,
        )
    }

    fn note_on(group: u8, key: u8) -> Ump {
        // MIDI 2.0 Note On: type 4, status 0x9, velocity 0xFFFF.
        Ump([
            0x4000_0000 | u32::from(group) << 24 | 0x90 << 16 | u32::from(key) << 8,
            0xFFFF_0000,
            0,
            0,
        ])
    }

    fn m1_note_on(group: u8, key: u8) -> Ump {
        Ump([
            0x2000_0000 | u32::from(group) << 24 | 0x90 << 16 | u32::from(key) << 8 | 0x40,
            0,
            0,
            0,
        ])
    }

    /// Every list `stage_ump` sends, as (protocol word, packets as (time, words)).
    fn staged(list: &mut EventList, events: &[UmpAt]) -> Vec<(u32, Vec<(u32, Vec<u32>)>)> {
        let mut sent = Vec::new();
        stage_ump(list, events, |l| {
            let w = l.as_words();
            let mut packets = Vec::new();
            let mut i = 2;
            for _ in 0..w[1] {
                let n = w[i + 2] as usize;
                packets.push((w[i], w[i + 3..i + 3 + n].to_vec()));
                i += 3 + n;
            }
            sent.push((w[0], packets));
        });
        sent
    }

    /// **UMP goes as it is, Group and frame offset included**: a MIDI 2.0 Note On
    /// of Group 5 is one packet, its words untouched, stamped with its offset.
    #[test]
    fn a_ump_packet_goes_unchanged_with_its_group_and_offset() {
        let mut list = EventList::with_capacity(64);
        let ev = note_on(5, 60);
        let sent = staged(&mut list, &[at(37, ev)]);
        assert_eq!(sent, vec![(2, vec![(37, ev.words().to_vec())])]);
        assert_eq!(
            ev.decode().ok().and_then(|m| match m {
                Message::Midi2(a, _) => Some((a.group, a.channel)),
                _ => None,
            }),
            Some((MidiGroup::of(5), MidiChannel::FIRST)),
            "the fixture is a Group 5 message"
        );
    }

    /// **A list holds one protocol**: MIDI 1.0 and MIDI 2.0 channel voice go as
    /// separate lists in order, and a SysEx7 packet rides in the list it
    /// follows, at its own frame (no reassembly, no reordering).
    #[test]
    fn protocols_split_into_lists_and_sysex_rides_along() {
        let mut list = EventList::with_capacity(64);
        let sysex = Ump([0x3016_0102, 0x0300_0000, 0, 0]);
        let sent = staged(
            &mut list,
            &[
                at(0, m1_note_on(0, 60)),
                at(4, sysex),
                at(8, note_on(0, 62)),
            ],
        );
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, 1);
        assert_eq!(sent[0].1.iter().map(|p| p.0).collect::<Vec<_>>(), [0, 4]);
        assert_eq!(sent[1].0, 2);
        assert_eq!(sent[1].1.iter().map(|p| p.0).collect::<Vec<_>>(), [8]);
    }

    /// **A full list is sent and a new one started, nothing lost**: capacity 16
    /// words holds two Note Ons (5 words each) and the third starts a list.
    #[test]
    fn a_full_list_is_sent_and_the_block_continues() {
        let mut list = EventList::with_capacity(16);
        let events: Vec<UmpAt> = (0..5).map(|i| at(i, note_on(0, 60 + i as u8))).collect();
        let sent = staged(&mut list, &events);
        let frames: Vec<u32> = sent.iter().flat_map(|l| l.1.iter().map(|p| p.0)).collect();
        assert_eq!(frames, [0, 1, 2, 3, 4]);
        assert!(sent.len() > 1);
        assert!(list.is_empty());
    }

    /// **Utility, Flex and Stream messages are not music** and are not sent; a
    /// block with nothing to send sends nothing.
    #[test]
    fn non_music_messages_are_not_sent() {
        let mut list = EventList::with_capacity(64);
        let jr_clock = Ump([0x0010_0000, 0, 0, 0]);
        let stream = Ump([0xF000_0000, 0, 0, 0]);
        assert!(staged(&mut list, &[at(0, jr_clock), at(0, stream)]).is_empty());
    }
}
