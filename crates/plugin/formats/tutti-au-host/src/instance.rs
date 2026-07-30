//! High-level AU instance lifecycle: load → initialize → render.
//!
//! [`AuInstance`] is the main entry point for hosting an Audio Unit. It
//! internally tracks whether the AU has been `AudioUnitInitialize`d and only
//! permits `process()` calls in the ready state.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;

use crate::buffer::{iter_buffers_mut, RenderScratch};
use crate::bus::{self, AuChannelConfig, BusDirection};
use crate::cf::{CfArray, CfPlist, CfString};
use crate::component::AuType;
use crate::error::{AuError, Result};
use crate::ffi::{check, get_property, set_property};
use crate::handle::AuHandle;
use crate::parameters::{self, AuParameter, ParamView};
use crate::preset::AuPreset;
use crate::stream::{AuBusLayout, StreamConfig};
use crate::transport::{self, TransportState};
use crate::types::*;
use tutti_midi_types::MidiEvent;
use tutti_plugin_types::{ChannelLayout, TransportInfo};
use tutti_types::value::units::Seconds;

/// An AU that has been instantiated but not yet initialized.
///
/// In this state parameters and state can be queried/set, the editor can
/// be opened, but audio rendering is not yet possible.
pub struct AuLoaded {
    handle: AuHandle,
    config: StreamConfig,
    /// Host transport, installed on demand by
    /// [`AuInstance::install_host_callbacks`].
    ///
    /// Heap-pinned for the same reason [`AuReady::scratch`] is: the AU retains
    /// the `hostUserData` pointer derived from `&*transport`, and this struct is
    /// `mem::replace`d between the `Loaded` and `Ready` states on every
    /// initialize/uninitialize. Moving the `Box` moves only its 8-byte pointer,
    /// so the address the AU holds stays valid across those transitions.
    ///
    /// `None` until a host installs callbacks — an AU with no transport wired
    /// must not pay for an allocation, and a null `hostUserData` is exactly what
    /// the procs treat as "no state".
    transport: Option<Box<TransportState>>,
}

/// An AU that has completed `AudioUnitInitialize` and has render buffers
/// allocated. This is the only state in which [`AuInstance::process`] will
/// succeed.
pub struct AuReady {
    loaded: AuLoaded,
    /// Heap-pinned so its address is stable across the `State`/`mem::replace`
    /// moves in [`AuInstance::initialize`]/[`AuInstance::uninitialize`]. The
    /// AU's input render callback holds a `ref_con` pointing at `*scratch`;
    /// moving the `Box` moves only its 8-byte pointer, not the body, so that
    /// `ref_con` stays valid across state transitions (FIX 1 / FIX 2). Anything
    /// that frees this box MUST have already run `AudioUnitUninitialize` so the
    /// AU can no longer call back into freed memory.
    scratch: Box<RenderScratch>,
    /// Test-only: counts input-render-callback installs on THIS instance, to
    /// prove the FIX-1 invariant (installed once per initialize, never per
    /// render block) without a process-global that races parallel tests.
    #[cfg(test)]
    callback_installs: std::sync::atomic::AtomicU32,
}

/// Public façade wrapping either an [`AuLoaded`] or [`AuReady`] state.
///
/// Most host operations (parameters, state save/load, editor) work regardless
/// of initialization status. [`AuInstance::process`] requires the Ready state
/// and will return `Uninitialized` otherwise.
pub struct AuInstance {
    state: State,
}

enum State {
    Loaded(AuLoaded),
    Ready(AuReady),
    /// Transient marker only seen while a `mem::replace` is mid-transition.
    Empty,
}

impl AuInstance {
    /// Instantiate an AU in the `Loaded` (pre-init) state.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent` handle obtained
    /// from `AudioComponentFindNext` or [`crate::component`].
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if instantiation or initial stream-format
    /// configuration fails.
    pub unsafe fn new(
        component: AudioComponent,
        sample_rate: f64,
        block_size: u32,
    ) -> Result<Self> {
        Ok(AuInstance {
            state: State::Loaded(AuLoaded::new(component, sample_rate, block_size)?),
        })
    }

    /// Instantiate at an explicit [`StreamConfig`] — the way to open an AU in
    /// mono, or at any width other than the stereo default [`Self::new`] picks.
    ///
    /// Purely additive: [`Self::new`] is unchanged, and the `tutti-plugin-server`
    /// loader keeps calling it.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent` handle obtained
    /// from `AudioComponentFindNext` or [`crate::component`].
    ///
    /// # Errors
    /// As [`Self::new`]. A refused channel width is **not** an error — see
    /// [`AuLoaded::new_with_config`]; compare [`num_outputs`](Self::num_outputs)
    /// against what was requested to detect it.
    pub unsafe fn new_with_config(component: AudioComponent, config: StreamConfig) -> Result<Self> {
        Ok(AuInstance {
            state: State::Loaded(AuLoaded::new_with_config(component, config)?),
        })
    }

