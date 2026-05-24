#[cfg(feature = "midi")]
use bevy_ecs::message::MessageWriter;
#[cfg(feature = "midi")]
use bevy_ecs::prelude::*;
#[cfg(feature = "midi-hardware")]
use bevy_log::warn;

#[cfg(feature = "midi")]
use super::events::MidiInputEvent;

#[cfg(feature = "midi")]
use super::components::MidiReceiver;

#[cfg(feature = "mpe")]
use super::components::MpeReceiver;

#[cfg(feature = "midi-hardware")]
use super::components::{ConnectMidiDevice, DisconnectMidiDevice};
#[cfg(feature = "midi-hardware")]
use super::events::MidiDeviceEvent;

#[cfg(feature = "midi")]
#[derive(Resource)]
pub struct MidiInputObserver {
    pub(crate) receiver: crossbeam_channel::Receiver<tutti::midi::MidiEvent>,
}

#[cfg(feature = "midi")]
#[derive(Resource)]
pub(crate) struct MidiObserverSender {
    pub(crate) sender: Option<crossbeam_channel::Sender<tutti::midi::MidiEvent>>,
}

#[cfg(feature = "midi-hardware")]
#[derive(Resource, Default)]
pub struct MidiDeviceState {
    pub(crate) connected: std::collections::HashSet<String>,
    pub(crate) last_check: Option<std::time::Instant>,
}

/// Sets up the UI observer on the hardware MIDI input port, funneling events
/// into `MidiInputObserver`'s channel. No-op when `midi-hardware` is disabled
/// (there's no hardware port to observe).
#[cfg(feature = "midi")]
pub(crate) fn midi_observer_setup_system(
    #[cfg(feature = "midi-hardware")] midi_io: Option<Res<crate::MidiIoRes>>,
    mut sender_res: ResMut<MidiObserverSender>,
) {
    let Some(sender) = sender_res.sender.take() else {
        return;
    };

    #[cfg(feature = "midi-hardware")]
    {
        let Some(midi_io) = midi_io else { return };
        midi_io.0.set_input_observer(sender);
    }

    #[cfg(not(feature = "midi-hardware"))]
    {
        let _ = sender;
    }
}

#[cfg(feature = "midi")]
pub fn midi_input_event_system(
    observer: Option<Res<MidiInputObserver>>,
    mut writer: MessageWriter<MidiInputEvent>,
) {
    let Some(observer) = observer else { return };
    while let Ok(event) = observer.receiver.try_recv() {
        writer.write(MidiInputEvent(event));
    }
}

#[cfg(feature = "midi")]
pub fn midi_routing_sync_system(
    graph: Option<ResMut<crate::TuttiGraphRes>>,
    changed: Query<&MidiReceiver, Changed<MidiReceiver>>,
    all_receivers: Query<&MidiReceiver>,
    mut removed: RemovedComponents<MidiReceiver>,
    #[cfg(feature = "mpe")] mpe_changed: Query<&MpeReceiver, Changed<MpeReceiver>>,
    #[cfg(feature = "mpe")] all_mpe_receivers: Query<&MpeReceiver>,
    #[cfg(feature = "mpe")] mut mpe_removed: RemovedComponents<MpeReceiver>,
) {
    let Some(mut graph) = graph else { return };

    #[allow(unused_mut)]
    let mut has_changes = !changed.is_empty() || removed.read().next().is_some();

    #[cfg(feature = "mpe")]
    {
        has_changes = has_changes || !mpe_changed.is_empty() || mpe_removed.read().next().is_some();
    }

    if !has_changes {
        return;
    }

    let table = graph.0.midi_route_mut();
    table.clear();
    for receiver in all_receivers.iter() {
        let unit_id = tutti::core::MidiUnitId::new(receiver.node_id.value());
        if let Some(ch) = receiver.channel {
            table.channel(ch, unit_id);
        } else {
            table.fallback(unit_id);
        }
    }

    // MPE receivers route all channels to one synth via fallback
    #[cfg(feature = "mpe")]
    for mpe_recv in all_mpe_receivers.iter() {
        table.fallback(tutti::core::MidiUnitId::new(mpe_recv.node_id.value()));
    }

    // `commit()` on TuttiGraph publishes both graph edits and the MIDI
    // routing table snapshot in one step.
    graph.0.commit();
}

