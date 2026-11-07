use super::*;

#[test]
fn every_listed_extension_is_decodable() {
    // The two functions cannot disagree: `can_decode` is the membership
    // test for the list, and a caller that filters with one and displays
    // the other must get the same answer.
    for ext in decodable_extensions() {
        assert!(can_decode(ext), "{ext} is listed but not decodable");
    }
}

#[test]
fn an_unknown_extension_is_never_decodable() {
    for ext in ["txt", "rs", "", "wavv", "a.wav"] {
        assert!(!can_decode(ext), "{ext:?} was reported decodable");
    }
}

#[test]
fn a_leading_dot_and_case_are_both_tolerated() {
    // Both spellings reach this from real callers: `Path::extension` gives
    // no dot, a user-typed filter usually has one.
    for ext in decodable_extensions() {
        let dotted = format!(".{ext}");
        let shouty = ext.to_ascii_uppercase();
        assert!(can_decode(&dotted), "{dotted} was rejected");
        assert!(can_decode(&shouty), "{shouty} was rejected");
    }
}

#[test]
fn the_list_holds_no_dots_and_no_uppercase() {
    // The contract callers rely on to compare against `Path::extension`
    // without normalising first.
    for ext in decodable_extensions() {
        assert!(!ext.starts_with('.'), "{ext} carries a leading dot");
        assert_eq!(*ext, ext.to_ascii_lowercase(), "{ext} is not lowercase");
    }
}

#[test]
fn the_list_has_no_duplicates() {
    // Two features listing the same extension would make a display string
    // read "wav, wav". Cheap to check, and the kind of thing a new codec
    // feature introduces by copy-paste.
    let all = decodable_extensions();
    for (i, a) in all.iter().enumerate() {
        assert!(
            !all[i + 1..].contains(a),
            "{a} appears twice in decodable_extensions()"
        );
    }
}

/// The list is *derived*, and this is the case that proves it: `OggReader`
/// declares seven extensions, and a hand-written list naming `ogg` alone
/// would hide decodable `.opus` and `.oga` files.
#[cfg(feature = "decode-ogg")]
#[test]
fn the_ogg_feature_covers_every_extension_its_reader_declares() {
    for ext in ["ogg", "oga", "ogv", "ogx", "ogm", "spx", "opus"] {
        assert!(can_decode(ext), "{ext} is decodable but was not listed");
    }
}

/// Likewise: the `decode-mp3` feature registers three descriptors, not one.
#[cfg(feature = "decode-mp3")]
#[test]
fn the_mp3_feature_covers_mp1_and_mp2_as_well() {
    for ext in ["mp1", "mp2", "mp3"] {
        assert!(can_decode(ext), "{ext} is decodable but was not listed");
    }
}

/// Compiled only where a codec is actually on, because with no features the
/// honest answer is an empty list and there is nothing to assert about it.
#[cfg(feature = "decode-wav")]
#[test]
fn a_wav_build_decodes_wav_and_its_container_alias() {
    assert!(can_decode("wav"));
    assert!(can_decode("wave"));
}

/// AIFF is *not* decodable, and this pins it so the next person to assume
/// otherwise gets a red test rather than a browser row that fails on click.
///
/// It reads like WAV and symphonia can do it, but this crate's `decode-wav` feature
/// maps to `["symphonia/wav", "symphonia/pcm"]` — `symphonia/aiff` is never
/// turned on. Delete this test when the feature is added, not before.
#[cfg(feature = "decode-wav")]
#[test]
fn aiff_is_not_decodable_however_much_it_looks_like_wav() {
    assert!(!can_decode("aiff"));
    assert!(!can_decode("aif"));
}

// No test for the codec-free build: this crate's test targets always have
// every codec on (its dev-dependency on itself), so such a test could never
// run here. The property holds by construction instead: every `add!` above is gated on
// its feature, and `cargo check -p tutti-codec` builds that configuration.
