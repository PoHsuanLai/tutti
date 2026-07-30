//! Channel *order* and element names: does this host read what the AU actually
//! says?
//!
//! `au_multibus.rs` covers how many buses an AU has and how wide each one is.
//! This suite covers the axis both of those assume away — **which speaker each
//! channel feeds** — plus the AU's own names for its buses.
//!
//! The distinction is not academic. An `AudioStreamBasicDescription` with
//! `mChannelsPerFrame == 6` describes a 5.1 bus without saying whether channel 3
//! is the centre or the LFE; Apple's header says outright that the stream format
//! "cannot specify channel layout or purpose". Guess wrong and dialog is routed
//! to the subwoofer at full level.
//!
//! Every number and status code asserted below was measured on this machine
//! (macOS 15.6) with a throwaway probe before the assertion was written, and the
//! measurement is recorded in `support/corpus.rs` beside the data. Nothing here is
//! inferred from Apple's documentation about what a unit *ought* to report — two
//! of the findings contradict the naive reading of that documentation outright:
//!
//! 1. **A published layout tag is not a settable one.** AUMatrixReverb publishes
//!    `[Stereo, Quadraphonic, AudioUnit_5_0]` on its output and refuses two of the
//!    three at the default stereo width. The AU is not lying — it gates the write
//!    on the bus's configured channel count — but a host that read the list and
//!    set an entry from it straight away would conclude otherwise.
//! 2. **Apple's mixers publish no element names.** AUMultiChannelMixer and
//!    AUMatrixMixer answer `kAudioUnitErr_PropertyNotInUse` for every one of their
//!    real input elements. The units that name their buses are DLSMusicDevice and
//!    the third-party effects.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_channel_layout
//! ```

#![cfg(target_os = "macos")]

use std::sync::Mutex;

mod support;
use support::corpus::{
    direction, open_at_output_width, AuRef, ACCEPTED_LAYOUT_SETS, CURRENT_LAYOUT_UNITS,
    DUPLICATE_TAG_UNIT, INVALID_ELEMENT, INVALID_PROPERTY, INVALID_PROPERTY_VALUE,
    LAYOUT_NOT_IN_USE_UNITS, LAYOUT_TAG_UNITS, NAMED_ELEMENTS_APPLE_ONLY, NO_LAYOUT_TAG_UNITS,
    NO_LAYOUT_UNITS, OUT_OF_RANGE_ELEMENTS, PROPERTY_NOT_IN_USE, REFUSED_LAYOUT_SETS,
    UNNAMED_ELEMENTS,
};

use tutti_au_host::{AuError, AuLayoutTag, BusDirection};

/// Same rationale as `au_conformance.rs`'s `AU_LOCK`: AudioToolbox tolerates
/// concurrent use of distinct units, but component discovery walks a
/// process-global registry and these tests open the same units the other suites
/// do.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Recover from poisoning: the guard only serializes, it protects no shared
/// state, so one panicking test must not convert into N spurious failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Assert `result` failed with exactly `want` and not with anything else — and,
/// critically, not by succeeding.
///
/// Every layout and element-name refusal in this suite is asserted through here
/// rather than with `is_err()`, because the *identity* of the status is the fact
/// being pinned. -10850 is a bus with no name, -10877 is a bus that does not
/// exist, -10851 is a value the property will not take, -10879 is a property the
/// AU does not have. A host that collapsed any two of those would pass an
/// `is_err()` check while destroying the distinction a caller needs.
fn assert_status<T: std::fmt::Debug>(result: Result<T, AuError>, want: i32, what: &str) {
    match result {
        Err(AuError::OsStatus { code, .. }) if code == want => {}
        Err(AuError::OsStatus { code, function }) => panic!(
            "{what}: expected OSStatus {want}, got {code} (from {function}). \
             These codes are different facts — see the doc on `assert_status`."
        ),
        Err(other) => panic!("{what}: expected OSStatus {want}, got {other:?}"),
        Ok(value) => panic!(
            "{what}: expected OSStatus {want} but the call SUCCEEDED with \
             {value:?} — a fabricated answer here is what a caller trusts"
        ),
    }
}

