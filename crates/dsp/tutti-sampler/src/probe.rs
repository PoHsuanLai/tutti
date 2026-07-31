//! Asking a file whether this sampler can stream it, before committing to play it.
//!
//! A caller choosing a playback tier ([`Source::Memory`](crate::Source) vs
//! [`Source::Disk`](crate::Source)) needs two facts about a file: how long it is,
//! and whether the butler can serve arbitrary ranges from it. Both come from the
//! container header, and neither needs a single audio packet decoded.
//!
//! # Why this lives in the sampler
//!
//! The answer is a fact about **this crate's streaming machinery**, not about the
//! file in isolation. "Can it stream" means "can the butler's incremental decoder
//! seek in it, given the codecs compiled into this build" — three conditions the
//! butler already applies internally when a stream is registered
//! (`butler::handlers::open_stream`). A host deriving the same verdict from
//! `Wave::probe_metadata` would be re-implementing a rule it cannot see, and the
//! two would drift the moment the butler's capabilities changed.
//!
//! So the butler and the host now read the **same function**. That is the whole
//! point of it being public.
//!
//! # What this is not
//!
//! It is not a decode, and it does not open a decoder. `Wave::probe_metadata`
//! reads the container head and stops — the same probe `Wave::load_with_peaks`
//! does, minus the decode loop. Deciding *how much* of a file to keep resident is
//! the caller's policy; this only reports what the file allows.

use std::path::Path;

use tutti_core::{ChannelLayout, SampleRate, Samples};

/// What a header says about a file, and what this sampler can do with it.
///
/// Returned by [`probe`]. Every field is header-derived — no audio was decoded.
///
/// `PartialEq` but not `Eq`: `SampleRate` is float-backed, like every measure
/// unit in the engine's vocabulary. A host caching these compares them with
/// `==` as usual; it just cannot use one as a `HashMap` key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleFacts {
    /// Total frames, if the container reports one.
    ///
    /// `None` for some streamed/VBR formats that do not store a frame count.
    /// A caller sizing a preload window against this must treat `None` as
    /// "unknown", not as "zero" — and note that [`streamable`](Self::streamable)
    /// is already `false` whenever this is `None`, because the butler cannot
    /// stream a file whose end it cannot find.
    pub frames: Option<Samples>,
    /// The file's own rate, which need not be the engine's.
    pub sample_rate: SampleRate,
    /// The file's channel count.
    pub layout: ChannelLayout,
    /// **Whether this build's butler can stream this file.**
    ///
    /// The sampler's verdict, not a property of the file alone. False when the
    /// container reports no frame count, when the decoder declines to seek, or
    /// when no codec feature covering this format was compiled in.
    ///
    /// A caller that sees `false` has no tier choice left: the file must be
    /// played wholly from memory, because the streaming path does not exist for
    /// it. That is the same fallback the butler takes internally
    /// (`load_wave` + `LruCache`).
    pub streamable: bool,
}

impl SampleFacts {
    /// This file's duration in frames, or `Samples(0)` when unknown.
    ///
    /// A convenience for the common "how big is it" question. Deliberately does
    /// **not** hide the `None`: a caller deciding residency must distinguish
    /// "empty" from "unmeasurable", so this is for display and rough sizing
    /// rather than for policy. Policy reads [`frames`](Self::frames).
    pub fn frames_or_zero(self) -> Samples {
        self.frames.unwrap_or(Samples(0))
    }
}

/// Errors a probe can report. Terminal — none of these fix themselves on retry.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// The file could not be opened, or has no track this build can decode.
    ///
    /// Carries the decoder's message as a `String` rather than the error itself:
    /// symphonia's `Error` is neither `Clone` nor `PartialEq`, and a probe result
    /// is something a host caches and compares. Nothing branches on the text.
    #[error("could not read {path}: {message}")]
    Unreadable { path: String, message: String },
}

