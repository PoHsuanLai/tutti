//! Parameter↔MIDI mapping against real AUs: the CC→parameter routing table.
//!
//! `src/midi_map.rs`'s unit tests pin the ABI, the flag decode and the round
//! trip over hand-built structs. This suite asserts what those cannot: that the
//! table a real AU accepts is the table it hands back, that a mapped CC actually
//! *moves* the parameter, and that "learn" mode completes.
//!
//! ## This one is implemented, and that is unusual here
//!
//! Two of this crate's other capability surfaces are ghosts:
//! `MIDIOutputCallbackInfo` is published by nothing, and `HostCallbacks` is
//! accepted by ~35 units and called by none. This family is different — **2 of
//! 59 installed components implement it** (AUSampler `aumu`/`samp`/`appl` and
//! AUMIDISynth `aumu`/`msyn`/`appl`) and both work end to end. So the tests
//! here assert positives, not absences, with one exception: the deprecated
//! property 17 sweep, which asserts nothing on this machine answers it.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_midi_map
//! ```
//!
//! No SDK, no display, no env vars: both implementers ship with macOS, so their
//! absence is a hard failure rather than a skip. See `support/corpus.rs`.
//!
//! ## Every test here was proven load-bearing by mutation
//!
//! Each of the 15 mutations below was applied to `src/`, the suite run, and the
//! mutation reverted. All 15 were caught. Recorded because two of them exposed a
//! test that could **not** fail:
//!
//! | mutation to `src/` | caught by |
//! |---|---|
//! | drop the `& 0x0F` channel mask in `to_raw` | `midi_map::tests::an_out_of_range_channel_cannot_corrupt_the_command_nibble` |
//! | `sub_range` sets the `Toggle` bit instead of `SubRange` | `sub_range_travels_with_its_flag`, `each_flag_round_trips_independently`, `every_field_survives_a_round_trip_through_a_real_au` |
//! | `from_raw` reads `sub_range` as always `Some` | `sub_range_travels_with_its_flag`, + 7 real-AU tests |
//! | `reserved1: 1` instead of `0` | `midi_map::tests::reserved_fields_are_always_zero` |
//! | `0xD0` decodes as `ControlChange` not `ChannelPressure` | `every_trigger_kind_survives_a_real_au`, `trigger_round_trips_through_the_status_byte` |
//! | `set_all(&[])` writes an empty table through | 15 real-AU tests, incl. `an_empty_set_clears_the_table_despite_the_au_refusing_an_empty_write` |
//! | `hot_map` ignores `mStatus`, always returns `Some` | `an_unarmed_hot_map_read_answers_noerr_not_the_documented_error`, `arming_a_hot_map_and_sending_a_cc_completes_the_mapping` |
//! | `add` writes property 41 instead of 42 | `a_second_mapping_on_the_same_parameter_replaces_the_first` |
//! | `remove` writes property 42 instead of 43 | `remove_matches_on_the_parameter_triple_alone`, `removing_an_absent_mapping_is_ignored`, + 2 |
//! | `targets_same_parameter` ignores the element | `targets_same_parameter_keys_on_the_documented_triple` |
//! | `decode_table` uses `chunks` not `chunks_exact` | `decode_table_handles_whole_and_partial_buffers` |
//! | the capability gate always returns `true` | `a_unit_without_mapping_support_refuses_every_write`, `exactly_two_installed_components_implement_the_mapping_family`, + 2 |
//! | empty `add` writes through instead of returning `Ok` | `empty_add_and_remove_are_no_ops` |
//! | drop the `MidiController` unit arm | `parameters::tests::the_midi_controller_unit_is_not_unknown` |
//! | `any_note` sets the `AnyChannel` bit | `each_flag_round_trips_independently` |
//!
//! The two tests the exercise fixed, both of which passed against a mutated
//! source before being repaired:
//!
//! * `an_out_of_range_channel_cannot_corrupt_the_command_nibble` used
//!   `channel: 16` — the natural off-by-one from 1-based MIDI channel numbering.
//!   `0xB0 | 16 == 0xB0`, because bit 4 is already set in the CC status nibble,
//!   so the value was absorbed and the mask could be deleted freely. Now uses
//!   64, 100, 255 and 17, and asserts the table contains a value the unmasked
//!   path corrupts.
//! * dropping the `MidiController` arm was caught by nothing, because **no
//!   installed AU reports unit 12** — a real-AU test is impossible. Covered by a
//!   unit test on the decode instead, which is the honest place for it.

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    every_component, AuRef, DELAY, INVALID_PROPERTY, LOWPASS, MIDI_SYNTH, N_BAND_EQ, SAMPLER,
    THIRD_PARTY,
};

use tutti_au_host::types::{
    K_AUDIO_UNIT_PROPERTY_MIDI_CONTROL_MAPPING, K_AUDIO_UNIT_SCOPE_GLOBAL,
    K_AUDIO_UNIT_SCOPE_INPUT, K_AUDIO_UNIT_SCOPE_OUTPUT,
};
use tutti_au_host::AuInstance;
use tutti_au_host::{AuError, AuMidiMapping, MidiEvent, MidiTrigger};
use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