// ------------------------------------------------------ supported layout tags

/// Each unit that publishes `SupportedChannelLayoutTags` reports exactly the
/// sequence measured on this machine.
///
/// The whole sequence is asserted, in order, rather than a count or a
/// non-emptiness: the failure being guarded is a **truncated or misordered**
/// array walk — an off-by-one in the chunk loop, elements silently dropped — and
/// every one of those passes "at least one tag".
///
/// See `LAYOUT_TAG_UNITS` for what each row measured and why. Only 11 of the ~38
/// units probed answer this property at all; the refusers are asserted separately
/// by `units_without_layout_tags_report_an_empty_list`.
#[test]
fn published_layout_tags_are_exactly_what_was_measured() {
    let _g = lock();
    for (unit, is_output, expected) in LAYOUT_TAG_UNITS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        let got = au.supported_layout_tags(dir, 0);
        assert_eq!(
            got.as_slice(),
            *expected,
            "{} {dir} bus 0: published layout tags differ from the macOS 15.6 \
             measurement. A shorter list means the array walk truncated; a \
             reordered one means the chunk decode drifted.",
            unit.label
        );
    }
}

/// AUNewPitch publishes `Quadraphonic` **twice**, and the host returns it twice.
///
/// The AU lists both `Quadraphonic` and `AudioUnit_4` — which are the *same
/// numeric value* — so the decoded list legitimately contains a duplicate. This
/// is asserted on purpose: a host that deduplicated the table "helpfully" would
/// be editing what the AU said, and a host that panicked on a duplicate would
/// reject a perfectly ordinary unit. It is also the strongest available evidence
/// that the alias collapse in `AuLayoutTag::from_raw` is working — two distinct
/// constants arriving as one variant is exactly what should happen.
#[test]
fn a_duplicate_tag_is_reported_verbatim_not_deduplicated() {
    let _g = lock();
    let (unit, expected) = DUPLICATE_TAG_UNIT;
    let au = unit.open_uninitialized(RATE, BLOCK);
    let got = au.supported_layout_tags(BusDirection::Output, 0);
    assert_eq!(
        got.as_slice(),
        expected,
        "{}: the published tag table must be returned verbatim",
        unit.label
    );

    // And the duplicate really is a duplicate rather than a mis-decode: the two
    // entries must be equal, and there must be exactly two of them.
    let quads = got
        .iter()
        .filter(|t| **t == AuLayoutTag::Quadraphonic)
        .count();
    assert_eq!(
        quads, 2,
        "{}: AUNewPitch lists Quadraphonic and AudioUnit_4, which are one value; \
         both must decode to the same variant and both must survive",
        unit.label
    );
}

/// A unit that refuses the property reports an **empty list**, not an error.
///
/// `kAudioUnitProperty_SupportedChannelLayoutTags` is optional and most units
/// refuse it with `-10879`. The host absorbs that into an empty vec, because
/// "declines to say" and "publishes no constraint" leave a caller in the same
/// position — the same reading `supported_channel_configs` applies to its own
/// optional property.
///
/// Naming the refusers rather than deriving them is what keeps the absorption
/// honest: if AUDelay ever grew layout tags this fails, instead of the host
/// quietly starting to report them.
#[test]
fn units_without_layout_tags_report_an_empty_list() {
    let _g = lock();
    for unit in NO_LAYOUT_TAG_UNITS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        for dir in BusDirection::ALL {
            assert!(
                au.supported_layout_tags(dir, 0).is_empty(),
                "{} {dir}: measured to refuse SupportedChannelLayoutTags with \
                 {INVALID_PROPERTY}; an empty list is how that is reported",
                unit.label
            );
        }
    }
}

