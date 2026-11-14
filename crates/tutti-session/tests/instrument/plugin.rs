//! The instrument contract (`instrument_support`) against Plugin.

contract!(crate::instrument_support::subjects::Plugin, gaps = 0);

/// Bounces of the shared plugin hosted in this process. Here rather than in
/// the `plugin` binary: that one links the reference probe, and two plugins'
/// entry points do not link into one binary.
#[cfg(feature = "export")]
mod bounce {
    use std::sync::Arc;
    use tutti_export::Produced;
    use tutti_plugin::catalog::{Hosting, PluginRef, PluginState};
    use tutti_session::{
        Bounce, BounceLatency, BounceTail, ExportClock, PluginLoadRequest, Range, Root, Stem,
    };
    use tutti_tempo::TempoMap;
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_types::{Beat, Bpm, ChannelLayout, Seconds};
    use tutti_ump::MidiChannel;

    use crate::instrument_support::rig::{RATE, offline_session};
    use crate::instrument_support::subjects::Plugin;
    use tutti_ump::{Message, NoteNumber, NoteVelocity, Voice2};

    /// **A plugin told its render mode only by reactivation still plays the
    /// bounce's first block.** The shared plugin's CLAP shell has no
    /// `clap.render`, so the bounce's fork is reactivated in offline mode when
    /// it is built; that finishes before the fork renders, so the block with
    /// the note has a Processor and the note sounds. The bounce fails a render
    /// that dropped an event, so a note dropped unplayed fails it.
    #[test]
    fn an_in_process_instrument_without_a_live_render_mode_bounces_its_clip() {
        let real = tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin");
        let plugin = PluginRef::file(tutti_fixtures::publish_with_extension(&real, "clap"))
            .expect("a plugin file")
            .with_hosting(Hosting::InProcess);
        let mut session = offline_session();
        let request = PluginLoadRequest::new(plugin.clone())
            .restoring(PluginState::new(plugin, Plugin::notes_state()))
            .expect("the state is the plugin's own");
        let main = tutti_plugin_types::MainThread::claim().expect("one test, one process");
        let node = session
            .add_plugin_blocking(&main, request)
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
}
