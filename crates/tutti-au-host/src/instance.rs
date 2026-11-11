//! [`AuLoaded`]: an Audio Unit that is instantiated and configured but not
//! initialized (not rendering).
//!
//! In this state parameters and state can be read and written, presets and
//! bypass work, the editor can be opened and the unit's identity can be
//! queried. [`Loaded::activate`](tutti_plugin_types::Loaded::activate) consumes
//! it and returns the realtime [`AuProcessor`](crate::AuProcessor) and the
//! main-thread [`AuController`](crate::AuController); the controller derefs to
//! `AuLoaded`, so every call here stays reachable while audio runs. The traits
//! the format implements are in `control` (the control surface both halves
//! carry) and `lifecycle` (the transitions).
//!
//! Most of what this type offers beyond the shared
//! [`Control`](tutti_plugin_types::Control) is AU-specific and has no shared
//! shape to translate into: MIDI mappings, nick and context names,
//! presentation latency, layout tags, document state, preset files and render
//! quality. A few names are the plain-unit forms of the normalized
//! [`Control`](tutti_plugin_types::Control) calls (`set_parameter`,
//! `get_parameter`, `parameters`): reach those through the trait
//! (`Control::set_parameter(&mut au, ..)`) when the normalized one is meant.

#![cfg(target_os = "macos")]

use std::cell::Cell;
use std::sync::Arc;

use tutti_au_ffi::property::{BypassEffect, ClassInfo, ClassInfoFromDocument, Latency};
use tutti_au_ffi::{AuEditor, Element, HostCallbacks, Scope, TransportState, Unit};
use tutti_plugin_types::{
    ChannelLayout, MainThread, PluginDescriptor, RequestWord, TransportInfo,
};
use tutti_types::Samples;
use tutti_types::value::units::Seconds;

use crate::bus::{self, AuChannelConfig, BusDirection};
use crate::channel_layout::{self, AuLayoutTag};
use crate::component::{AuComponentRef, AuType};
use crate::error::Result;
use crate::handle::AuHandle;
use crate::identity;
use crate::midi_map::{self, AuMidiMapping};
use crate::midi_out::{self, AuMidiOutput, MidiOutSink, MidiOutputInfo};
use crate::parameters::{self, AuParameter, ParamView};
use crate::preset::AuPreset;
use crate::ranges::RangeTable;
use crate::stream::{AuBusLayout, StreamConfig};
use crate::transport;
#[cfg(feature = "test-support")]
use crate::types::AudioUnit;
use crate::control::describe;
use crate::watch::PropertyWatch;

/// An AU that has been instantiated but not yet initialized.
pub struct AuLoaded {
    /// Declared **first** so it drops first: the registration holds the unit
    /// (and keeps it alive), and a delivery after its owner has gone is wasted
    /// work. `None` until [`watch_properties`](Self::watch_properties), or when
    /// the listener could not be created, in which case the figures read at
    /// load stand until the next ask.
    watch: Option<PropertyWatch>,
    handle: AuHandle,
    config: StreamConfig,
    /// Host transport, installed on demand by
    /// [`install_host_callbacks`](Self::install_host_callbacks).
    ///
    /// The AU retains the `hostUserData` pointer derived from the state, which
    /// lives on the heap behind an `Arc`, so this struct (and its halves) move
    /// between activations without moving what the AU holds. `None` until a host
    /// installs callbacks (an AU with no transport wired must not pay for an
    /// allocation, and a null `hostUserData` is exactly what the procs treat as
    /// "no state").
    transport: Option<HostCallbacks>,
    /// The open editor, when one is. Owned here so the editor calls read like
    /// their VST3 and CLAP counterparts, with the Cocoa specifics staying inside
    /// [`AuEditor`].
    editor: Option<AuEditor>,
    /// What the property listener raised since the last
    /// [`take_requests`](Control::take_requests).
    pub(crate) requests: Arc<RequestWord>,
    /// The declared plain range of every parameter, read at load and again when
    /// the unit says its parameter list moved.
    pub(crate) ranges: RangeTable,
    /// The plugin's identity, with the editor measured at construction: a Cocoa
    /// view is a property of the instance, readable before `initialize`.
    descriptor: PluginDescriptor,
    /// Whether `kAudioUnitProperty_OfflineRender` was last written as offline.
    offline: bool,
    /// Whether the unit took the last render mode written to it: the live
    /// probe behind `Features::RENDER_MODE`, since the property is optional.
    mode_accepted: bool,
    /// The last latency read that succeeded: what a refusal answers, since a
    /// unit that stops answering keeps what it last reported rather than being
    /// silently recompensated to zero, which would move audio.
    last_latency: Cell<Samples>,
    /// Where the unit came from, when [`load`](Self::load) found it.
    pub(crate) origin: Option<tutti_plugin_types::Origin>,
}