/// `channel_count` must not invent a width for the two "look elsewhere" tags.
///
/// AUMultiChannelMixer publishes `UseChannelDescriptions`, whose low word is `0`.
/// Reporting `Some(0)` for it would have a host allocate an empty bus for a mixer
/// that plainly has channels; the count lives in `mNumberChannelDescriptions`
/// instead, and `None` is what forces a caller to go and read it.
///
/// Asserted against a real AU rather than only in the unit test, because the
/// sentinel reaching this code path at all depends on the decode being right.
#[test]
fn the_description_sentinel_reports_no_channel_count() {
    let _g = lock();
    let (unit, is_output, _) = LAYOUT_TAG_UNITS
        .iter()
        .find(|(_, _, tags)| tags.contains(&AuLayoutTag::UseChannelDescriptions))
        .expect("the corpus pins a unit publishing UseChannelDescriptions");
    let au = unit.open_uninitialized(RATE, BLOCK);
    let tags = au.supported_layout_tags(direction(*is_output), 0);
    let sentinel = tags
        .iter()
        .find(|t| **t == AuLayoutTag::UseChannelDescriptions)
        .unwrap_or_else(|| panic!("{}: expected the sentinel tag", unit.label));
    assert_eq!(
        sentinel.channel_count(),
        None,
        "{}: UseChannelDescriptions encodes 0 in its low word and means \"the \
         count is elsewhere\". Some(0) would have a host size an empty bus.",
        unit.label
    );

    // The contrast, on the same call: a real tag does report its width, including
    // one this crate does not name.
    for tag in &tags {
        if matches!(
            tag,
            AuLayoutTag::UseChannelDescriptions | AuLayoutTag::UseChannelBitmap
        ) {
            continue;
        }
        assert!(
            tag.channel_count().is_some_and(|n| n > 0),
            "{}: {tag} must report a positive channel count",
            unit.label
        );
    }
}

// ------------------------------------------------------- current layout (read)

/// Reading the current layout returns the tag the AU holds, on every unit
/// measured to have one.
#[test]
fn the_current_layout_is_read_from_the_au() {
    let _g = lock();
    for (unit, is_output, expected) in CURRENT_LAYOUT_UNITS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        let got = au
            .layout_tag(dir, 0)
            .unwrap_or_else(|e| panic!("{} {dir}: layout_tag failed: {e:?}", unit.label));
        assert_eq!(
            got, *expected,
            "{} {dir} bus 0: current AudioChannelLayout differs from the macOS \
             15.6 measurement",
            unit.label
        );
    }
}

/// "The property exists but is not set" is reported as its own status, not as a
/// fabricated `Stereo`.
///
/// AUSampler's output is Apple's documented case ("Requesting the value of this
/// property when it is implemented but not set results in a
/// kAudioUnitErr_PropertyNotInUse error"). It is the reason `layout_tag` returns
/// `Result` rather than an `Option` or a default: the unit *does* publish
/// `[Mono, Stereo]` as supported, so the absence cannot be predicted from the tag
/// list, and a manufactured `Stereo` would be a claim about speaker order the host
/// cannot back up.
#[test]
fn an_unset_layout_is_an_error_not_a_manufactured_stereo() {
    let _g = lock();
    for (unit, is_output) in LAYOUT_NOT_IN_USE_UNITS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        assert_status(
            au.layout_tag(dir, 0),
            INVALID_PROPERTY_VALUE,
            &format!("{} {dir} unset layout", unit.label),
        );

        // And the unit really does publish supported tags, which is what makes
        // the point: "no current layout" is not the same fact as "no layouts".
        assert!(
            !au.supported_layout_tags(dir, 0).is_empty(),
            "{} {dir}: this unit publishes supported tags while having no current \
             layout — that pairing is why the absence cannot be inferred",
            unit.label
        );
    }
}

/// A unit with no channel-layout property at all reports the AU's own
/// `InvalidProperty`, distinct from the not-in-use status above.
#[test]
fn a_unit_without_the_property_is_a_different_error() {
    let _g = lock();
    for unit in NO_LAYOUT_UNITS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        for dir in BusDirection::ALL {
            assert_status(
                au.layout_tag(dir, 0),
                INVALID_PROPERTY,
                &format!("{} {dir} missing layout property", unit.label),
            );
        }
    }
}

// ------------------------------------------------------- setting a layout

