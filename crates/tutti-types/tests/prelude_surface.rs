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
    Amplitude as _, Beat as _, BeatDuration as _, Bpm as _, Cents as _, Db as _, Depth as _,
    Hz as _, Note as _, Param as _, ParamAddr as _, Phase as _, PhaseIncrement as _,
    PitchClass as _, Q as _, SamplePosition as _, SampleRate as _, Samples as _, Seconds as _,
    Semitones as _, Tail as _, UnitAddr as _, UnitParam as _, Velocity as _,
};
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::{
    Amplitude as _, Beat as _, BeatDuration as _, Bpm as _, Cents as _, Db as _, Depth as _,
    Hz as _, Note as _, Param as _, ParamAddr as _, Phase as _, PhaseIncrement as _,
    PitchClass as _, Q as _, SamplePosition as _, SampleRate as _, Samples as _, Seconds as _,
    Semitones as _, Tail as _, UnitAddr as _, UnitParam as _, Velocity as _,
};

// Channels, topology, buffers, I/O edge, meter.
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::prelude::{
    AudioIn as _, AudioOut as _, ChannelLayout as _, ChannelTopology as _, Interleaved as _,
    InterleavedMut as _, MeterMap as _, NoteValue as _, Speaker as _, StereoPlanes as _,
    TimeSignature as _,
};
#[allow(
    unused_imports,
    reason = "resolution IS the assertion — nothing uses these"
)]
use tutti_types::{
    AudioIn as _, AudioOut as _, ChannelLayout as _, ChannelTopology as _, Interleaved as _,
    InterleavedMut as _, MeterMap as _, NoteValue as _, Speaker as _, StereoPlanes as _,
    TimeSignature as _,
};

/// `pump` is a function rather than a type, so it needs a reference rather than
/// a `use ... as _` to prove both paths name the same item.
#[test]
fn the_pump_helper_resolves_through_both_paths() {
    type Fn_ = fn(&mut Silence, &mut Sink, &mut [f32]) -> tutti_types::Samples;
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
    fn poll_into(&mut self, out: &mut [f32]) -> tutti_types::Samples {
        out.fill(0.0);
        tutti_types::Samples::from_interleaved_len(out.len(), tutti_types::ChannelLayout::MONO)
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
/// `ON_EMPTY` is an associated const on `AudioIn`, and `OnEmpty` is its type;
/// `poll_into` returns a `Samples` frame count. A consumer writing an impl
/// needs all three, so a prelude that carried the trait alone would be an
/// incomplete forward — the caller could name the trait but not write the impl.
#[test]
fn implementing_audio_in_needs_nothing_beyond_the_prelude() {
    use tutti_types::prelude::*;

    struct Quiet;
    impl AudioIn<f32> for Quiet {
        const ON_EMPTY: OnEmpty = OnEmpty::EndOfStream;
        fn layout(&self) -> ChannelLayout {
            ChannelLayout::MONO
        }
        fn poll_into(&mut self, out: &mut [f32]) -> Samples {
            out.fill(0.0);
            Samples::from_interleaved_len(out.len(), self.layout())
        }
    }

    let mut s = Quiet;
    let mut buf = [1.0f32; 4];
    assert_eq!(s.poll_into(&mut buf), Samples(4));
    assert_eq!(buf, [0.0; 4]);
    assert_eq!(s.layout(), ChannelLayout::MONO);
}

/// Every name the prelude exports is also exported at the root, whether or not
/// the lists above name it.
///
/// The `use ... as _` lines above only check the names they list, so a name
/// added to the prelude and left off both lists would still pass. This reads
/// `src/lib.rs` and compares the two `pub use` sets by identifier. It is
/// textual: it strips `//` and `/* */` comments, sees `pub use` statements and
/// no glob (`*`) re-export, treats any identifier in a root `pub use` as
/// exported at the root, and does not follow `as` renames or macro-generated
/// exports. A `//` inside a string literal in `lib.rs` would cut its line.
///
/// Mutation (run): add `pub use crate::graph::Source;` to the prelude → fails
/// naming `Source`.
#[test]
fn every_prelude_name_is_exported_at_the_root() {
    let lib = include_str!("../src/lib.rs");
    let mut code: String = lib
        .lines()
        .map(|l| l.split("//").next().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    while let Some(i) = code.find("/*") {
        let j = code[i..].find("*/").expect("a closed block comment") + i + 2;
        code.replace_range(i..j, " ");
    }
    let modules: Vec<&str> = code
        .split("mod ")
        .skip(1)
        .map(|rest| {
            rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap()
        })
        .collect();
    let start = code.find("pub mod prelude").expect("a prelude module");
    let end = start + code[start..].find("\n}").expect("the prelude block ends");
    let prelude = &code[start..end];
    let root = format!("{}{}", &code[..start], &code[end..]);
    assert!(
        !prelude.contains("::*"),
        "a glob re-export cannot be checked"
    );

    let names = |text: &str| -> Vec<String> {
        let mut out = Vec::new();
        for stmt in text.split("pub use ").skip(1) {
            let stmt = stmt.split(';').next().unwrap();
            for word in stmt.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                let path_part =
                    ["", "crate", "self", "super", "as"].contains(&word) || modules.contains(&word);
                if !path_part {
                    out.push(word.to_owned());
                }
            }
        }
        out
    };
    let at_root = names(&root);
    let in_prelude = names(prelude);
    assert!(
        in_prelude.len() > 30,
        "parsed too few prelude names: {in_prelude:?}"
    );
    for name in in_prelude {
        assert!(
            at_root.contains(&name),
            "`{name}` is in the prelude but not at the crate root"
        );
    }
}
