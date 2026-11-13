//! The experiment against a fake that does exactly what a test says: rings a
//! set number of frames after its trigger, delays its onset, adds a constant
//! above or below the floor. The oracles are the figures the fake was built
//! with, never another render.

use super::*;
use tutti_graph::MAX_NODE_TAIL;
use tutti_plugin_types::{AudioBufferMut, PluginResult, Processing};

/// A processor that answers a trigger with `delay` frames of nothing, then a
/// held `LEVEL` for `ring` more frames. An effect is triggered by any non-zero
/// input frame, an instrument by its note-on (held until the note-off, then
/// `ring` more frames). `bias` is added to every output frame, always.
struct Fake {
    delay: usize,
    ring: usize,
    bias: f32,
    age: Option<usize>,
    held: bool,
}

const LEVEL: f32 = 0.1;

impl Fake {
    fn new(delay: usize, ring: usize) -> Self {
        Self {
            delay,
            ring,
            bias: 0.0,
            age: None,
            held: false,
        }
    }

    fn with_bias(mut self, bias: f32) -> Self {
        self.bias = bias;
        self
    }

    /// The next frame's output, from the state.
    fn tick(&mut self) -> f32 {
        let sound = if self.held {
            LEVEL
        } else {
            match self.age {
                Some(a) if (self.delay..=self.delay + self.ring).contains(&a) => LEVEL,
                _ => 0.0,
            }
        };
        if let Some(a) = &mut self.age {
            *a += 1;
        }
        sound + self.bias
    }
}

impl Processor for Fake {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let AudioBufferMut::F32(buf) = &mut io.buffer else {
            unreachable!("the probe renders 32-bit");
        };
        let mut events = io.ctx.midi_events.iter().peekable();
        for i in 0..buf.num_samples {
            while let Some((offset, ump)) = events.next_if(|(o, _)| o.index() == i) {
                let _ = offset;
                // A note-on is a status nibble 9 (MIDI 2.0 type 4 word 0, bits 20..24).
                if (ump.0[0] >> 20) & 0xF == 0x9 {
                    self.held = true;
                } else {
                    self.held = false;
                    self.age = Some(0);
                }
            }
            if buf.inputs.iter().any(|ch| ch[i] != 0.0) {
                self.age = Some(0);
            }
            let v = self.tick();
            for ch in buf.outputs.iter_mut() {
                ch[i] = v;
            }
        }
        Ok(())
    }

    fn stop(&mut self) {}

    fn reset(&mut self) {
        self.age = None;
        self.held = false;
    }
}

fn config(rate: SampleRate, block: MaxBlock) -> ProbeConfig {
    ProbeConfig {
        rate,
        block,
        floor: PROBE_FLOOR,
        state: StateHash::of(b""),
    }
}

fn primary() -> ProbeConfig {
    config(CONFIGS[0].0, CONFIGS[0].1)
}

fn effect(tail: PluginTail, latency: usize) -> Subject {
    Subject {
        inputs: 2,
        outputs: 2,
        instrument: false,
        tail,
        latency: Samples(latency),
    }
}

fn instrument(tail: PluginTail) -> Subject {
    Subject {
        inputs: 0,
        outputs: 2,
        instrument: true,
        tail,
        latency: Samples(0),
    }
}

fn finite(n: usize) -> PluginTail {
    PluginTail::Finite(Samples(n))
}

/// **A plugin that rings exactly as long as it says is `Honest`**, and the
/// figure is the ring (the last frame above the floor, from the impulse).
#[test]
fn a_tail_that_equals_the_claim_is_honest() {
    let m = measure_at(&mut Fake::new(0, 8191), effect(finite(8191), 0), primary()).unwrap();
    assert_eq!(m.tail, Some(Samples(8191)));
    assert_eq!(m.verdict.tail, TailVerdict::Honest);
    assert_eq!(m.latency, Some(Samples(0)));
    assert_eq!(m.verdict.latency, LatencyVerdict::Honest);
    assert!(m.verdict.is_honest());
}

/// **A plugin that rings longer than it says is `TailLonger`** with both
/// figures, and a claim one frame short is enough.
#[test]
fn a_tail_longer_than_the_claim_is_tail_longer() {
    for (claim, ring) in [(8191, 8191 + 4096), (8190, 8191)] {
        let m = measure_at(&mut Fake::new(0, ring), effect(finite(claim), 0), primary()).unwrap();
        assert_eq!(
            m.verdict.tail,
            TailVerdict::TailLonger {
                reported: Samples(claim),
                measured: Samples(ring)
            },
            "claim {claim}, ring {ring}"
        );
    }
}

/// **A claim longer than the ring is not a lie**: the claim is a ceiling.
#[test]
fn a_claim_longer_than_the_ring_is_honest() {
    let m = measure_at(&mut Fake::new(0, 100), effect(finite(8191), 0), primary()).unwrap();
    assert_eq!(m.verdict.tail, TailVerdict::Honest);
    assert_eq!(m.tail, Some(Samples(100)));
}

