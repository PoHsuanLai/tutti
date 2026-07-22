//! MIDI Polyphonic Expression (MPE) read-side integration.
//!
//! [`mpe_setup_system`] reads [`MpeModeConfig`] at
//! startup and, if MPE is enabled and a [`MidiBusRes`](crate::MidiBusRes) is
//! present, installs a [`MpeProcessor`](tutti_midi_runtime::MpeProcessor) on the
//! bus and hands its `Arc<PerNoteExpression>` to [`MpeExpressionResource`] —
//! the lock-free read surface that synth voices sample per-note pitch-bend /
//! pressure / slide from.

use bevy_app::{App, Plugin, Startup};
use bevy_ecs::prelude::*;

/// Live per-note MPE expression state, wrapping an
/// [`Arc<tutti_midi_runtime::PerNoteExpression>`] from a tutti-side
/// [`MpeProcessor`](tutti_midi_runtime::MpeProcessor).
///
/// The Arc is lock-free and safe to read from any thread; the writer
/// is the audio (or MIDI-input) thread feeding the `MpeProcessor`.
/// When MPE is disabled or no processor has been wired yet, this
/// resource is `Disabled` and the readers return defaults.
///
/// # Lifecycle
///
/// [`mpe_setup_system`] initialises this resource as `Disabled`. App
/// code that owns an `MpeProcessor` calls
/// [`MpeExpressionResource::set_expression`] with the processor's
/// `Arc<PerNoteExpression>` to switch the resource into the `Live`
/// variant. The processor itself is fed MIDI events by whoever owns
/// it (audio thread, UI thread observer, etc.) — wiring an integrated
/// engine-side feed is a follow-up; this resource only handles the
/// *read* side.
#[derive(Resource, Default, Clone)]
pub struct MpeExpressionResource(Option<std::sync::Arc<tutti_midi_runtime::PerNoteExpression>>);

impl MpeExpressionResource {
    /// Construct from an existing processor's expression handle.
    pub fn from_expression(expr: std::sync::Arc<tutti_midi_runtime::PerNoteExpression>) -> Self {
        Self(Some(expr))
    }

    /// Replace the expression backing. Pass `None` to disable.
    pub fn set_expression(
        &mut self,
        expr: Option<std::sync::Arc<tutti_midi_runtime::PerNoteExpression>>,
    ) {
        self.0 = expr;
    }

    /// Combined per-note + global pitch bend, -1.0..=1.0, for the note addressed
    /// by `id`. Returns 0.0 when no processor is wired.
    pub fn pitch_bend(&self, id: tutti_midi_types::NoteId) -> f32 {
        self.0
            .as_ref()
            .map(|e| e.get_pitch_bend(id))
            .unwrap_or(0.0)
    }

    /// max(per-note, global) pressure, 0.0..=1.0, for the note addressed by `id`.
    /// Returns 0.0 when no processor is wired.
    pub fn pressure(&self, id: tutti_midi_types::NoteId) -> f32 {
        self.0.as_ref().map(|e| e.get_pressure(id)).unwrap_or(0.0)
    }

    /// CC74 slide (timbre / brightness), 0.0..=1.0, for the note addressed by
    /// `id`. Returns the CC74 rest position (0.5) when no processor is wired.
    pub fn slide(&self, id: tutti_midi_types::NoteId) -> f32 {
        self.0.as_ref().map(|e| e.get_slide(id)).unwrap_or(0.5)
    }

    /// Whether the note addressed by `id` is currently held. `false` when no
    /// processor is wired.
    pub fn is_note_active(&self, id: tutti_midi_types::NoteId) -> bool {
        self.0.as_ref().map(|e| e.is_active(id)).unwrap_or(false)
    }

    /// Whether a processor has been wired.
    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Direct access to the underlying expression handle, if wired.
    /// Useful for tests and for callers that want to share the Arc.
    pub fn expression(&self) -> Option<std::sync::Arc<tutti_midi_runtime::PerNoteExpression>> {
        self.0.clone()
    }
}

/// Configures the MPE mode that [`mpe_setup_system`] installs at
/// startup. Insert this *before* `TuttiPlugin` runs to override the
/// default. Default is [`MpeMode::Disabled`](crate::MpeMode::Disabled) — apps
/// that want MPE installation flip this to `LowerZone` / `UpperZone` / `DualZone`.
#[derive(bevy_ecs::resource::Resource, Debug, Clone)]
pub struct MpeModeConfig(pub crate::MpeMode);

impl Default for MpeModeConfig {
    fn default() -> Self {
        Self(crate::MpeMode::Disabled)
    }
}