/// Serializes instantiate/dispose against component enumeration, for the reason
/// `au_conformance.rs`'s `AU_LOCK` does. Recovered from poisoning so one real
/// failure does not become N spurious ones.
static AU_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// The two units on this machine that implement the family, as
/// `(reference, gain_parameter_id)`.
///
/// Defined here rather than in `support/corpus.rs` because it is this suite's
/// fixture alone. Parameter `900` is `Gain` on both, range `-96..=12` dB —
/// measured, and load-bearing for
/// [`a_mapped_cc_actually_moves_the_parameter`]: a mapping is only observable
/// through a parameter whose value the host can read back.
const IMPLEMENTERS: &[(AuRef, u32)] = &[(SAMPLER, 900), (MIDI_SYNTH, 900)];

/// Units that refuse the family outright — one wide-parameter effect, one
/// minimal one, one middling. Every Apple effect answers the same way, so three
/// is enough to pin the *shape* of the refusal; the exhaustive claim is made by
/// [`exactly_two_installed_components_implement_the_mapping_family`].
const REFUSERS: &[AuRef] = &[DELAY, LOWPASS, N_BAND_EQ];

/// Clear an AU's table through the read-then-remove path, so each test starts
/// from a known state.
///
/// Not `set_parameter_midi_mappings(&[])` for its own sake — that *is* the
/// read-then-remove path — but named so a failure here reads as "setup", not as
/// the assertion under test.
fn clear(au: &mut AuInstance) {
    au.set_parameter_midi_mappings(&[])
        .expect("clearing the table must be accepted");
    assert!(
        au.parameter_midi_mappings()
            .expect("read back after clear")
            .is_empty(),
        "the table must be empty after a clear"
    );
}

// ----------------------------------------------------- capability & absence

/// The capability gate must agree with the table read: a unit that answers
/// property 41 must also let the table be read, and one that refuses must refuse
/// both. Otherwise a host branching on the gate would hit an error it was told
/// could not happen.
#[test]
fn the_capability_gate_agrees_with_the_table_read() {
    let _g = lock();
    for (unit, _) in IMPLEMENTERS {
        let au = unit.open(RATE, BLOCK);
        assert!(
            au.supports_parameter_midi_mapping(),
            "{} implements property 41 (measured), so the gate must say so",
            unit.label
        );
        au.parameter_midi_mappings().unwrap_or_else(|e| {
            panic!(
                "{}: the gate said yes, so the read must succeed, got {e:?}",
                unit.label
            )
        });
    }
    for unit in REFUSERS {
        let au = unit.open(RATE, BLOCK);
        assert!(
            !au.supports_parameter_midi_mapping(),
            "{} does not implement property 41 (measured), so the gate must say no",
            unit.label
        );
        let err = au
            .parameter_midi_mappings()
            .expect_err("the gate said no, so the read must fail");
        // The exact status, not `is_err()`: -10879 is "does not implement this
        // property", which a host treats as routine. Any other code would mean
        // something else went wrong and must not be swallowed as "no mapping".
        assert!(
            matches!(err, AuError::OsStatus { code, .. } if code == INVALID_PROPERTY),
            "{}: expected kAudioUnitErr_InvalidProperty ({INVALID_PROPERTY}), got {err:?}",
            unit.label
        );
    }
}

/// The exhaustive claim, over the whole component registry rather than a chosen
/// corpus: **exactly** AUSampler and AUMIDISynth implement property 41.
///
/// Asserted as an equality on the set, not as ">= 2", so this test fails in both
/// directions. A newly installed unit that implements it is *good news* and the
/// message says so — it means the family has a third implementer to test
/// against, and the constant here should be widened after confirming it. A
/// missing implementer means the AU environment is broken.
#[test]
fn exactly_two_installed_components_implement_the_mapping_family() {
    let _g = lock();
    let expected = ["AUSampler", "AUMIDISynth"];
    let mut found: Vec<String> = Vec::new();

    for info in every_component() {
        // SAFETY: `component` came from `AudioComponentFindNext`.
        let Ok(mut au) = (unsafe { AuInstance::new(info.component, RATE, BLOCK) }) else {
            // A unit that cannot be instantiated cannot be probed. AUAudioTapIO
            // is the one such unit here; it is an output unit, and the family is
            // an instrument surface, so nothing is lost.
            continue;
        };
        // Probe initialized as well as loaded: several AU properties only appear
        // after `AudioUnitInitialize`, and property 41 reports size 0 before it
        // even on the implementers.
        let _ = au.initialize();
        if au.supports_parameter_midi_mapping() {
            found.push(info.name.clone());
        }
    }

    let mut matched: Vec<&str> = Vec::new();
    let mut unexpected: Vec<&String> = Vec::new();
    for name in &found {
        match expected.iter().find(|e| name.contains(**e)) {
            Some(e) => matched.push(e),
            None => unexpected.push(name),
        }
    }
    matched.sort_unstable();
    matched.dedup();

    assert!(
        unexpected.is_empty(),
        "a component implements kAudioUnitProperty_AllParameterMIDIMappings that \
         was not measured to: {unexpected:?}. THIS IS GOOD NEWS — it is a third \
         unit the mapping path can be tested against. Confirm it round-trips a \
         mapping, then add it to `IMPLEMENTERS` and to this test's `expected`. \
         Do NOT relax this assertion to make it pass."
    );
    assert_eq!(
        matched.len(),
        expected.len(),
        "expected both {expected:?} to implement property 41; found {found:?}. \
         Both ship with macOS, so a missing one means the AU environment is \
         broken rather than that an optional plugin is absent."
    );
}