/// **The figure does not depend on where the block edges fall**: the same ring
/// at both configurations, one a block of 512 and one of 64, at two rates.
#[test]
fn the_figure_is_the_same_at_every_configuration() {
    for (rate, block) in CONFIGS {
        let m = measure_at(
            &mut Fake::new(0, 8191),
            effect(finite(8191), 0),
            config(rate, block),
        )
        .unwrap();
        assert_eq!(m.tail, Some(Samples(8191)), "{rate:?} {block:?}");
        assert_eq!(m.config.block, block);
    }
}

/// **A plugin whose output stays above the floor with silence in is
/// `NeverSettles`, not `TailLonger`**, and nothing else is judged.
#[test]
fn a_plugin_that_makes_its_own_sound_never_settles() {
    let bias = Db(-60.0).to_amplitude().get();
    let m = measure_at(
        &mut Fake::new(0, 8191).with_bias(bias),
        effect(finite(8191), 0),
        primary(),
    )
    .unwrap();
    let TailVerdict::NeverSettles { level } = m.verdict.tail else {
        panic!("{:?}", m.verdict.tail);
    };
    assert!(
        (level.get() - -60.0).abs() < 0.01,
        "the level it held: {level:?}"
    );
    assert!(matches!(
        m.verdict.latency,
        LatencyVerdict::NeverSettles { .. }
    ));
    assert_eq!((m.tail, m.latency), (None, None));
}

/// **The floor is `-100 dB`, strictly**: a constant under it is silence, one
/// over it is sound.
#[test]
fn the_floor_is_the_boundary_of_sound() {
    let under = Db(-101.0).to_amplitude().get();
    let over = Db(-99.0).to_amplitude().get();
    let quiet = measure_at(
        &mut Fake::new(0, 100).with_bias(under),
        effect(finite(100), 0),
        primary(),
    )
    .unwrap();
    assert_eq!(quiet.verdict.tail, TailVerdict::Honest);
    let loud = measure_at(
        &mut Fake::new(0, 100).with_bias(over),
        effect(finite(100), 0),
        primary(),
    )
    .unwrap();
    assert!(matches!(
        loud.verdict.tail,
        TailVerdict::NeverSettles { .. }
    ));
}

/// A NaN is above the floor: a plugin that writes one is not quiet.
#[test]
fn a_nan_is_not_quiet() {
    assert!(above(f32::NAN, 1e-5));
    assert!(above(f32::INFINITY, 1e-5));
    assert!(!above(1e-5, 1e-5));
    assert!(!above(-1e-6, 1e-5));
}

/// **An unbounded or unknown claim is `NotChecked` for the tail**, and the
/// latency is still judged: the claim leaves nothing to hold the tail to, and
/// nothing to say about the latency.
#[test]
fn an_unbounded_claim_is_not_checked_but_its_latency_is() {
    for (claim, why) in [
        (PluginTail::Unbounded, NotChecked::UnboundedClaim),
        (PluginTail::Unknown, NotChecked::UnknownClaim),
    ] {
        let m = measure_at(&mut Fake::new(64, 50_000), effect(claim, 0), primary()).unwrap();
        assert_eq!(m.verdict.tail, TailVerdict::NotChecked(why));
        assert_eq!(m.tail, None);
        assert_eq!(
            m.verdict.latency,
            LatencyVerdict::LatencyDiffers {
                reported: Samples(0),
                measured: Samples(64)
            }
        );
        assert_eq!(m.latency, Some(Samples(64)));
    }
}

/// **A reported latency that is the onset is `Honest`, and the tail is counted
/// from after it**: a tail excludes latency, so a plugin that delays by 137 and
/// rings 5000 beyond says 5000.
#[test]
fn the_tail_excludes_the_reported_latency() {
    let m = measure_at(
        &mut Fake::new(137, 5000),
        effect(finite(5000), 137),
        primary(),
    )
    .unwrap();
    assert_eq!(m.verdict.latency, LatencyVerdict::Honest);
    assert_eq!(m.latency, Some(Samples(137)));
    assert_eq!(m.tail, Some(Samples(5000)));
    assert_eq!(m.verdict.tail, TailVerdict::Honest);
}

/// **A first sound elsewhere than reported is `LatencyDiffers`**, recorded and
/// not corrected; and it is independent of the tail, which here is also longer.
#[test]
fn a_latency_that_differs_is_recorded_beside_a_longer_tail() {
    let m = measure_at(
        &mut Fake::new(200, 5000),
        effect(finite(100), 137),
        primary(),
    )
    .unwrap();
    assert_eq!(
        m.verdict.latency,
        LatencyVerdict::LatencyDiffers {
            reported: Samples(137),
            measured: Samples(200)
        }
    );
    assert!(matches!(m.verdict.tail, TailVerdict::TailLonger { .. }));
    assert!(!m.verdict.is_honest());
}

