//! The shared plugin hosted in this process, bounced, on the process's main thread.
//!
//! A bounce forks the plugin, and an in-process plugin forks on the main thread only
//! (`MainThreadMarker::new` asks the OS): the default harness runs every test on a thread it
//! spawns, so this target sets `harness = false` and owns `main()`
//! (`tutti_fixtures::main_thread_cases`). It is a binary of its own, not a module of
//! `instrument`: the shared plugin's entry points do not link into the binary that links the
//! reference probe, and that one is `instrument`'s neighbour.

use std::sync::Arc;
use tutti_export::Produced;
use tutti_plugin::catalog::{Hosting, PluginRef, PluginState};
use tutti_session::{
    Bounce, BounceLatency, BounceTail, ExportClock, PluginLoadRequest, Range, Root, Session, Stem,
};
use tutti_tempo::TempoMap;
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::{Beat, Bpm, ChannelLayout, MaxBlock, SampleRate, Samples, Seconds};
use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};

const RATE: SampleRate = SampleRate::SR_48K;

/// An offline session at [`RATE`] with blocks of up to 128 frames.
fn offline_session() -> Session {
    Session::builder()
        .sample_rate(RATE)
        .max_block(MaxBlock::of(Samples(128)))
        .build()
        .expect("an offline session builds")
}

/// The state that puts the shared plugin in `Notes` mode: its mode parameter set,
/// saved (`core.rs`, "State").
fn notes_state() -> Vec<u8> {
    let core = tutti_shared_plugin::core::Core::new();
    core.set_param(tutti_shared_plugin::core::MODE, 3.0 / 4.0);
    core.save()
}

/// **A plugin told its render mode only by reactivation still plays the
/// bounce's first block.** The shared plugin's CLAP shell has no
/// `clap.render`, so the bounce's fork is reactivated in offline mode when
/// it is built; that finishes before the fork renders, so the block with
/// the note has a Processor and the note sounds. The bounce fails a render
/// that dropped an event, so a note dropped unplayed fails it.
fn an_in_process_instrument_without_a_live_render_mode_bounces_its_clip() {
    let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
    let plugin = PluginRef::file(tutti_fixtures::publish_with_extension(&real, "clap"))
        .expect("a plugin file")
        .with_hosting(Hosting::InProcess);
    let mut session = offline_session();
    let request = PluginLoadRequest::new(plugin.clone())
        .restoring(PluginState::new(plugin, notes_state()))
        .expect("the state is the plugin's own");
    let main = tutti_fixtures::main_thread();
    let node = session
        .add_plugin_blocking(main, request)
        .expect("the shared plugin loads in process");
    session.graph().connect(node, Root).unwrap();
    let on = tutti_midi_runtime::TimedMidiEvent::new(
        Beat(1.0),
        Message::midi2(
            MidiChannel::FIRST,
            Voice2::note_on(NoteNumber::of(60), NoteVelocity::MAX),
        ),
    )
    .unwrap();
    session
        .midi()
        .play_clip(node, Beat(0.0), vec![on])
        .expect("the plugin takes a clip");
    let _ = session.update();
    // 90 BPM: beat 1 is 2/3 s in, inside the 1.5 s bounced.
    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo_map: Arc::new(TempoMap::constant(Bpm::of(90.0)).expect("a tempo")),
        sample_rate: RATE,
        loop_range: None,
    }));
    let bounce = Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(Seconds(0.0), Seconds(1.5)))
        .sample_rate(RATE)
        .clock(ExportClock::timeline(timeline))
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
        .stem_with(Stem::buffers(node.erase()));
    let planes = match session.bounce(bounce) {
        Ok(out) => match out.into_parts().1.pop() {
            Some(Produced::Buffers(rendered)) => rendered.into_planes(),
            other => panic!("expected one stem held in memory, got {other:?}"),
        },
        Err(e) => panic!("the bounce failed: {e:?}"),
    };
    // Notes mode renders the held key (60) on channel 0.
    assert!(
        planes[0].contains(&60.0),
        "the note played at beat 1 sounds in the bounce"
    );
}

fn main() {
    tutti_fixtures::main_thread_cases(tutti_fixtures::cases![
        an_in_process_instrument_without_a_live_render_mode_bounces_its_clip,
    ]);
}