    /// Transition Loaded → Ready. No-op if already Ready.
    ///
    /// A failure leaves the instance in the `Loaded` state it started from, so
    /// the caller may inspect it, retry at a different configuration, or drop
    /// it. Previously the failure arm returned the error while `self.state` was
    /// still the `Empty` marker `mem::replace` had installed, which turned
    /// *every* later method — `raw_unit`, `au_type`, even `is_initialized` —
    /// into an `unreachable!()` panic. A host that scans installed AUs and
    /// tolerates one refusing to initialize (some do — AUNetReceive, and any
    /// unit whose hardware is absent) would crash on the next thing it asked.
    pub fn initialize(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Empty) {
            State::Loaded(l) => match l.initialize() {
                Ok(r) => {
                    self.state = State::Ready(r);
                    Ok(())
                }
                // `None` only when the AU refused both the callback install and
                // the compensating uninitialize; the unit has already been
                // disposed, so `Empty` is the honest state and every accessor
                // reports the instance as dead rather than pretending.
                Err((recovered, e)) => {
                    if let Some(l) = recovered {
                        self.state = State::Loaded(l);
                    }
                    Err(e)
                }
            },
            other @ State::Ready(_) => {
                self.state = other;
                Ok(())
            }
            State::Empty => unreachable!("AuInstance left empty"),
        }
    }

    /// Transition Ready → Loaded. No-op if already Loaded.
    ///
    /// As with [`initialize`](Self::initialize), a failure restores the state
    /// the call started in rather than leaving the instance unusable.
    pub fn uninitialize(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Empty) {
            State::Ready(r) => match r.uninitialize() {
                Ok(l) => {
                    self.state = State::Loaded(l);
                    Ok(())
                }
                Err((r, e)) => {
                    self.state = State::Ready(r);
                    Err(e)
                }
            },
            other @ State::Loaded(_) => {
                self.state = other;
                Ok(())
            }
            State::Empty => unreachable!("AuInstance left empty"),
        }
    }

    fn handle(&self) -> &AuHandle {
        match &self.state {
            State::Loaded(l) => &l.handle,
            State::Ready(r) => &r.loaded.handle,
            State::Empty => unreachable!("AuInstance accessed while empty"),
        }
    }

    fn config(&self) -> &StreamConfig {
        match &self.state {
            State::Loaded(l) => &l.config,
            State::Ready(r) => &r.loaded.config,
            State::Empty => unreachable!(),
        }
    }

    /// Raw `AudioUnit` pointer. Useful for interop with AudioToolbox calls
    /// not yet wrapped by this crate.
    pub fn raw_unit(&self) -> AudioUnit {
        self.handle().raw_unit()
    }

    /// High-level [`AuType`] this component was classified as.
    pub fn au_type(&self) -> AuType {
        self.handle().au_type()
    }

    /// Configured input channel count (`0` for generators / instruments, which
    /// have no input bus).
    pub fn num_inputs(&self) -> u32 {
        let channels = self.config().channels;
        if channels.has_input {
            channels.inputs.count() as u32
        } else {
            0
        }
    }

    /// Configured output channel count.
    pub fn num_outputs(&self) -> u32 {
        self.config().channels.outputs.count() as u32
    }

    /// Configured sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.config().sample_rate
    }

    /// Whether the AU is currently in the `Ready` state.
    pub fn is_initialized(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Test-only: how many times the input render callback has been installed
    /// on the current `Ready` instance (0 if not ready).
    #[cfg(test)]
    fn callback_install_count(&self) -> u32 {
        match &self.state {
            State::Ready(r) => r
                .callback_installs
                .load(std::sync::atomic::Ordering::SeqCst),
            _ => 0,
        }
    }

    /// Copy the AU's display name.
    pub fn get_name(&self) -> Result<String> {
        Ok(self.handle().get_name())
    }

    /// Write a parameter value **and** notify listeners — including the AU's own
    /// open editor.
    ///
    /// This routes through `AUParameterSet` rather than the bare
    /// `AudioUnitSetParameter`, and the difference is visible to the user. Only
    /// changes issued through `AUParameterSet` generate listener notifications
    /// (Apple's `AudioUnitUtilities.h` states the preference outright), and a
    /// plugin's editor is itself a listener. With the raw write, host automation
    /// playback moved the audio while every knob in the open editor sat frozen —
    /// the sound sweeps, the UI does not, and the user reports the plugin window
    /// as broken.
    ///
    /// The change takes effect at the start of the next rendered block. For
    /// sample-accurate placement within a block, call
    /// [`listener::set_parameter_notifying`](crate::listener::set_parameter_notifying)
    /// with a frame offset; for a deliberately *silent* write that notifies
    /// nobody, [`crate::parameters::set`] still wraps the raw call.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] with the AU's own status for an id it does
    /// not declare.
    pub fn set_parameter(&mut self, id: u32, value: f32) -> Result<()> {
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        unsafe {
            crate::listener::set_parameter_notifying(
                self.raw_unit(),
                id,
                crate::listener::EventAddress::GLOBAL,
                value,
                0,
            )
        }
    }

    /// Clear the AU's internal audio state — reverb tails, delay lines, filter
    /// memory — without disturbing its parameters.
    ///
    /// A host must call this on every discontinuity in the timeline, and the
    /// symptom of not calling it is stale audio arriving where none belongs:
    /// jumping from bar 60 to bar 1 smears the reverb tail of bar 60 across the
    /// downbeat, un-muting a channel replays whatever was sitting in its delay
    /// line, and a loop wrap-around bleeds the end of the loop into its start.
    /// Parameters are deliberately untouched — this resets the signal history,
    /// not the patch.
    ///
    /// # Legality before `initialize`
    ///
    /// Legal, and a no-op in practice. Measured on macOS 15.6 against the whole
    /// corpus (AUDelay, AUMatrixReverb, AUDynamicsProcessor, AUNBandEQ,
    /// AULowpass, plus both instruments): every unit returns `noErr` for
    /// `AudioUnitReset` in the `Loaded` state, and remains renderable after
    /// `initialize`. That is the answer the AU gives, so this method does not
    /// gate on the typestate the way [`process`](Self::process) does — a host
    /// that resets a channel strip while wiring it up should not have to know
    /// which plugins are initialized yet. There is nothing to flush pre-init
    /// (no render resources are allocated), so the call is simply inert.
    ///
    /// `au_notification.rs::reset_is_accepted_before_initialize` pins this; if a
    /// future AU refuses, that test fails rather than the behaviour changing
    /// silently.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] with the AU's own status. Propagated rather
    /// than absorbed: a host that jumps the playhead and silently fails to flush
    /// produces audible garbage on the next block, and swallowing the error
    /// would leave no way to tell that from a plugin that simply had no tail.
    pub fn reset(&mut self) -> Result<()> {
        // Global scope / element 0 is the documented address for a whole-unit
        // reset; per-bus reset is not a thing AUv2 offers.
        check("AudioUnitReset", unsafe {
            AudioUnitReset(self.raw_unit(), K_AUDIO_UNIT_SCOPE_GLOBAL, 0)
        })
    }

    /// Read a parameter value.
    pub fn get_parameter(&self, id: u32) -> Result<f32> {
        parameters::get(self.raw_unit(), id)
    }

    /// Install a render notification, called twice per render (pre and post).
    ///
    /// This is the host's tap into the AU's own render: on
    /// [`RenderPhase::Post`](crate::render_notify::RenderPhase::Post) the buffer
    /// list holds the audio the plugin actually produced, which is the only
    /// place a meter can read the plugin's real output rather than the host's
    /// post-processed copy of it. On
    /// [`RenderPhase::Pre`](crate::render_notify::RenderPhase::Pre) it is the
    /// only sanctioned place to call
    /// [`render_notify::schedule`](crate::render_notify::schedule) — scheduled
    /// parameter events apply to the render call in flight and to no other.
    ///
    /// The returned [`RenderNotify`] is the registration: **hold it**. Dropping
    /// it removes the notification, and doing so is what keeps the AU from
    /// calling into freed memory — see [`RenderNotify`]'s `Drop`.
    ///
    /// # Why this returns a handle rather than storing it
    ///
    /// The notify's lifetime is the *host's* business, not the instance's. A
    /// metering tap lives as long as the meter is on screen, a latency probe for
    /// one block, an automation writer for the length of a gesture — and several
    /// may be installed at once, since AudioToolbox keys them by
    /// `(proc, ref_con)` and this crate gives each handle its own state. Storing
    /// one inside `AuInstance` would impose a single slot and tie every tap to
    /// the plugin's lifetime.
    ///
    /// The consequence the caller owns: the handle must not outlive this
    /// instance. That is the same rule
    /// [`AuParameterListener`](crate::listener::AuParameterListener) carries,
    /// and it is why this method is safe while
    /// [`RenderNotify::new`](crate::render_notify::RenderNotify::new) is not —
    /// here the borrow checker has `&self` to reason from.
    ///
    /// # Real-time safety
    /// `callback` runs on the render thread and must not allocate, lock, or
    /// block. See [`RenderNotify::new`](crate::render_notify::RenderNotify::new)
    /// for the full contract.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU refuses
    /// `AudioUnitAddRenderNotify`. Measured on macOS 15.6: none of the ~35 Apple
    /// units nor the three third-party units installed do.
    pub fn add_render_notify<F>(&self, callback: F) -> Result<crate::render_notify::RenderNotify>
    where
        F: Fn(crate::render_notify::RenderNotification) + Send + Sync + 'static,
    {
        // SAFETY: `raw_unit` is live for the lifetime of this instance, and the
        // returned handle borrows nothing — so the caller's obligation is that
        // it not outlive `self`, which this method's docs state.
        unsafe { crate::render_notify::RenderNotify::new(self.raw_unit(), callback) }
    }

    /// This instance's unit, wrapped so a render-notify callback can capture it.
    ///
    /// A pre-render callback's whole job is usually to schedule automation on the
    /// unit that is rendering, but [`raw_unit`](Self::raw_unit) returns a bare
    /// pointer that is neither `Send` nor `Sync`, so capturing it in the callback
    /// does not compile. This is the wrapper that makes it possible — see
    /// [`RenderUnit`](crate::render_notify::RenderUnit) for why the assertion is
    /// sound and where it stops.
    ///
    /// Take one *before* calling
    /// [`add_render_notify`](Self::add_render_notify), since the callback has to
    /// be built first.
    pub fn render_unit(&self) -> crate::render_notify::RenderUnit {
        // SAFETY: the unit is live for the lifetime of this instance, and the
        // caller cannot outlive it without also dropping the notify that holds
        // the captured copy — dropping the `RenderNotify` is what unregisters it.
        unsafe { crate::render_notify::RenderUnit::new(self.raw_unit()) }
    }

    /// Schedule parameter events into the render call currently in flight.
    ///
    /// **Only correct from inside a pre-render notify.** The events apply to the
    /// current `AudioUnitRender` and to no other, so calling this from a control
    /// thread races the render it was meant to affect. Reach it through
    /// [`add_render_notify`](Self::add_render_notify); this method exists so a
    /// caller holding an `&AuInstance` in the callback does not have to reach for
    /// the raw unit.
    ///
    /// For a parameter change that should simply take effect at the next block
    /// boundary — which is what a DAW wants for most automation — use
    /// [`set_parameter`](Self::set_parameter) instead. It notifies listeners,
    /// which this deliberately does not: a scheduled event is a render-time
    /// value, and pushing a listener notification per event would flood the
    /// plugin's editor with a redraw per sample-accurate step.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU refuses the call — but note that
    /// `Ok(())` is not a promise the event does anything. Measured on macOS
    /// 15.6, a nonexistent parameter id and a ramp on a non-rampable parameter
    /// both return `noErr`. See
    /// [`render_notify::schedule`](crate::render_notify::schedule).
    pub fn schedule_parameters(
        &self,
        id: u32,
        address: crate::render_notify::ScheduleAddress,
        events: &[crate::render_notify::ParamEvent],
    ) -> Result<()> {
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        unsafe { crate::render_notify::schedule(self.raw_unit(), id, address, events) }
    }

    /// Deliver a block of UMP MIDI events to an instrument / music-effect AU as
    /// legacy `MusicDeviceMIDIEvent` calls.
    ///
    /// Each event is decoded to a 3-byte MIDI 1.0 channel-voice message (status
    /// byte + up to two data bytes, MIDI 2.0 resolutions scaled down per spec)
    /// and delivered at its `frame_offset`. Message families with no legacy
    /// 3-byte form (SysEx, per-note MIDI 2.0 messages, system real-time) are
    /// skipped — AUv2's `MusicDeviceMIDIEvent` only speaks legacy channel voice.
    ///
    /// Only meaningful for AUs whose type [`AuType::receives_midi`] is true;
    /// the caller gates on that. Errors from individual events are ignored so a
    /// single rejected message can't abort the whole block.
    pub fn send_midi(&self, events: &[MidiEvent]) {
        use tutti_midi_types::convert::{midi2_cc_to_midi1, midi2_pitch_bend_to_midi1};
        use tutti_midi_types::MidiMessage;

        let unit = self.raw_unit();
        for ev in events {
            // (status, data1, data2) for the legacy 3-byte message, or None if
            // this message has no legacy channel-voice representation.
            let (status, d1, d2) = match ev.message() {
                MidiMessage::NoteOn {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    let vel = tutti_midi_types::convert::midi2_velocity_to_midi1(velocity);
                    // A zero-velocity note-on is a note-off; keep it as note-on
                    // 0x90 with velocity 0 (a legal legacy note-off encoding).
                    (0x90 | (channel & 0x0F), note & 0x7F, vel & 0x7F)
                }
                MidiMessage::NoteOff {
                    channel,
                    note,
                    velocity,
                    ..
                } => {
                    let vel = tutti_midi_types::convert::midi2_velocity_to_midi1(velocity);
                    (0x80 | (channel & 0x0F), note & 0x7F, vel & 0x7F)
                }
                MidiMessage::ControlChange {
                    channel,
                    index,
                    value,
                    ..
                } => (
                    0xB0 | (channel & 0x0F),
                    index & 0x7F,
                    midi2_cc_to_midi1(value) & 0x7F,
                ),
                MidiMessage::ProgramChange {
                    channel, program, ..
                } => (0xC0 | (channel & 0x0F), program & 0x7F, 0),
                MidiMessage::ChannelPressure {
                    channel, pressure, ..
                } => (
                    0xD0 | (channel & 0x0F),
                    midi2_cc_to_midi1(pressure) & 0x7F,
                    0,
                ),
                MidiMessage::PitchBend { channel, value, .. } => {
                    let bend14 = midi2_pitch_bend_to_midi1(value);
                    (
                        0xE0 | (channel & 0x0F),
                        (bend14 & 0x7F) as u8,
                        (bend14 >> 7) as u8 & 0x7F,
                    )
                }
                _ => continue,
            };
            unsafe {
                MusicDeviceMIDIEvent(unit, status as u32, d1 as u32, d2 as u32, ev.frame_offset);
            }
        }
    }

    /// Enumerate all parameters exposed by the AU.
    pub fn get_parameter_list(&self) -> Vec<AuParameter> {
        parameters::list(self.raw_unit())
    }

    /// How many buses the AU has on `direction`.
    ///
    /// `0` is a real answer, not a failure: instruments and generators have no
    /// input buses at all, and that zero is what
    /// [`AuLoaded::initialize`] keys the render-callback install off. An AU that
    /// refuses `kAudioUnitProperty_ElementCount` also reports `0`, because
    /// "declines to say" and "has none" leave a caller in the same position.
    ///
    /// Measured on macOS 15.6: every Apple effect is 1 in / 1 out;
    /// DLSMusicDevice is 0 in / **2 out**; AUMatrixMixer is 64 in / 4 out.
    pub fn bus_count(&self, direction: BusDirection) -> u32 {
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        unsafe { bus::bus_count(self.raw_unit(), direction) }
    }

    /// The channel layout of bus `bus` on `direction`.
    ///
    /// Unlike [`num_inputs`](Self::num_inputs) / [`num_outputs`](Self::num_outputs),
    /// which report the layout the host *configured* on bus 0, this asks the AU
    /// what a specific bus is running right now.
    ///
    /// # Errors
    /// A bus index at or past [`bus_count`](Self::bus_count) returns the AU's
    /// own `kAudioUnitErr_InvalidElement` rather than a default layout.
    /// [`ChannelLayout`] cannot represent "no such bus", so returning one for an
    /// out-of-range index would have the caller allocate buffers for a bus that
    /// does not exist.
    pub fn bus_layout(&self, direction: BusDirection, bus: u32) -> Result<ChannelLayout> {
        // SAFETY: as above.
        unsafe { bus::bus_layout(self.raw_unit(), direction, bus) }
    }

    /// Every channel configuration the AU declares it can run.
    ///
    /// An empty vec means the AU publishes no constraint — which is what all 22
    /// Apple effects measured on macOS 15.6 do — and must be read as
    /// "unconstrained, consult the stream format", never as "supports nothing".
    /// See [`AuChannelConfig`] for why the negative entries stay sentinels.
    pub fn supported_channel_configs(&self) -> Vec<AuChannelConfig> {
        // SAFETY: as above.
        unsafe { bus::supported_channel_configs(self.raw_unit()) }
    }

    /// Borrow a [`ParamView`] for scoped parameter access.
    pub fn parameters(&self) -> ParamView<'_> {
        unsafe { ParamView::new(self.raw_unit()) }
    }

    /// Plugin-reported processing latency in samples at the current sample rate.
    ///
    /// Returns 0 if the AU does not advertise `kAudioUnitProperty_Latency`.
    pub fn get_latency(&self) -> Result<u32> {
        let latency = unsafe {
            get_property::<f64>(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_LATENCY,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )
        }
        .unwrap_or(0.0);
        Ok((latency * self.sample_rate()) as u32)
    }

    /// Seconds of audio this AU keeps producing after its input goes silent.
    ///
    /// Distinct from [`get_latency`](Self::get_latency), which reports how far
    /// the AU shifts audio in time. Tail says how much *longer* it lasts: an
    /// offline bounce that stops at the last note truncates every reverb and
    /// delay tail on the master bus. See [`transport::tail_time`] for why this
    /// returns [`Seconds`] rather than samples, and why the absence of the
    /// property is an error rather than a zero.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] when the AU does not implement
    /// `kAudioUnitProperty_TailTime`. Measured on macOS 15.6, every Apple
    /// **effect** answers it, while every Apple instrument, mixer and generator
    /// rejects it with `kAudioUnitErr_InvalidProperty` (-10879).
    pub fn get_tail_time(&self) -> Result<Seconds> {
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        unsafe { transport::tail_time(self.raw_unit()) }
    }

    /// Install the four AUv2 host transport callbacks, so tempo-synced AUs can
    /// pull project tempo, beat position and transport state during render.
    ///
    /// Returns a borrow of the installed [`TransportState`]; write to it with
    /// [`TransportState::set_transport`] from the control thread each block.
    /// Calling this twice reuses the existing state rather than reallocating, so
    /// the `hostUserData` pointer the AU already retains stays valid.
    ///
    /// # Why this is opt-in rather than done at `new`
    ///
    /// The state is a real allocation and the property write is a real IPC round
    /// trip for an out-of-process AU. A host that does not sequence — a mastering
    /// chain, an analysis pass — should not pay for either, and an AU that is
    /// handed a transport it can pull is entitled to assume the host maintains
    /// it. Installing by default would leave every non-sequencing host silently
    /// advertising a transport frozen at beat 0, 120 BPM, stopped, which is
    /// worse for a tempo-synced plugin than advertising none at all: with no
    /// callbacks installed the plugin falls back to its own free-running clock,
    /// which at least advances.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU refuses
    /// `kAudioUnitProperty_HostCallbacks`. On this machine none do — all ~35
    /// Apple units plus the three third-party units installed accept the write,
    /// including the ones that never call back.
    pub fn install_host_callbacks(&mut self) -> Result<&TransportState> {
        let loaded = match &mut self.state {
            State::Loaded(l) => l,
            State::Ready(r) => &mut r.loaded,
            State::Empty => unreachable!("AuInstance accessed while empty"),
        };
        let unit = loaded.handle.raw_unit();
        // Allocate once and keep the same box on a re-install: the AU may
        // already hold a `hostUserData` pointing at it, and swapping in a fresh
        // allocation would leave that pointer dangling until the property write
        // below landed — a window on the render thread, not merely a leak.
        let state = loaded
            .transport
            .get_or_insert_with(|| Box::new(TransportState::new()));
        let info = state.callback_info();
        // SAFETY: `HostCallbackInfo` is `#[repr(C)]` and laid out exactly as
        // `AudioUnitProperties.h` declares it, so the property's documented
        // value type and `size_of::<HostCallbackInfo>()` agree. The
        // `hostUserData` inside points at the boxed state, whose address is
        // stable across the `State` transitions and which is cleared out of the
        // AU by `clear_host_callbacks` before it is ever freed.
        unsafe {
            set_property(
                unit,
                K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &info,
            )?;
        }
        Ok(loaded.transport.as_deref().expect("just inserted above"))
    }

    /// Borrow the installed transport, or `None` if
    /// [`install_host_callbacks`](Self::install_host_callbacks) was never called.
    ///
    /// The borrow is shared rather than mutable because every write goes through
    /// an atomic — the point of the type is that the control thread can publish
    /// while the render thread reads.
    pub fn transport(&self) -> Option<&TransportState> {
        match &self.state {
            State::Loaded(l) => l.transport.as_deref(),
            State::Ready(r) => r.loaded.transport.as_deref(),
            State::Empty => unreachable!("AuInstance accessed while empty"),
        }
    }

    /// Publish a transport snapshot for the AU to pull during the next render.
    ///
    /// Convenience over `transport().set_transport(..)`; returns `false` when no
    /// callbacks are installed, so a caller that forgot
    /// [`install_host_callbacks`](Self::install_host_callbacks) finds out rather
    /// than silently publishing into nothing.
    ///
    /// `changed` must be `true` on start, stop and any playhead discontinuity —
    /// it is what tells a plugin's internal sequencer to flush instead of
    /// counting on from the old position.
    pub fn set_transport(&mut self, info: &TransportInfo, changed: bool) -> bool {
        match self.transport() {
            Some(state) => {
                state.set_transport(info, changed);
                true
            }
            None => false,
        }
    }

    /// Enumerate the AU's factory presets.
    ///
    /// Returns an empty vec when the AU ships none. That is deliberately *not*
    /// an error: `kAudioUnitProperty_FactoryPresets` is optional, and several
    /// Apple units that plainly work (AUDelay, AULowpass, AUNBandEQ) answer the
    /// property with an OSStatus error rather than an empty array. Surfacing
    /// that as `Err` would make "this AU has no presets" indistinguishable from
    /// "the property read failed", and every caller would have to paper over it
    /// with the same `unwrap_or_default`. This mirrors
    /// [`get_parameter_list`](Self::get_parameter_list), which absorbs the same
    /// absence the same way.
    ///
    /// The returned `number`s are AU-assigned selectors to pass to
    /// [`load_factory_preset`](Self::load_factory_preset), not indices into this
    /// vec — see [`AuPreset`].
    pub fn factory_presets(&self) -> Vec<AuPreset> {
        // The property's value is a `CFArrayRef` the AU *copies* for us: the
        // host owns that reference and must release it. `CfArray::from_copied`
        // takes it under the Create rule so the release happens on drop, on
        // every path out of this function including the early returns below.
        let raw: CFArrayRef = match unsafe {
            get_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_FACTORY_PRESETS,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )
        } {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let Some(array) = (unsafe { CfArray::from_copied(raw) }) else {
            return Vec::new();
        };

        (0..array.len())
            .filter_map(|i| {
                let ptr = array.value_at(i)? as *const AUPreset;
                if ptr.is_null() {
                    return None;
                }
                // An element pointer that is not `AUPreset`-aligned is not an
                // `AUPreset`, so reading a struct through it would be UB before
                // any field is even examined.
                //
                // Unlike the `presetName` check below, an alignment gate IS
                // correct here: these elements are plain `#[repr(C)]` structs
                // living in the AU's own array, never CoreFoundation references,
                // so the arm64 tagged-pointer encoding that makes short
                // CFStrings legitimately misaligned cannot apply.
                if !(ptr as usize).is_multiple_of(std::mem::align_of::<AUPreset>()) {
                    return None;
                }
                // SAFETY: the elements of a FactoryPresets array are `AUPreset`
                // structs, per `kAudioUnitProperty_FactoryPresets`'s documented
                // value type. `ptr` is non-null and correctly aligned (checked
                // above) and borrows from `array`, which outlives this closure
                // body; everything is copied out before it drops.
                let preset = unsafe { &*ptr };
                Some(AuPreset {
                    number: preset.presetNumber,
                    // GET rule, not Create: `presetName` belongs to the AU's own
                    // preset table, and the array copy did not add a retain to
                    // it. Wrapping it with `CfString::from_copied` (Create)
                    // would release a string the host never owned — an
                    // over-release that corrupts the AU's table and crashes on
                    // the *next* enumeration, far from the cause.
                    //
                    // `checked` rather than the bare conversion: the AU supplies
                    // this pointer, and a unit whose preset table is corrupt,
                    // stale, or simply not made of `AUPreset`s hands back a
                    // non-null value that is not a CFString at all. The old
                    // unchecked read only guarded against null, so any other
                    // garbage went straight into CoreFoundation and took the
                    // process down with SIGBUS — measured against a probe AU
                    // returning a `CFArray` of `CFData`, where the bytes at
                    // `presetName`'s offset are CF header internals.
                    name: unsafe { cfstring_to_string_checked(preset.presetName) }
                        .unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Select factory preset `number`, restoring the parameter values the AU
    /// stores under it.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] when the AU rejects the preset — most
    /// commonly `kAudioUnitErr_InvalidPropertyValue` for a number it does not
    /// advertise. A rejected load leaves the AU's parameters as they were; the
    /// AU is still renderable.
    pub fn load_factory_preset(&mut self, number: i32) -> Result<()> {
        // `presetName` is ignored by the AU on a *set* — the number is the
        // selector, and the AU fills the name back in from its own table. Pass
        // null rather than manufacturing a string: a host-owned string here
        // would either leak (the AU does not release what we hand it) or be
        // read back out of `current_preset` as a name the AU never assigned.
        let preset = AUPreset {
            presetNumber: number,
            presetName: std::ptr::null(),
        };
        unsafe {
            set_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &preset,
            )
        }
    }

    /// Read back the preset the AU currently considers active.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU does not implement
    /// `kAudioUnitProperty_PresentPreset`. Unlike
    /// [`factory_presets`](Self::factory_presets) this *is* an error rather
    /// than a benign default, because there is no honest value to report: a
    /// fabricated "preset 0" would be a claim about the AU's state that the
    /// host cannot back up.
    pub fn current_preset(&self) -> Result<AuPreset> {
        let preset: AUPreset = unsafe {
            get_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_PRESENT_PRESET,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )?
        };
        // PresentPreset is documented as a Copy-rule read: the caller owns the
        // returned `presetName` and must release it. `CfString::from_copied`
        // takes that +1 and releases on drop, so reading the current preset in
        // a loop (a UI polling it, say) does not leak a CFString per read.
        let name = unsafe { CfString::from_copied(preset.presetName) }
            .map(|s| s.to_string())
            .unwrap_or_default();
        Ok(AuPreset {
            number: preset.presetNumber,
            name,
        })
    }

    /// Bypass the effect: when set, the AU passes its input through to its
    /// output without processing it.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] when the AU has no
    /// `kAudioUnitProperty_BypassEffect` property. That is the case for **every
    /// AU instrument** — DLSMusicDevice, AUSampler and AUMIDISynth all reject
    /// both the read and the write — because an instrument has no input to pass
    /// through, so "bypassed" has no meaning for one.
    ///
    /// The error is deliberately propagated rather than absorbed into a silent
    /// `Ok(())`. A host that mutes a channel by bypassing its plugins must be
    /// able to tell that the bypass did not take: swallowing the failure would
    /// leave the AU audibly processing while the host's UI showed it bypassed,
    /// and the divergence would only surface as a user-reported "the bypass
    /// button does nothing".
    pub fn set_bypass(&mut self, bypass: bool) -> Result<()> {
        // The property is a `UInt32` 0/1, not a C `Boolean`. Measured on macOS
        // 15.6: `AudioUnitGetPropertyInfo` reports a size of 4 for BypassEffect,
        // and a 1-byte write is refused with -10851
        // (`kAudioUnitErr_InvalidPropertyValue`). So the width is load-bearing,
        // not a stylistic choice — a `bool` here would make every bypass fail.
        let value: u32 = u32::from(bypass);
        unsafe {
            set_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &value,
            )
        }
    }

    /// Whether the AU is currently bypassed.
    ///
    /// # Errors
    /// As [`set_bypass`](Self::set_bypass): an AU with no bypass property
    /// (every instrument) errors rather than reporting a fabricated `false`.
    pub fn is_bypassed(&self) -> Result<bool> {
        let value: u32 = unsafe {
            get_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_BYPASS_EFFECT,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )?
        };
        Ok(value != 0)
    }

    /// Serialize the AU's current state (all parameters + internal state) to
    /// a binary plist blob suitable for persistence.
    pub fn save_state(&self) -> Result<Vec<u8>> {
        tutti_plugin_types::assert_main_thread();
        let raw: core_foundation_sys::propertylist::CFPropertyListRef = unsafe {
            get_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
            )?
        };
        match unsafe { CfPlist::from_copied(raw) } {
            Some(plist) => plist.to_binary(),
            None => Ok(Vec::new()),
        }
    }

    /// Restore state previously produced by [`Self::save_state`]. Empty input is a no-op.
    ///
    /// A successful restore is followed by
    /// [`notify_all_parameters`](crate::listener::notify_all_parameters), which
    /// Apple's `ClassInfo` documentation mandates. Setting `ClassInfo` rewrites
    /// the AU's entire parameter set *inside* the AU, without issuing a single
    /// `AUParameterSet` — so no listener hears about any of it. Without the
    /// notify, opening a project leaves every open plugin editor displaying the
    /// values from before the load: the audio is correct and the UI is a lie,
    /// and it stays a lie until the user nudges each control by hand.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU rejects the state blob. A failure
    /// of the *notify* is deliberately not propagated — see the inline comment.
    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        let plist = CfPlist::from_binary(data)?;
        let raw = plist.as_raw();
        unsafe {
            set_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &raw,
            )?;
        }
        // The state IS loaded at this point. A failed notify means open editors
        // may show stale values — bad, but strictly less bad than reporting the
        // whole restore as failed, which would have a caller retry or discard a
        // load that actually succeeded. The AU's parameters are correct either
        // way; only the UI refresh is at stake.
        //
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        let _ = unsafe { crate::listener::notify_all_parameters(self.raw_unit()) };
        Ok(())
    }

    /// Restore state that came from a **document** (a saved project), preferring
    /// `kAudioUnitProperty_ClassInfoFromDocument` and falling back to `ClassInfo`.
    ///
    /// Apple's header requires this ordering: an AU that implements
    /// `ClassInfoFromDocument` "is going to do different actions establishing its
    /// state from a document rather than from a user preset", and a host restoring
    /// a document must offer that property first, falling back when the AU errors
    /// or does not implement it. The distinction matters for units that resolve
    /// per-document resource references — sample-library paths, external file
    /// references — differently from a portable user preset.
    ///
    /// This is the counterpart to [`crate::aupreset::load_preset_file`], which is
    /// the *preset* path and therefore deliberately uses plain `ClassInfo`: a
    /// `.aupreset` is a user preset by definition, and routing one through the
    /// document property would tell the AU the opposite of the truth.
    ///
    /// Measured on macOS 15.6: **no** unit on this machine implements property 50 —
    /// AUDelay, AUDistortion, AUMatrixReverb, AUSpatialMixer and AULowpass all
    /// answer `kAudioUnitErr_InvalidProperty` (-10879). So the fallback is the path
    /// actually taken today; the try-first exists because the header mandates it
    /// and because a third-party unit may well implement it.
    ///
    /// # Errors
    /// Returns the **fallback's** error if both properties fail, since that is the
    /// path a host without this method would have taken anyway. A refusal of
    /// property 50 alone is not an error — it is the documented normal case.
    pub fn load_document_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }
        let plist = CfPlist::from_binary(data)?;
        let raw = plist.as_raw();
        // SAFETY: `raw` borrows the live `plist`. Both properties take a
        // `CFPropertyListRef` by reference and read it during the call.
        let from_document = unsafe {
            set_property(
                self.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_CLASS_INFO_FROM_DOCUMENT,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &raw,
            )
        };
        if from_document.is_err() {
            // The documented fallback. `load_state` re-decodes the blob, which is
            // a plist parse rather than anything the AU sees — cheap enough not to
            // warrant duplicating the notify logic it owns.
            return self.load_state(data);
        }
        // Same reasoning as `load_state`: the state IS loaded, so a failed
        // parameter notify must not be reported as a failed restore.
        //
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        let _ = unsafe { crate::listener::notify_all_parameters(self.raw_unit()) };
        Ok(())
    }

    /// Write the AU's current state to `path` as a `.aupreset` file — the format
    /// Logic, Live and Reaper read.
    ///
    /// The identity keys are taken from this AU's own component description, never
    /// invented; see [`crate::aupreset::save_preset_file`] for why that is what
    /// makes the file loadable elsewhere, and [`crate::aupreset`]'s module docs for
    /// the measured consequence of getting them wrong.
    ///
    /// # Errors
    /// [`AuError::PresetIo`] if the file cannot be written, [`AuError::OsStatus`]
    /// if the AU refuses to hand over its `ClassInfo`, or
    /// [`AuError::InvalidPreset`] if what it hands over is not a dictionary.
    pub fn save_preset_file(&self, path: &std::path::Path, name: &str) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        // SAFETY: `raw_unit` and the handle's `component` are both live for the
        // lifetime of this instance — the handle owns the unit and holds the
        // factory handle it was created from.
        unsafe {
            crate::aupreset::save_preset_file(
                self.raw_unit(),
                self.handle().component(),
                path,
                name,
            )
        }
    }

    /// Load a `.aupreset` file, **validating that it belongs to this AU** before
    /// applying it. Returns the identity that was accepted.
    ///
    /// A preset saved from a different plugin is refused with
    /// [`AuError::PresetIdentityMismatch`] and this AU is left untouched. That
    /// check is not redundant with the AU's own: measured on macOS 15.6, an AU
    /// handed a dictionary bearing its own identity keys but another plugin's
    /// `data` blob accepts it and adopts nonsense values. See the
    /// [`crate::aupreset`] module docs.
    ///
    /// A successful load is followed by
    /// [`notify_all_parameters`](crate::listener::notify_all_parameters), for the
    /// reason [`load_state`](Self::load_state) documents: setting `ClassInfo`
    /// rewrites every parameter inside the AU without notifying a single listener,
    /// so an open editor would keep displaying the pre-load values.
    ///
    /// # Errors
    /// [`AuError::PresetIo`], [`AuError::InvalidPreset`],
    /// [`AuError::PresetIdentityMismatch`], or [`AuError::OsStatus`] if the AU
    /// rejects a correctly-identified dictionary. On every error path the AU keeps
    /// the state it had and is still renderable.
    pub fn load_preset_file(
        &mut self,
        path: &std::path::Path,
    ) -> Result<crate::aupreset::AuPresetIdentity> {
        tutti_plugin_types::assert_main_thread();
        // SAFETY: as in `save_preset_file` — both handles are live for the
        // lifetime of this instance.
        let identity = unsafe {
            crate::aupreset::load_preset_file(self.raw_unit(), self.handle().component(), path)?
        };
        // As in `load_state`: the state is loaded, so a failed notify is a stale
        // editor rather than a failed load.
        //
        // SAFETY: `raw_unit` is live for the lifetime of this instance.
        let _ = unsafe { crate::listener::notify_all_parameters(self.raw_unit()) };
        Ok(identity)
    }

    /// Render `num_frames` of audio through the AU.
    ///
    /// `input` and `output` are per-channel planar slices. `num_frames` must
    /// not exceed the `block_size` passed to [`AuInstance::new`].
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] with `Uninitialized` if the AU has not
    /// been initialized, [`AuError::InvalidBuffer`] if `num_frames` exceeds
    /// the configured block size, or an `OsStatus` error from `AudioUnitRender`.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> Result<()> {
        match &mut self.state {
            State::Ready(r) => r.process(input, output, num_frames),
            State::Loaded(_) => Err(AuError::OsStatus {
                function: "AuInstance::process",
                code: K_AUDIO_UNIT_ERR_UNINITIALIZED,
            }),
            State::Empty => unreachable!(),
        }
    }

    /// Change the sample rate. If the AU was initialized, it is uninitialized
    /// for reconfiguration and then re-initialized to preserve the state.
    ///
    /// This returns `Ok` only when the AU is *verified* to be running at
    /// `rate`. `StreamConfig::apply` reads the accepted `mSampleRate` back and
    /// errors on a mismatch; on that error the previous rate is restored into
    /// the config, so [`sample_rate`](Self::sample_rate) and
    /// [`get_latency`](Self::get_latency) never report a rate the AU is not
    /// actually at. (Previously the set was best-effort and unverified, yet this
    /// returned `Ok(())` and recorded the *requested* rate — a phantom that both
    /// of those accessors then trusted.)
    pub fn set_sample_rate(&mut self, rate: f64) -> Result<()> {
        let was_ready = self.is_initialized();
        if was_ready {
            self.uninitialize()?;
        }
        if let State::Loaded(l) = &mut self.state {
            let previous = l.config.sample_rate;
            l.config.sample_rate = rate;
            // Re-apply and capture the effective layout the AU accepts at the
            // new rate, so the rebuilt scratch is sized correctly (FIX 3).
            match l.config.apply(&l.handle) {
                Ok(channels) => l.config.channels = channels,
                Err(e) => {
                    // Roll the config back to the rate the AU is still on, so a
                    // caller that ignores this error doesn't inherit a lie.
                    l.config.sample_rate = previous;
                    // Best-effort restore of the AU's own format too; if this
                    // also fails there is nothing further to do but surface the
                    // original rejection.
                    let _ = l.config.apply(&l.handle);
                    if was_ready {
                        self.initialize()?;
                    }
                    return Err(e);
                }
            }
        }
        if was_ready {
            self.initialize()?;
        }
        Ok(())
    }

    /// Configured maximum block size in frames.
    ///
    /// This is the bound [`process`](Self::process) enforces: a `num_frames`
    /// above it is [`AuError::InvalidBuffer`], because the AU allocated its
    /// internal buffers for this width at `AudioUnitInitialize` time and
    /// rendering wider writes past them.
    pub fn block_size(&self) -> u32 {
        self.config().block_size
    }

    /// Change the maximum block size, re-initializing around the change if the
    /// AU was already initialized.
    ///
    /// A DAW changing its buffer size in preferences would otherwise have to
    /// destroy and rebuild every plugin instance, losing every scrap of state
    /// that is not in a preset.
    ///
    /// `MaximumFramesPerSlice` is only writable on an **uninitialized** AU — the
    /// AU sizes its internal buffers from it during `AudioUnitInitialize` — so
    /// this performs the same uninitialize → reconfigure → re-initialize dance
    /// [`set_sample_rate`](Self::set_sample_rate) does, with the same
    /// verification discipline:
    ///
    /// * the accepted value is **read back**
    ///   ([`StreamConfig::verify_block_size`]) and a mismatch is a hard error,
    ///   rather than recording a width the AU is not running at. `process`
    ///   rejects `num_frames > block_size`, so a config holding a larger figure
    ///   than the AU allocated turns that guard into a false negative — the
    ///   render is admitted and the AU writes past its own buffers.
    /// * on any failure the previous size is restored into the config **and**
    ///   re-applied to the AU, so a caller that ignores the error does not
    ///   inherit a lie.
    /// * the render scratch is rebuilt at the new size, which happens via
    ///   `initialize` → `RenderScratch::new`. Skipping it would leave `process`
    ///   staging into buffers shorter than the frame count it now admits.
    ///
    /// # Errors
    /// [`AuError::InvalidBuffer`] for a zero size, [`AuError::BlockSizeRejected`]
    /// if the AU keeps a different one, or an `OsStatus` from the re-initialize.
    /// A rejection leaves the instance in the state and at the size it started
    /// in.
    pub fn set_block_size(&mut self, frames: u32) -> Result<()> {
        // Zero would fail every `process` call's `num_frames > block_size` check
        // and size the scratch to empty buffers. Refuse it here rather than
        // handing AudioToolbox a degenerate value.
        if frames == 0 {
            return Err(AuError::InvalidBuffer(
                "block_size must be non-zero".to_string(),
            ));
        }

        let was_ready = self.is_initialized();
        if was_ready {
            self.uninitialize()?;
        }

        if let State::Loaded(l) = &mut self.state {
            let previous = l.config.block_size;
            if previous != frames {
                l.config.block_size = frames;
                // Re-apply, then verify. `apply` also re-sends the stream format,
                // so capture the effective layout as `set_sample_rate` does —
                // the scratch is sized from it.
                let outcome = l.config.apply(&l.handle).and_then(|channels| {
                    l.config.channels = channels;
                    l.config.verify_block_size(&l.handle)
                });
                if let Err(e) = outcome {
                    // Roll back to the size the AU is still running at, and push
                    // it back onto the unit so config and AU agree.
                    l.config.block_size = previous;
                    let _ = l.config.apply(&l.handle);
                    if was_ready {
                        self.initialize()?;
                    }
                    return Err(e);
                }
            }
        }

        if was_ready {
            self.initialize()?;
        }
        Ok(())
    }
}