/// The deprecated `kAudioUnitProperty_MIDIControlMapping` (17) is implemented by
/// **nothing** on this machine — which is why `src/midi_map.rs` ships no decoder
/// for it.
///
/// A failure here is *good news* and must not be silenced: it means a unit
/// answering only the deprecated form exists, and the `AudioUnitMIDIControlMapping`
/// fallback can finally be written against a real implementer instead of guessed.
/// Probed at all three scopes and in both lifecycle phases, because a property
/// can appear in only one of six combinations.
#[test]
fn nothing_implements_the_deprecated_midi_control_mapping_property() {
    let _g = lock();
    let mut hits: Vec<String> = Vec::new();

    for info in every_component() {
        // SAFETY: `component` came from `AudioComponentFindNext`.
        let Ok(mut au) = (unsafe { AuInstance::new(info.component, RATE, BLOCK) }) else {
            continue;
        };
        for phase in ["loaded", "initialized"] {
            if phase == "initialized" && au.initialize().is_err() {
                break;
            }
            for scope in [
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                K_AUDIO_UNIT_SCOPE_INPUT,
                K_AUDIO_UNIT_SCOPE_OUTPUT,
            ] {
                let mut size = 0u32;
                let mut writable = 0u8;
                // Raw, because there is no wrapper for a property this crate
                // deliberately does not support — the whole point of the test is
                // to check nothing needs one.
                let status = unsafe {
                    tutti_au_host::types::AudioUnitGetPropertyInfo(
                        au.raw_unit(),
                        K_AUDIO_UNIT_PROPERTY_MIDI_CONTROL_MAPPING,
                        scope,
                        0,
                        &mut size,
                        &mut writable,
                    )
                };
                if status == tutti_au_host::types::NO_ERR {
                    hits.push(format!(
                        "{} ({phase}, scope {scope}, {size} bytes)",
                        info.name
                    ));
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "kAudioUnitProperty_MIDIControlMapping (17) is implemented by {hits:?}. \
         THIS IS GOOD NEWS: the deprecated read-only form was skipped precisely \
         because 0 of 59 components answered it, making a decoder untestable. \
         A hit means the AudioUnitMIDIControlMapping fallback can now be written \
         against a real unit. Write it — do NOT delete this assertion."
    );
}

/// The third-party units installed here refuse the family, so no third-party
/// coverage of it exists. Recorded as an assertion rather than a comment so the
/// claim decays loudly: a plugin update that adds mapping support fails this and
/// the suite gains a non-Apple subject.
///
/// Absent third-party units are skipped, not failed — the crate's rule is that
/// only Apple's units are guaranteed present.
#[test]
fn no_installed_third_party_unit_implements_the_mapping_family() {
    let _g = lock();
    let mut supported: Vec<&str> = Vec::new();
    let mut probed = 0usize;

    for unit in THIRD_PARTY {
        let Some(info) = unit.find() else { continue };
        probed += 1;
        // SAFETY: `component` came from `AudioComponentFindNext`.
        let Ok(mut au) = (unsafe { AuInstance::new(info.component, RATE, BLOCK) }) else {
            continue;
        };
        let _ = au.initialize();
        if au.supports_parameter_midi_mapping() {
            supported.push(unit.label);
        }
    }

    assert!(
        supported.is_empty(),
        "{supported:?} now implement the parameter↔MIDI mapping family. THIS IS \
         GOOD NEWS — the suite has a non-Apple subject for the first time. Add it \
         to the round-trip tests rather than relaxing this assertion."
    );
    // Guard against the vacuous pass: with no third-party units installed the
    // loop above proves nothing, and reporting `ok` for that is the silent-skip
    // shape this crate's corpus docs call out.
    assert!(
        probed > 0,
        "no third-party AU was found, so this test asserted nothing. Install one \
         of {:?} or accept that the claim is untested here.",
        THIRD_PARTY.iter().map(|u| u.label).collect::<Vec<_>>()
    );
}

// ----------------------------------------------------- round trip

/// A fresh implementer has no mappings — `noErr` with an empty table, not an
/// error. The distinction is the whole reason
/// `supports_parameter_midi_mapping` exists rather than "does the read succeed".
#[test]
fn a_fresh_instance_has_an_empty_mapping_table() {
    let _g = lock();
    for (unit, _) in IMPLEMENTERS {
        let au = unit.open(RATE, BLOCK);
        let table = au
            .parameter_midi_mappings()
            .unwrap_or_else(|e| panic!("{}: fresh read failed: {e:?}", unit.label));
        assert!(
            table.is_empty(),
            "{}: a fresh instance must report no mappings, got {table:?}",
            unit.label
        );
    }
}

/// Every field of a mapping — including the sub-range bounds and all four
/// behavioural flags — must survive a write to a real AU and a read back.
///
/// This is the test that makes the "a flags word is not a bool" modelling
/// load-bearing: the mapping written here sets `SubRange | Toggle | Bipolar |
/// Bipolar_On` (flags word `60`) with bounds `0.25..=0.75`, on CC 74 channel 3.
/// A host that stored the flags as a single boolean, or dropped the bounds, could
/// not produce this value at all.
#[test]
fn every_field_survives_a_round_trip_through_a_real_au() {
    let _g = lock();
    for (unit, param) in IMPLEMENTERS {
        let mut au = unit.open(RATE, BLOCK);
        clear(&mut au);

        let sent = AuMidiMapping {
            sub_range: Some((0.25, 0.75)),
            toggle: true,
            bipolar: true,
            bipolar_on: true,
            ..AuMidiMapping::control_change(*param, 3, 74)
        };
        au.add_parameter_midi_mapping(&[sent])
            .unwrap_or_else(|e| panic!("{}: add failed: {e:?}", unit.label));

        let table = au
            .parameter_midi_mappings()
            .unwrap_or_else(|e| panic!("{}: read back failed: {e:?}", unit.label));
        // Verified by READ-BACK, never by the add's status: this crate has twice
        // been burned by a `noErr` that meant nothing (a bogus parameter id is
        // accepted by this very property).
        assert_eq!(
            table,
            vec![sent],
            "{}: the AU must hand back exactly what was written",
            unit.label
        );

        // And the individual accessors, so a field silently defaulting to the
        // same value as the sent one cannot hide behind struct equality.
        let got = table[0];
        assert_eq!(got.trigger, MidiTrigger::ControlChange { controller: 74 });
        assert_eq!(got.channel, 3);
        assert_eq!(got.sub_range, Some((0.25, 0.75)));
        assert!(got.toggle && got.bipolar && got.bipolar_on);
        assert!(!got.any_channel && !got.any_note);
    }
}

/// Each trigger the header tabulates must survive a real AU. Notes carry a note
/// number, `ChannelPressure` and `PitchBend` carry nothing — and the AU must not
/// reinterpret one as another.
///
/// Written one mapping at a time on the *same* parameter, because the AU keeps
/// only one mapping per parameter: each write replaces the last, so the table is
/// always a single entry and the assertion is unambiguous.
#[test]
fn every_trigger_kind_survives_a_real_au() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let triggers = [
        MidiTrigger::NoteOn { note: 60 },
        MidiTrigger::NoteOff { note: 61 },
        MidiTrigger::KeyPressure { note: 62 },
        MidiTrigger::ControlChange { controller: 1 },
        MidiTrigger::ProgramChange { patch: 7 },
        MidiTrigger::ChannelPressure,
        MidiTrigger::PitchBend,
    ];
    for trigger in triggers {
        let sent = AuMidiMapping {
            trigger,
            channel: 5,
            ..AuMidiMapping::control_change(param, 5, 0)
        };
        au.add_parameter_midi_mapping(&[sent])
            .unwrap_or_else(|e| panic!("add {trigger:?} failed: {e:?}"));
        let table = au.parameter_midi_mappings().expect("read back");
        assert_eq!(
            table.len(),
            1,
            "one mapping per parameter: {trigger:?} must replace the last, got {table:?}"
        );
        assert_eq!(
            table[0].trigger, trigger,
            "{trigger:?} must not be reinterpreted by the AU"
        );
        assert_eq!(table[0].channel, 5, "{trigger:?} channel");
    }
}

/// The any-channel flag must reach the AU and come back, and must be
/// distinguishable from an explicit channel-0 mapping. Without the flag a
/// keyboard on channel 5 would silently fail to drive a mapping bound to
/// channel 0 — the exact silent-nothing this module exists to prevent.
#[test]
fn the_any_channel_flag_survives_a_real_au() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let any = AuMidiMapping::control_change_any_channel(param, 1);
    au.add_parameter_midi_mapping(&[any]).expect("add any");
    let got = au.parameter_midi_mappings().expect("read")[0];
    assert!(got.any_channel, "the any-channel flag must round-trip");
    assert_eq!(got, any);

    // The explicit form on the same parameter replaces it, and must NOT carry
    // the flag — proving the two are distinguishable through the AU.
    let explicit = AuMidiMapping::control_change(param, 0, 1);
    au.add_parameter_midi_mapping(&[explicit])
        .expect("add explicit");
    let got = au.parameter_midi_mappings().expect("read")[0];
    assert!(
        !got.any_channel,
        "an explicit-channel mapping must not come back flagged any-channel"
    );
    assert_eq!(got, explicit);
    assert_ne!(got, any);
}

/// The any-note flag on a note trigger, which the header restricts to note
/// on/off and polyphonic pressure. Separate from the CC round trip because the
/// flag sits in a different bit and applies to a different trigger family — a
/// `|=` written against the wrong constant would pass the CC test.
#[test]
fn the_any_note_flag_survives_a_real_au() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let sent = AuMidiMapping {
        trigger: MidiTrigger::NoteOn { note: 0 },
        any_note: true,
        ..AuMidiMapping::control_change(param, 0, 0)
    };
    au.add_parameter_midi_mapping(&[sent]).expect("add");
    let got = au.parameter_midi_mappings().expect("read")[0];
    assert!(got.any_note, "the any-note flag must round-trip");
    assert!(
        got.trigger.is_note_command(),
        "the flag must arrive on a note command, got {:?}",
        got.trigger
    );
    assert_eq!(got, sent);
}

