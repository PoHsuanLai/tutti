//! Conformance for `.aupreset` file I/O — the interchange format for AU state.
//!
//! A `.aupreset` is how a patch leaves this host and reaches Logic, Live or
//! Reaper, and how a downloaded preset gets in. So the suite has to prove two
//! different things, and the second is the one that is easy to fake:
//!
//! * **Round-trip** — a file this host writes restores the state it captured.
//!   Every such test mutates the parameter *away* first, so a load that did
//!   nothing at all cannot pass by accident.
//! * **Interoperability** — the format is what other hosts actually speak. A
//!   round-trip cannot show this: a host that wrote and read its own private
//!   dialect would pass every round-trip test ever written. Only loading a file
//!   *Apple* authored can, which is what
//!   `an_apple_authored_preset_file_loads_and_applies` does — see
//!   `support/corpus.rs::find_apple_preset_for`.
//!
//! ## Why validation is the centre of gravity here
//!
//! Applying a preset means handing the AU an opaque `data` blob it casts to its
//! own internal state struct. That is an untrusted-input path, and the host is
//! the *only* guard on it. Measured on macOS 15.6, in two steps:
//!
//! * AUDistortion's `ClassInfo` offered to AUDelay verbatim is refused (-10851),
//!   by all seven units tried — the four Apple ones plus TDR Nova, TAL Reverb 4
//!   and TAL-NoiseMaker. So an AU appears to check.
//! * But it checks the **identity keys, not the blob**. Relabel AUDistortion's
//!   dictionary with AUDelay's type/subtype/manufacturer and AUDelay **accepts**
//!   it, adopting Dry/Wet 4.6, Delay Time 9.73 s and a Lowpass Cutoff of 0.5 Hz
//!   where it had 15 kHz — an inaudible plugin with no visible cause.
//!
//! So `a_preset_from_a_different_au_is_refused` is not a politeness check. The AU
//! trusts what it is told; if this host does not compare the codes, nothing does.
//!
//! ## The malformed-input half
//!
//! `src/instance.rs` already has `corrupt_state_is_rejected` for the raw blob
//! path. These cover the **file** path, where the bytes come off disk and can be
//! anything: not a plist, a plist whose root is an array, a dictionary missing the
//! identity keys, a truncated plist, and an empty file. Each must be a typed error
//! and never a panic — and in every case the AU must still **render afterwards**,
//! because a host that rejects a bad preset by leaving the plugin wedged has
//! turned a bad file into a dead channel.
//!
//! Per the review lesson: where a rendered buffer is asserted to be *small*, it is
//! also asserted **finite**. `peak()` folds with `f32::max`, which returns the
//! non-NaN operand, so an all-NaN buffer has a peak of 0.0 and would sail through
//! a magnitude-only check.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_aupreset
//! ```
//!
//! Temp files go under the OS temp dir via [`TempPreset`], which deletes on drop
//! (including on panic) so a failing assertion does not leave litter. Nothing is
//! written inside the repo. As in `au_conformance.rs`, a missing corpus unit
//! **fails** rather than skipping.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod support;
use support::corpus::{
    all_finite, find_apple_preset_for, peak, render, silence, DELAY, DISTORTION, LOWPASS,
    SPATIAL_MIXER,
};

use tutti_au_host::AuError;

/// Serializes these tests against each other, mirroring `AU_LOCK` in
/// `au_conformance.rs`: component discovery walks a process-global registry and
/// several tests here open the same unit. A separate `static` because each test
/// binary links its own copy.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would turn one
/// real failure into N spurious ones. The guard is only a serializer — there is no
/// shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// A temp-file path that deletes itself on drop.
///
/// RAII rather than a `remove_file` at the end of each test body, because a
/// failing assertion panics past that line and would leave the file behind. Named
/// with the test's own label plus the process id so two suites (or two cargo
/// invocations) cannot collide on one path.
struct TempPreset(PathBuf);