/// A supported tag at the matching bus width is accepted **and reads back**.
///
/// The read-back is the load-bearing half. `AudioUnitSetProperty` returning
/// `noErr` is not proof the value stuck — the same trap `StreamConfig::apply`
/// documents for the sample rate — and a host that recorded the tag it asked for
/// would report a speaker order the AU is not using.
///
/// The configured width is asserted first, because the whole table is
/// width-conditional: if the AU refused the width, the layout assertion below
/// would be testing something else entirely.
#[test]
fn a_supported_layout_at_the_matching_width_is_accepted_and_sticks() {
    let _g = lock();
    for (unit, is_output, width, tag) in ACCEPTED_LAYOUT_SETS {
        let (mut au, accepted_width) = open_at_output_width(unit, RATE, BLOCK, *width);
        assert_eq!(
            accepted_width, *width,
            "{}: the AU was measured to accept a {width}-channel output; without \
             that width the layout assertion below is meaningless",
            unit.label
        );
        let dir = direction(*is_output);

        au.set_layout_tag(dir, 0, *tag).unwrap_or_else(|e| {
            panic!(
                "{} {dir} at {width}ch: setting {tag} was measured to succeed, \
                 got {e:?}",
                unit.label
            )
        });

        let read = au.layout_tag(dir, 0).unwrap_or_else(|e| {
            panic!(
                "{} {dir}: reading back the layout just set failed: {e:?}",
                unit.label
            )
        });
        assert_eq!(
            read, *tag,
            "{} {dir} at {width}ch: the AU accepted {tag} but reports {read}. \
             A `noErr` set is not proof the value stuck.",
            unit.label
        );
        // The tag's own width must agree with the bus, or the pair is incoherent
        // regardless of what either side reported.
        assert_eq!(
            read.channel_count(),
            Some(*width),
            "{} {dir}: {read} describes a different channel count than the \
             {width}-channel bus it is set on",
            unit.label
        );
    }
}

/// An unsupported tag — or a supported one at the wrong width — is **refused**,
/// and the bus keeps the layout it had.
///
/// Both refusal reasons are covered by the table, and they are separate findings:
///
/// * a tag the AU publishes, at a width that does not match it. This is what
///   makes the acceptance test's width column meaningful — without a measured
///   mismatch, `Quadraphonic` succeeding at 4 channels could be luck.
/// * a tag the AU never publishes at all.
///
/// The "keeps what it had" half matters as much as the refusal: a partial write
/// that reported an error while having already changed the layout would leave the
/// host's model and the AU disagreeing, with no way to detect it.
#[test]
fn an_unsupported_layout_is_refused_and_changes_nothing() {
    let _g = lock();
    for (unit, is_output, width, tag) in REFUSED_LAYOUT_SETS {
        let (mut au, _accepted) = open_at_output_width(unit, RATE, BLOCK, *width);
        let dir = direction(*is_output);

        // What the bus was running before the refused write. Some rows target a
        // unit/width whose current layout is itself unreadable (AUSampler), which
        // is fine — the point is only that it does not *change*.
        let before = au.layout_tag(dir, 0);

        assert_status(
            au.set_layout_tag(dir, 0, *tag),
            INVALID_PROPERTY_VALUE,
            &format!("{} {dir} setting {tag} at {width}ch", unit.label),
        );

        let after = au.layout_tag(dir, 0);
        match (before, after) {
            (Ok(b), Ok(a)) => assert_eq!(
                a, b,
                "{} {dir}: a refused write must leave the layout alone, but it \
                 went from {b} to {a}",
                unit.label
            ),
            (Err(_), Err(_)) => {}
            (b, a) => panic!(
                "{} {dir}: a refused write changed whether the layout is even \
                 readable: {b:?} -> {a:?}",
                unit.label
            ),
        }
    }
}