/// A plugin that makes no sound after the impulse has no onset to compare and no
/// tail to measure: both are `NotChecked`, never a pass.
#[test]
fn a_silent_plugin_has_no_onset() {
    let mut fake = Fake::new(0, 0);
    fake.delay = usize::MAX / 2;
    let m = measure_at(&mut fake, effect(PluginTail::None, 0), primary()).unwrap();
    assert_eq!(
        m.verdict.latency,
        LatencyVerdict::NotChecked(NotChecked::Silent)
    );
    assert_eq!(m.latency, None);
    assert_eq!(
        m.verdict.tail,
        TailVerdict::NotChecked(NotChecked::Silent),
        "no ring is not a ring of none"
    );
    assert_eq!(m.tail, None);
}

/// **An instrument is played a note, and its tail runs from the note-off**: the
/// 100 ms the note is held is not tail. Its latency is not judged, since its
/// first sound is its attack.
#[test]
fn an_instruments_tail_is_measured_from_its_note_off() {
    let honest = measure_at(&mut Fake::new(0, 3000), instrument(finite(3000)), primary()).unwrap();
    assert_eq!(honest.tail, Some(Samples(3000)));
    assert_eq!(honest.verdict.tail, TailVerdict::Honest);
    assert_eq!(
        honest.verdict.latency,
        LatencyVerdict::NotChecked(NotChecked::Instrument)
    );
    // From the note-off, less the reported latency: a plugin that sounds 137
    // frames late (reported) and rings 3000 more after the note-off reads 3000,
    // not 3137 and not the 100 ms the note was held.
    let late = Subject {
        latency: Samples(137),
        ..instrument(finite(3000))
    };
    let m = measure_at(&mut Fake::new(137, 3000), late, primary()).unwrap();
    assert_eq!(m.tail, Some(Samples(3000)));
    assert_eq!(m.verdict.tail, TailVerdict::Honest);
    let longer = measure_at(&mut Fake::new(0, 3000), instrument(finite(2999)), primary()).unwrap();
    assert_eq!(
        longer.verdict.tail,
        TailVerdict::TailLonger {
            reported: Samples(2999),
            measured: Samples(3000)
        }
    );
}

/// **An instrument that never reaches the floor within the budget is
/// `NotChecked`**: a held note has no lower bound to report.
#[test]
fn an_instrument_that_does_not_settle_in_the_budget_is_not_checked() {
    let forever = 70 * 48_000;
    let m = measure_at(
        &mut Fake::new(0, forever),
        instrument(finite(1000)),
        primary(),
    )
    .unwrap();
    assert_eq!(
        m.verdict.tail,
        TailVerdict::NotChecked(NotChecked::NeverReachedFloor)
    );
    assert_eq!(m.tail, None);
}

/// **An effect still sounding at the budget is `TailLonger` by a lower bound**
/// past the graph's own bound, so the ceiling it raises is `Unbounded`, not a
/// figure the plugin outlives.
#[test]
fn an_effect_that_outlasts_the_budget_raises_the_ceiling_to_unbounded() {
    let forever = 70 * 48_000;
    let m = measure_at(
        &mut Fake::new(0, forever),
        effect(finite(1000), 0),
        primary(),
    )
    .unwrap();
    let TailVerdict::TailLonger { measured, .. } = m.verdict.tail else {
        panic!("{:?}", m.verdict.tail);
    };
    assert!(measured > MAX_NODE_TAIL, "{measured:?}");
    let ceiling = Processing::new(finite(1000)).with_measured(m).ceiling();
    assert_eq!(ceiling, PluginTail::Unbounded);
}

/// **The budget is the graph's bound plus a margin at every rate**: a ring past
/// it reads as a figure beyond `MAX_NODE_TAIL` at each configuration, even at
/// 44.1 kHz where 65 s of audio is fewer frames than the bound.
#[test]
fn the_budget_exceeds_the_graphs_bound_at_every_rate() {
    for (rate, block) in CONFIGS {
        let forever = MAX_NODE_TAIL.get() + 10 * 48_000;
        let m = measure_at(
            &mut Fake::new(0, forever),
            effect(finite(1000), 0),
            config(rate, block),
        )
        .unwrap();
        let TailVerdict::TailLonger { measured, .. } = m.verdict.tail else {
            panic!("{rate:?}: {:?}", m.verdict.tail);
        };
        assert!(measured > MAX_NODE_TAIL, "{rate:?}: {measured:?}");
        assert_eq!(
            Processing::new(finite(1000)).with_measured(m).ceiling(),
            PluginTail::Unbounded
        );
    }
}

/// The record of a measured honest plugin raises no ceiling; a longer one raises it
/// to the figure.
#[test]
fn a_measurement_raises_the_ceiling_by_what_it_found() {
    let m = measure_at(&mut Fake::new(0, 9000), effect(finite(8191), 0), primary()).unwrap();
    assert_eq!(
        Processing::new(finite(8191)).with_measured(m).ceiling(),
        finite(9000)
    );
}