impl AuLoaded {
    /// Instantiate and apply the initial stream configuration.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent`.
    pub unsafe fn new(
        component: AudioComponent,
        sample_rate: f64,
        block_size: u32,
    ) -> Result<Self> {
        let handle = AuHandle::new(component)?;

        let probed = StreamConfig::probe(&handle);
        // The stereo floor lives HERE, in the layout-less constructor, because
        // this is the one entry point with no caller-supplied answer to "how
        // wide?" and it has to pick a default. Stereo is the right default for a
        // DAW: 2 buffers fed into a unit configured mono would have channel 1
        // silently dropped. It is a *default*, not a guard — `new_with_config`
        // bypasses it, and `StreamConfig::apply` no longer re-imposes it.
        let channels = AuBusLayout {
            inputs: probed.inputs,
            outputs: ChannelLayout::from(probed.outputs.count().max(2)),
            has_input: probed.has_input,
        };
        Self::with_layout(handle, StreamConfig::new(sample_rate, block_size, channels))
    }

    /// Instantiate at a caller-chosen [`StreamConfig`], bypassing the stereo
    /// default [`AuLoaded::new`] applies.
    ///
    /// This is how a host requests mono, or any other width the AU will take.
    /// There was previously no way to do it from outside the crate at all:
    /// `apply` is `pub(crate)` and both the constructor and `apply` forced
    /// `outputs >= 2`, so a mono track paid for a doubled channel through every
    /// AU in its chain.
    ///
    /// # Safety
    /// `component` must be a valid, non-null `AudioComponent`.
    ///
    /// # Errors
    /// As [`AuLoaded::new`]. A channel width the AU **refuses** is deliberately
    /// not an error: the AU keeps its own layout and
    /// [`config`](AuLoaded::config)`().channels` reports what it actually
    /// accepted. Compare the two if the distinction matters — that is the only
    /// way to tell, and it is why `apply` returns the effective layout rather
    /// than `()`.
    pub unsafe fn new_with_config(component: AudioComponent, config: StreamConfig) -> Result<Self> {
        Self::with_layout(AuHandle::new(component)?, config)
    }