// ----------------------------------------------------- table operations

/// Adding two mappings on **different** parameters keeps both; adding two on the
/// **same** parameter keeps one, the last.
///
/// Both halves are Apple's documented rule ("there can be only one mapping per
/// parameter … it replaces the previous mapping") and both were measured. The
/// comparison is order-insensitive because AUSampler returns the table
/// reordered — an index-wise assertion would fail for the wrong reason.
#[test]
fn a_second_mapping_on_the_same_parameter_replaces_the_first() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    // Different parameters: both survive.
    let a = AuMidiMapping::control_change(param, 0, 10);
    let b = AuMidiMapping::control_change(param + 1, 0, 11);
    au.add_parameter_midi_mapping(&[a, b]).expect("batch add");
    let table = au.parameter_midi_mappings().expect("read");
    assert_eq!(table.len(), 2, "two parameters, two mappings: {table:?}");
    for expected in [a, b] {
        assert!(
            table.contains(&expected),
            "{expected:?} missing from {table:?} (the AU reorders, so this is a \
             set comparison)"
        );
    }

    // Same parameter again: replaced, not appended.
    let a_again = AuMidiMapping::control_change(param, 0, 20);
    au.add_parameter_midi_mapping(&[a_again]).expect("replace");
    let table = au.parameter_midi_mappings().expect("read");
    assert_eq!(
        table.len(),
        2,
        "the replacement must not grow the table: {table:?}"
    );
    assert!(table.contains(&a_again), "the new mapping must be present");
    assert!(
        !table.contains(&a),
        "the replaced mapping must be gone, got {table:?}"
    );
}

