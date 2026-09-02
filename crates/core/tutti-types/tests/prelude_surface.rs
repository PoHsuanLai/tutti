//! The prelude is a shorthand for the root, never a second path to a type.
//!
//! Every name in `tutti_types::prelude` must also resolve at `tutti_types::`.
//! If one does not, the prelude has introduced a name the root lacks — which is
//! the "two paths per type" problem this crate's layout exists to remove, just
//! re-created one level over.
//!
//! These are compile-time assertions: the file failing to build IS the failure.
//! Each `use` below names the same item twice, once through each path, and the
//! aliases keep the two from colliding.

// Measurement vocabulary.
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::prelude::{
    Amplitude as _, Beat as _, BeatDuration as _, Bpm as _, CCNumber as _, Cents as _, Db as _,
    Depth as _, Hz as _, MidiChannel as _, MidiGroup as _, Note as _, Param as _, ParamAddr as _,
    Phase as _, PhaseIncrement as _, PitchClass as _, SamplePosition as _, SampleRate as _,
    Samples as _, Seconds as _, Semitones as _, Tail as _, UnitParam as _, Velocity as _, Q as _,
};
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::{
    Amplitude as _, Beat as _, BeatDuration as _, Bpm as _, CCNumber as _, Cents as _, Db as _,
    Depth as _, Hz as _, MidiChannel as _, MidiGroup as _, Note as _, Param as _, ParamAddr as _,
    Phase as _, PhaseIncrement as _, PitchClass as _, SamplePosition as _, SampleRate as _,
    Samples as _, Seconds as _, Semitones as _, Tail as _, UnitParam as _, Velocity as _, Q as _,
};

// Channels, topology, buffers, I/O edge, meter, RT.
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::prelude::{
    AudioIn as _, AudioOut as _, ChannelLayout as _, ChannelTopology as _, Interleaved as _,
    InterleavedMut as _, MeterMap as _, NoteValue as _, RtPublish as _, RtRef as _, Speaker as _,
    StereoPlanes as _, TimeSignature as _,
};
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::{
    AudioIn as _, AudioOut as _, ChannelLayout as _, ChannelTopology as _, Interleaved as _,
    InterleavedMut as _, MeterMap as _, NoteValue as _, RtPublish as _, RtRef as _, Speaker as _,
    StereoPlanes as _, TimeSignature as _,
};

/// `pump` is a function rather than a type, so it needs a reference rather than
/// a `use ... as _` to prove both paths name the same item.
#[test]
fn the_pump_helper_resolves_through_both_paths() {
    type Fn_ = fn(&mut Silence, &mut Sink, &mut [f32]) -> usize;
    let via_root: Fn_ = tutti_types::pump;
    let via_prelude: Fn_ = {
        use tutti_types::prelude::pump;
        pump
    };
    assert_eq!(via_root as usize, via_prelude as usize);
}

struct Silence;
impl tutti_types::AudioIn<f32> for Silence {
    const ON_EMPTY: tutti_types::OnEmpty = tutti_types::OnEmpty::EndOfStream;
    fn layout(&self) -> tutti_types::ChannelLayout {
        tutti_types::ChannelLayout::MONO
    }
    fn poll_into(&mut self, out: &mut [f32]) -> usize {
        out.fill(0.0);
        out.len()
    }
}

struct Sink;
impl tutti_types::AudioOut<f32> for Sink {
    fn layout(&self) -> tutti_types::ChannelLayout {
        tutti_types::ChannelLayout::MONO
    }
    fn write(&mut self, _samples: &[f32]) {}
    // `finalize` returns `std::io::Error`, which is std's — nothing this crate
    // could put in a prelude. Noted here so the omission reads as deliberate.
    fn finalize(self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

/// The prelude carries the whole `AudioIn` contract, not just the trait name.
///
/// `ON_EMPTY` is an associated const on `AudioIn`, and `OnEmpty` is its type. A
/// consumer writing an impl needs both, so a prelude that carried the trait
/// alone would be an incomplete forward — the caller could name the trait but
/// not write the impl.
#[test]
fn implementing_audio_in_needs_nothing_beyond_the_prelude() {
    use tutti_types::prelude::*;

    struct Quiet;
    impl AudioIn<f32> for Quiet {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::MONO
        }
        fn poll_into(&mut self, out: &mut [f32]) -> usize {
            out.fill(0.0);
            out.len()
        }
    }

    let mut s = Quiet;
    let mut buf = [1.0f32; 4];
    assert_eq!(s.poll_into(&mut buf), 4);
    assert_eq!(buf, [0.0; 4]);
    assert_eq!(s.layout(), ChannelLayout::MONO);
}
