//! Which audio formats *this build* can decode.
//!
//! # Why this is a function and not a constant list
//!
//! Codec support is a cargo feature, and `default = []` on this crate — a host
//! that names none can decode nothing. Even `tutti-sampler`, whose own default
//! is `["wav"]`, only forwards: its `wav = ["tutti-core/wav"]` enables no
//! decoder, and the line that pulls one in is this crate's
//! `wav = ["fundsp/wav"]`.
//!
//! So "supported" is a property of the resolved feature set, and the only place
//! that can answer it honestly is where the features terminate. Anywhere else
//! is a copy that goes stale the first time a host builds without `flac`.
//!
//! # Who needs it
//!
//! Anything that decides what to *show* before it tries to decode. A browser
//! listing `.mp3` files on a build without the mp3 codec renders rows that fail
//! on click, with nothing pointing at "this build has no decoder" — the file is
//! fine, the engine simply cannot read it, and that is a different message from
//! a corrupt file.
//!
//! # What this is not
//!
//! Not a claim that a given file will decode. An extension is a filename
//! convention: a `.wav` holding a codec `fundsp` does not implement still
//! fails, and a mislabelled file fails whatever this returns. This answers
//! "is it worth offering?", which is the question a file browser has.

/// Every file extension this build can decode, lowercase and without a dot.
///
/// Empty when no codec feature is enabled, which is the default for this crate
/// and is a legitimate configuration — a host doing pure synthesis needs no
/// decoders at all. Callers must handle the empty case rather than assuming at
/// least `wav`.
///
/// Order is stable (the order the features are declared in) so a caller can use
/// it to build a display string without sorting.
///
/// ```
/// // On a build with no codec features this is empty; with `wav` it contains
/// // "wav". Either way it never contains a leading dot or an uppercase letter.
/// for ext in tutti_core::codec::decodable_extensions() {
///     assert!(!ext.starts_with('.'));
///     assert_eq!(*ext, ext.to_ascii_lowercase());
/// }
/// ```
#[must_use]
pub fn decodable_extensions() -> &'static [&'static str] {
    &[
        #[cfg(feature = "wav")]
        "wav",
        // `.wave` is the same RIFF container under a longer name. Listed
        // beside `wav` rather than treated as an alias, because a caller
        // matching on extensions needs both spellings and has no way to know
        // they are the same format.
        #[cfg(feature = "wav")]
        "wave",
        #[cfg(feature = "flac")]
        "flac",
        #[cfg(feature = "mp3")]
        "mp3",
        #[cfg(feature = "ogg")]
        "ogg",
        // No AIFF. Checked rather than assumed, and the first version of this
        // module got it wrong: symphonia *has* an `aiff` feature
        // (`symphonia-format-riff/aiff`), but `fundsp`'s `wav` maps to
        // `["symphonia/wav", "symphonia/pcm"]` and never enables it. So no
        // current build decodes AIFF, however much the format resembles WAV.
        //
        // Adding it is a one-line change in `fundsp-tutti/Cargo.toml` plus an
        // entry here. Until then, listing it would be exactly the lie this
        // module exists to stop.
    ]
}

/// Whether this build can decode a file with this extension.
///
/// Case-insensitive, and tolerant of a leading dot, because both spellings turn
/// up: `Path::extension` yields `wav`, while a filter string a user typed or a
/// config file holds is as likely to say `.wav`. Normalising here rather than at
/// each call site is what stops one caller lowercasing and another forgetting —
/// the exact shape that let `.RS` slip past one of three predicates in dawai's
/// old browser.
///
/// ```
/// # use tutti_core::codec::can_decode;
/// // Whatever this build enables, these three agree with each other.
/// assert_eq!(can_decode("wav"), can_decode(".WAV"));
/// assert_eq!(can_decode("wav"), can_decode("Wav"));
/// // An extension no feature covers is never decodable.
/// assert!(!can_decode("txt"));
/// ```
#[must_use]
pub fn can_decode(extension: &str) -> bool {
    let ext = extension.trim_start_matches('.').to_ascii_lowercase();
    decodable_extensions().contains(&ext.as_str())
}

#[cfg(test)]
mod tests {
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

    /// Compiled only where a codec is actually on, because with no features the
    /// honest answer is an empty list and there is nothing to assert about it.
    #[cfg(feature = "wav")]
    #[test]
    fn a_wav_build_decodes_wav_and_its_container_alias() {
        assert!(can_decode("wav"));
        assert!(can_decode("wave"));
    }

    /// AIFF is *not* decodable, and this pins it so the next person to assume
    /// otherwise gets a red test rather than a browser row that fails on click.
    ///
    /// It reads like WAV and symphonia can do it, but `fundsp`'s `wav` feature
    /// maps to `["symphonia/wav", "symphonia/pcm"]` — `symphonia/aiff` is never
    /// turned on. Delete this test when the feature is added, not before.
    #[cfg(feature = "wav")]
    #[test]
    fn aiff_is_not_decodable_however_much_it_looks_like_wav() {
        assert!(!can_decode("aiff"));
        assert!(!can_decode("aif"));
    }

    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    #[test]
    fn a_build_with_no_codecs_decodes_nothing() {
        // `default = []` on this crate, so this is the *default* build rather
        // than an exotic one. A caller assuming at least wav is wrong here.
        assert!(decodable_extensions().is_empty());
        assert!(!can_decode("wav"));
    }
}