/// Initialise [`MpeExpressionResource`].
///
/// If [`MpeModeConfig`] is set to anything other than `Disabled` and
/// a [`MidiBusRes`](crate::MidiBusRes) is present, install an
/// [`MpeProcessor`](tutti_midi_runtime::MpeProcessor) on the bus and
/// hand its `Arc<PerNoteExpression>` to the resource. Otherwise
/// inserts the resource in disabled state — `MpeExpressionResource`
/// then returns defaults from every reader.
pub(crate) fn mpe_setup_system(mut commands: Commands, world: &World) {
    use tutti_midi_runtime::MpeProcessor;

    let mode = world
        .get_resource::<MpeModeConfig>()
        .map(|c| c.0)
        .unwrap_or(crate::MpeMode::Disabled);

    if matches!(mode, crate::MpeMode::Disabled) {
        commands.insert_resource(MpeExpressionResource::default());
        return;
    }

    let Some(bus) = world.get_resource::<crate::MidiBusRes>() else {
        commands.insert_resource(MpeExpressionResource::default());
        return;
    };

    let processor = MpeProcessor::new(mode);
    let expression = bus.0.install_mpe(processor);
    commands.insert_resource(MpeExpressionResource::from_expression(expression));
}

/// Installs the MPE processor (per [`MpeModeConfig`]) and publishes the
/// read-side [`MpeExpressionResource`] at startup.
pub struct MpePlugin;

impl Plugin for MpePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, mpe_setup_system);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwired_returns_defaults() {
        let id = tutti_midi_types::NoteId::from_channel_note(0, 60);
        let r = MpeExpressionResource::default();
        assert_eq!(r.pitch_bend(id), 0.0);
        assert_eq!(r.pressure(id), 0.0);
        assert_eq!(r.slide(id), 0.5);
        assert!(!r.is_note_active(id));
        assert!(!r.is_enabled());
    }

    #[test]
    fn wired_round_trips_expression() {
        let id = tutti_midi_types::NoteId::from_channel_note(0, 60);
        let expr = std::sync::Arc::new(tutti_midi_runtime::PerNoteExpression::new());
        expr.note_on(id);
        expr.set_pitch_bend(id, 0.5);
        expr.set_pressure(id, 0.75);
        expr.set_slide(id, 0.25);

        let r = MpeExpressionResource::from_expression(expr);
        assert!(r.is_enabled());
        assert!(r.is_note_active(id));
        assert!((r.pitch_bend(id) - 0.5).abs() < 1e-6);
        assert!((r.pressure(id) - 0.75).abs() < 1e-6);
        assert!((r.slide(id) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn mpe_setup_with_mode_installs_processor_on_bus() {
        // End-to-end: MidiBusRes + MpeModeConfig present at startup.
        // The setup system installs the processor on the bus and the
        // resource exposes the live PerNoteExpression. Queueing a
        // note-on through the bus updates the resource's read.
        use crate::{MidiEvent, MpeMode, MpeZoneConfig};
        use tutti_midi_runtime::MidiBus;
        use tutti_midi_types::MidiUnitId;

        let mut world = World::new();
        let bus = MidiBus::new();
        world.insert_resource(crate::MidiBusRes(bus.clone()));
        world.insert_resource(MpeModeConfig(MpeMode::LowerZone(MpeZoneConfig::lower(15))));

        let mut schedule = Schedule::default();
        schedule.add_systems(mpe_setup_system);
        schedule.run(&mut world);

        let r = world.resource::<MpeExpressionResource>();
        assert!(r.is_enabled(), "setup should install live expression");

        // Subscribe a unit so the bus has somewhere to deliver to.
        let id = MidiUnitId::new(1);
        let (sender, _recv) = tutti_midi_runtime::MidiEventSlot::pair(id);
        bus.insert(sender);

        let note_on =
            MidiEvent::note_on(0, 2, 60, tutti_midi_types::convert::midi1_velocity_to_midi2(100));
        bus.queue(id, &[note_on]);

        // Channel 2 is a lower-zone member; the processor keys expression by the
        // (channel, note) identity.
        let n60 = tutti_midi_types::NoteId::from_channel_note(2, 60);
        assert!(r.is_note_active(n60), "note 60 should be active after queue");
    }

    #[test]
    fn mpe_setup_disabled_mode_inserts_default_resource() {
        use crate::MpeMode;
        use tutti_midi_runtime::MidiBus;

        let mut world = World::new();
        world.insert_resource(crate::MidiBusRes(MidiBus::new()));
        world.insert_resource(MpeModeConfig(MpeMode::Disabled));

        let mut schedule = Schedule::default();
        schedule.add_systems(mpe_setup_system);
        schedule.run(&mut world);

        let r = world.resource::<MpeExpressionResource>();
        assert!(!r.is_enabled(), "Disabled mode → resource is disabled");
    }
}