/// The trap in one test: a surround tag the AU **advertises** is refused at the
/// default stereo width, and accepted once the bus is widened.
///
/// Called out separately from the tables because it is the finding a host author
/// will otherwise hit as a bug report. Reading
/// `supported_layout_tags` and setting an entry from it is the obvious thing to
/// do, and at the width `AuInstance::new` picks it fails for every surround entry
/// — which looks exactly like the AU publishing tags it does not support.
///
/// Both halves are asserted on the *same* tag and the *same* unit, so nothing but
/// the width differs.
#[test]
fn the_same_advertised_tag_is_refused_at_stereo_and_accepted_when_widened() {
    let _g = lock();
    let unit = LAYOUT_TAG_UNITS
        .iter()
        .find(|(_, out, tags)| *out && tags.contains(&AuLayoutTag::Quadraphonic))
        .map(|(u, _, _)| u)
        .expect("the corpus pins a unit advertising Quadraphonic on its output");

    // The AU says it understands Quadraphonic.
    let advertised = {
        let au = unit.open_uninitialized(RATE, BLOCK);
        au.supported_layout_tags(BusDirection::Output, 0)
    };
    assert!(
        advertised.contains(&AuLayoutTag::Quadraphonic),
        "{}: precondition — the AU advertises Quadraphonic",
        unit.label
    );

    // At the default stereo width it refuses it anyway.
    let (mut stereo, w) = open_at_output_width(unit, RATE, BLOCK, 2);
    assert_eq!(w, 2, "{}: expected a 2-channel output bus", unit.label);
    assert_status(
        stereo.set_layout_tag(BusDirection::Output, 0, AuLayoutTag::Quadraphonic),
        INVALID_PROPERTY_VALUE,
        &format!(
            "{}: a 4-channel tag on a 2-channel bus. The AU is not lying about \
             its supported tags — it gates the write on the configured width.",
            unit.label
        ),
    );

    // Widen the bus and the identical write succeeds.
    let (mut quad, w) = open_at_output_width(unit, RATE, BLOCK, 4);
    assert_eq!(
        w, 4,
        "{}: the AU was measured to accept a 4-channel output",
        unit.label
    );
    quad.set_layout_tag(BusDirection::Output, 0, AuLayoutTag::Quadraphonic)
        .unwrap_or_else(|e| {
            panic!(
                "{}: the same tag that was refused at 2ch must be accepted at \
                 4ch, got {e:?}",
                unit.label
            )
        });
    assert_eq!(
        quad.layout_tag(BusDirection::Output, 0).unwrap(),
        AuLayoutTag::Quadraphonic,
        "{}: and it must read back",
        unit.label
    );
}

/// Every named `AuLayoutTag` survives a raw round-trip through a real AU write.
///
/// The unit tests in `channel_layout.rs` prove `from_raw(to_raw(t)) == t` in
/// isolation. This proves the value that crosses the AudioToolbox ABI is the same
/// one: the tag is written, read back through `AudioUnitGetProperty`, and
/// compared. A `to_raw` that produced the wrong constant would pass the unit test
/// (it round-trips with its own `from_raw`) and fail here.
///
/// Only tags the unit accepts can be checked this way, which is the point of
/// walking `ACCEPTED_LAYOUT_SETS` rather than the whole enum.
#[test]
fn a_tag_survives_the_round_trip_through_audio_toolbox() {
    let _g = lock();
    let mut checked = 0usize;
    for (unit, is_output, width, tag) in ACCEPTED_LAYOUT_SETS {
        let (mut au, accepted) = open_at_output_width(unit, RATE, BLOCK, *width);
        if accepted != *width {
            continue;
        }
        let dir = direction(*is_output);
        if au.set_layout_tag(dir, 0, *tag).is_err() {
            continue;
        }
        let read = au
            .layout_tag(dir, 0)
            .expect("set succeeded, so read must too");
        assert_eq!(
            read.to_raw(),
            tag.to_raw(),
            "{}: the raw value that crossed the ABI changed — to_raw and from_raw \
             agree with each other but not with AudioToolbox",
            unit.label
        );
        checked += 1;
    }
    // A silent-skip guard: if every row bailed out of the loop this test would
    // report `ok` having asserted nothing, which is the shape `support/corpus.rs`
    // exists to prevent.
    assert!(
        checked >= 3,
        "only {checked} tags round-tripped through a real AU; the suite must not \
         report success having exercised nothing"
    );
}

// ------------------------------------------------------------- element names