impl std::fmt::Debug for AuLoaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuLoaded")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AuLoaded {
    /// Instantiates and applies the initial stream configuration.
    ///
    /// # Errors
    /// [`AuError::LoadFailed`](crate::AuError::LoadFailed) or
    /// [`AuError::RequiresAsyncInstantiation`](crate::AuError::RequiresAsyncInstantiation)
    /// if the component cannot be instantiated, or the stream-configuration
    /// errors of [`StreamConfig`]'s `apply`.
    pub fn new(component: AuComponentRef, sample_rate: f64, block_size: u32) -> Result<Self> {
        let handle = AuHandle::new(component)?;

        let probed = StreamConfig::probe(&handle);
        // The stereo floor lives HERE, in the layout-less constructor, because
        // this is the one entry point with no caller-supplied answer to "how
        // wide?" and it has to pick a default. Stereo is the right default for a
        // DAW: 2 buffers fed into a unit configured mono would have channel 1
        // silently dropped. It is a *default*, not a guard: `new_with_config`
        // bypasses it, and `StreamConfig::apply` does not re-impose it.
        let channels = AuBusLayout {
            inputs: probed.inputs,
            outputs: ChannelLayout::from(probed.outputs.count().max(2)),
            has_input: probed.has_input,
        };
        Self::with_layout(handle, StreamConfig::new(sample_rate, block_size, channels))
    }

    /// Instantiates at a caller-chosen [`StreamConfig`], bypassing the stereo
    /// default [`AuLoaded::new`] applies.
    ///
    /// This is how a host requests mono, or any other width the AU will take.
    /// It is the only route to a sub-stereo width from outside the crate:
    /// `apply` is `pub(crate)`, and without this a mono track pays for a doubled
    /// channel through every AU in its chain.
    ///
    /// # Errors
    /// As [`AuLoaded::new`].
    pub fn new_with_config(component: AuComponentRef, config: StreamConfig) -> Result<Self> {
        Self::with_layout(AuHandle::new(component)?, config)
    }

    /// Shared tail of both constructors: apply the config, verify the block size
    /// stuck, then record the layout the AU actually accepted.
    ///
    /// # Errors
    /// [`AuError::BlockSizeRejected`](crate::AuError::BlockSizeRejected) when the
    /// AU kept a `MaximumFramesPerSlice` other than the requested one, plus
    /// whatever [`StreamConfig::apply`] propagates.
    ///
    /// The block size is verified here, not only when an activation changes it,
    /// because this is the tail **every** load funnels through and the hazard is
    /// identical on both paths: an AU that clamps to its own maximum returns
    /// `noErr` from the set while keeping a smaller figure, and the recorded
    /// larger number then turns the render's `num_frames > block_size` guard into
    /// a false negative: the render is admitted and the AU writes past buffers it
    /// sized for fewer frames.
    ///
    /// A failure is an `Err` rather than a silent adoption of the accepted
    /// figure, which matches how the two other non-negotiable properties are
    /// handled: `apply` `?`s on the `MaximumFramesPerSlice` *set* and makes a
    /// rejected sample rate fatal. The channel layout is the deliberate
    /// exception: it is *returned* rather than enforced because a narrower width
    /// is recoverable by resizing the scratch, and nothing downstream indexes
    /// past an allocation on account of it.
    fn with_layout(handle: AuHandle, mut config: StreamConfig) -> Result<Self> {
        // `apply` returns the layout the AU actually accepted, which may differ
        // from what was requested. Store the effective layout so the render
        // scratch is later sized to the real topology, not the wished-for one.
        config.channels = config.apply(&handle)?;
        config.verify_block_size(&handle)?;

        let descriptor = describe(&handle);
        let ranges = RangeTable::read(handle.unit());
        Ok(Self {
            watch: None,
            handle,
            config,
            transport: None,
            editor: None,
            requests: Arc::new(RequestWord::new()),
            ranges,
            descriptor,
            offline: false,
            mode_accepted: false,
            last_latency: Cell::new(Samples::ZERO),
            origin: None,
        })
    }

    /// Borrows the underlying [`AuHandle`].
    pub fn handle(&self) -> &AuHandle {
        &self.handle
    }

    /// Borrows the configured [`StreamConfig`].
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// The plugin's catalog identity, with the editor measured at construction.
    pub(crate) fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// Starts watching the properties whose load-time values would otherwise go
    /// stale (latency, tail and the parameter list): a change raises a request,
    /// which [`take_requests`](Control::take_requests) hands out. Idempotent.
    ///
    /// `false` when AudioToolbox refuses the listener, which leaves the figures
    /// as read; a unit whose latency cannot be watched is still a usable plugin.
    pub fn watch_properties(&mut self) -> bool {
        if self.watch.is_none() {
            self.watch = PropertyWatch::install(self.handle.unit(), Arc::clone(&self.requests));
        }
        self.watch.is_some()
    }

    /// The word the property listener raises, for a caller that needs to say a
    /// change no installed unit will make on request.
    pub fn requests(&self) -> &Arc<RequestWord> {
        &self.requests
    }

    /// Whether the unit took the last render mode written to it (activation
    /// writes the mode of its [`Prepare`](tutti_plugin_types::Prepare); so does
    /// [`Control::set_render_mode`]). `kAudioUnitProperty_OfflineRender` is
    /// optional, so this is the live probe behind `Features::RENDER_MODE`.
    pub fn render_mode_accepted(&self) -> bool {
        self.mode_accepted
    }

    /// Writes the render mode, recording whether the unit took it.
    pub(crate) fn write_render_mode(&mut self, offline: bool) -> bool {
        let accepted = crate::offline::set_offline_render(self.handle.unit(), offline).is_ok();
        if accepted {
            self.offline = offline;
        }
        self.mode_accepted = accepted;
        accepted
    }

    /// Whether the last render mode written was offline.
    pub(crate) fn wrote_offline(&self) -> bool {
        self.offline
    }

    /// Re-applies the stream configuration at a new rate and block ceiling.
    ///
    /// A refusal rolls the configuration back to the one the unit is still on,
    /// and pushes it onto the unit again, so a caller that ignores the error
    /// does not inherit a lie.
    ///
    /// # Errors
    /// The stream-configuration errors of `apply`, or
    /// [`AuError::BlockSizeRejected`](crate::AuError::BlockSizeRejected).
    pub(crate) fn reconfigure(&mut self, sample_rate: f64, block_size: u32) -> Result<()> {
        if sample_rate == self.config.sample_rate && block_size == self.config.block_size {
            return Ok(());
        }
        let previous = self.config;
        let mut next = previous;
        next.sample_rate = sample_rate;
        next.block_size = block_size;
        // Re-apply and capture the effective layout the AU accepts at the new
        // rate, so the rebuilt scratch is sized to the real topology: the AU may
        // take a different width at a different rate. `apply` also re-sends the
        // stream format.
        let outcome = next.apply(&self.handle).and_then(|channels| {
            next.channels = channels;
            next.verify_block_size(&self.handle)
        });
        match outcome {
            Ok(()) => {
                self.config = next;
                Ok(())
            }
            Err(e) => {
                // Best-effort restore of the AU's own format too; if this also
                // fails there is nothing further to do but surface the original
                // rejection.
                let _ = previous.apply(&self.handle);
                Err(e)
            }
        }
    }

    /// The instance, for the safe AudioToolbox calls on it: the free functions of
    /// [`crate::parameters`], [`crate::identity`] and [`crate::offline`] take it.
    pub fn unit(&self) -> &Unit {
        self.handle.unit()
    }

    /// The raw `AudioUnit` pointer, for a test that makes an AudioToolbox call
    /// this crate does not wrap. Using it is the caller's `unsafe`.
    #[cfg(feature = "test-support")]
    pub fn raw_unit(&self) -> AudioUnit {
        self.unit().as_raw()
    }

    /// Installs the host transport callbacks (`kAudioUnitProperty_HostCallbacks`)
    /// so the AU can pull tempo, beat and transport state during its render,
    /// returning the state the host publishes into.
    ///
    /// Offered before `initialize`, once, off the audio thread. A unit that
    /// refuses the property runs on its own clock; the refusal is the error, and
    /// no transport is kept for it, so [`transport`](Self::transport) is `Some`
    /// exactly when the unit took the callbacks.
    ///
    /// # Errors
    /// The `OSStatus` of the property write.
    pub fn install_host_callbacks(&mut self) -> Result<&TransportState> {
        // Allocate once and keep the same state on a re-install: the AU may
        // already hold a `hostUserData` pointing at it, and swapping in a fresh
        // allocation would leave that pointer dangling until the property write
        // landed: a window on the render thread, not merely a leak.
        let callbacks = self
            .transport
            .take()
            .unwrap_or_else(|| HostCallbacks::new(self.handle.unit()));
        // A refusal drops what was made, so nothing is kept for a unit that did
        // not take the callbacks, and a re-install after one starts afresh.
        callbacks.install()?;
        Ok(self.transport.insert(callbacks).state())
    }

    /// The state the host transport callbacks read, shared with the render
    /// thread that publishes into it. `None` unless the callbacks are installed.
    pub(crate) fn share_transport(&self) -> Option<Arc<TransportState>> {
        self.transport.as_ref().map(HostCallbacks::share_state)
    }

    /// The state as the unit's `ClassInfo` plist, empty when the AU answers null.
    pub(crate) fn capture_state(&self) -> Result<Vec<u8>> {
        Ok(self
            .unit()
            .copy_plist::<ClassInfo>(Scope::GLOBAL, Element::ZERO)?)
    }

    /// Restores a `ClassInfo` plist and tells every open editor the values moved
    /// (a failure to notify is not a failure to restore). Empty data is a no-op.
    pub(crate) fn restore_state(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.unit()
            .set_plist::<ClassInfo>(Scope::GLOBAL, Element::ZERO, data)?;
        let _ = crate::listener::notify_all_parameters(self.unit());
        Ok(())
    }

    /// Whether an editor is open on this unit.
    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    pub(crate) fn editor_slot(&mut self) -> &mut Option<AuEditor> {
        &mut self.editor
    }

    pub(crate) fn last_latency(&self) -> &Cell<Samples> {
        &self.last_latency
    }


    /// High-level [`AuType`] this component was classified as.
    pub fn au_type(&self) -> AuType {
        self.handle.au_type()
    }


    /// Configured input channel count (`0` for generators / instruments, which
    /// have no input bus).
    pub fn num_inputs(&self) -> u32 {
        let channels = self.config.channels;
        if channels.has_input {
            channels.inputs.count() as u32
        } else {
            0
        }
    }


    /// Configured output channel count.
    pub fn num_outputs(&self) -> u32 {
        self.config.channels.outputs.count() as u32
    }


    /// Configured sample rate in Hz.
    pub fn sample_rate(&self) -> f64 {
        self.config.sample_rate
    }


    /// Copies the AU's display name.
    ///
    /// # Errors
    ///
    /// Never returns `Err`; a name that cannot be read is reported as
    /// `"<unknown>"`.
    pub fn get_name(&self) -> Result<String> {
        Ok(self.handle.get_name())
    }


    /// Writes a parameter value **and** notify listeners — including the AU's own
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
        Ok(crate::listener::set_parameter_notifying(
            self.unit(),
            id,
            crate::listener::EventAddress::GLOBAL,
            value,
            0,
        )?)
    }


    /// Reads a parameter's current value, in the parameter's own units, from
    /// the global scope, element 0.
    ///
    /// # Errors
    ///
    /// Returns [`AuError::OsStatus`] if `AudioUnitGetParameter` fails, e.g. for
    /// an unknown `id`.
    pub fn get_parameter(&self, id: u32) -> Result<f32> {
        parameters::get(self.unit(), id)
    }


    /// Installs a render notification, called twice per render (pre and post).
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
    /// one inside `AuLoaded` would impose a single slot and tie every tap to
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
        Ok(crate::render_notify::RenderNotify::new(
            self.unit(),
            callback,
        )?)
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
        self.unit().render_unit()
    }


    /// Schedules parameter events into the render call currently in flight.
    ///
    /// **Only correct from inside a pre-render notify.** The events apply to the
    /// current `AudioUnitRender` and to no other, so calling this from a control
    /// thread races the render it was meant to affect. Reach it through
    /// [`add_render_notify`](Self::add_render_notify); this method exists so a
    /// caller holding an `&AuLoaded` in the callback does not have to reach for
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
        Ok(crate::render_notify::schedule(
            &self.unit().render_unit(),
            id,
            address,
            events,
        )?)
    }


    /// Enumerates all parameters exposed by the AU.
    pub fn get_parameter_list(&self) -> Vec<AuParameter> {
        parameters::list(self.unit())
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
        bus::bus_count(self.unit(), direction)
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
        bus::bus_layout(self.unit(), direction, bus)
    }


    /// Every channel configuration the AU declares it can run.
    ///
    /// An empty vec means the AU publishes no constraint — which is what all 22
    /// Apple effects measured on macOS 15.6 do — and must be read as
    /// "unconstrained, consult the stream format", never as "supports nothing".
    /// See [`AuChannelConfig`] for why the negative entries stay sentinels.
    pub fn supported_channel_configs(&self) -> Vec<AuChannelConfig> {
        bus::supported_channel_configs(self.unit())
    }


    /// Every channel *order* the AU says it can run bus `bus` of `direction` in.
    ///
    /// Complements [`supported_channel_configs`](Self::supported_channel_configs),
    /// which answers "how many channels": this answers "in what speaker order".
    /// The stream format carries only a count, so without this a 6-channel bus's
    /// centre and LFE are indistinguishable and dialog can land in the subwoofer.
    ///
    /// An empty vec means the AU publishes no constraint and must be read as
    /// "declines to say", never "supports nothing" — see
    /// [`channel_layout::supported_layout_tags`] for the measured breakdown (only
    /// 11 of ~38 units answer at all on macOS 15.6).
    pub fn supported_layout_tags(&self, direction: BusDirection, bus: u32) -> Vec<AuLayoutTag> {
        channel_layout::supported_layout_tags(self.unit(), direction, bus)
    }


    /// The channel order bus `bus` of `direction` is running right now.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] with the AU's own status; the three seen in
    /// practice are `kAudioUnitErr_InvalidProperty` (no layout property at all),
    /// `kAudioUnitErr_PropertyNotInUse` (the property exists but no layout is
    /// set — measured on AUSampler's output) and
    /// `kAudioUnitErr_InvalidElement` (no such bus). Deliberately not flattened
    /// into a default: picking a speaker order for a bus whose order is unknown
    /// is the bug this whole property exists to prevent. See
    /// [`channel_layout::layout_tag`].
    pub fn layout_tag(&self, direction: BusDirection, bus: u32) -> Result<AuLayoutTag> {
        channel_layout::layout_tag(self.unit(), direction, bus)
    }


    /// Asks the AU to run bus `bus` of `direction` in the `tag` channel order.
    ///
    /// # A published tag is not automatically a settable tag
    ///
    /// The AU checks the tag's channel count against the width already configured
    /// on that bus and refuses any mismatch — so at the default stereo width
    /// every surround tag an AU advertises is rejected. Set the width first, via
    /// [`new_with_config`](Self::new_with_config). The measured table is in
    /// [`channel_layout::set_layout_tag`], which also explains why this does not
    /// widen the format on the caller's behalf.
    ///
    /// # Errors
    /// [`AuError::OsStatus`], typically `kAudioUnitErr_InvalidPropertyValue` for
    /// a tag this bus will not take at its current width. Propagated rather than
    /// absorbed for the reason [`set_bypass`](Self::set_bypass)'s is: a host that
    /// believes it set 5.1 while the AU kept stereo routes six channels into a
    /// two-channel bus and never learns why.
    pub fn set_layout_tag(
        &mut self,
        direction: BusDirection,
        bus: u32,
        tag: AuLayoutTag,
    ) -> Result<()> {
        channel_layout::set_layout_tag(self.unit(), direction, bus, tag)
    }


    /// The AU's own name for bus `bus` of `direction` — "Sidechain", "stereo mix".
    ///
    /// Without this a multi-input plugin's buses render as "Bus 1..N" in the host
    /// UI while the plugin has perfectly good names for them, and a sidechain
    /// input looks identical to a second audio input.
    ///
    /// # Errors
    /// [`AuError::OsStatus`]. The two statuses are **different facts** and
    /// neither becomes an empty string:
    /// `kAudioUnitErr_PropertyNotInUse` (-10850) is a real bus the AU gave no
    /// name, `kAudioUnitErr_InvalidElement` (-10877) is a bus that does not
    /// exist. A host sizing buffers cannot afford to confuse them.
    ///
    /// Measured on macOS 15.6: every Apple **mixer** answers -10850 for all of
    /// its real buses — they publish no element names at all — while
    /// DLSMusicDevice names its two outputs ("stereo mix", "unused") and the
    /// third-party effects name theirs (TDR Nova: "Input", "Sidechain",
    /// "Output"). See [`channel_layout::element_name`], which also documents the
    /// retain-count leak this read would have had.
    pub fn element_name(&self, direction: BusDirection, bus: u32) -> Result<String> {
        channel_layout::element_name(self.unit(), direction, bus)
    }


    /// What the AU publishes about its MIDI **output** streams, or `None` if it
    /// publishes nothing.
    ///
    /// This is the real capability gate for MIDI-out hosting, and the one a host
    /// should branch on. `None` — "not a MIDI source" — is what **every** AU
    /// installed on this machine answers (measured: 0 of 138 components), so no
    /// arpeggiator or step-sequencer AU is available here to exercise the path
    /// end-to-end.
    ///
    /// Note the asymmetry with [`install_midi_output`](Self::install_midi_output):
    /// 45 of those same units happily **accept** the callback write while
    /// publishing no output streams, so a successful install proves nothing. See
    /// [`midi_out::midi_output_info`].
    pub fn midi_output_info(&self) -> Option<MidiOutputInfo> {
        midi_out::midi_output_info(self.unit())
    }


    /// Installs `sink` as the destination for MIDI this AU generates during render.
    ///
    /// Needed for any AU that produces notes — an arpeggiator, a step sequencer,
    /// a chord generator. Without it the AU's notes exist only inside the plugin:
    /// nothing can record them, route them to a second instrument, or draw them.
    ///
    /// The returned [`AuMidiOutput`] is the registration. **Keep it alive for as
    /// long as the callback should stay installed** — dropping it withdraws the
    /// callback from the AU before freeing the state the AU points at, which is
    /// the ordering that keeps the AU from dereferencing freed memory on its
    /// render thread. [`AuMidiOutput::remove`] does the same and surfaces the AU's
    /// status.
    ///
    /// `sink` is called **on the render thread** and must not allocate, lock or
    /// block; it receives a borrowed slice precisely so it cannot park one. The
    /// full contract is on [`MidiOutSink`].
    ///
    /// # Errors
    /// [`AuError::OsStatus`] if the AU refuses the property. Measured on macOS
    /// 15.6: the output units refuse; the effects, instruments and mixers accept.
    ///
    /// # Panics
    /// Never — but note the callback body cannot panic across the FFI boundary
    /// either: a panic escaping `sink` is caught and reported as a render error
    /// rather than unwinding into AudioToolbox.
    pub fn install_midi_output(&mut self, sink: MidiOutSink) -> Result<AuMidiOutput> {
        Ok(midi_out::install(self.unit(), sink)?)
    }


    /// Tells the AU where it sits in the host's project — `"track 3"`.
    ///
    /// Purely presentational: the AU shows it in its own window so a user with
    /// several instances open can tell them apart. Set it when the plugin is
    /// inserted and again whenever the slot is renamed or the plugin moves.
    ///
    /// This is the *slot*, not the instance — see
    /// [`set_nick_name`](Self::set_nick_name) for the half that belongs in a
    /// saved session.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it,
    /// though all 52 instantiable units measured on macOS 15.6 do.
    pub fn set_context_name(&self, name: &str) -> Result<()> {
        identity::set_context_name(self.unit(), name)
    }


    /// Reads back the name set by [`set_context_name`](Self::set_context_name).
    ///
    /// All 57 instantiable units measured return the exact string written, so
    /// a test can assert the write *landed* rather than that it returned
    /// `noErr`.
    ///
    /// # Errors
    /// As [`set_context_name`](Self::set_context_name).
    pub fn context_name(&self) -> Result<Option<String>> {
        identity::context_name(self.unit())
    }


    /// Give this instance its own name, distinct from another load of the same
    /// AU.
    ///
    /// Unlike [`set_context_name`](Self::set_context_name), this is the
    /// instance's identity rather than its slot, so it is what a host persists
    /// in a session and restores on load.
    ///
    /// # Errors
    /// As [`set_context_name`](Self::set_context_name).
    pub fn set_nick_name(&self, name: &str) -> Result<()> {
        identity::set_nick_name(self.unit(), name)
    }


    /// Reads back the name set by [`set_nick_name`](Self::set_nick_name).
    ///
    /// `Ok(None)` means the AU implements the property but has no name set —
    /// distinct from the `Err` returned by one that does not implement it.
    ///
    /// # Errors
    /// As [`set_context_name`](Self::set_context_name).
    pub fn nick_name(&self) -> Result<Option<String>> {
        identity::nick_name(self.unit())
    }


    /// The AU's own shortlist of its most important parameters, in its own
    /// priority order.
    ///
    /// What to show in a compact view — a channel strip, a collapsed rack row —
    /// instead of truncating [`parameters`](Self::parameters). The AU curates
    /// *and reorders*: AUDynamicsProcessor leads its overview with parameter
    /// ids `[4, 5, 6, 0, 1, 2]`, so taking the first N of the parameter list
    /// yields a different and worse set.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from the 26 of 52 units that do not
    /// implement it — the common case, and a normal answer rather than a fault.
    /// Fall back to [`parameters`](Self::parameters).
    pub fn parameters_for_overview(&self) -> Result<Vec<identity::OverviewParameter>> {
        identity::parameters_for_overview(self.unit())
    }


    /// Filesystem path of the AU's icon, for a plugin browser row.
    ///
    /// `Ok(None)` means the AU implements the property but supplied no URL, or
    /// supplied one that does not name a file.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement it;
    /// 36 of 52 measured return a usable URL.
    pub fn icon_location(&self) -> Result<Option<String>> {
        identity::icon_location(self.unit())
    }


    /// Whether this AU implements the parameter↔MIDI mapping family at all.
    ///
    /// The capability gate to branch on before offering a MIDI-learn UI, and the
    /// counterpart of [`midi_output_info`](Self::midi_output_info). Unlike that
    /// one, this capability **is** implemented on this machine — measured on
    /// macOS 15.6, `true` for exactly 2 of 59 installed components (AUSampler
    /// and AUMIDISynth), `false` for the other 57, including every effect and
    /// every third-party unit installed here.
    pub fn supports_parameter_midi_mapping(&self) -> bool {
        midi_map::supports_parameter_midi_mapping(self.unit())
    }


    /// The AU's current parameter↔MIDI mapping table.
    ///
    /// This is the surface VST3 spells `IMidiMapping`, inverted: the AU owns the
    /// table and does the routing itself during render, so a host reads and
    /// writes it rather than consulting it per block. Mod wheel, breath,
    /// expression and sustain reach many instruments only this way.
    ///
    /// Empty means "implements the property, has no mappings" — what a fresh
    /// AUSampler reports. The order is **the AU's**, not insertion order
    /// (AUSampler reorders), so compare tables as sets.
    ///
    /// # Errors
    /// [`AuError::OsStatus`] with `kAudioUnitErr_InvalidProperty` (-10879) from
    /// the 57 of 59 units that do not implement the family. That is the routine
    /// answer for an effect, not a fault — branch on
    /// [`supports_parameter_midi_mapping`](Self::supports_parameter_midi_mapping)
    /// first if the distinction matters.
    pub fn parameter_midi_mappings(&self) -> Result<Vec<AuMidiMapping>> {
        midi_map::all(self.unit())
    }


    /// Adds `mappings`, replacing any that already target the same parameter.
    ///
    /// Verified end-to-end on macOS 15.6, which is what separates this from the
    /// crate's other "accepted but never called" surfaces: mapping CC 20 to
    /// AUSampler's parameter 900 (`Gain`, `-96..=12` dB), then sending CC 20 =
    /// 127 through [`send_midi`](Self::send_midi) and rendering one block, moved
    /// the parameter from `0` to `12`. The AU does the routing; the host only
    /// installs the table.
    ///
    /// An empty slice is a no-op — a zero-length property write is `paramErr`
    /// (-50) on both implementers.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
    /// family. **A `noErr` proves nothing about the mapping being meaningful**:
    /// a parameter id no parameter uses is accepted *and* appears in the
    /// read-back table (measured: id `99999` on a ~10-parameter unit), as is an
    /// out-of-range scope. Cross-check against
    /// [`get_parameter_list`](Self::get_parameter_list), and verify by reading
    /// [`parameter_midi_mappings`](Self::parameter_midi_mappings) back rather
    /// than by this call's status.
    pub fn add_parameter_midi_mapping(&mut self, mappings: &[AuMidiMapping]) -> Result<()> {
        midi_map::add(self.unit(), mappings)
    }


    /// Removes the mappings targeting each argument's
    /// `(scope, element, parameter_id)`.
    ///
    /// Only that triple is matched — the trigger and flags are ignored — so a
    /// caller can pass a mapping it read back, or one naming just the parameter.
    /// A mapping that is not installed is silently ignored, as the header
    /// specifies and both implementers honour.
    ///
    /// Empty slice is a no-op, for the reason
    /// [`add_parameter_midi_mapping`](Self::add_parameter_midi_mapping) gives.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
    /// family.
    pub fn remove_parameter_midi_mapping(&mut self, mappings: &[AuMidiMapping]) -> Result<()> {
        midi_map::remove(self.unit(), mappings)
    }


    /// Replaces the AU's entire mapping table with `mappings`.
    ///
    /// The obvious way to *clear* the table — writing an empty one — does not
    /// work: a NULL/0-size write is `kAudioUnitErr_InvalidPropertyValue`
    /// (-10851), a non-NULL/0-length write is `paramErr` (-50), and the table is
    /// unchanged either way. So an empty slice routes through read-then-remove
    /// instead, which is the only path both implementers accept. That is not
    /// atomic — see [`midi_map::set_all`].
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
    /// family; the empty-slice path also surfaces the read's error.
    pub fn set_parameter_midi_mappings(&mut self, mappings: &[AuMidiMapping]) -> Result<()> {
        midi_map::set_all(self.unit(), mappings)
    }


    /// Arms "learn" mode: the AU maps the **next MIDI message it sees** to the
    /// parameter `mapping` names.
    ///
    /// Supply only the parameter target — the AU fills in the trigger and channel
    /// from whatever arrives. Poll
    /// [`hot_mapped_parameter`](Self::hot_mapped_parameter) for the result, or
    /// watch the property through [`crate::listener`]; the header says the AU
    /// fires a notification when the mapping completes.
    ///
    /// Verified: arming parameter 2 on AUSampler and sending CC 11 produced
    /// `ControlChange { controller: 11 }` and grew the table by one entry.
    ///
    /// # Errors
    /// `kAudioUnitErr_InvalidProperty` from a unit that does not implement the
    /// family.
    pub fn hot_map_parameter(&mut self, mapping: &AuMidiMapping) -> Result<()> {
        midi_map::arm_hot_map(self.unit(), mapping)
    }


    /// The mapping a hot map just completed, or `None` if none has.
    ///
    /// `None` rather than an error, because the AU's status cannot be trusted
    /// here: the header says an unarmed AU should answer
    /// `kAudioUnitErr_InvalidPropertyValue`, and **neither implementer does** —
    /// both return `noErr` with an all-zero struct even on a fresh instance. A
    /// host branching on the `Result` would read that as a real "note off, note
    /// 0" mapping and offer to bind it. See [`midi_map::hot_map`] for the
    /// `mStatus == 0` rule that replaces the status check.
    pub fn hot_mapped_parameter(&self) -> Option<AuMidiMapping> {
        midi_map::hot_map(self.unit())
    }


    /// Whether a completed hot map is waiting to be read.
    ///
    /// Named form of [`hot_mapped_parameter`](Self::hot_mapped_parameter)
    /// returning `is_some()`, so a learn UI can poll without reconstructing the
    /// `mStatus == 0` rule.
    pub fn hot_map_pending(&self) -> bool {
        midi_map::hot_map_pending(self.unit())
    }


    /// Borrows a [`ParamView`] for scoped parameter access.
    pub fn parameters(&self) -> ParamView<'_> {
        ParamView::new(self.unit())
    }


    /// Returns the plugin-reported processing latency in samples at the current
    /// sample rate.
    ///
    /// Zero is a real answer here, not a fallback. Measured across every AU
    /// registered on macOS 15.6 (29 effects, instruments and music effects):
    /// **all 29 answer, none refuses**, and the ones with no latency answer
    /// `0.0` — AUDelay, AUSampler, DLSMusicDevice and AUMatrixReverb all do.
    /// So a zero from this function means the AU said zero. Contrast
    /// [`transport::tail_time`](crate::transport), where refusal is the norm:
    /// every Apple instrument and mixer rejects `TailTime` with -10879.
    ///
    /// Returns [`Samples`] rather than a bare `u32` so the unit travels with the
    /// value; the AU reports seconds, and the rate scaling is what turns one
    /// into the other.
    ///
    /// # Errors
    ///
    /// Returns [`AuError::OsStatus`] if the AU refuses
    /// `kAudioUnitProperty_Latency`. A refusal is propagated rather than
    /// flattened to zero.
    pub fn get_latency(&self) -> Result<Samples> {
        let seconds: f64 = self.unit().get::<Latency>(Scope::GLOBAL, Element::ZERO)?;
        // Round rather than truncate: a latency the AU reports as an exact
        // sample count can land a hair below it in f64, and truncating there
        // reports one sample less than the plugin actually delays — PDC then
        // under-compensates by a sample on every such plugin.
        Ok(Seconds(seconds).to_samples(tutti_types::SampleRate::new(self.sample_rate())))
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
        transport::tail_time(self.unit())
    }


    /// Tells this AU how far downstream of it the listener is, on one bus.
    ///
    /// The inverse direction of [`get_latency`](Self::get_latency): that asks
    /// the AU what it costs the host, this tells the AU what the rest of the
    /// chain costs it, so a plugin doing look-ahead metering can align its
    /// display against audio the user has not heard yet.
    ///
    /// Advisory only — nothing in the audio path depends on it, and an AU that
    /// ignores it renders identically. Call once per *active* bus, per Apple's
    /// header, and again whenever the downstream chain changes; a stale value
    /// is what makes a meter drift away from the audio.
    ///
    /// # Errors
    /// Propagates the AU's refusal. Measured on macOS 15.6, **every one of the
    /// 39 instantiable registered units refuses** with
    /// `kAudioUnitErr_InvalidProperty` (-10879), so a caller treating this as
    /// best-effort should discard the `Result` explicitly rather than expect a
    /// success. See
    /// [`transport::set_presentation_latency`] for the full measurement and for
    /// why the refusal is not absorbed here.
    pub fn set_presentation_latency(
        &self,
        direction: BusDirection,
        bus: u32,
        latency: Seconds,
    ) -> Result<()> {
        transport::set_presentation_latency(self.unit(), direction, bus, latency)
    }


    /// Which parameters writing the meta-parameter `id` may silently move.
    ///
    /// `None` means the AU did not answer — **not** that nothing moves. See
    /// [`parameters::dependents_of`] for why those two must stay distinct, and
    /// for the measurement that no unit on this machine answers at all despite
    /// 28 of their parameters carrying a meta flag.
    ///
    /// Global scope, which is where every meta-flagged parameter measured here
    /// lives; a mixer-hosting caller reaches for
    /// [`parameters::dependents_of_at`] directly, as with the rest of this
    /// crate's per-element parameter surface.
    pub fn dependent_parameters(&self, id: u32) -> Option<Vec<parameters::DependentParam>> {
        parameters::dependents_of(self.unit(), id)
    }


    /// Borrows the installed transport, or `None` if
    /// [`install_host_callbacks`](Self::install_host_callbacks) was never called.
    ///
    /// The borrow is shared rather than mutable because every write goes through
    /// an atomic — the point of the type is that the control thread can publish
    /// while the render thread reads.
    pub fn transport(&self) -> Option<&TransportState> {
        self.transport.as_ref().map(HostCallbacks::state)
    }


    /// Publishes a transport snapshot for the AU to pull during the next render.
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


    /// Enumerates the AU's factory presets.
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
        // The property's value is a `CFArrayRef` the AU *copies*, whose elements
        // are bare `AUPreset` structs; `Unit::factory_presets` reads it under the
        // Create rule for the array and the Get rule for each name, and checks
        // each name is a string (a unit with a corrupt table must not take the
        // process down; see `tests/au_misbehaving.rs`). A refusal to answer is
        // the "no presets" answer, not an error.
        self.unit()
            .factory_presets()
            .unwrap_or_default()
            .into_iter()
            .map(|(number, name)| AuPreset { number, name })
            .collect()
    }


    /// Selects factory preset `number`, restoring the parameter values the AU
    /// stores under it.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] when the AU rejects the preset — most
    /// commonly `kAudioUnitErr_InvalidPropertyValue` for a number it does not
    /// advertise. A rejected load leaves the AU's parameters as they were; the
    /// AU is still renderable.
    pub fn load_factory_preset(&mut self, number: i32) -> Result<()> {
        Ok(self.unit().set_present_preset(number)?)
    }


    /// Reads back the preset the AU currently considers active.
    ///
    /// # Errors
    /// Returns [`AuError::OsStatus`] if the AU does not implement
    /// `kAudioUnitProperty_PresentPreset`. Unlike
    /// [`factory_presets`](Self::factory_presets) this *is* an error rather
    /// than a benign default, because there is no honest value to report: a
    /// fabricated "preset 0" would be a claim about the AU's state that the
    /// host cannot back up.
    pub fn current_preset(&self) -> Result<AuPreset> {
        let (number, name) = self.unit().present_preset()?;
        Ok(AuPreset { number, name })
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
        let value: u32 = u32::from(bypass);
        Ok(self
            .unit()
            .set::<BypassEffect>(Scope::GLOBAL, Element::ZERO, &value)?)
    }


    /// Whether the AU is currently bypassed.
    ///
    /// # Errors
    /// As [`set_bypass`](Self::set_bypass): an AU with no bypass property
    /// (every instrument) errors rather than reporting a fabricated `false`.
    pub fn is_bypassed(&self) -> Result<bool> {
        let value: u32 = self
            .unit()
            .get::<BypassEffect>(Scope::GLOBAL, Element::ZERO)?;
        Ok(value != 0)
    }


    /// Whether the AU has been told it is rendering offline.
    ///
    /// See [`offline::set_offline_render`](crate::offline) for what the flag
    /// means and what a bouncing host loses without it.
    ///
    /// # Errors
    /// [`AuError::OsStatus`] when the AU does not implement
    /// `kAudioUnitProperty_OfflineRender` — which on macOS 15.6 is every Apple
    /// effect and mixer; only the two instruments implement it. The refusal is
    /// propagated rather than reported as `false`, because a host that cannot
    /// tell "in real-time mode" from "has no offline mode" cannot warn the user
    /// that an export may not match the audition.
    pub fn is_offline_render(&self) -> Result<bool> {
        crate::offline::is_offline_render(self.unit())
    }


    /// Tells the AU whether this render is a faster-than-real-time bounce.
    ///
    /// A host doing an offline export sets this on every AU in the graph before
    /// initializing them, and clears it when returning to live monitoring.
    /// [`crate::offline::set_offline_render`] carries the full account of what
    /// changes inside the AU — dropout protection, resampling kernel length,
    /// dither determinism — and why setting it *before* `initialize` matters.
    ///
    /// # Errors
    /// As [`is_offline_render`](Self::is_offline_render).
    pub fn set_offline_render(&mut self, offline: bool) -> Result<()> {
        crate::offline::set_offline_render(self.unit(), offline)
    }


    /// Whether the AU is willing to render with its input and output buffers
    /// aliasing the same memory.
    ///
    /// A host that gets `true` may skip one buffer copy per block per plugin.
    /// Measured on macOS 15.6, six Apple effects advertise the property and all
    /// six say yes; no unit on the system says no. See
    /// [`crate::offline::supports_in_place`] for the list, and for why this crate
    /// exposes only the read even though the property is writable.
    ///
    /// # Errors
    /// [`AuError::OsStatus`] when the AU does not implement
    /// `kAudioUnitProperty_InPlaceProcessing`. Not flattened to `false`: since no
    /// unit measured actually reports `0`, doing so would turn "never mentioned
    /// it" into "forbids it" and make the host's census of the property a
    /// fiction.
    pub fn supports_in_place_processing(&self) -> Result<bool> {
        crate::offline::supports_in_place(self.unit())
    }


    /// The AU's current render-quality setting, `0..=`[`RENDER_QUALITY_MAX`](crate::offline::RENDER_QUALITY_MAX).
    ///
    /// # Errors
    /// [`AuError::OsStatus`] when the AU has no
    /// `kAudioUnitProperty_RenderQuality`, which is 8 of the 12 corpus units.
    pub fn render_quality(&self) -> Result<u32> {
        crate::offline::render_quality(self.unit())
    }


    /// Sets the AU's render quality, `0` (cheapest) to
    /// [`RENDER_QUALITY_MAX`](crate::offline::RENDER_QUALITY_MAX) (best).
    ///
    /// # Errors
    /// [`AuError::InvalidBuffer`] for a value above the maximum — enforced here
    /// because three of the four units that implement the property accept
    /// out-of-range values with `noErr` and read them straight back, so the AU's
    /// own answer cannot be trusted to catch it. [`AuError::OsStatus`] when the
    /// AU has no such property. [`crate::offline::set_render_quality`] carries the
    /// measured table.
    pub fn set_render_quality(&mut self, quality: u32) -> Result<()> {
        crate::offline::set_render_quality(self.unit(), quality)
    }


    /// Restores state that came from a **document** (a saved project), preferring
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
    pub fn load_document_state(&mut self, _main: &MainThread, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // A plist that does not decode fails here and again in `set_state`, so
        // the fallback cannot mask it.
        let from_document =
            self.unit()
                .set_plist::<ClassInfoFromDocument>(Scope::GLOBAL, Element::ZERO, data);
        if from_document.is_err() {
            return self.restore_state(data);
        }
        let _ = crate::listener::notify_all_parameters(self.unit());
        Ok(())
    }


    /// Writes the AU's current state to `path` as a `.aupreset` file — the format
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
    pub fn save_preset_file(
        &self,
        _main: &MainThread,
        path: &std::path::Path,
        name: &str,
    ) -> Result<()> {
        crate::aupreset::save_preset_file(self.unit(), self.handle.component(), path, name)
    }


    /// Loads a `.aupreset` file, **validating that it belongs to this AU** before
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
    /// reason [`set_state`](Self::set_state) documents: setting `ClassInfo`
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
        _main: &MainThread,
        path: &std::path::Path,
    ) -> Result<crate::aupreset::AuPresetIdentity> {
        let identity =
            crate::aupreset::load_preset_file(self.unit(), self.handle.component(), path)?;
        // As in `set_state`: the state is loaded, so a failed notify is a stale
        // editor rather than a failed load.
        //
        let _ = crate::listener::notify_all_parameters(self.unit());
        Ok(identity)
    }


    /// Configured maximum block size in frames.
    ///
    /// This is the bound the render enforces: a `num_frames`
    /// above it is [`AuError::InvalidBuffer`], because the AU allocated its
    /// internal buffers for this width at `AudioUnitInitialize` time and
    /// rendering wider writes past them.
    pub fn block_size(&self) -> u32 {
        self.config.block_size
    }



}