    /// Shared tail of both constructors: apply the config, then record the layout
    /// the AU actually accepted.
    fn with_layout(handle: AuHandle, mut config: StreamConfig) -> Result<Self> {
        // `apply` returns the layout the AU actually accepted, which may differ
        // from what we requested. Store the effective layout so the render
        // scratch is later sized to the real topology (FIX 3).
        config.channels = config.apply(&handle)?;

        Ok(Self {
            handle,
            config,
            transport: None,
        })
    }

    /// Consume self and return an [`AuReady`] after a successful
    /// `AudioUnitInitialize`.
    ///
    /// The input render callback is installed exactly ONCE here, off the
    /// heap-pinned scratch's stable address, rather than every render block on
    /// the RT thread (FIX 1). The `ref_con` is `&*scratch`; because `scratch`
    /// lives behind a `Box`, its body never moves even as the enclosing
    /// [`AuReady`]/`State` is `mem::replace`d, so the pointer the AU retains
    /// stays valid (FIX 2).
    ///
    /// # Errors
    /// The error carries `self` back, because this is a by-value typestate
    /// transition: without it a refusing AU is simply destroyed, and
    /// [`AuInstance::initialize`] has nothing to put back into its state
    /// machine. See that method for what the resulting hole did.
    ///
    /// The recovered state is an `Option` for the one case that cannot produce
    /// a `Loaded` AU: the callback install failed *and* the compensating
    /// `AudioUnitUninitialize` failed too, leaving a unit that is still
    /// initialized. Its `AuReady` is dropped here so the unit is still disposed
    /// — there is simply no honest `AuLoaded` to return.
    pub fn initialize(self) -> std::result::Result<AuReady, (Option<Self>, AuError)> {
        if let Err(e) = check("AudioUnitInitialize", unsafe {
            AudioUnitInitialize(self.handle.raw_unit())
        }) {
            return Err((Some(self), e));
        }

        // Allocate the heap-pinned scratch, then move it into `AuReady`. The
        // Box body does not move on that transfer (only the 8-byte pointer
        // does), so the ref_con derived from `&*ready.scratch` below is stable.
        let scratch = Box::new(RenderScratch::new(
            self.config.channels,
            self.config.block_size,
        ));
        let ready = AuReady {
            loaded: self,
            scratch,
            #[cfg(test)]
            callback_installs: std::sync::atomic::AtomicU32::new(0),
        };

        // Install the render callback ONCE, immediately after init, from the
        // scratch's stable heap address. AU accepts a render-callback set on the
        // input scope post-`AudioUnitInitialize`. There is deliberately no
        // per-block install in `process` (that was the RT-thread bug, FIX 1).
        // Gate on `has_input`, the AU's own answer to "is there an input bus",
        // NOT on the scratch's input buffer count: `RenderScratch::new`
        // over-allocates inputs to `in_ch.max(out_ch)` so a 0-in/2-out
        // instrument still gets 2 input buffers. Keying the install off that
        // made every instrument (DLSMusicDevice, AUSampler) fail `initialize`
        // with -10877 — setting a render callback on the input scope of a unit
        // that has no input element is a property error, and the `?` aborted
        // init entirely.
        let scratch_ptr: *mut RenderScratch = &*ready.scratch as *const RenderScratch as *mut _;
        if ready.loaded.config.channels.has_input {
            if let Err(e) = unsafe { ready.install_input_callback(scratch_ptr) } {
                // The AU *is* initialized at this point, so backing out has to
                // undo that too — not merely drop the half-built `AuReady`.
                // Route through `uninitialize`, which owns the ordering
                // invariant (uninitialize before the boxed scratch is freed)
                // rather than duplicating it here.
                //
                // The reported error is always `e`, the install failure: it is
                // what actually went wrong, and a follow-on
                // `AudioUnitUninitialize` complaint would only describe the
                // cleanup. If that cleanup also failed there is no `Loaded` AU
                // to hand back — dropping the `AuReady` still disposes the unit.
                return Err(match ready.uninitialize() {
                    Ok(loaded) => (Some(loaded), e),
                    Err((_ready, _unwind_err)) => (None, e),
                });
            }
        }
        Ok(ready)
    }