/// Read `path`'s header and report what this sampler can do with it.
///
/// **Blocking file I/O.** A host on a frame budget runs this off the main thread;
/// this crate deliberately does not choose a task system for it.
///
/// ```no_run
/// # use tutti_sampler::probe;
/// let facts = probe("kick.wav")?;
/// if facts.streamable {
///     // free to pick either tier
/// } else {
///     // must be resident — the streaming path does not exist for this file
/// }
/// # Ok::<(), tutti_sampler::ProbeError>(())
/// ```
pub fn probe(path: impl AsRef<Path>) -> Result<SampleFacts, ProbeError> {
    let path = path.as_ref();
    let meta = tutti_core::Wave::probe_metadata(path).map_err(|e| ProbeError::Unreadable {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(SampleFacts {
        frames: meta.total_frames.map(|f| Samples(f as usize)),
        sample_rate: SampleRate::from(meta.sample_rate),
        layout: ChannelLayout::from(meta.channels),
        streamable: streamable(path, &meta),
    })
}

/// The butler's conditions for streaming a file, in the order it applies them.
///
/// Mirrors `butler::handlers::open_stream` — and the mirroring is the reason this
/// is a named function rather than an inline `&&`: if the butler's conditions
/// change, this is the one other place that must move, and a grep for either name
/// finds both.
fn streamable(path: &Path, meta: &tutti_core::WaveMetadata) -> bool {
    // 1. No frame count means no seekable end — the butler falls back whole-file.
    if meta.total_frames.is_none() {
        return false;
    }
    // 2. The incremental decoder must actually open.
    let Ok(decoder) = tutti_core::FileIn::open(path, None) else {
        return false;
    };
    // 3. Defensive, and inherited from `open_stream`: a decoder that reports
    //    itself non-seekable despite a frame count is still not streamable.
    decoder.seekable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal real PCM wav. `frames` frames of stereo silence.
    fn write_wav(name: &str, frames: usize) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("tutti_sampler_probe_tests");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).expect("create");
        let data = (frames * 2 * 2) as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&48_000u32.to_le_bytes()).unwrap();
        f.write_all(&192_000u32.to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data.to_le_bytes()).unwrap();
        f.write_all(&vec![0u8; data as usize]).unwrap();
        path
    }

    #[test]
    #[cfg(feature = "wav")]
    fn a_wav_reports_its_header_without_decoding() {
        let path = write_wav("probe_ok.wav", 4800);
        let facts = probe(&path).expect("readable");
        assert_eq!(facts.frames, Some(Samples(4800)));
        assert_eq!(facts.sample_rate, SampleRate::from(48_000u32));
        assert_eq!(facts.layout.count(), 2);
        assert!(
            facts.streamable,
            "a local PCM wav has a frame count and seeks"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_default() {
        // The alternative — returning zeroed facts — would let a caller build a
        // voice on a file that is not there and hear nothing, with no error
        // anywhere. Same reasoning as `frames_or_zero` refusing to hide `None`.
        let err = probe("/nonexistent/nowhere.wav").unwrap_err();
        assert!(matches!(err, ProbeError::Unreadable { .. }));
    }

    #[test]
    #[cfg(feature = "wav")]
    fn frames_or_zero_does_not_hide_an_unknown_length() {
        // The convenience and the policy field disagree on purpose: one is for
        // display, the other for decisions.
        let unknown = SampleFacts {
            frames: None,
            sample_rate: SampleRate::from(48_000u32),
            layout: ChannelLayout::Stereo,
            streamable: false,
        };
        assert_eq!(unknown.frames_or_zero(), Samples(0));
        assert_eq!(unknown.frames, None, "the policy field stays honest");
    }

    /// An unstreamable file is not an error — it is a legitimate verdict that
    /// removes the caller's tier *choice* rather than its ability to play.
    #[test]
    #[cfg(feature = "wav")]
    fn a_zero_length_file_still_probes() {
        let path = write_wav("probe_empty.wav", 0);
        let facts = probe(&path).expect("an empty wav is still a valid wav");
        assert_eq!(facts.frames, Some(Samples(0)));
        std::fs::remove_file(&path).ok();
    }
}