/// Remove matches on `(scope, element, parameter_id)` **alone**: a mapping
/// constructed with a deliberately different trigger and flags still removes the
/// installed one.
///
/// This is the behaviour a host relies on to unbind a parameter without first
/// reading what it was bound to. If remove compared whole structs, the call
/// would silently do nothing and the parameter would stay mapped.
#[test]
fn remove_matches_on_the_parameter_triple_alone() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let installed = AuMidiMapping {
        sub_range: Some((0.1, 0.9)),
        toggle: true,
        ..AuMidiMapping::control_change(param, 7, 74)
    };
    au.add_parameter_midi_mapping(&[installed]).expect("add");
    assert_eq!(au.parameter_midi_mappings().expect("read").len(), 1);

    // A stand-in naming only the parameter — different trigger, no flags.
    let stand_in = AuMidiMapping {
        trigger: MidiTrigger::PitchBend,
        ..AuMidiMapping::control_change(param, 0, 0)
    };
    assert!(
        stand_in.targets_same_parameter(&installed),
        "the stand-in must name the same parameter"
    );
    assert_ne!(stand_in, installed, "but must not be an equal struct");

    au.remove_parameter_midi_mapping(&[stand_in])
        .expect("remove");
    assert!(
        au.parameter_midi_mappings().expect("read").is_empty(),
        "a mapping named only by its parameter triple must be removable"
    );
}

/// Removing a mapping that is not installed is a no-op, and does not disturb the
/// mappings that are. The header specifies exactly this ("if a mapping is
/// specified that does not currently exist … the audio unit should ignore the
/// request"), and both implementers honour it with `noErr`.
#[test]
fn removing_an_absent_mapping_is_ignored() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let keep = AuMidiMapping::control_change(param, 0, 1);
    au.add_parameter_midi_mapping(&[keep]).expect("add");

    let ghost = AuMidiMapping::control_change(param + 500, 0, 2);
    au.remove_parameter_midi_mapping(&[ghost])
        .expect("removing an absent mapping must be accepted, not an error");
    assert_eq!(
        au.parameter_midi_mappings().expect("read"),
        vec![keep],
        "removing an absent mapping must leave the table alone"
    );
}

/// `set_parameter_midi_mappings` **replaces** the table rather than merging into
/// it: three mappings in, one written, one left.
///
/// Distinguishes set from add, which is the whole reason both exist. An
/// implementation that forwarded `set` to the add property would leave four.
#[test]
fn set_replaces_the_whole_table() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let initial = [
        AuMidiMapping::control_change(param, 0, 1),
        AuMidiMapping::control_change(param + 1, 0, 2),
        AuMidiMapping::control_change(param + 2, 0, 3),
    ];
    au.add_parameter_midi_mapping(&initial).expect("seed");
    assert_eq!(au.parameter_midi_mappings().expect("read").len(), 3);

    let replacement = AuMidiMapping::control_change(param, 0, 7);
    au.set_parameter_midi_mappings(&[replacement])
        .expect("set-all");
    assert_eq!(
        au.parameter_midi_mappings().expect("read"),
        vec![replacement],
        "set must replace the table, not merge into it"
    );
}

