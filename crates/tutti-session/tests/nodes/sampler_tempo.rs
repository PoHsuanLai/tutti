//! A sampler node the session's graph adds reads the session's tempo map
//! (doc 020, stage 5), through the builder's engine. Needs the `sampler`
//! feature (`SessionNode` for the sampler's nodes).

use std::sync::Arc;
use tutti_types::MaxBlock;

use tutti_session::{OutPort, Root, Session};
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// An offline session at `RATE`, stereo, graph blocks of up to 1 024 frames.
fn offline() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(RATE),
            ChannelLayout::STEREO,
            MaxBlock::of(Samples(1_024)),
        )
        .expect("builds")
}

/// `frames` stereo frames of `s` in `block`-frame device blocks, the
/// transport rolling from the first block.
fn run(s: &Session, frames: usize, block: usize) -> Vec<f32> {
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0.0f32; block * 2];
    while out.len() < frames * 2 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

/// **A sampler node the graph adds reads the session's tempo map** (doc 020,
/// stage 5): `Graph::add` binds it to the session's reader
/// ([`SessionNode::bind_tempo`](tutti_session::SessionNode::bind_tempo)), so
/// a clip placed at beat 0 plays its samples one a frame through a step in
/// the map instead of being re-seated at the new tempo. The wave is a ramp
/// (sample `i` is `i / 2^17`), 120 BPM and 60 from beat 1 (frame 24 000): at
/// every frame the output is source frame `f` (the map's definition: a clip
/// from frame 0 is at frame `f`), where a clip re-seated at the step would
/// jump from source frame 23 999 to 48 000.
#[test]
fn a_sampler_node_added_to_the_graph_follows_the_sessions_tempo_map() {
    use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoicePool, VoiceSource};
    use tutti_tempo::{Shape, TempoMap, TempoPoint};
    use tutti_types::{Beat, Bpm};

    let mut wave = tutti_codec::Wave::new(1, SampleRate::new(RATE));
    for i in 0..RATE as usize * 2 {
        wave.push_frame(&[i as f32 / 131_072.0]);
    }
    let source = MemorySource::placed(Arc::new(wave), Beat(0.0), None).expect("a placeable wave");
    let mut pool = VoicePool::new();
    pool.insert_voice(
        SlotId(1),
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback::default(),
        },
    );
    let mut s = offline();
    let map =
        TempoMap::new(
            [(0.0, 120.0), (1.0, 60.0)]
                .into_iter()
                .map(|(beat, bpm)| TempoPoint {
                    beat: Beat(beat),
                    tempo: Bpm::of(bpm),
                    shape: Shape::Step,
                }),
        )
        .expect("a map");
    s.transport().settings().set_tempo_map(Arc::new(map));
    let (voice, _handle) = s.graph().add(pool);
    for port in 0..2 {
        s.graph()
            .set_sources_hard(Root::output(port), [OutPort::new(voice, port)])
            .unwrap();
    }
    let _ = s.update();
    let out = run(&s, 60_000, 512);
    for f in 0..60_000 {
        let want = f as f32 / 131_072.0;
        assert_eq!(out[2 * f].to_bits(), want.to_bits(), "frame {f}");
    }
}