#[cfg(feature = "midi-hardware")]
pub fn midi_device_connect_system(
    mut commands: Commands,
    midi_io: Option<Res<crate::MidiIoRes>>,
    connect_query: Query<(Entity, &ConnectMidiDevice), Added<ConnectMidiDevice>>,
    disconnect_query: Query<(Entity, &DisconnectMidiDevice), Added<DisconnectMidiDevice>>,
    mut device_events: MessageWriter<MidiDeviceEvent>,
    mut state: ResMut<MidiDeviceState>,
) {
    let Some(midi_io) = midi_io else { return };

    for (entity, connect) in connect_query.iter() {
        match midi_io.0.connect_input_by_name(&connect.name) {
            Ok(()) => {
                if state.connected.insert(connect.name.clone()) {
                    device_events.write(MidiDeviceEvent::Connected {
                        name: connect.name.clone(),
                    });
                }
            }
            Err(e) => {
                warn!("Failed to connect MIDI device '{}': {}", connect.name, e);
            }
        }
        commands.entity(entity).remove::<ConnectMidiDevice>();
    }

    for (entity, disconnect) in disconnect_query.iter() {
        midi_io.0.disconnect_input(&disconnect.name);
        if state.connected.remove(&disconnect.name) {
            device_events.write(MidiDeviceEvent::Disconnected {
                name: disconnect.name.clone(),
            });
        }
        commands.entity(entity).remove::<DisconnectMidiDevice>();
    }
}

/// Polls every 2s; emits `MidiDeviceEvent::Disconnected` for each device that
/// vanished and `Connected` for any new device that appeared (e.g., a hot-plug
/// or an external connection through another part of the app).
#[cfg(feature = "midi-hardware")]
pub fn midi_device_poll_system(
    midi_io: Option<Res<crate::MidiIoRes>>,
    mut state: ResMut<MidiDeviceState>,
    mut device_events: MessageWriter<MidiDeviceEvent>,
) {
    let Some(midi_io) = midi_io else { return };
    let now = std::time::Instant::now();

    if let Some(last) = state.last_check {
        if now.duration_since(last).as_secs() < 2 {
            return;
        }
    }
    state.last_check = Some(now);

    let live: std::collections::HashSet<String> =
        midi_io.0.connected_input_names().into_iter().collect();

    for name in state.connected.difference(&live).cloned().collect::<Vec<_>>() {
        state.connected.remove(&name);
        device_events.write(MidiDeviceEvent::Disconnected { name });
    }
    for name in live.difference(&state.connected).cloned().collect::<Vec<_>>() {
        state.connected.insert(name.clone());
        device_events.write(MidiDeviceEvent::Connected { name });
    }
}

// =========================================================================
// MIDI Sequence playback
// =========================================================================

#[cfg(feature = "midi")]
use super::components::MidiSequence;

/// Tracks which notes are currently sounding for a [`MidiSequence`].
#[cfg(feature = "midi")]
#[derive(Component, Default)]
pub struct MidiSequenceState {
    active_notes: std::collections::HashSet<u8>,
}

/// Auto-inserts [`MidiSequenceState`] on entities that have [`MidiSequence`]
/// but not yet a state component.
#[cfg(feature = "midi")]
pub fn midi_sequence_setup_system(
    mut commands: Commands,
    query: Query<Entity, (With<MidiSequence>, Without<MidiSequenceState>)>,
) {
    for entity in query.iter() {
        commands.entity(entity).insert(MidiSequenceState::default());
    }
}