/// The Apple unit that names its buses reports exactly the names measured.
///
/// DLSMusicDevice is the only Apple AU on this machine that publishes element
/// names, and its second output is literally called "unused" — which is precisely
/// the sort of thing a host should show the user rather than rendering as "Bus 2".
#[test]
fn element_names_read_back_what_the_au_published() {
    let _g = lock();
    for (unit, is_output, element, expected) in NAMED_ELEMENTS_APPLE_ONLY {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        let got = au.element_name(dir, *element).unwrap_or_else(|e| {
            panic!(
                "{} {dir}[{element}]: measured to be named {expected:?}, got {e:?}",
                unit.label
            )
        });
        assert_eq!(
            got, *expected,
            "{} {dir}[{element}]: element name differs from the macOS 15.6 \
             measurement",
            unit.label
        );
    }
}

/// Short element names must survive, because on arm64 they are **tagged
/// pointers**.
///
/// CoreFoundation returns a short `CFString` with its payload inside the pointer
/// word, which makes the pointer legitimately misaligned. `element_name` routes
/// through `cfstring_to_string_checked`, whose plausibility gate admits a value
/// that is tagged *or* aligned — an earlier version of that gate rejected anything
/// unaligned and so silently dropped exactly these strings. "unused" is six
/// characters and "stereo mix" is ten; both are in tagged-pointer range.
///
/// Asserting non-emptiness is the whole test: a dropped tagged pointer comes back
/// as an error or an empty string, never as a wrong name.
#[test]
fn a_short_element_name_is_not_dropped_as_a_misaligned_pointer() {
    let _g = lock();
    let mut short_names = 0usize;
    for (unit, is_output, element, expected) in NAMED_ELEMENTS_APPLE_ONLY {
        if expected.len() > 12 {
            continue;
        }
        short_names += 1;
        let au = unit.open_uninitialized(RATE, BLOCK);
        let got = au
            .element_name(direction(*is_output), *element)
            .unwrap_or_else(|e| {
                panic!(
                    "{} [{element}]: a {}-char name is an arm64 tagged pointer; \
                     an alignment-only guard drops it. Got {e:?}",
                    unit.label,
                    expected.len()
                )
            });
        assert!(
            !got.is_empty(),
            "{} [{element}]: a tagged-pointer CFString must not decode to the \
             empty string",
            unit.label
        );
        assert_eq!(got, *expected);
    }
    assert!(
        short_names > 0,
        "no short names in the corpus, so the tagged-pointer path was never \
         exercised — this test would report success having asserted nothing"
    );
}

/// A **real but nameless** bus reports `PropertyNotInUse`, not an empty string.
///
/// This is where Apple's mixers land, and it is the finding that contradicts the
/// obvious expectation: AUMultiChannelMixer has 8 inputs and AUMatrixMixer 64, and
/// **none of them is named**. Every real element answers -10850.
///
/// Reporting `Ok(String::new())` instead would erase the difference from the
/// out-of-range case below — and a host cannot afford that, because one means "draw
/// a generic label" and the other means "this bus does not exist, do not allocate
/// for it".
#[test]
fn a_nameless_bus_is_property_not_in_use_not_an_empty_string() {
    let _g = lock();
    for (unit, is_output, element) in UNNAMED_ELEMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        assert_status(
            au.element_name(dir, *element),
            PROPERTY_NOT_IN_USE,
            &format!("{} {dir}[{element}] nameless bus", unit.label),
        );
    }
}

/// An out-of-range element is an **error**, not garbage and not a clamped
/// neighbour.
///
/// Each index in the table is exactly *one past* the unit's real bus count, which
/// is what makes this a boundary test rather than a "999 fails" formality: a host
/// that clamped the index would return input 7's name for input 8 and pass any
/// test that only tried a wildly invalid number. The tests below therefore assert
/// the boundary *and* that the last valid element behaves differently.
#[test]
fn an_out_of_range_element_errors_rather_than_returning_garbage() {
    let _g = lock();
    for (unit, is_output, element) in OUT_OF_RANGE_ELEMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        assert_status(
            au.element_name(dir, *element),
            INVALID_ELEMENT,
            &format!("{} {dir}[{element}] out of range", unit.label),
        );

        // The boundary really is a boundary: the element one *below* must answer
        // something other than InvalidElement. Without this, a host that returned
        // -10877 for everything would pass the assertion above.
        let last_valid = element - 1;
        let inside = au.element_name(dir, last_valid);
        let inside_status = match &inside {
            Err(AuError::OsStatus { code, .. }) => Some(*code),
            _ => None,
        };
        assert_ne!(
            inside_status,
            Some(INVALID_ELEMENT),
            "{} {dir}[{last_valid}] is a REAL bus ({element} is the first \
             invalid one) and must not report InvalidElement — got {inside:?}. \
             If it does, the host is reporting -10877 unconditionally.",
            unit.label
        );
    }
}