/// Clearing the table works, and works *despite* the AU refusing an empty write.
///
/// This is the AU deviation `midi_map::set_all` exists to paper over: writing a
/// zero-length table answers `paramErr` (-50) and a NULL one answers
/// `kAudioUnitErr_InvalidPropertyValue` (-10851), leaving the table **unchanged**
/// in both cases. A host that took the header at face value would silently fail
/// to unbind anything. Asserted by observing the resulting table size, not by the
/// call's status.
#[test]
fn an_empty_set_clears_the_table_despite_the_au_refusing_an_empty_write() {
    let _g = lock();
    for (unit, param) in IMPLEMENTERS {
        let mut au = unit.open(RATE, BLOCK);
        au.add_parameter_midi_mapping(&[
            AuMidiMapping::control_change(*param, 0, 1),
            AuMidiMapping::control_change(param + 1, 0, 2),
        ])
        .unwrap_or_else(|e| panic!("{}: seed failed: {e:?}", unit.label));
        assert_eq!(
            au.parameter_midi_mappings().expect("read").len(),
            2,
            "{}: seeded",
            unit.label
        );

        au.set_parameter_midi_mappings(&[])
            .unwrap_or_else(|e| panic!("{}: clear failed: {e:?}", unit.label));
        assert!(
            au.parameter_midi_mappings().expect("read").is_empty(),
            "{}: the table must actually be empty after a clear — the AU refuses \
             a zero-length write, so this only passes via read-then-remove",
            unit.label
        );
    }
}

/// A mapping batch and a remove batch each go through in **one** call, so a host
/// binding a whole controller layout does not pay a property write per mapping.
#[test]
fn add_and_remove_accept_batches() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let batch: Vec<AuMidiMapping> = (0..3)
        .map(|i| AuMidiMapping::control_change(param + i, 0, 20 + i as u8))
        .collect();
    au.add_parameter_midi_mapping(&batch).expect("batch add");
    let table = au.parameter_midi_mappings().expect("read");
    assert_eq!(table.len(), 3, "one call, three mappings: {table:?}");

    au.remove_parameter_midi_mapping(&table)
        .expect("batch remove");
    assert!(
        au.parameter_midi_mappings().expect("read").is_empty(),
        "one call must remove all three"
    );
}

/// An empty add or remove is a no-op that reports success without reaching the
/// AU — a zero-length property write is `paramErr` (-50) on both implementers,
/// so passing one through would surface a confusing error for an operation that
/// asked for nothing.
///
/// Includes a **refuser**: an empty call must not even reach the property, so it
/// must succeed on a unit that implements nothing.
#[test]
fn empty_add_and_remove_are_no_ops() {
    let _g = lock();
    let (unit, param) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let keep = AuMidiMapping::control_change(param, 0, 1);
    au.add_parameter_midi_mapping(&[keep]).expect("seed");

    au.add_parameter_midi_mapping(&[]).expect("empty add");
    au.remove_parameter_midi_mapping(&[]).expect("empty remove");
    assert_eq!(
        au.parameter_midi_mappings().expect("read"),
        vec![keep],
        "an empty add/remove must leave the table untouched"
    );

    // On a unit with no mapping support at all: still Ok, because the FFI call
    // is never made.
    let mut effect = DELAY.open(RATE, BLOCK);
    assert!(!effect.supports_parameter_midi_mapping());
    effect
        .add_parameter_midi_mapping(&[])
        .expect("an empty add must not reach an unsupported property");
    effect
        .remove_parameter_midi_mapping(&[])
        .expect("an empty remove must not reach an unsupported property");
}

// ----------------------------------------------------- the point of it all

/// **A mapped CC actually moves the parameter.** This is the assertion the whole
/// module exists for, and the one that separates this family from the crate's
/// other "accepted but never called" surfaces.
///
/// Mapping CC 20 to `Gain` (`-96..=12` dB), then sending CC 20 = 127 and
/// rendering one block, must drive the parameter to its maximum. Measured on
/// macOS 15.6: `0` before, `12` after, on both implementers.
///
/// The value is asserted as "moved to the top of the range", not as an exact
/// float, because the mapping curve is the AU's business; what must hold is that
/// the CC reached the parameter at all. The `before != after` check is what makes
/// the test load-bearing — an unmapped CC leaves the parameter at `0`.
#[test]
fn a_mapped_cc_actually_moves_the_parameter() {
    let _g = lock();
    for (unit, param) in IMPLEMENTERS {
        let mut au = unit.open(RATE, BLOCK);
        clear(&mut au);

        let range = au
            .get_parameter_list()
            .into_iter()
            .find(|p| p.id == *param)
            .unwrap_or_else(|| {
                panic!(
                    "{}: parameter {param} must exist — it is this fixture's \
                     observable target",
                    unit.label
                )
            })
            .range;
        let before = au
            .get_parameter(*param)
            .unwrap_or_else(|e| panic!("{}: read before: {e:?}", unit.label));

        // The mapping and the event must name the same controller, so bind it
        // once rather than repeating the number on both sides. 20 is one of the
        // undefined controllers, chosen so no AU reacts to it by default.
        let mapped_cc = CCNumber::new(20);
        au.add_parameter_midi_mapping(&[AuMidiMapping::control_change(*param, 0, mapped_cc.get())])
            .unwrap_or_else(|e| panic!("{}: add failed: {e:?}", unit.label));
        au.send_midi(&[MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            mapped_cc,
            tutti_midi_types::convert::midi1_cc_to_midi2(127),
        )]);

        // Instruments have no input bus, so render with no input buffers.
        let mut output = vec![vec![0.0f32; BLOCK as usize]; au.num_outputs() as usize];
        let mut out_refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        au.process(&[], &mut out_refs, BLOCK)
            .unwrap_or_else(|e| panic!("{}: render failed: {e:?}", unit.label));

        let after = au
            .get_parameter(*param)
            .unwrap_or_else(|e| panic!("{}: read after: {e:?}", unit.label));
        assert_ne!(
            before, after,
            "{}: CC 20 = 127 mapped to parameter {param} must MOVE it — this is \
             the whole point of the mapping table, and an unmapped CC leaves it \
             at {before}",
            unit.label
        );
        assert_eq!(
            after, range.max,
            "{}: a full-scale controller must drive parameter {param} to the top \
             of its {:?}..={:?} range",
            unit.label, range.min, range.max
        );

        // The rendered block must be finite as well as bounded. `f32::max`
        // returns the non-NaN operand, so a peak-only check reports 0.0 for an
        // all-NaN buffer — a mapping that corrupted the AU's state would pass a
        // "small peak" assertion while producing garbage.
        assert!(
            output.iter().flatten().all(|s| s.is_finite()),
            "{}: the render after a mapped CC produced a non-finite sample",
            unit.label
        );
    }
}