/// Ticks all [`MidiSequence`] entities, firing note_on/note_off based on
/// the transport's current beat position.
#[cfg(feature = "midi")]
pub fn midi_sequence_tick_system(
    transport: Option<Res<crate::TransportRes>>,
    midi: Option<Res<crate::MidiBusRes>>,
    mut query: Query<(&MidiSequence, &mut MidiSequenceState)>,
) {
    let Some(transport) = transport else { return };
    let Some(midi) = midi else { return };

    if !transport.0.is_playing() {
        // All-notes-off when transport is not rolling
        for (seq, mut state) in query.iter_mut() {
            let unit_id = tutti::core::MidiUnitId::new(seq.target.value());
            for note in state.active_notes.drain() {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
        }
        return;
    }

    let beat = transport.0.current_beat();

    for (seq, mut state) in query.iter_mut() {
        let unit_id = tutti::core::MidiUnitId::new(seq.target.value());
        let local_beat = if seq.loop_enabled && seq.duration_beats > 0.0 {
            let offset = beat - seq.start_beat;
            ((offset % seq.duration_beats) + seq.duration_beats) % seq.duration_beats
        } else {
            beat - seq.start_beat
        };

        // Outside range (non-looped)
        if !seq.loop_enabled && (local_beat < 0.0 || local_beat > seq.duration_beats) {
            for note in state.active_notes.drain() {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
            continue;
        }

        // Determine which notes should be active at this beat
        let mut should_be_active = std::collections::HashSet::new();
        for n in &seq.notes {
            if local_beat >= n.start && local_beat < n.start + n.duration {
                should_be_active.insert(n.note);
            }
        }

        // Note-off for notes that ended
        for &note in &state.active_notes {
            if !should_be_active.contains(&note) {
                let event = note_off_event(note);
                midi.0.queue(unit_id, &[event]);
            }
        }

        // Note-on for newly active notes
        for n in &seq.notes {
            if should_be_active.contains(&n.note) && !state.active_notes.contains(&n.note) {
                let event = note_on_event(n.note, n.velocity);
                midi.0.queue(unit_id, &[event]);
            }
        }

        state.active_notes = should_be_active;
    }
}

/// Channel-0 MIDI 2.0 note-on event with a 7-bit MIDI 1 velocity
/// (upconverted to the 16-bit MIDI 2 velocity range).
#[cfg(feature = "midi")]
fn note_on_event(note: u8, velocity_midi1: u8) -> tutti::midi::MidiEvent {
    tutti::midi::MidiEvent::note_on(0, 0, note, (velocity_midi1 as u16) << 9)
}

#[cfg(feature = "midi")]
fn note_off_event(note: u8) -> tutti::midi::MidiEvent {
    tutti::midi::MidiEvent::note_off(0, 0, note, 0)
}

/// Live per-note MPE expression state, wrapping an
/// [`Arc<tutti::midi_runtime::PerNoteExpression>`] from a tutti-side
/// [`MpeProcessor`].
///
/// The Arc is lock-free and safe to read from any thread; the writer
/// is the audio (or MIDI-input) thread feeding the `MpeProcessor`.
/// When MPE is disabled or no processor has been wired yet, this
/// resource is `Disabled` and the readers return defaults.
///
/// # Lifecycle
///
/// `mpe_setup_system` initialises this resource as `Disabled`. App
/// code that owns an `MpeProcessor` calls
/// [`MpeExpressionResource::set_expression`] with the processor's
/// `Arc<PerNoteExpression>` to switch the resource into the `Live`
/// variant. The processor itself is fed MIDI events by whoever owns
/// it (audio thread, UI thread observer, etc.) — wiring an integrated
/// engine-side feed is a follow-up; this resource only handles the
/// *read* side.
#[cfg(feature = "mpe")]
#[derive(Resource, Default, Clone)]
pub struct MpeExpressionResource(Option<std::sync::Arc<tutti::midi_runtime::PerNoteExpression>>);

#[cfg(feature = "mpe")]
#[allow(
    dead_code,
    reason = "Public surface that callers (downstream apps) flip to live by passing \
              an MpeProcessor's expression handle. No in-tree consumer yet."
)]
impl MpeExpressionResource {
    /// Construct from an existing processor's expression handle.
    pub fn from_expression(expr: std::sync::Arc<tutti::midi_runtime::PerNoteExpression>) -> Self {
        Self(Some(expr))
    }

    /// Replace the expression backing. Pass `None` to disable.
    pub fn set_expression(&mut self, expr: Option<std::sync::Arc<tutti::midi_runtime::PerNoteExpression>>) {
        self.0 = expr;
    }

    /// Combined per-note + global pitch bend, -1.0..=1.0.
    /// Returns 0.0 when no processor is wired.
    pub fn pitch_bend(&self, note: u8) -> f32 {
        self.0
            .as_ref()
            .map(|e| e.get_pitch_bend(note))
            .unwrap_or(0.0)
    }

    /// max(per-note, global) pressure, 0.0..=1.0. Returns 0.0 when
    /// no processor is wired.
    pub fn pressure(&self, note: u8) -> f32 {
        self.0
            .as_ref()
            .map(|e| e.get_pressure(note))
            .unwrap_or(0.0)
    }

    /// CC74 slide (timbre / brightness), 0.0..=1.0. Returns the CC74
    /// rest position (0.5) when no processor is wired.
    pub fn slide(&self, note: u8) -> f32 {
        self.0.as_ref().map(|e| e.get_slide(note)).unwrap_or(0.5)
    }

    /// Whether the note is currently held. `false` when no processor
    /// is wired.
    pub fn is_note_active(&self, note: u8) -> bool {
        self.0.as_ref().map(|e| e.is_active(note)).unwrap_or(false)
    }

    /// Whether a processor has been wired.
    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Direct access to the underlying expression handle, if wired.
    /// Useful for tests and for callers that want to share the Arc.
    pub fn expression(&self) -> Option<std::sync::Arc<tutti::midi_runtime::PerNoteExpression>> {
        self.0.clone()
    }
}

#[cfg(all(feature = "mpe", test))]
mod mpe_tests {
    use super::*;

    #[test]
    fn unwired_returns_defaults() {
        let r = MpeExpressionResource::default();
        assert_eq!(r.pitch_bend(60), 0.0);
        assert_eq!(r.pressure(60), 0.0);
        assert_eq!(r.slide(60), 0.5);
        assert!(!r.is_note_active(60));
        assert!(!r.is_enabled());
    }

