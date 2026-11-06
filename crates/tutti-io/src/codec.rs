//! Which audio formats *this build* can decode.
//!
//! # Why this is a function and not a constant list
//!
//! Codec support is a cargo feature, and `default = []` on this crate — a host
//! that names none can decode nothing. Even `tutti-sampler`, whose own default
//! is `["wav"]`, only forwards: its `wav = ["tutti-io/wav"]` enables no
//! decoder, and the line that pulls one in is this crate's
//! `wav = ["dep:symphonia", "symphonia/wav", "symphonia/pcm"]`.
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
//! convention: a `.wav` holding a codec this build does not enable still
//! fails, and a mislabelled file fails whatever this returns. This answers
//! "is it worth offering?", which is the question a file browser has.

/// Returns every file extension this build can decode, lowercase and without
/// a dot.
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
/// for ext in tutti_io::decodable_extensions() {
///     assert!(!ext.starts_with('.'));
///     assert_eq!(*ext, ext.to_ascii_lowercase());
/// }
/// ```
#[must_use]
pub fn decodable_extensions() -> &'static [&'static str] {
    &EXTENSIONS
}

/// The extensions, derived once from the readers this build registers.
///
/// # Derived, not written down
///
/// Every reader already declares its own extensions in a `Descriptor` — the
/// same data the probe registers itself from — so a hand-written list here
/// would be a second copy of something upstream owns, and would drift
/// (`OggReader` declares seven extensions, `MpaReader` three).
///
/// What stays hand-written is the **feature → reader** mapping below, because
/// that is tutti's own knowledge and not symphonia's: nothing upstream can know
/// that this crate's `wav` resolves to `symphonia/{wav,pcm}` and never
/// `symphonia/aiff`.
///
/// A `LazyLock` rather than a `const`, because `Descriptor::extensions` is only
/// reachable through a trait method. The work is a few slice copies, once per
/// process.
static EXTENSIONS: std::sync::LazyLock<Vec<&'static str>> = std::sync::LazyLock::new(|| {
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    use symphonia::core::probe::QueryDescriptor;

    #[allow(
        unused_mut,
        reason = "mutated only by the feature-gated `add!` expansions; with every codec feature off the list stays empty"
    )]
    let mut exts: Vec<&'static str> = Vec::new();

    // Append every extension a reader declares, skipping repeats. Two containers
    // claiming one extension is possible in principle, and a duplicate would
    // make this list unusable for building a display string.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    macro_rules! add {
        ($reader:ty) => {
            for d in <$reader as QueryDescriptor>::query() {
                for e in d.extensions {
                    if !exts.contains(e) {
                        exts.push(e);
                    }
                }
            }
        };
    }

    // One arm per *feature*. AIFF's absence is the case worth stating: symphonia
    // has an `AiffReader` in the very crate that provides `WavReader`
    // (`symphonia-format-riff`), but this crate's `wav` enables `symphonia/wav`
    // and `symphonia/pcm` only — never `symphonia/aiff`. So no current build
    // decodes AIFF, however much the format resembles WAV, and listing it would
    // be exactly the lie this module exists to stop. Adding it is one line in
    // this crate's `Cargo.toml` plus an arm here.
    #[cfg(feature = "wav")]
    add!(symphonia::default::formats::WavReader);
    #[cfg(feature = "flac")]
    add!(symphonia::default::formats::FlacReader);
    #[cfg(feature = "mp3")]
    add!(symphonia::default::formats::MpaReader);
    #[cfg(feature = "ogg")]
    add!(symphonia::default::formats::OggReader);

    // The descriptors are upstream data, so the case guarantee this module's
    // docs make is asserted rather than assumed. symphonia documents them as
    // case-insensitive and writes them lowercase; a release that shipped an
    // uppercase one would otherwise silently break `can_decode`, which
    // lowercases its argument before comparing.
    debug_assert!(
        exts.iter().all(|e| **e == *e.to_ascii_lowercase()),
        "symphonia declared a non-lowercase extension: {exts:?}"
    );

    exts
});

/// Returns whether this build can decode a file with this extension.
///
/// Case-insensitive, and tolerant of a leading dot, because both spellings turn
/// up: `Path::extension` yields `wav`, while a filter string a user typed or a
/// config file holds is as likely to say `.wav`.
///
/// ```
/// # use tutti_io::can_decode;
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
mod tests;