/// An unmapped CC leaves the parameter alone — the control for
/// [`a_mapped_cc_actually_moves_the_parameter`].
///
/// Without this, that test would pass just as well if the AU moved `Gain` in
/// response to *any* CC, or on any render, and would be proving nothing about
/// the mapping table.
#[test]
fn an_unmapped_cc_does_not_move_the_parameter() {
    let _g = lock();
    for (unit, param) in IMPLEMENTERS {
        let mut au = unit.open(RATE, BLOCK);
        clear(&mut au);

        let before = au.get_parameter(*param).expect("read before");
        // Same CC, same value, same render — but no mapping installed.
        au.send_midi(&[MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::new(20),
            tutti_midi_types::convert::midi1_cc_to_midi2(127),
        )]);
        let mut output = vec![vec![0.0f32; BLOCK as usize]; au.num_outputs() as usize];
        let mut out_refs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        au.process(&[], &mut out_refs, BLOCK).expect("render");

        assert_eq!(
            au.get_parameter(*param).expect("read after"),
            before,
            "{}: with no mapping installed, CC 20 must NOT move parameter {param}",
            unit.label
        );
    }
}

// ----------------------------------------------------- hot map / learn

/// Hot map completes: arm a parameter, send a CC, and the AU reports the mapping
/// it made and adds it to the table.
///
/// Measured on macOS 15.6: arming parameter 900 and sending CC 11 produced
/// `ControlChange { controller: 11 }` and grew the table from 0 to 1.
#[test]
fn arming_a_hot_map_and_sending_a_cc_completes_the_mapping() {
    let _g = lock();
    for (unit, param) in IMPLEMENTERS {
        let mut au = unit.open(RATE, BLOCK);
        clear(&mut au);

        // Nothing armed: no pending map. See the next test for why this is
        // decided on the struct rather than on the property's status.
        assert!(
            au.hot_mapped_parameter().is_none(),
            "{}: a fresh instance must report no pending hot map",
            unit.label
        );

        // Arm with only the parameter target — the AU fills in the trigger.
        let armed = AuMidiMapping {
            trigger: MidiTrigger::Other {
                status: 0,
                data1: 0,
            },
            ..AuMidiMapping::control_change(*param, 0, 0)
        };
        au.hot_map_parameter(&armed)
            .unwrap_or_else(|e| panic!("{}: arm failed: {e:?}", unit.label));
        assert!(
            au.hot_mapped_parameter().is_none(),
            "{}: armed but nothing received yet must still read as no mapping — \
             the AU has not put a status byte in it",
            unit.label
        );

        au.send_midi(&[MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::EXPRESSION,
            tutti_midi_types::convert::midi1_cc_to_midi2(64),
        )]);

        let learned = au.hot_mapped_parameter().unwrap_or_else(|| {
            panic!(
                "{}: after arming parameter {param} and sending CC 11, the AU \
                 must report the mapping it made",
                unit.label
            )
        });
        assert_eq!(
            learned.trigger,
            MidiTrigger::ControlChange { controller: 11 },
            "{}: the AU must have learned the controller that arrived",
            unit.label
        );
        assert_eq!(
            learned.parameter_id, *param,
            "{}: the learned mapping must target the armed parameter",
            unit.label
        );

        // And the completed mapping joins the real table, which is where the
        // routing actually happens.
        let table = au.parameter_midi_mappings().expect("read");
        assert!(
            table.iter().any(|m| m.parameter_id == *param
                && m.trigger == MidiTrigger::ControlChange { controller: 11 }),
            "{}: the learned mapping must appear in the table, got {table:?}",
            unit.label
        );
    }
}