    #[test]
    fn wired_round_trips_expression() {
        let expr = std::sync::Arc::new(tutti::midi_runtime::PerNoteExpression::new());
        expr.note_on(60);
        expr.set_pitch_bend(60, 0.5);
        expr.set_pressure(60, 0.75);
        expr.set_slide(60, 0.25);

        let r = MpeExpressionResource::from_expression(expr);
        assert!(r.is_enabled());
        assert!(r.is_note_active(60));
        assert!((r.pitch_bend(60) - 0.5).abs() < 1e-6);
        assert!((r.pressure(60) - 0.75).abs() < 1e-6);
        assert!((r.slide(60) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn mpe_setup_with_mode_installs_processor_on_bus() {
        // End-to-end: MidiBusRes + MpeModeConfig present at startup.
        // The setup system installs the processor on the bus and the
        // resource exposes the live PerNoteExpression. Queueing a
        // note-on through the bus updates the resource's read.
        use bevy_ecs::prelude::*;
        use tutti::midi::{MidiEvent, MpeMode, MpeZoneConfig};
        use tutti::midi_runtime::MidiBus;
        use tutti::core::MidiUnitId;

        let mut world = World::new();
        let bus = MidiBus::new();
        world.insert_resource(crate::resources::MidiBusRes(bus.clone()));
        world.insert_resource(MpeModeConfig(MpeMode::LowerZone(MpeZoneConfig::lower(15))));

        let mut schedule = Schedule::default();
        schedule.add_systems(mpe_setup_system);
        schedule.run(&mut world);

        let r = world.resource::<MpeExpressionResource>();
        assert!(r.is_enabled(), "setup should install live expression");

        // Subscribe a unit so the bus has somewhere to deliver to.
        let id = MidiUnitId::new(1);
        let (sender, _recv) = tutti::midi_runtime::MidiEventSlot::pair(id);
        bus.insert(sender);

        let note_on = MidiEvent::note_on(0, 2, 60, 100u16 << 9);
        bus.queue(id, &[note_on]);

        assert!(r.is_note_active(60), "note 60 should be active after queue");
    }

    #[test]
    fn mpe_setup_disabled_mode_inserts_default_resource() {
        use bevy_ecs::prelude::*;
        use tutti::midi::MpeMode;
        use tutti::midi_runtime::MidiBus;

        let mut world = World::new();
        world.insert_resource(crate::resources::MidiBusRes(MidiBus::new()));
        world.insert_resource(MpeModeConfig(MpeMode::Disabled));

        let mut schedule = Schedule::default();
        schedule.add_systems(mpe_setup_system);
        schedule.run(&mut world);

        let r = world.resource::<MpeExpressionResource>();
        assert!(!r.is_enabled(), "Disabled mode → resource is disabled");
    }
}

/// Configures the MPE mode that [`mpe_setup_system`] installs at
/// startup. Insert this *before* `TuttiPlugin` runs to override the
/// default. Default is [`MpeMode::Disabled`] — apps that want MPE
/// installation flip this to `LowerZone` / `UpperZone` / `DualZone`.
#[cfg(feature = "mpe")]
#[derive(bevy_ecs::resource::Resource, Debug, Clone)]
pub struct MpeModeConfig(pub tutti::midi::MpeMode);

#[cfg(feature = "mpe")]
impl Default for MpeModeConfig {
    fn default() -> Self {
        Self(tutti::midi::MpeMode::Disabled)
    }
}

/// Initialise [`MpeExpressionResource`].
///
/// If [`MpeModeConfig`] is set to anything other than `Disabled` and
/// a [`MidiBusRes`](crate::resources::MidiBusRes) is present, install an
/// [`MpeProcessor`](tutti::midi_runtime::MpeProcessor) on the bus and
/// hand its `Arc<PerNoteExpression>` to the resource. Otherwise
/// inserts the resource in disabled state — `MpeExpressionResource`
/// then returns defaults from every reader.
#[cfg(feature = "mpe")]
pub(crate) fn mpe_setup_system(mut commands: Commands, world: &World) {
    use tutti::midi_runtime::MpeProcessor;

    let mode = world
        .get_resource::<MpeModeConfig>()
        .map(|c| c.0.clone())
        .unwrap_or(tutti::midi::MpeMode::Disabled);

    if matches!(mode, tutti::midi::MpeMode::Disabled) {
        commands.insert_resource(MpeExpressionResource::default());
        return;
    }

    let Some(bus) = world.get_resource::<crate::resources::MidiBusRes>() else {
        commands.insert_resource(MpeExpressionResource::default());
        return;
    };

    let processor = MpeProcessor::new(mode);
    let expression = bus.0.install_mpe(processor);
    commands.insert_resource(MpeExpressionResource::from_expression(expression));
}