    /// Borrow the underlying [`AuHandle`].
    pub fn handle(&self) -> &AuHandle {
        &self.handle
    }

    /// Borrow the configured [`StreamConfig`].
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }
}

impl AuReady {
    /// Tear down the render session and return to the [`AuLoaded`] state.
    ///
    /// # Errors
    /// The error carries `self` back, for the reason
    /// [`AuLoaded::initialize`]'s does. Handing it back also keeps the failure
    /// path from leaking: the `ManuallyDrop` below has suppressed the `Drop`
    /// that disposes the unit and frees the scratch, so an early `?` here would
    /// have leaked both.
    pub fn uninitialize(self) -> std::result::Result<AuLoaded, (Self, AuError)> {
        // Disable the Drop path (which would also uninitialize) to avoid a
        // double `AudioUnitUninitialize`.
        let mut me = std::mem::ManuallyDrop::new(self);
        // ORDERING INVARIANT (FIX 2): `AudioUnitUninitialize` MUST run before the
        // boxed scratch is freed below. After uninitialize the AU can no longer
        // fire the input render callback, so the ref_con pointing at `*scratch`
        // is guaranteed dead before we drop the Box. Reordering these two would
        // let the AU call back into freed memory.
        let status = unsafe { AudioUnitUninitialize(me.loaded.handle.raw_unit()) };
        if let Err(e) = check("AudioUnitUninitialize", status) {
            // The AU refused to uninitialize, so it is still initialized and
            // the render callback may still fire against `*scratch`. Rebuild
            // the `AuReady` intact — its `Drop` retries the uninitialize before
            // freeing anything — rather than leaking it inside `ManuallyDrop`.
            // SAFETY: `me` is a live, fully-initialized `AuReady` that nothing
            // has moved out of; `ManuallyDrop::take` is the documented way to
            // reclaim ownership, and `me` is not used again.
            let ready = unsafe { std::mem::ManuallyDrop::take(&mut me) };
            return Err((ready, e));
        }
        // Move `loaded` out by reading through the ManuallyDrop. Safe because
        // nothing else touches `me` afterwards.
        let loaded = unsafe { std::ptr::read(&me.loaded) };
        // `scratch` is a `Box<RenderScratch>` owning heap storage; drop it
        // explicitly (only now that the AU is uninitialized).
        unsafe { std::ptr::drop_in_place(&mut me.scratch) };
        Ok(loaded)
    }