/// The AU deviation that shapes `hot_mapped_parameter`'s signature: the header
/// promises `kAudioUnitErr_InvalidPropertyValue` from an unarmed read, and
/// **neither implementer delivers it** — both answer `noErr` with an all-zero
/// struct.
///
/// So a host cannot use the status to tell "nothing pending" from "mapping
/// complete". This test pins the deviation with the raw property read, so that if
/// a future macOS starts honouring the header the `mStatus == 0` workaround can
/// be revisited deliberately rather than discovered by a bug report.
#[test]
fn an_unarmed_hot_map_read_answers_noerr_not_the_documented_error() {
    let _g = lock();
    for (unit, _) in IMPLEMENTERS {
        let au = unit.open(RATE, BLOCK);
        // 32 bytes: the `AUParameterMIDIMapping` size, pinned by
        // `midi_map::tests::the_mapping_struct_matches_the_c_abi`.
        let mut buf = [0u8; 32];
        let mut size = buf.len() as u32;
        let status = unsafe {
            tutti_au_host::types::AudioUnitGetProperty(
                au.raw_unit(),
                44, // kAudioUnitProperty_HotMapParameterMIDIMapping
                K_AUDIO_UNIT_SCOPE_GLOBAL,
                0,
                buf.as_mut_ptr() as *mut std::os::raw::c_void,
                &mut size,
            )
        };
        assert_eq!(
            status,
            tutti_au_host::types::NO_ERR,
            "{}: measured deviation — an unarmed HotMap read answers noErr, not \
             the kAudioUnitErr_InvalidPropertyValue the header specifies. If this \
             now fails, the OS has started honouring the header and \
             `midi_map::hot_map`'s mStatus==0 rule can be revisited.",
            unit.label
        );
        assert!(
            buf.iter().all(|b| *b == 0),
            "{}: the unarmed struct must be all-zero — that zeroed mStatus is the \
             only signal `hot_map` has to work from, got {buf:?}",
            unit.label
        );
        // Which is exactly why the wrapper reports None rather than a mapping.
        assert!(
            au.hot_mapped_parameter().is_none(),
            "{}: an all-zero struct must not be reported as a real \
             'note off, note 0' mapping",
            unit.label
        );
    }
}

// ----------------------------------------------------- the noErr trap

/// **A `noErr` from `add` proves nothing.** A mapping naming a parameter id no
/// parameter uses is accepted *and appears in the read-back table*.
///
/// This is the crate's twice-paid trap, now confirmed for this property family
/// too (45 AUs accept the MIDI-callback write while publishing no outputs;
/// `AudioUnitScheduleParameters` accepts a bogus id). Recorded as a test so the
/// docs' warning is not merely a claim, and so a host author reading this suite
/// sees why cross-checking against `get_parameter_list` is on them.
///
/// A failure here is *good news* — it means the AU started validating — and the
/// message says so.
#[test]
fn the_au_does_not_validate_the_parameter_id() {
    let _g = lock();
    let (unit, _) = IMPLEMENTERS[0];
    let mut au = unit.open(RATE, BLOCK);
    clear(&mut au);

    let real_ids: Vec<u32> = au.get_parameter_list().iter().map(|p| p.id).collect();
    const BOGUS: u32 = 99_999;
    assert!(
        !real_ids.contains(&BOGUS),
        "the fixture needs an id no parameter uses; {BOGUS} collides with {real_ids:?}"
    );

    let nonsense = AuMidiMapping::control_change(BOGUS, 0, 20);
    let accepted = au.add_parameter_midi_mapping(&[nonsense]).is_ok();
    let table = au.parameter_midi_mappings().expect("read");

    assert!(
        accepted && table.contains(&nonsense),
        "measured: {} accepts a mapping for nonexistent parameter {BOGUS} with \
         noErr and stores it (accepted={accepted}, table={table:?}). If this now \
         fails the AU has started validating, which is GOOD NEWS — relax \
         `add_parameter_midi_mapping`'s docs rather than this assertion.",
        unit.label
    );
}

/// An AU that does not implement the family refuses every *non-empty* write with
/// `kAudioUnitErr_InvalidProperty`, on all four entry points.
///
/// Asserted per-method rather than once, because each writes a different
/// property id: a wrapper wired to the wrong constant would be caught here and
/// nowhere else.
#[test]
fn a_unit_without_mapping_support_refuses_every_write() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    assert!(!au.supports_parameter_midi_mapping());
    let m = [AuMidiMapping::control_change(0, 0, 1)];

    // Plain `fn` pointers rather than boxed closures: each takes the same
    // `(unit, mappings)` shape, so nothing needs to be captured, and a table of
    // four `fn`s reads at a glance where four `Box<dyn FnOnce>`s do not.
    type Write = fn(&mut AuInstance, &[AuMidiMapping]) -> tutti_au_host::Result<()>;
    let attempts: [(&str, Write); 4] = [
        ("add", |au, m| au.add_parameter_midi_mapping(m)),
        ("remove", |au, m| au.remove_parameter_midi_mapping(m)),
        ("set", |au, m| au.set_parameter_midi_mappings(m)),
        ("hot_map", |au, m| au.hot_map_parameter(&m[0])),
    ];
    for (name, attempt) in attempts {
        let err = attempt(&mut au, &m).expect_err(&format!(
            "{name} must be refused by an AU with no mapping support"
        ));
        assert!(
            matches!(err, AuError::OsStatus { code, .. } if code == INVALID_PROPERTY),
            "{name}: expected kAudioUnitErr_InvalidProperty ({INVALID_PROPERTY}), got {err:?}"
        );
    }

    // And reading the hot map is None rather than a panic or a false mapping.
    assert!(au.hot_mapped_parameter().is_none());
}
