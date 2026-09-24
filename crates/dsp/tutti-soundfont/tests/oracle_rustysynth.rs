//! Differential test: the vendored rustysynth fork vs upstream 1.3.6.
//!
//! # What the oracle is, and why it is independent
//!
//! The oracle is **upstream `rustysynth` 1.3.6 from crates.io**, pulled in as
//! `rustysynth_upstream` beside the vendored `rustysynth-tutti` fork. The two
//! are separate source trees resolved from separate registries; nothing links
//! them but the version number.
//!
//! The fork's own package description says what it is — "Tutti fork with Clone
//! support" — and a `derive(Clone)` adds no arithmetic. So the expected result
//! here is not "close": it is **bit-identical**, and anything less is a real
//! divergence worth naming.
//!
//! # Tolerance rationale
//!
//! There is none, deliberately. Both sides run the same synthesis on the same
//! `.sf2`, so the claim is exact equality and the assertion is `!=` on `f32`,
//! not an epsilon. This is the cheapest oracle in the engine to justify: no
//! tolerance to argue about, no topology difference to excuse, and the fixture
//! (`assets/soundfonts/TimGM6mb.sf2`) is already in the tree.
//!
//! A missing fixture is a **hard failure** naming the resolved path, never a
//! skip: silently passing when the asset is absent would leave this test
//! reporting green in exactly the checkout where it verified nothing.
//!
//! What it protects: the next time someone patches the vendored copy for a real
//! reason, this test says exactly which samples moved. Today it says the fork
//! is a pure `Clone` patch, which is a fact worth pinning.

use std::fs::File;
use std::sync::Arc;

const SF2: &str = "../../../assets/soundfonts/TimGM6mb.sf2";
const SR: i32 = 44_100;
const FRAMES: usize = 44_100; // 1 s: attack, sustain and the start of release.

/// The fixture, or a hard failure naming where it was looked for.
///
/// Not a skip. A `return` here would make the test report green in the one
/// checkout where it verified nothing — the failure mode this whole file exists
/// to rule out for the synth itself.
fn sf2_path() -> std::path::PathBuf {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SF2);
    assert!(
        p.is_file(),
        "the SoundFont fixture is missing: {}\n\
         (resolved from CARGO_MANIFEST_DIR + {SF2}). Fetch it with \
         `assets/soundfonts/download-timgm6mb.sh`.",
        p.display()
    );
    p
}

/// Render one note through the **vendored** synth.
fn render_fork(preset: i32, key: i32, vel: i32) -> (Vec<f32>, Vec<f32>) {
    use rustysynth::{SoundFont, Synthesizer, SynthesizerSettings};

    let mut f = File::open(sf2_path()).expect("open sf2");
    let sf = Arc::new(SoundFont::new(&mut f).expect("parse sf2"));
    let settings = SynthesizerSettings::new(SR);
    let mut synth = Synthesizer::new(&sf, &settings).expect("synth");

    synth.process_midi_message(0, 0xC0, preset, 0); // program change
    synth.note_on(0, key, vel);

    let mut l = vec![0.0f32; FRAMES];
    let mut r = vec![0.0f32; FRAMES];
    synth.render(&mut l, &mut r);
    (l, r)
}

/// Render the same note through **upstream** 1.3.6 — the oracle.
fn render_upstream(preset: i32, key: i32, vel: i32) -> (Vec<f32>, Vec<f32>) {
    use rustysynth_upstream::{SoundFont, Synthesizer, SynthesizerSettings};

    let mut f = File::open(sf2_path()).expect("open sf2");
    let sf = Arc::new(SoundFont::new(&mut f).expect("parse sf2"));
    let settings = SynthesizerSettings::new(SR);
    let mut synth = Synthesizer::new(&sf, &settings).expect("synth");

    synth.process_midi_message(0, 0xC0, preset, 0);
    synth.note_on(0, key, vel);

    let mut l = vec![0.0f32; FRAMES];
    let mut r = vec![0.0f32; FRAMES];
    synth.render(&mut l, &mut r);
    (l, r)
}

/// The fork must render exactly what upstream renders.
///
/// **Bit-identical, not within a tolerance.** The fork adds `derive(Clone)` and
/// nothing else; `Clone` performs no arithmetic, so a single differing sample
/// means the vendored copy has picked up a change nobody documented.
///
/// Mutation: scale the vendored fork's left-channel copy in `render` by
/// 1.000001 → fails at preset 0, channel L, sample 0, on a difference in the
/// 7th significant figure (0.0000008009945 vs 0.00000080099375). That is the
/// strength a zero tolerance buys: no epsilon here would have caught it.
#[test]
fn the_vendored_fork_renders_exactly_what_upstream_does() {
    // Three presets across the GM set, so this is not a claim about one voice:
    // 0 = Acoustic Grand, 40 = Violin (looped sustain), 73 = Flute.
    for (preset, key, vel) in [(0, 60, 100), (40, 69, 80), (73, 72, 120)] {
        let (fl, fr) = render_fork(preset, key, vel);
        let (ul, ur) = render_upstream(preset, key, vel);

        // A silent render would pass a bit-equality check trivially, so prove
        // there is signal before comparing — the `dry`-control lesson from the
        // sampler and export harnesses.
        let energy: f32 = fl.iter().map(|s| s.abs()).sum();
        assert!(
            energy > 1.0,
            "preset {preset}: the fork rendered near-silence ({energy}) — \
             the harness is broken, not the fork"
        );

        for (ch, (a, b)) in [("L", (&fl, &ul)), ("R", (&fr, &ur))] {
            let bad = (0..FRAMES).find(|&i| a[i] != b[i]);
            assert!(
                bad.is_none(),
                "preset {preset} ch{ch}: fork and upstream diverge at sample {} \
                 ({} vs {})",
                bad.unwrap(),
                a[bad.unwrap()],
                b[bad.unwrap()]
            );
        }
    }
}