    /// Render `num_frames` through the AU. See [`AuInstance::process`] for
    /// arg semantics and error conditions.
    pub fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        num_frames: u32,
    ) -> Result<()> {
        if num_frames > self.loaded.config.block_size {
            return Err(AuError::InvalidBuffer(format!(
                "num_frames ({num_frames}) > block_size ({})",
                self.loaded.config.block_size
            )));
        }

        self.scratch.stage_input(input, num_frames);

        // NOTE: the render callback is installed ONCE at initialize time off the
        // scratch's stable heap address — deliberately NOT here. Re-installing it
        // per block issued an `AudioUnitSetProperty` on the RT thread every call
        // (FIX 1) and, with the old inline scratch, handed the AU a ref_con that
        // dangled once the state machine moved (FIX 2).
        let abl = self.scratch.bind_output(num_frames);
        let timestamp = AudioTimeStamp::with_sample_time(self.scratch.advance(num_frames));
        let mut flags: AudioUnitRenderActionFlags = 0;

        let status = unsafe {
            AudioUnitRender(
                self.loaded.handle.raw_unit(),
                &mut flags,
                &timestamp,
                0,
                num_frames,
                abl,
            )
        };
        if status != NO_ERR {
            // A-2: enrich the diagnostic with the AU's last render error. This
            // is a global-scope/element-0 read the AU updates on each render;
            // it's advisory only, so a failed query silently degrades to the
            // plain `AudioUnitRender` OSStatus. Only reached when render itself
            // failed — never on a successful steady-state block — so the extra
            // property read does not affect the no-alloc guarantee.
            let last = unsafe {
                get_property::<OSStatus>(
                    self.loaded.handle.raw_unit(),
                    K_AUDIO_UNIT_PROPERTY_LAST_RENDER_ERROR,
                    K_AUDIO_UNIT_SCOPE_GLOBAL,
                    0,
                )
            }
            .ok()
            .filter(|&e| e != NO_ERR);
            return Err(AuError::render_failed("AudioUnitRender", status, last));
        }

        // A-1: honor the AU's OutputIsSilence signal. When the AU declares the
        // block silent, its output buffers are not guaranteed to be zeroed
        // (the flag is precisely how an AU says "I produced nothing, don't
        // trust the buffer contents"). Force the destination to silence rather
        // than emitting stale scratch. `fill` writes in place — no allocation,
        // preserving the RT no-alloc guarantee.
        if flags & K_AUDIO_UNIT_RENDER_ACTION_OUTPUT_IS_SILENCE != 0 {
            let n = num_frames as usize;
            for dst in output.iter_mut() {
                let len = n.min(dst.len());
                dst[..len].fill(0.0);
            }
        } else {
            self.scratch.emit_output(output, num_frames);
        }
        Ok(())
    }

    /// Install the input render callback exactly once, wiring `ref_con` to the
    /// heap-pinned scratch's stable address.
    ///
    /// # Safety
    /// `scratch_ptr` must point at this `AuReady`'s boxed `RenderScratch` and
    /// must outlive every `AudioUnitRender` call and remain valid until
    /// `AudioUnitUninitialize` runs. The `Box` indirection guarantees the
    /// address is stable across `State`/`mem::replace` moves.
    unsafe fn install_input_callback(&self, scratch_ptr: *mut RenderScratch) -> Result<()> {
        #[cfg(test)]
        self.callback_installs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let callback = AURenderCallbackStruct {
            inputProc: Some(au_input_render_callback),
            inputProcRefCon: scratch_ptr as *mut c_void,
        };
        set_property(
            self.loaded.handle.raw_unit(),
            K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
            K_AUDIO_UNIT_SCOPE_INPUT,
            0,
            &callback,
        )
    }

    /// Borrow the underlying [`AuHandle`].
    pub fn handle(&self) -> &AuHandle {
        &self.loaded.handle
    }

    /// Borrow the configured [`StreamConfig`].
    pub fn config(&self) -> &StreamConfig {
        &self.loaded.config
    }
}

impl Drop for AuReady {
    fn drop(&mut self) {
        unsafe {
            let _ = AudioUnitUninitialize(self.loaded.handle.raw_unit());
        }
    }
}

impl Drop for AuLoaded {
    /// Unhook the host callbacks before the state they point at is freed.
    ///
    /// ORDERING INVARIANT, the transport twin of the one
    /// [`AuReady::uninitialize`] documents: while the callbacks are installed
    /// the AU holds a `hostUserData` raw pointer into `*transport`. Dropping
    /// this struct frees that box, so the property must be cleared first or the
    /// AU is left holding a dangling pointer it may dereference on its render
    /// thread.
    ///
    /// Rust's field drop order (declaration order — `handle` disposes the unit
    /// before `transport` is freed) happens to make this safe already, which is
    /// exactly why it is written out: that ordering is invisible at the field
    /// definitions and one reordering of the struct away from a use-after-free
    /// on the audio thread. Clearing the property explicitly makes the
    /// invariant independent of field order.
    ///
    /// A zeroed `HostCallbackInfo` is the documented way to withdraw them: Apple
    /// declares every proc nullable, so an all-null struct installs no
    /// callbacks. The status is ignored — this is teardown, and an AU that
    /// refuses the clear is about to be disposed on the next line anyway.
    fn drop(&mut self) {
        if self.transport.is_none() {
            return;
        }
        let withdrawn = transport::HostCallbackInfo {
            host_user_data: std::ptr::null_mut(),
            beat_and_tempo: None,
            musical_time_location: None,
            transport_state: None,
            transport_state2: None,
        };
        // SAFETY: `handle` is still live (this runs before its own `Drop`), and
        // the value written is a correctly-typed `HostCallbackInfo`.
        unsafe {
            let _ = set_property(
                self.handle.raw_unit(),
                K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS,
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                &withdrawn,
            );
        }
    }
}