impl TempPreset {
    fn new(label: &str) -> Self {
        let name = format!("tutti-au-{}-{}.aupreset", label, std::process::id());
        Self(std::env::temp_dir().join(name))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPreset {
    fn drop(&mut self) {
        // Best-effort: the file may never have been created (a save that failed is
        // exactly what several tests assert), and a missing file is not an error.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Write `bytes` to a temp path, for the malformed-input tests.
fn write_bytes(label: &str, bytes: &[u8]) -> TempPreset {
    let tmp = TempPreset::new(label);
    std::fs::write(tmp.path(), bytes).expect("writing a temp file must succeed");
    tmp
}

/// Assert the AU still renders finite audio, and report `context` if it does not.
///
/// Every malformed-preset test ends with this. Rejecting a bad file is only half
/// the contract — the other half is that the plugin is still usable, since a host
/// that wedges a channel on a bad preset has made the failure worse than the file.
///
/// Silence in, so the assertion is on *finiteness and boundedness*, not on the
/// unit's transfer function. The finite check is not redundant with the magnitude
/// check: `peak()` folds with `f32::max`, which returns the non-NaN operand, so an
/// all-NaN buffer reports a peak of 0.0 and would pass a magnitude test alone.
fn assert_still_renders(au: &mut tutti_au_host::AuInstance, context: &str) {
    let input = silence(au.num_inputs().max(1) as usize, BLOCK as usize);
    let mut output = silence(au.num_outputs() as usize, BLOCK as usize);
    render(au, &input, &mut output, BLOCK)
        .unwrap_or_else(|e| panic!("{context}: the AU stopped rendering after the rejection: {e}"));
    assert!(
        all_finite(&output),
        "{context}: the AU emitted non-finite samples after the rejection. \
         Asserted separately from the magnitude below because peak() folds with \
         f32::max, which returns the non-NaN operand — an all-NaN block has a \
         peak of 0.0 and would pass the magnitude check alone."
    );
    assert!(
        peak(&output) < 1.0,
        "{context}: silence in produced a peak of {} out — the AU's state is \
         corrupt even though the preset was refused",
        peak(&output)
    );
}

// -------------------------------------------------------------- round-tripping

/// Saving then loading must restore the parameter the save captured.
///
/// The parameter is mutated to a *second* distinct value between the save and the
/// load, which is what makes this test meaningful: without that step a
/// `load_preset_file` that did nothing whatsoever would still leave the parameter
/// at the saved value and pass.
///
/// Measured on macOS 15.6: AUDelay's "Delay Time" (id 1) sits at 1.0 on a fresh
/// instance and takes both 0.75 and 0.05, so the three values are distinct and the
/// restore is unambiguous.
#[test]
fn a_saved_preset_restores_the_parameter_it_captured() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let target = params
        .iter()
        .find(|p| p.name == "Delay Time")
        .expect("AUDelay must expose a 'Delay Time' parameter");

    const SAVED: f32 = 0.75;
    const MUTATED: f32 = 0.05;

    au.set_parameter(target.id, SAVED).unwrap();
    let captured = au.get_parameter(target.id).unwrap();
    assert!(
        (captured - SAVED).abs() < 1e-4,
        "the AU did not take the value being saved; got {captured}"
    );

    let tmp = TempPreset::new("roundtrip");
    au.save_preset_file(tmp.path(), "RoundTrip").unwrap();

    // Move it away, so a no-op load cannot pass.
    au.set_parameter(target.id, MUTATED).unwrap();
    let moved = au.get_parameter(target.id).unwrap();
    assert!(
        (moved - MUTATED).abs() < 1e-4,
        "the mutation-away step did not take, so the restore below would be \
         untestable; got {moved}"
    );

    let identity = au.load_preset_file(tmp.path()).unwrap();
    let restored = au.get_parameter(target.id).unwrap();
    assert!(
        (restored - SAVED).abs() < 1e-4,
        "loading the preset restored {restored}, expected the saved {SAVED}. \
         (The value had been moved to {MUTATED} first, so this is a real restore \
         rather than a no-op.)"
    );
    assert_eq!(
        identity.name.as_deref(),
        Some("RoundTrip"),
        "the loaded identity must carry the name the save was given"
    );
}

/// The written file must be a real property list with the keys Apple's header
/// names, because that — not this host's own reader — is what other hosts parse.
///
/// The identity keys are asserted against the AU's **own** component codes rather
/// than against constants, since the contract is that they are derived from the AU
/// and not invented. `data` is asserted present and non-empty: it is the actual
/// state, and a file carrying only identity keys would round-trip through this
/// host's validation and restore nothing.
#[test]
fn a_written_preset_is_a_plist_with_the_documented_keys() {
    let _g = lock();
    let au = DELAY.open(RATE, BLOCK);
    let tmp = TempPreset::new("keys");
    au.save_preset_file(tmp.path(), "KeyCheck").unwrap();

    // Parse with `plutil`, i.e. with CoreFoundation's own reader through a
    // completely separate process. Asserting via this crate's parser would be
    // circular — a private dialect would satisfy it.
    let out = std::process::Command::new("plutil")
        .arg("-p")
        .arg(tmp.path())
        .output()
        .expect("plutil ships with macOS");
    assert!(
        out.status.success(),
        "plutil could not parse the file this host wrote, so no other host will \
         either: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dump = String::from_utf8_lossy(&out.stdout);

    for key in ["type", "subtype", "manufacturer", "version", "name", "data"] {
        assert!(
            dump.contains(&format!("\"{key}\"")),
            "the written preset is missing the `{key}` key. Apple's \
             AudioUnitProperties.h defines it as part of the ClassInfo \
             dictionary; a file without it is not a .aupreset. Got:\n{dump}"
        );
    }

    // The identity must be the AU's own, read back through this crate's parser and
    // compared against what AudioToolbox says the component is.
    let info = DELAY.require();
    let identity = tutti_au_host::read_preset_metadata(tmp.path()).unwrap();
    assert!(
        identity.matches(
            u32::from_be_bytes(*b"aufx"),
            info.sub_type,
            info.manufacturer_code
        ),
        "the file's identity {identity:?} does not name the AU it was saved from \
         (subtype {:#x}, manufacturer {:#x}). The keys must come from the AU's own \
         component description, never from a caller-supplied guess.",
        info.sub_type,
        info.manufacturer_code
    );
    assert_eq!(
        identity.name.as_deref(),
        Some("KeyCheck"),
        "the name written must be the one the caller passed"
    );
}

/// Metadata must be readable without instantiating or touching any AU.
///
/// This is what a preset browser is built on: it lists hundreds of files, and
/// loading each one into a live plugin merely to read its label would be both slow
/// and a side effect a listing must not have.
///
/// The proof that nothing was applied: a *second* AU's parameter is sampled before
/// and after the metadata read, and the read is of a preset saved from a
/// **different** unit at a non-default value. If `read_preset_metadata` applied
/// anything, that parameter would move.
#[test]
fn metadata_can_be_read_without_applying_anything() {
    let _g = lock();

    // A preset from AUDelay, holding a deliberately non-default value.
    let mut delay = DELAY.open(RATE, BLOCK);
    let delay_params = delay.get_parameter_list();
    let delay_target = delay_params
        .iter()
        .find(|p| p.name == "Delay Time")
        .expect("AUDelay must expose 'Delay Time'");
    delay.set_parameter(delay_target.id, 0.9).unwrap();
    let tmp = TempPreset::new("metaonly");
    delay.save_preset_file(tmp.path(), "MetaOnly").unwrap();

    // A live AULowpass, whose state must be undisturbed by reading the file.
    let mut lowpass = LOWPASS.open(RATE, BLOCK);
    let lp_params = lowpass.get_parameter_list();
    let lp_target = &lp_params[0];
    let before = lowpass.get_parameter(lp_target.id).unwrap();

    let identity = tutti_au_host::read_preset_metadata(tmp.path())
        .expect("reading metadata off a well-formed preset must succeed");

    assert_eq!(identity.name.as_deref(), Some("MetaOnly"));
    assert_eq!(
        identity.component_type,
        u32::from_be_bytes(*b"aufx"),
        "the metadata must report the type recorded in the file"
    );
    assert_eq!(
        identity.sub_type,
        u32::from_be_bytes(*b"dely"),
        "the metadata must report the subtype recorded in the file"
    );

    let after = lowpass.get_parameter(lp_target.id).unwrap();
    assert_eq!(
        before, after,
        "reading preset metadata changed a live AU's parameter ({before} -> \
         {after}). Listing a preset folder must never push state into a plugin."
    );
    assert_still_renders(&mut lowpass, "after a metadata-only read");
}

// -------------------------------------------------------------- the validation

/// A preset saved from a different AU must be **refused**, not applied.
///
/// The reason this matters is measured, not theoretical — see the module docs. The
/// AU validates the identity keys and not the blob, so a mislabelled file reaches
/// its internal state struct intact. This host is the layer that has to compare.
///
/// Both directions are checked (a delay preset offered to a distortion and the
/// reverse) so the refusal cannot be an artefact of one unit's blob happening to
/// be the wrong size for the other.
#[test]
fn a_preset_from_a_different_au_is_refused() {
    let _g = lock();

    let delay = DELAY.open(RATE, BLOCK);
    let delay_file = TempPreset::new("xdelay");
    delay
        .save_preset_file(delay_file.path(), "FromDelay")
        .unwrap();

    let distortion = DISTORTION.open(RATE, BLOCK);
    let dist_file = TempPreset::new("xdist");
    distortion
        .save_preset_file(dist_file.path(), "FromDistortion")
        .unwrap();

    for (label, mut victim, foreign, expected_sub) in [
        (
            "AUDistortion",
            DISTORTION.open(RATE, BLOCK),
            delay_file.path(),
            "dely",
        ),
        ("AUDelay", DELAY.open(RATE, BLOCK), dist_file.path(), "dist"),
    ] {
        // Capture enough state to prove nothing moved.
        let params = victim.get_parameter_list();
        let before: Vec<f32> = params
            .iter()
            .map(|p| victim.get_parameter(p.id).unwrap_or(f32::NAN))
            .collect();

        match victim.load_preset_file(foreign) {
            Err(AuError::PresetIdentityMismatch(mismatch)) => {
                let (file_sub_type, au_sub_type) = (&mismatch.file_sub_type, &mismatch.au_sub_type);
                assert_eq!(
                    file_sub_type, expected_sub,
                    "{label}: the error must name the subtype the FILE claims"
                );
                assert_ne!(
                    au_sub_type, file_sub_type,
                    "{label}: a mismatch error whose two subtypes agree is \
                     reporting nonsense"
                );
            }
            Err(other) => panic!(
                "{label}: a foreign preset must fail as PresetIdentityMismatch so a \
                 host can tell 'wrong plugin' from 'broken file'; got {other:?}"
            ),
            Ok(id) => panic!(
                "{label}: ACCEPTED a preset belonging to {id:?}. This is the \
                 corruption path the module exists to close — measured on macOS \
                 15.6, an AU handed a foreign blob under its own identity adopts \
                 garbage parameter values (AUDelay took a 0.5 Hz lowpass cutoff \
                 where it had 15 kHz)."
            ),
        }

        // The refusal must be inert: the AU keeps every parameter it had.
        let after: Vec<f32> = params
            .iter()
            .map(|p| victim.get_parameter(p.id).unwrap_or(f32::NAN))
            .collect();
        assert_eq!(
            before, after,
            "{label}: the refused load still moved the AU's parameters. A \
             validation that mutates before it refuses is worse than none."
        );
        assert_still_renders(
            &mut victim,
            &format!("{label} after a refused foreign preset"),
        );
    }
}

/// The identity comparison must require **all three** codes to agree.
///
/// A unit test on `matches` rather than a plugin test, because the negative cases
/// cannot be produced with real AUs: no two installed units share a subtype but
/// differ in manufacturer. Each arm below is a collision that would occur in the
/// wild — a subtype is only unique *within* a manufacturer's catalog, so two
/// vendors reusing `"dely"` is expected, not exotic.
#[test]
fn identity_matching_requires_all_three_codes() {
    let fx = u32::from_be_bytes(*b"aufx");
    let dely = u32::from_be_bytes(*b"dely");
    let appl = u32::from_be_bytes(*b"appl");

    let identity = tutti_au_host::AuPresetIdentity {
        component_type: fx,
        sub_type: dely,
        manufacturer: appl,
        version: 0,
        name: None,
    };

    assert!(
        identity.matches(fx, dely, appl),
        "the exact triple must match"
    );
    assert!(
        !identity.matches(u32::from_be_bytes(*b"aumu"), dely, appl),
        "a different component TYPE must not match: an instrument and an effect \
         that share a subtype are unrelated plugins"
    );
    assert!(
        !identity.matches(fx, u32::from_be_bytes(*b"dist"), appl),
        "a different SUBTYPE must not match — this is the common case, two \
         plugins from one vendor"
    );
    assert!(
        !identity.matches(fx, dely, u32::from_be_bytes(*b"TDRl")),
        "a different MANUFACTURER must not match. Subtypes are only unique within \
         a vendor's catalog, so two vendors both using `dely` is expected; \
         matching on subtype alone would apply one vendor's blob to the other's \
         plugin."
    );
}

/// A version difference alone must NOT block a load.
///
/// This pins the judgement call documented in `src/aupreset.rs`: a preset saved
/// from v1.0 of a plugin has to keep working after the user updates to v1.1, which
/// is the normal case rather than the exception. Only the AU knows whether its own
/// blob format changed between versions, and it receives the `version` value to
/// decide with — so the host reports the version and does not adjudicate it.
///
/// Asserted through `matches`, which is where the policy lives: the struct carries
/// a version and the comparison ignores it.
#[test]
fn a_version_difference_alone_does_not_block_a_load() {
    let fx = u32::from_be_bytes(*b"aufx");
    let dely = u32::from_be_bytes(*b"dely");
    let appl = u32::from_be_bytes(*b"appl");

    let old = tutti_au_host::AuPresetIdentity {
        component_type: fx,
        sub_type: dely,
        manufacturer: appl,
        version: 0,
        name: None,
    };
    let newer = tutti_au_host::AuPresetIdentity {
        version: 0x0001_0100,
        ..old.clone()
    };

    assert!(
        old.matches(fx, dely, appl) && newer.matches(fx, dely, appl),
        "two presets differing only in `version` must both match the same AU. \
         Refusing on version would invalidate a user's whole preset library on \
         every plugin update, to prevent a corruption the AU itself is better \
         placed to detect."
    );
    assert_ne!(
        old.version, newer.version,
        "the version must still be REPORTED, so a host can warn even though the \
         match ignores it"
    );
}

// ------------------------------------------------------------ malformed input

/// Every shape of bad file must be a typed error, never a panic — and the AU must
/// still render afterwards.
///
/// `src/instance.rs::corrupt_state_is_rejected` covers the raw blob; this is the
/// file path, where bytes come off disk and can be anything. The cases are chosen
/// to hit distinct branches:
///
/// * **not a plist** — arbitrary bytes; CoreFoundation's parser refuses
/// * **truncated plist** — a real binary plist cut in half, which is what an
///   interrupted download or a full disk actually produces. Distinct from random
///   bytes because the header parses and the trailer does not.
/// * **root is not a dictionary** — a legal plist whose root is an array. Reading
///   this as a dictionary is the type-confusion the code's `CFGetTypeID` gate
///   exists for.
/// * **missing identity keys** — a legal dictionary with no `type`/`subtype`/
///   `manufacturer`. It cannot be validated, so it must not be applied.
/// * **empty file** — zero bytes, named separately because CoreFoundation reports
///   it with the same opaque failure as corruption.
#[test]
fn every_malformed_preset_is_a_typed_error_and_leaves_the_au_renderable() {
    let _g = lock();

    // A genuine binary plist to truncate, produced by the host itself.
    let good = {
        let au = DELAY.open(RATE, BLOCK);
        let tmp = TempPreset::new("tosnip");
        au.save_preset_file(tmp.path(), "ToSnip").unwrap();
        std::fs::read(tmp.path()).unwrap()
    };
    assert!(
        good.len() > 40,
        "the reference preset is too small to truncate meaningfully ({} bytes)",
        good.len()
    );

    // A legal plist whose root is an ARRAY, not a dictionary.
    let array_root = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
        \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\"><array><string>not a dict</string></array></plist>"
        .to_vec();

    // A legal DICTIONARY with none of the identity keys.
    let no_identity = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
        \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
        <plist version=\"1.0\"><dict><key>name</key><string>Nameless</string>\
        </dict></plist>"
        .to_vec();

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "not-a-plist",
            b"this is not a property list, it is prose".to_vec(),
        ),
        ("truncated", good[..good.len() / 2].to_vec()),
        ("array-root", array_root),
        ("missing-identity", no_identity),
        ("empty", Vec::new()),
    ];

    for (label, bytes) in cases {
        let tmp = write_bytes(label, &bytes);
        let mut au = DELAY.open(RATE, BLOCK);

        // The load must fail, and specifically as InvalidPreset: these are broken
        // FILES, which a host reports differently from "wrong plugin"
        // (PresetIdentityMismatch) and from an AU's own refusal (OsStatus).
        match au.load_preset_file(tmp.path()) {
            Err(AuError::InvalidPreset(_)) => {}
            Err(other) => panic!(
                "{label}: expected AuError::InvalidPreset so a host can say 'this \
                 file is broken' rather than 'wrong plugin'; got {other:?}"
            ),
            Ok(id) => panic!(
                "{label}: a malformed file was ACCEPTED as {id:?}. Applying \
                 unvalidated bytes as an AU's internal state is the corruption \
                 path this suite exists to close."
            ),
        }

        // Metadata reads must fail the same way, since a preset browser hits these
        // files first and must not panic while listing a folder.
        assert!(
            matches!(
                tutti_au_host::read_preset_metadata(tmp.path()),
                Err(AuError::InvalidPreset(_))
            ),
            "{label}: read_preset_metadata must also report InvalidPreset — a \
             browser scanning a folder of junk must not panic"
        );

        assert_still_renders(&mut au, &format!("after rejecting the {label} preset"));
    }
}

/// A path that does not exist must be a filesystem error, distinct from a corrupt
/// file.
///
/// The distinction is what lets a host retry or re-prompt: "the file moved" and
/// "the file is broken" are different problems with different fixes, and
/// collapsing them into one error loses that.
#[test]
fn a_missing_file_is_an_io_error_not_a_format_error() {
    let _g = lock();
    let missing = std::env::temp_dir().join("tutti-au-definitely-not-here.aupreset");
    let _ = std::fs::remove_file(&missing);

    let mut au = DELAY.open(RATE, BLOCK);
    match au.load_preset_file(&missing) {
        Err(AuError::PresetIo(_)) => {}
        other => panic!(
            "a nonexistent path must report PresetIo, not a format error — a host \
             distinguishes 'the file moved' from 'the file is broken'; got {other:?}"
        ),
    }
    assert!(matches!(
        tutti_au_host::read_preset_metadata(&missing),
        Err(AuError::PresetIo(_))
    ));
    assert_still_renders(&mut au, "after a missing-file load");
}

// ------------------------------------------------------------ interoperability

/// A `.aupreset` file **Apple authored** must load and apply.
///
/// This is the only test in the suite that proves *interoperability* rather than
/// self-consistency, and it is the point of the whole feature. Every round-trip
/// test above would pass even if this host had invented a private dialect that no
/// other program could read; only parsing a file written by someone else can rule
/// that out.
///
/// Measured on macOS 15.6: 55 Apple-authored `.aupreset` files live under
/// `/System/Library/Audio/Tunings/**/AU/` bearing `aumx`/`3dem`/`appl`, i.e.
/// AUSpatialMixer — which `auval -v aumx 3dem appl` passes. Their dictionaries
/// carry the same six keys this host writes plus a `data` blob, and AUSpatialMixer
/// additionally stores `InputProperties`/`OutputProperties`/`GlobalProperties`,
/// which the load path preserves by applying the parsed dictionary verbatim rather
/// than a re-serialization of the keys it recognises.
///
/// Note `/Library/Audio/Presets/` — the directory the AU docs name for loose
/// presets — does **not exist** on this machine, and no `.aupreset` file appears
/// anywhere under `/Library/Audio` or `~/Library/Audio`: Apple's units ship their
/// presets as in-bundle factory presets. The Tunings tree is where genuine Apple
/// preset *files* actually are.
///
/// If a future OS drops these files the test reports that rather than silently
/// passing, because a skip here would retire the suite's only interop evidence.
#[test]
fn an_apple_authored_preset_file_loads_and_applies() {
    let _g = lock();

    let Some(preset) = find_apple_preset_for(&SPATIAL_MIXER) else {
        panic!(
            "no Apple-authored .aupreset found for AUSpatialMixer under {:?}. \
             55 such files were measured on macOS 15.6; their absence retires \
             this suite's only interoperability evidence, so it is reported \
             rather than skipped. If the OS genuinely no longer ships them, \
             replace this with a checked-in fixture authored by another host.",
            support::corpus::APPLE_PRESET_DIRS
        );
    };

    // Read it without applying first: a browser must be able to list Apple's own
    // files, not merely this host's.
    let metadata = tutti_au_host::read_preset_metadata(&preset).unwrap_or_else(|e| {
        panic!(
            "failed to parse Apple's own preset {}: {e}. This host cannot read the \
             format it claims to write.",
            preset.display()
        )
    });
    let info = SPATIAL_MIXER.require();
    assert!(
        metadata.matches(
            u32::from_be_bytes(*b"aumx"),
            info.sub_type,
            info.manufacturer_code
        ),
        "the preset picked ({}) does not identify AUSpatialMixer: {metadata:?}",
        preset.display()
    );

    let mut au = SPATIAL_MIXER.open(RATE, BLOCK);
    let applied = au.load_preset_file(&preset).unwrap_or_else(|e| {
        panic!(
            "AUSpatialMixer refused an Apple-authored preset ({}): {e}. Either the \
             identity comparison is wrong or the dictionary is not reaching the AU \
             intact — this host's validation must not reject files the format's \
             own author wrote.",
            preset.display()
        )
    });
    assert_eq!(
        applied, metadata,
        "the identity reported by the load must match the metadata-only read of \
         the same file"
    );

    assert_still_renders(&mut au, "after applying an Apple-authored preset");
}

/// Presets must survive a save/load across two *separate instances* of the AU.
///
/// The round-trip test above reuses one instance, which cannot distinguish a real
/// serialization from an AU that happened to keep its state in memory. Sharing a
/// patch means the loading instance is a different process entirely, and a fresh
/// instance is the closest this suite gets to that.
#[test]
fn a_preset_crosses_between_two_instances() {
    let _g = lock();

    let target_name = "Delay Time";
    const SAVED: f32 = 0.6;

    let tmp = TempPreset::new("crossinstance");
    let saved_value = {
        let mut writer = DELAY.open(RATE, BLOCK);
        let params = writer.get_parameter_list();
        let target = params.iter().find(|p| p.name == target_name).unwrap();
        writer.set_parameter(target.id, SAVED).unwrap();
        writer
            .save_preset_file(tmp.path(), "CrossInstance")
            .unwrap();
        writer.get_parameter(target.id).unwrap()
    };

    // A brand-new instance, which starts at the factory default (1.0, measured) —
    // so the restore below moves it.
    let mut reader = DELAY.open(RATE, BLOCK);
    let params = reader.get_parameter_list();
    let target = params.iter().find(|p| p.name == target_name).unwrap();
    let fresh = reader.get_parameter(target.id).unwrap();
    assert!(
        (fresh - saved_value).abs() > 1e-3,
        "a fresh AUDelay already sits at the saved value ({fresh}), so this test \
         could not tell a real restore from a no-op. Pick a different value."
    );

    reader.load_preset_file(tmp.path()).unwrap();
    let restored = reader.get_parameter(target.id).unwrap();
    assert!(
        (restored - saved_value).abs() < 1e-4,
        "a preset written by one instance restored {restored} in another, expected \
         {saved_value}. Sharing a patch depends on exactly this."
    );
    assert_still_renders(&mut reader, "after a cross-instance restore");
}

// ------------------------------------------------- the document-restore variant

/// `load_document_state` must restore state even though no unit on this machine
/// implements `kAudioUnitProperty_ClassInfoFromDocument`.
///
/// Apple's header requires a host restoring a *document* to try property 50 first
/// and fall back to `ClassInfo` when the AU errors or does not implement it.
/// Measured on macOS 15.6: **every** unit probed answers 50 with
/// `kAudioUnitErr_InvalidProperty` (-10879) — AUDelay, AUDistortion,
/// AUMatrixReverb, AUSpatialMixer, AULowpass. So the fallback is the live path,
/// and what must be proved is that the try-first does not break it: an
/// implementation that propagated the -10879 would fail every project load.
///
/// Mutated away before the restore, as in the round-trip test, so a no-op cannot
/// pass.
#[test]
fn document_state_restores_through_the_documented_fallback() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    let params = au.get_parameter_list();
    let target = params.iter().find(|p| p.name == "Delay Time").unwrap();

    au.set_parameter(target.id, 0.8).unwrap();
    let blob = au.get_state().unwrap();
    assert!(!blob.is_empty(), "get_state produced nothing to restore");

    au.set_parameter(target.id, 0.1).unwrap();
    assert!((au.get_parameter(target.id).unwrap() - 0.1).abs() < 1e-4);

    au.load_document_state(&blob).unwrap_or_else(|e| {
        panic!(
            "load_document_state failed: {e}. Every unit measured refuses \
             ClassInfoFromDocument (-10879), so this must have fallen back to \
             ClassInfo — propagating the refusal would break every project load."
        )
    });
    let restored = au.get_parameter(target.id).unwrap();
    assert!(
        (restored - 0.8).abs() < 1e-4,
        "the document restore left {restored}, expected 0.8"
    );

    // Empty input is a documented no-op, not an error: a project with no saved
    // plugin state must load rather than fail.
    au.load_document_state(&[]).unwrap();
    assert!(
        (au.get_parameter(target.id).unwrap() - 0.8).abs() < 1e-4,
        "an empty document blob must be inert, not destructive"
    );

    assert_still_renders(&mut au, "after a document-state restore");
}

/// Saving must not depend on the AU being initialized.
///
/// A host saves a project while wiring plugins up, and `ClassInfo` is a
/// global-scope property with no render resources behind it — the same reasoning
/// `presets_and_bypass_work_before_initialize` records for the preset and bypass
/// properties. Gating preset I/O on the Ready state would make a host initialize
/// every plugin just to write a file.
#[test]
fn presets_round_trip_before_initialize() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);
    assert!(!au.is_initialized());

    let params = au.get_parameter_list();
    let target = params
        .iter()
        .find(|p| p.name == "Delay Time")
        .expect("parameters must be enumerable before initialize");
    au.set_parameter(target.id, 0.42).unwrap();

    let tmp = TempPreset::new("preinit");
    au.save_preset_file(tmp.path(), "PreInit")
        .expect("saving a preset in the Loaded state must work");

    au.set_parameter(target.id, 0.9).unwrap();
    let identity = au
        .load_preset_file(tmp.path())
        .expect("loading a preset in the Loaded state must work");
    assert_eq!(identity.name.as_deref(), Some("PreInit"));
    let restored = au.get_parameter(target.id).unwrap();
    assert!(
        (restored - 0.42).abs() < 1e-4,
        "a pre-initialize round trip restored {restored}, expected 0.42"
    );

    // And the AU must still initialize and render afterwards.
    au.initialize()
        .expect("the AU must still initialize after pre-init preset I/O");
    assert_still_renders(&mut au, "after a pre-initialize round trip");
}