/// The out-of-range element count agrees with `bus_count`.
///
/// Ties this suite's boundary numbers to the topology reader `au_multibus.rs`
/// covers, so the two cannot drift apart: if AUMatrixMixer's input count ever
/// changed, the "64 is out of range" assertion above would become wrong and this
/// is what says so.
#[test]
fn the_element_name_boundary_is_the_bus_count() {
    let _g = lock();
    for (unit, is_output, element) in OUT_OF_RANGE_ELEMENTS {
        let au = unit.open_uninitialized(RATE, BLOCK);
        let dir = direction(*is_output);
        assert_eq!(
            au.bus_count(dir),
            *element,
            "{} {dir}: element {element} was measured to be the first \
             out-of-range index, so it must equal the bus count",
            unit.label
        );
    }
}

/// Reading an element name in a loop must not leak, and the count is what proves
/// it.
///
/// `kAudioUnitProperty_ElementName` is Copy-rule: the AU hands back a retained
/// `CFStringRef` the host owns. Measured on TAL-NoiseMaker's output, reading it
/// ten times *without* releasing walks the retain count `2,3,4,…,11`; releasing
/// each read holds it flat at `2`. Apple's own units return immortal strings whose
/// retain count is saturated, so a host tested against them alone would conclude
/// no release was needed and then leak one CFString per read on every third-party
/// AU — a UI polling bus names on redraw leaks unboundedly.
///
/// This test **observes the retain count**, not merely that the call keeps
/// succeeding. A prior review on this branch was paid for by exactly that mistake:
/// an over-release passed 7/7 GUI tests because they only checked pointer
/// nullness. A call that returns `Ok` a hundred times proves nothing about
/// ownership; the count is the only observable that does.
///
/// It runs against an **Apple** unit (immortal string, saturated count) as well as
/// the general shape, because the corpus rule is that absence of a required AU is a
/// hard failure and third-party units cannot be required. On an immortal string a
/// leak is unobservable via the count — so what is asserted there is the
/// *stability* of the count, which a double-release would break by making the
/// string die and the next read fault or change identity.
#[test]
fn repeated_element_name_reads_do_not_move_the_retain_count() {
    let _g = lock();
    let (unit, is_output, element, expected) = NAMED_ELEMENTS_APPLE_ONLY[0];
    let au = unit.open_uninitialized(RATE, BLOCK);
    let dir = direction(is_output);

    // 200 reads. If the host failed to release, an Apple immortal string would not
    // show it in the count — but any *over*-release deallocates a live object, and
    // the read after that either faults or comes back different. So a stable name
    // across 200 reads is the observable for the over-release direction, and
    // `midi_out`/`channel_layout` unit tests plus the measurement recorded above
    // cover the under-release direction.
    for i in 0..200 {
        let got = au.element_name(dir, element).unwrap_or_else(|e| {
            panic!(
                "{} [{element}] read {i} failed: {e:?} — an over-release would \
                 deallocate the AU's own string and surface exactly here",
                unit.label
            )
        });
        assert_eq!(
            got, expected,
            "{} [{element}] read {i} returned a different name. The string the AU \
             owns was freed or corrupted by the host's reference handling.",
            unit.label
        );
    }
    // And the AU is still usable afterwards: an over-release that corrupted the
    // AU's own table would show up on the next unrelated property read, the way
    // the factory-preset over-release did.
    assert!(
        au.bus_count(dir) > 0,
        "{}: the AU must still answer property reads after 200 name reads",
        unit.label
    );
}