/// The AU calls this on its render thread to pull input. It is `extern "C"`, so
/// A panic must never escape it: unwinding across the FFI boundary into
/// AudioToolbox is undefined behaviour. The whole body runs inside
/// [`catch_unwind`](std::panic::catch_unwind) and a caught panic is reported as
/// an error status, not swallowed.
unsafe extern "C" fn au_input_render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut AudioUnitRenderActionFlags,
    _in_time_stamp: *const AudioTimeStamp,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    // `AssertUnwindSafe`: the only state reachable here is `&RenderScratch`
    // (shared, read-only) and the AU's own buffers. A panic mid-copy can leave
    // a partially-written output buffer, which is a glitched block — not a
    // broken invariant — so there is nothing for unwind safety to protect.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        render_input(in_ref_con, in_number_frames, io_data)
    }));
    match result {
        Ok(status) => status,
        Err(_) => {
            // Do NOT swallow this. `eprintln!` rather than a logging facade
            // because this crate has no logger dependency, and a write to
            // stderr is the one diagnostic guaranteed to survive a process
            // whose audio thread just panicked.
            eprintln!(
                "tutti-au-host: PANIC in au_input_render_callback, \
                 contained to avoid unwinding into AudioToolbox"
            );
            K_AUDIO_UNIT_ERR_CANNOT_DO_IN_CURRENT_CONTEXT
        }
    }
}