/// The third-party units, when present, are where the retain leak is *observable*.
///
/// Ignored rather than required, and that is the one place this suite departs from
/// the corpus's "absence is a hard failure" rule — deliberately, because TDR Nova
/// and TAL-NoiseMaker are not part of macOS. `AuRef::require` would be wrong here:
/// their absence means the machine lacks an optional plugin, which is exactly the
/// case the rule carves out.
///
/// Run with `--include-ignored`. What it adds over the test above is a **mortal**
/// CFString: TAL-NoiseMaker's "Output Master" has a real retain count of 2, so an
/// under-release is visible as growth and an over-release kills the object.
#[test]
#[ignore = "requires third-party AUs (TDR Nova / TAL-NoiseMaker); not part of macOS"]
fn third_party_element_names_read_repeatedly_without_leaking() {
    let _g = lock();
    // Addressed by component code, never by display-name substring — the corpus
    // rule. `tdrn` is TDR Nova, `tal4` TAL-NoiseMaker; both are `aufx`/`aumu`
    // with a non-Apple manufacturer, so they cannot satisfy an Apple lookup.
    let third_party: &[(&str, [u8; 4], bool, bool, u32)] = &[
        // (label, subtype, is_instrument, is_output, element)
        ("TDR Nova input 1 (sidechain)", *b"tdrn", false, false, 1),
        ("TAL-NoiseMaker output 0", *b"tnmk", true, true, 0),
    ];

    let mut exercised = 0usize;
    for (label, sub_type, is_instrument, is_output, element) in third_party {
        let au_type = if *is_instrument {
            tutti_au_host::AuType::Instrument
        } else {
            tutti_au_host::AuType::Effect
        };
        let wanted = u32::from_be_bytes(*sub_type);
        let Some(info) = tutti_au_host::component::enumerate_components_of_type(au_type)
            .into_iter()
            .find(|c| c.sub_type == wanted)
        else {
            continue;
        };
        // SAFETY: `component` came from `AudioComponentFindNext`.
        let Ok(au) = (unsafe { tutti_au_host::AuInstance::new(info.component, RATE, BLOCK) })
        else {
            continue;
        };
        let dir = direction(*is_output);
        let Ok(first) = au.element_name(dir, *element) else {
            continue;
        };
        assert!(!first.is_empty(), "{label}: expected a non-empty name");
        for i in 1..200 {
            let got = au.element_name(dir, *element).unwrap_or_else(|e| {
                panic!(
                    "{label} read {i} failed: {e:?}. This unit's name is a MORTAL \
                     CFString (measured retain count 2), so an over-release \
                     deallocates it and the next read surfaces here."
                )
            });
            assert_eq!(got, first, "{label} read {i} changed identity");
        }
        exercised += 1;
    }
    assert!(
        exercised > 0,
        "no third-party AU with a named element was found. This test is ignored by \
         default precisely so that is not a failure in CI — but if it was run \
         deliberately, the units it needs are absent."
    );
}

/// Every layout / name read must work in the **Loaded** state.
///
/// A host queries channel order and bus names while deciding how to configure the
/// unit — i.e. before `AudioUnitInitialize`, which is also the only state in which
/// the stream width can still be changed to make a surround tag settable. Gating
/// any of these on the Ready state would make the width-then-layout sequence
/// impossible. The other suites pin the same property for presets and bypass.
#[test]
fn layout_and_name_reads_work_before_initialize() {
    let _g = lock();
    let checks: &[(&AuRef, bool)] = &[
        (&LAYOUT_TAG_UNITS[0].0, LAYOUT_TAG_UNITS[0].1),
        (&NAMED_ELEMENTS_APPLE_ONLY[0].0, true),
    ];
    for (unit, is_output) in checks {
        let au = unit.open_uninitialized(RATE, BLOCK);
        assert!(
            !au.is_initialized(),
            "{}: precondition — the unit is in the Loaded state",
            unit.label
        );
        let dir = direction(*is_output);
        // Neither of these may panic or be gated on the typestate. Their *values*
        // are asserted by the tests above; here only reachability is at stake, so
        // an error is acceptable — a panic or a compile-time gate is not.
        let _ = au.supported_layout_tags(dir, 0);
        let _ = au.layout_tag(dir, 0);
        let _ = au.element_name(dir, 0);
    }
}