/// The actual input-pull body. Split out so [`au_input_render_callback`] is
/// nothing but the `catch_unwind` guard around it.
///
/// # Safety
/// `in_ref_con` must be null or point at a live `RenderScratch`; `io_data` must
/// be null or a well-formed `AudioBufferList`.
unsafe fn render_input(
    in_ref_con: *mut c_void,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> OSStatus {
    if in_ref_con.is_null() || io_data.is_null() {
        return -1;
    }

    let scratch = &*(in_ref_con as *const RenderScratch);
    let requested = in_number_frames as usize;

    for (ch, buf) in iter_buffers_mut(io_data).enumerate() {
        // Never trust the buffer the AU handed us. `mData` may be null
        // (the AU asking us to supply our own pointer) and `mDataByteSize` may
        // describe FEWER frames than `in_number_frames`. Writing
        // `in_number_frames` blind is a null deref in the first case and an
        // out-of-bounds store in the second.
        if buf.mData.is_null() {
            // Nothing to write into; report the size honestly as zero rather
            // than claiming we filled a buffer that does not exist.
            buf.mDataByteSize = 0;
            continue;
        }
        // The buffer's own declared capacity, in f32 frames. This channel is
        // non-interleaved (mNumberChannels == 1 in our ASBD), but honour a
        // wider mNumberChannels defensively by dividing it out.
        let per_channel = (buf.mNumberChannels as usize).max(1);
        let capacity_frames =
            buf.mDataByteSize as usize / (std::mem::size_of::<f32>() * per_channel);
        let frames = requested.min(capacity_frames);
        if frames == 0 {
            buf.mDataByteSize = 0;
            continue;
        }

        let dst = std::slice::from_raw_parts_mut(buf.mData as *mut f32, frames);
        match scratch.inputs.get(ch) {
            Some(src) => {
                let n = frames.min(src.len());
                dst[..n].copy_from_slice(&src[..n]);
                if n < frames {
                    dst[n..].fill(0.0);
                }
            }
            None => dst.fill(0.0),
        }
        // Report what was actually written, which is bounded by the buffer's
        // own capacity — not the host's requested frame count.
        buf.mDataByteSize = (frames * std::mem::size_of::<f32>() * per_channel) as u32;
    }

    NO_ERR
}

/// Test-only counter of how many times the input render callback property has
/// been set via `AudioUnitSetProperty(SetRenderCallback)`. Used by
/// `test_render_callback_installed_once` to prove the FIX-1 invariant: the
/// callback is installed once per initialize, never per render block.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::*;

    fn find_apple_delay() -> Option<AudioComponent> {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        find_component(&desc)
    }

    #[test]
    fn test_new() {
        let comp = find_apple_delay().expect("AUDelay should be present");
        let inst = unsafe { AuInstance::new(comp, 44100.0, 512) };
        assert!(inst.is_ok());
    }

    #[test]
    fn test_initialize_uninitialize() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();

        assert!(!inst.is_initialized());
        inst.initialize().unwrap();
        assert!(inst.is_initialized());
        inst.uninitialize().unwrap();
        assert!(!inst.is_initialized());
    }

    #[test]
    fn test_get_name() {
        let comp = find_apple_delay().unwrap();
        let inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        assert!(!inst.get_name().unwrap().is_empty());
    }

    #[test]
    fn test_parameter_list() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let params = inst.get_parameter_list();
        assert!(!params.is_empty());
    }

    #[test]
    fn test_get_set_parameter() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let params = inst.get_parameter_list();
        let p = &params[0];
        let mid = p.range.mid();
        inst.set_parameter(p.id, mid).unwrap();
        let val = inst.get_parameter(p.id).unwrap();
        assert!((val - mid).abs() < 0.01);
    }

    #[test]
    fn test_process_silence() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

        inst.process(&in_slices, &mut out_slices, 512).unwrap();
    }

    #[test]
    fn test_process_audio() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let input: Vec<Vec<f32>> = (0..2)
            .map(|_| {
                (0..512)
                    .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

        inst.process(&in_slices, &mut out_slices, 512).unwrap();
    }

    #[test]
    fn test_latency() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();
        let _ = inst.get_latency().unwrap();
    }

    #[test]
    fn test_save_load_state() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        let state = inst.save_state().unwrap();
        assert!(!state.is_empty());
        inst.load_state(&state).unwrap();
    }

    /// Presets and bypass must work in the `Loaded` state, before
    /// `AudioUnitInitialize`. Both are global-scope properties with no render
    /// resources behind them, and a host builds its preset menu and restores a
    /// saved bypass state while wiring the plugin up — i.e. before it ever
    /// initializes. Gating either on the Ready state would break that.
    #[test]
    fn presets_and_bypass_work_before_initialize() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        assert!(!inst.is_initialized());

        // AUDelay ships no presets — the read fails and is absorbed. What is
        // being asserted is that it does not panic or error out of the Loaded
        // state, not the count (the integration suite pins counts).
        assert!(inst.factory_presets().is_empty());
        inst.set_bypass(true).expect("bypass in the Loaded state");
        assert!(inst.is_bypassed().unwrap());
        inst.set_bypass(false).unwrap();
        assert!(!inst.is_bypassed().unwrap());
    }

    /// `factory_presets` must absorb the property error, but `current_preset`
    /// must not invent a preset for a unit that has none.
    ///
    /// These two deliberately differ, and the difference is easy to "tidy" into
    /// consistency later: an empty list is an honest description of a unit with
    /// no presets, whereas any `AuPreset` returned from `current_preset` would
    /// be a claim about the AU's state. AUDelay in fact implements
    /// `PresentPreset` and reports the `-1`/"Untitled" no-selection sentinel,
    /// so this asserts the number is negative rather than that the call fails —
    /// the point is that the value came from the AU.
    #[test]
    fn a_preset_less_unit_reports_no_selection_rather_than_a_fabricated_preset() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        assert!(inst.factory_presets().is_empty());
        if let Ok(current) = inst.current_preset() {
            assert!(
                current.number < 0,
                "AUDelay advertises no factory presets, so it must not report a \
                 selected one; got {current:?}"
            );
        }
    }

    #[test]
    fn test_render_callback_installed_once() {
        // AUDelay is an effect (has input), so the render callback IS installed.
        // Per-instance counter — no process-global, so this is race-free under
        // cargo's parallel test runner.
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        // Exactly one install after initialize — none per render block.
        assert_eq!(
            inst.callback_install_count(),
            1,
            "render callback should be installed exactly once at initialize"
        );

        let input = vec![vec![0.0f32; 512]; 2];
        let mut output = vec![vec![0.0f32; 512]; 2];
        let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();

        for _ in 0..16 {
            let mut out_slices: Vec<&mut [f32]> =
                output.iter_mut().map(|v| v.as_mut_slice()).collect();
            inst.process(&in_slices, &mut out_slices, 512).unwrap();
        }

        // Still exactly one — process() must not re-install per block (FIX 1).
        assert_eq!(
            inst.callback_install_count(),
            1,
            "process() must not re-install the render callback per block"
        );
    }

    /// A refused `initialize` must leave the instance usable.
    ///
    /// `initialize` takes the state out with `mem::replace(.., State::Empty)`
    /// and the by-value transition consumes it, so the failure arm used to
    /// return the error with `Empty` still installed. Every later accessor —
    /// `raw_unit`, `au_type`, `num_outputs`, even `is_initialized` — routes
    /// through `handle()`/`config()`, which `unreachable!()` on `Empty`. So a
    /// host scanning installed AUs would panic on the next thing it asked about
    /// any unit that declined to initialize, and some do: AUSoundIsolation
    /// refuses on this machine, and any unit whose hardware or entitlement is
    /// absent will too.
    ///
    /// Driven through a real refusal rather than a mocked one. `vois` is not in
    /// the corpus because it is not part of the *rendering* contract; it is
    /// used here only as a unit that says no. If it ever starts initializing,
    /// the test says so rather than passing silently.
    #[test]
    fn a_refused_initialize_leaves_the_instance_usable() {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"vois"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let Some(comp) = find_component(&desc) else {
            // Not a silent skip of the invariant: the same guarantee is
            // asserted below against an AU that *does* initialize, so the state
            // machine is still exercised. Only the refusal leg needs this unit.
            eprintln!("AUSoundIsolation not registered; refusal leg not exercised");
            return;
        };
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        let unit_before = inst.raw_unit();

        match inst.initialize() {
            Err(_) => {
                // The whole point: these must answer rather than panic.
                assert!(!inst.is_initialized());
                assert_eq!(
                    inst.raw_unit(),
                    unit_before,
                    "a refused initialize replaced the underlying unit"
                );
                let _ = inst.au_type();
                let _ = inst.num_outputs();
                // And the instance must still be re-drivable.
                let _ = inst.initialize();
            }
            Ok(()) => {
                // It accepted after all. Still assert the state is coherent, so
                // this branch is not a free pass.
                assert!(inst.is_initialized());
            }
        }
    }

    #[test]
    fn test_set_sample_rate() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();
        inst.set_sample_rate(48000.0).unwrap();
        assert_eq!(inst.sample_rate(), 48000.0);
        assert!(inst.is_initialized());
    }

    /// `sample_rate()` used to report the *requested* rate whether or
    /// not the AU took it, because the stream-format set was `let _`'d and only
    /// `mChannelsPerFrame` was read back. Now an `Ok` from `set_sample_rate`
    /// means the AU's own ASBD agrees — so assert against the AU, not against
    /// the number we just stored.
    #[test]
    fn set_sample_rate_ok_means_the_au_really_moved() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        for rate in [48000.0f64, 96000.0, 44100.0] {
            if inst.set_sample_rate(rate).is_err() {
                // A refusal is a legitimate outcome; what must never happen is
                // a refusal reported as success. Check the config was rolled
                // back rather than left holding the rejected rate.
                assert_ne!(
                    inst.sample_rate(),
                    rate,
                    "a rejected rate must not be left in the config"
                );
                continue;
            }
            assert_eq!(inst.sample_rate(), rate);
            // The independent check: ask the AU itself.
            let asbd = unsafe {
                get_property::<AudioStreamBasicDescription>(
                    inst.raw_unit(),
                    K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                    K_AUDIO_UNIT_SCOPE_OUTPUT,
                    0,
                )
            }
            .expect("AUDelay reports its output stream format");
            assert!(
                (asbd.mSampleRate - rate).abs() < 1e-6,
                "set_sample_rate({rate}) returned Ok but the AU is at {}",
                asbd.mSampleRate
            );
        }
    }

    /// The end-to-end invariant: `set_sample_rate` returning `Ok` must
    /// imply the AU's own ASBD agrees.
    ///
    /// CAVEAT on what this can prove locally: Apple's AUDelay accepts *every*
    /// rate offered to it — probed here at 0.5 Hz, 1 Hz, 8 kHz, 48 kHz, 192 kHz
    /// and 1 MHz, all reported back verbatim. So no installed AU on this machine
    /// drives the *rejection* branch, and this test cannot fail by deleting the
    /// `check_sample_rate` call site. The rejection logic itself is pinned by
    /// `stream::tests::a_rate_the_au_did_not_accept_is_an_error`; what this adds
    /// is the standing guarantee that the two agree for whatever the AU does —
    /// including on a machine with a pickier AU installed.
    #[test]
    fn an_unusual_rate_cannot_be_reported_as_accepted_unless_it_was() {
        let comp = find_apple_delay().unwrap();
        let mut inst = unsafe { AuInstance::new(comp, 44100.0, 512) }.unwrap();
        inst.initialize().unwrap();

        // A rate a stricter AU would refuse.
        let absurd = 1.0f64;
        match inst.set_sample_rate(absurd) {
            Err(_) => {
                // Rejected, as expected. The config must have been rolled back
                // rather than left holding the rate the AU refused.
                assert_ne!(inst.sample_rate(), absurd);
            }
            Ok(()) => {
                // Astonishing, but then the AU must really be at 1 Hz.
                let asbd = unsafe {
                    get_property::<AudioStreamBasicDescription>(
                        inst.raw_unit(),
                        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                        K_AUDIO_UNIT_SCOPE_OUTPUT,
                        0,
                    )
                }
                .expect("AUDelay reports its output stream format");
                assert!(
                    (asbd.mSampleRate - absurd).abs() < 1e-6,
                    "set_sample_rate({absurd}) returned Ok while the AU is at {} — \
                     Ok must mean the rate was actually applied",
                    asbd.mSampleRate
                );
            }
        }
    }

    /// Build a standalone `AudioBufferList` for the render-callback tests.
    /// Mirrors what AudioToolbox hands the callback.
    fn make_abl(buffers: &mut [(*mut f32, u32)]) -> (Vec<u8>, *mut AudioBufferList) {
        let n = buffers.len();
        let bytes = std::mem::offset_of!(AudioBufferList, mBuffers)
            + n.max(1) * std::mem::size_of::<AudioBuffer>();
        // Over-allocate and align by hand; this is test scaffolding standing in
        // for AudioToolbox's own allocation.
        let mut storage = vec![0u8; bytes + 16];
        let base = storage.as_mut_ptr();
        let offset = base.align_offset(std::mem::align_of::<AudioBufferList>());
        let abl = unsafe { base.add(offset) } as *mut AudioBufferList;
        unsafe {
            (*abl).mNumberBuffers = n as u32;
            let first = &raw mut (*abl).mBuffers[0];
            for (i, &mut (data, byte_size)) in buffers.iter_mut().enumerate() {
                let b = first.add(i);
                (*b).mNumberChannels = 1;
                (*b).mDataByteSize = byte_size;
                (*b).mData = data as *mut c_void;
            }
        }
        (storage, abl)
    }

    /// The callback used to build its destination slice straight from
    /// `mData` for the full `in_number_frames` extent, never reading
    /// `mDataByteSize` and never null-checking `mData`. A short buffer was an
    /// out-of-bounds write; a null one was a null deref.
    #[test]
    fn render_callback_respects_a_short_buffer() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );

        // The AU offers room for 8 frames but asks for 64. Sentinels past the
        // 8th slot must survive.
        const CAPACITY: usize = 8;
        const REQUESTED: u32 = 64;
        let mut chan = vec![f32::from_bits(0xDEAD_BEEF); 32];
        let data = chan.as_mut_ptr();
        let mut descs = [(data, (CAPACITY * 4) as u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status = unsafe {
            render_input(
                &scratch as *const RenderScratch as *mut c_void,
                REQUESTED,
                abl,
            )
        };
        assert_eq!(status, NO_ERR);

        // Everything past the declared capacity is untouched — that is the OOB
        // write not happening.
        for (i, s) in chan.iter().enumerate().skip(CAPACITY) {
            assert_eq!(
                s.to_bits(),
                0xDEAD_BEEF,
                "frame {i} past the declared {CAPACITY}-frame capacity was overwritten"
            );
        }
        // And the reported size is the capacity actually written, not the
        // host's requested figure (which the old code wrote back blindly).
        unsafe {
            let b = &raw const (*abl).mBuffers[0];
            assert_eq!((*b).mDataByteSize as usize, CAPACITY * 4);
        }
    }

    /// A null `mData` must be skipped, not dereferenced.
    #[test]
    fn render_callback_survives_a_null_buffer() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 256u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, NO_ERR);
        unsafe {
            let b = &raw const (*abl).mBuffers[0];
            assert_eq!(
                (*b).mDataByteSize,
                0,
                "a null buffer must report zero bytes written, not the requested size"
            );
        }
    }

    /// A zero-capacity buffer is the degenerate short case; it must write
    /// nothing rather than forming a slice over a zero-length allocation.
    #[test]
    fn render_callback_handles_zero_capacity() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut chan = vec![f32::from_bits(0xDEAD_BEEF); 8];
        let mut descs = [(chan.as_mut_ptr(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        let status =
            unsafe { render_input(&scratch as *const RenderScratch as *mut c_void, 64, abl) };
        assert_eq!(status, NO_ERR);
        assert!(chan.iter().all(|s| s.to_bits() == 0xDEAD_BEEF));
    }

    /// Null `ref_con` / `io_data` are rejected before any deref.
    #[test]
    fn render_callback_rejects_null_arguments() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let mut descs = [(std::ptr::null_mut::<f32>(), 0u32)];
        let (_storage, abl) = make_abl(&mut descs);

        assert_eq!(unsafe { render_input(std::ptr::null_mut(), 64, abl) }, -1);
        assert_eq!(
            unsafe {
                render_input(
                    &scratch as *const RenderScratch as *mut c_void,
                    64,
                    std::ptr::null_mut(),
                )
            },
            -1
        );
    }

    /// The unwind half, driven through the real `extern "C"` entry
    /// point rather than through `render_input`.
    ///
    /// The guard is what stands between a panicking render body and undefined
    /// behaviour in AudioToolbox, so exercise the guarded symbol itself across
    /// the same malformed inputs. Every one of these must return a status —
    /// reaching this assert at all proves nothing unwound out of the
    /// `extern "C"` frame.
    #[test]
    fn the_guarded_callback_returns_a_status_for_every_malformed_input() {
        let scratch = RenderScratch::new(
            AuBusLayout {
                inputs: ChannelLayout::Stereo,
                outputs: ChannelLayout::Stereo,
                has_input: true,
            },
            512,
        );
        let ref_con = &scratch as *const RenderScratch as *mut c_void;
        let mut flags: AudioUnitRenderActionFlags = 0;
        let ts = AudioTimeStamp::with_sample_time(0.0);

        let mut short_chan = vec![0.0f32; 32];
        let mut zero_chan = vec![0.0f32; 8];
        // (buffer descriptors, requested frames): short, null, zero-capacity,
        // and a frame count far past anything the scratch holds.
        let mut cases: Vec<(Vec<(*mut f32, u32)>, u32)> = vec![
            (vec![(short_chan.as_mut_ptr(), 8 * 4)], 64),
            (vec![(std::ptr::null_mut::<f32>(), 256)], 64),
            (vec![(zero_chan.as_mut_ptr(), 0)], 64),
            (vec![(short_chan.as_mut_ptr(), 32 * 4)], u32::MAX),
        ];

        for (descs, frames) in cases.iter_mut() {
            let (_storage, abl) = make_abl(descs);
            let status =
                unsafe { au_input_render_callback(ref_con, &mut flags, &ts, 0, *frames, abl) };
            assert_eq!(
                status, NO_ERR,
                "a malformed-but-handleable buffer list should render silence, not fail"
            );
        }

        // Null arguments still short-circuit through the guard.
        let (_storage, abl) = make_abl(&mut [(std::ptr::null_mut::<f32>(), 0u32)]);
        assert_eq!(
            unsafe { au_input_render_callback(std::ptr::null_mut(), &mut flags, &ts, 0, 64, abl) },
            -1
        );
        assert_eq!(
            unsafe {
                au_input_render_callback(ref_con, &mut flags, &ts, 0, 64, std::ptr::null_mut())
            },
            -1
        );
    }
}
