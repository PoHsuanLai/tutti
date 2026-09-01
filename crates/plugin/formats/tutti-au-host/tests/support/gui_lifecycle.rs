// The AU Cocoa editor-lifecycle tests and their helpers, shared verbatim by two
// targets:
//
// - `tests/au_gui_lifecycle.rs` — the default cargo harness, where every test
//   is `#[ignore]`d. Cargo runs `#[test]`s on worker threads, and AppKit
//   requires the process main thread, so running them there would be undefined
//   behaviour rather than a test.
// - `tests/au_gui_lifecycle_main.rs` — a `harness = false` binary that owns
//   `main()`, and so runs these on the main thread. That is the only
//   configuration macOS accepts, and it matches how a real host drives an
//   editor.
//
// It lives under `tests/support/` because cargo compiles every top-level file
// in `tests/` as its own target; a shared module must sit in a subdirectory or
// it would be built a third time on its own.
//
// Each test below is wrapped in `gui_test!`, which the two roots define
// differently: the harness one attaches `#[test]`/`#[ignore]`, the main-thread
// one emits a plain function. The attributes cannot be written here directly —
// rustc strips an `#[ignore]` function out of a `harness = false` binary, so
// the runner would have nothing left to call.

use std::sync::Mutex;

use tutti_au_host::AuEditor;
use tutti_au_host::AuInstance;
use tutti_au_host::types::K_AUDIO_UNIT_ERR_INVALID_PROPERTY;
use tutti_au_host::AuError;

use support::corpus::{AuRef, DELAY, WITHOUT_COCOA_VIEW, WITH_COCOA_VIEW};

/// AudioToolbox tolerates concurrent use of *distinct* units, but component
/// discovery walks a process-global registry and opening a Cocoa view loads a
/// shared view-factory bundle into the process. Serializing keeps one test's
/// bundle load from racing another's. Mirrors `AU_LOCK` in `au_conformance.rs`.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// `AU_LOCK` is poisoned by any panicking test, and a poisoned lock would
/// convert one real failure into N spurious ones. The guard is only a
/// serializer — there is no shared state to be left inconsistent — so recover.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

/// Open `au`'s editor with no parent view.
///
/// `AuEditor::open(unit, None)` instantiates the plugin's `NSView` without
/// adding it to a window hierarchy, which is why this suite needs no winit,
/// no event loop and no window — unlike the VST3 equivalent, where the plugin
/// is handed a parent handle and cannot be created without one. Verified
/// empirically before these tests were written: all 22 view-advertising units
/// on this machine instantiate headless.
///
/// # Safety
/// `au` must be a live, initialized `AuInstance`, and the caller must be on the
/// macOS main thread — which is the whole point of the `harness = false`
/// runner.
unsafe fn open_headless(au: &AuInstance) -> tutti_au_host::Result<AuEditor> {
    // The size the host would like; a plugin is free to ignore it. 800×600 is
    // what this crate hardcoded before the parameter existed, kept here so
    // these tests exercise the same request they always did.
    let preferred = tutti_plugin_types::EditorSize {
        width: 800,
        height: 600,
    };
    AuEditor::open(au.raw_unit(), None, preferred)
}

/// Instantiate and initialize `unit`, panicking with its label on failure.
///
/// Absence of an AU is a hard failure (see `support/corpus.rs`); this keeps
/// that rule on the editor path too.
fn ready(unit: &AuRef) -> AuInstance {
    unit.open(RATE, BLOCK)
}

/// The Objective-C `retainCount` of `view`, or 0 for a null pointer.
///
/// Only ever compared against *itself* across cycles — see the call site for why
/// the absolute value is not a contract. `objc_msgSend` is declared here rather
/// than pulled from `objc2` because this is a bare `NSUInteger` return with no
/// argument marshalling, and the encoding-verification machinery `objc2` layers
/// on `msg_send!` is exactly what a debugging read does not want.
///
/// # Safety
/// `view` must be null or a live Objective-C object pointer.
unsafe fn retain_count(view: *mut std::os::raw::c_void) -> usize {
    unsafe { send(view, c"retainCount") }
}

/// Take a reference on `view`, so it outlives a `close` that releases it.
///
/// # Safety
/// `view` must be null or a live Objective-C object pointer.
unsafe fn retain(view: *mut std::os::raw::c_void) {
    unsafe { send(view, c"retain") };
}

/// Give up a reference taken by [`retain`].
///
/// # Safety
/// `view` must be null or a live object for which the caller holds a reference,
/// and must not be read afterwards.
unsafe fn release(view: *mut std::os::raw::c_void) {
    unsafe { send(view, c"release") };
}

/// Send a nullary Objective-C selector and return the raw word it produced.
///
/// `objc_msgSend` is declared here rather than reached through `objc2`'s
/// `msg_send!` because these are bare nullary sends with no argument
/// marshalling, and the encoding verification `msg_send!` layers on is exactly
/// what a debugging read does not want (that machinery is why the crate needs
/// `relax-void-encoding` elsewhere).
///
/// # Safety
/// `view` must be null or a live Objective-C object pointer that responds to
/// `sel`.
unsafe fn send(view: *mut std::os::raw::c_void, sel: &std::ffi::CStr) -> usize {
    if view.is_null() {
        return 0;
    }
    unsafe extern "C" {
        fn objc_msgSend(receiver: *mut std::os::raw::c_void, sel: *const std::os::raw::c_void)
            -> usize;
        fn sel_registerName(name: *const std::os::raw::c_char) -> *const std::os::raw::c_void;
    }
    unsafe { objc_msgSend(view, sel_registerName(sel.as_ptr())) }
}

gui_test! {
/// `has_editor()` must agree with whether `open()` actually succeeds.
///
/// The two answer the same question by different means: `has_editor` reads
/// `AudioUnitGetPropertyInfo(kAudioUnitProperty_CocoaUI)`, while `open` reads
/// the property, loads the advertised bundle, and asks the factory for a view.
/// A disagreement in either direction is a real host bug with a user-visible
/// symptom:
///
/// - `has_editor` true where `open` fails is what feeds a DAW's "this plugin
///   has a UI" affordance. `tutti-plugin-server` maps this onto the descriptor's
///   editor feature flag, so the host offers an Open Editor button that errors
///   when the user takes it.
/// - `has_editor` false where `open` succeeds hides a working GUI behind the
///   generic parameter view forever.
///
/// The check is a *contrast* across both kinds rather than a single assertion:
/// a `has_editor` hardcoded either way — which is the shape the bug would take
/// — satisfies one group and fails the other. The test refuses to pass unless
/// both groups were actually exercised.
fn has_editor_agrees_with_opening_one() {
    let _g = lock();
    let mut checked = 0;
    let mut with = 0;
    let mut without = 0;
    let mut disagreements = Vec::new();

    let all = WITH_COCOA_VIEW
        .iter()
        .map(|(unit, _, _)| unit)
        .chain(WITHOUT_COCOA_VIEW.iter());

    for unit in all {
        let au = ready(unit);
        let claims = AuEditor::has_editor(au.raw_unit());
        // SAFETY: `au` is live and initialized, and this runs on the main
        // thread under the `harness = false` runner.
        let opened = unsafe { open_headless(&au) };
        match &opened {
            Ok(_) => with += 1,
            Err(_) => without += 1,
        }
        if claims != opened.is_ok() {
            disagreements.push(format!(
                "{}: has_editor()={claims} but open() {}",
                unit.label,
                match &opened {
                    Ok(_) => "succeeded".to_string(),
                    Err(e) => format!("failed with {e:?}"),
                }
            ));
        }
        checked += 1;
    }

    assert!(
        checked >= 2 && with > 0 && without > 0,
        "this test needs at least one AU with a Cocoa view and one without to \
         be meaningful; checked {checked} ({with} with, {without} without)"
    );
    assert!(
        disagreements.is_empty(),
        "has_editor() disagrees with reality:\n  {}",
        disagreements.join("\n  ")
    );
}
}

gui_test! {
/// The documented open → size → close contract, on every view-advertising unit.
///
/// Three promises in `AuEditor`'s docs, each with a distinct failure:
///
/// - `editor_size()` while open is the view's own frame. A host sizes its
///   plugin window from this; a wrong answer clips the GUI or leaves dead
///   space around it. The expected sizes are *measured* per unit rather than
///   merely asserted non-zero, so a host that starts reporting the 800x600 it
///   requests from the factory — instead of the frame the view came back with —
///   is caught.
/// - `close()` is idempotent. A host closes on window-close and again on
///   teardown; a second `release` on an already-released `NSView` is an
///   over-release crash, and the null-out is what prevents it.
/// - `editor_size()` is `{0,0}` after close. Reading the frame of a released
///   view is a use-after-free that usually returns plausible garbage, so a host
///   sizing a window from it would look fine right up until it did not.
fn open_size_close_is_idempotent_and_reports_zero_after() {
    let _g = lock();
    for (unit, want_w, want_h) in WITH_COCOA_VIEW {
        let au = ready(unit);
        // SAFETY: as in `has_editor_agrees_with_opening_one`.
        let mut editor = unsafe { open_headless(&au) }
            .unwrap_or_else(|e| panic!("{}: open failed: {e:?}", unit.label));

        let size = editor.editor_size();
        assert_eq!(
            (size.width, size.height),
            (*want_w, *want_h),
            "{}: editor_size() is {}x{}, but this unit's view measured \
             {want_w}x{want_h} — the host is reporting something other than \
             the view's own frame",
            unit.label,
            size.width,
            size.height
        );

        editor.close();
        let after = editor.editor_size();
        assert_eq!(
            (after.width, after.height),
            (0, 0),
            "{}: editor_size() returned {}x{} after close, but the docs promise \
             zero — a host would size a window from a released view",
            unit.label,
            after.width,
            after.height
        );

        // Reaching the assertion below at all is the idempotence result: a
        // second `release` on the already-released view would have crashed the
        // process here rather than failed a test.
        editor.close();
        editor.close();
        let after_repeat = editor.editor_size();
        assert_eq!(
            (after_repeat.width, after_repeat.height),
            (0, 0),
            "{}: repeated close changed the reported size",
            unit.label
        );
    }
}
}

gui_test! {
/// `view_ptr()` must be non-null exactly while the editor is open.
///
/// This is the pointer a host embeds in its own window hierarchy. Null while
/// open means the host has nothing to parent and the GUI never appears;
/// non-null after close is a dangling `NSView*` that the host would happily
/// add as a subview — a use-after-free at the exact moment a user reopens a
/// plugin window.
fn view_ptr_is_non_null_only_while_open() {
    let _g = lock();
    for (unit, _, _) in WITH_COCOA_VIEW {
        let au = ready(unit);
        // SAFETY: as above.
        let mut editor = unsafe { open_headless(&au) }
            .unwrap_or_else(|e| panic!("{}: open failed: {e:?}", unit.label));

        assert!(
            !editor.view_ptr().is_null(),
            "{}: view_ptr() is null while the editor is open — a host would \
             have nothing to parent the GUI into",
            unit.label
        );
        // The editor must also know which unit it belongs to; a host with
        // several plugins open routes events by this.
        assert_eq!(
            editor.unit(),
            au.raw_unit(),
            "{}: editor reports a different AudioUnit than it was opened for",
            unit.label
        );

        editor.close();
        assert!(
            editor.view_ptr().is_null(),
            "{}: view_ptr() still points at the released NSView after close — \
             a host re-parenting it would use freed memory",
            unit.label
        );
    }
}
}

gui_test! {
/// Repeated open/close cycles must neither crash nor drift.
///
/// Each `open` retains the factory's view and each `close` releases it. An
/// unbalanced pair is invisible on the first cycle and fatal later: one retain
/// too many leaks an `NSView` (and the plugin's whole GUI object graph) every
/// time a user opens a plugin window, and one release too many is a crash on a
/// view AppKit still holds. Cycling drives that imbalance until it shows.
///
/// The size assertion is what makes this more than a smoke test: a fresh view
/// on cycle eight must be the same geometry as on cycle one. Measured stable
/// across 8 cycles on every corpus unit; drift would mean the host is
/// accumulating state between opens.
fn repeated_open_close_cycles_stay_balanced() {
    let _g = lock();
    // Eight cycles: enough that a per-open leak or an over-release shows,
    // while keeping the suite fast — each open loads a real plugin GUI.
    const CYCLES: usize = 8;

    for (unit, want_w, want_h) in WITH_COCOA_VIEW {
        let au = ready(unit);
        for cycle in 0..CYCLES {
            // SAFETY: as above.
            let mut editor = unsafe { open_headless(&au) }.unwrap_or_else(|e| {
                panic!("{}: open failed on cycle {cycle}: {e:?}", unit.label)
            });
            let size = editor.editor_size();
            assert_eq!(
                (size.width, size.height),
                (*want_w, *want_h),
                "{}: cycle {cycle} reported {}x{}, but cycle 0 measured \
                 {want_w}x{want_h} — geometry is drifting across opens",
                unit.label,
                size.width,
                size.height
            );
            assert!(!editor.view_ptr().is_null());

            // Observe the retain count, not only the pointer. Without this the
            // test's own name is a claim it never checks: adding a second
            // `release` to `AuEditor::close` — a textbook over-release, and UB —
            // passed 7/7, because every assertion here was about nullness.
            //
            // `retainCount` is a debugging read whose absolute value is not a
            // contract: AppKit holds references of its own. What is meaningful is
            // that it does not *drift* across identical cycles — a per-open leak
            // walks it up, an over-release walks it down or crashes on the way.
            //
            // Measured over 8 cycles on macOS 15.6: AULowpass 3, AUDelay 5,
            // DLSMusicDevice 58 — each bit-stable across every cycle. AUSampler
            // sits near 1712 and jitters by up to 2, because its view is a
            // heavyweight shared object with AppKit activity of its own that has
            // nothing to do with our retain. So the exact-match check applies
            // only where the count is small enough to be attributable, and
            // `DRIFT_ATTRIBUTABLE_BELOW` is set an order of magnitude above the
            // largest stable count and far below AUSampler's.
            const DRIFT_ATTRIBUTABLE_BELOW: usize = 500;
            // Measure what `close` itself does to the count, sampled either side
            // of it through a pointer saved beforehand. Two details make this the
            // form that works:
            //
            //  * Sampling *before* close only cannot see an over-release at all —
            //    the extra `release` happens inside `close`, and the next cycle
            //    asks the factory for a fresh view, so the damage never lands in a
            //    pre-close sample. That is why the earlier nullness-only version
            //    of this test passed with a deliberate double release.
            //  * The absolute count is not ours to predict: AppKit and the
            //    plugin's own object graph hold references too (measured: AUDelay
            //    5, AULowpass 3, DLSMusicDevice 58, all bit-stable across 8
            //    cycles). So assert the *delta* — `close` must give up exactly the
            //    one reference `open` took.
            //
            // Our own `retain` keeps the object alive to be asked after close,
            // and is subtracted out by comparing deltas rather than absolutes.
            //
            // AUSampler's count sits near 1712 and jitters by up to 2 from AppKit
            // activity unrelated to our retain, so units above
            // `DRIFT_ATTRIBUTABLE_BELOW` are exempted: there the noise exceeds
            // the signal, and a false failure would be worse than no check.
            let view = editor.view_ptr();
            // SAFETY: `view` is the live view `open_headless` just returned. The
            // retain is balanced by the `release` below.
            unsafe { retain(view) };
            let before = unsafe { retain_count(view) };
            editor.close();
            let after = unsafe { retain_count(view) };
            if before < DRIFT_ATTRIBUTABLE_BELOW {
                assert_eq!(
                    after + 1,
                    before,
                    "{}: cycle {cycle} — the view's retain count went {before} → \
                     {after} across close, but close must give up exactly the one \
                     reference open took. {}",
                    unit.label,
                    if after + 1 < before {
                        "It released too many times (over-release: UB on a view \
                         AppKit still holds)."
                    } else {
                        "It released too few (every plugin window leaks its whole \
                         GUI object graph)."
                    }
                );
            }
            // SAFETY: gives up the retain taken above; `view` is not read after.
            unsafe { release(view) };
            continue;
        }
    }
}
}

gui_test! {
/// An AU with no Cocoa view must be refused with an error, not a crash or a
/// null view handed back as success.
///
/// This is the earliest rejection point in `create_view`:
/// `AudioUnitGetPropertyInfo(kAudioUnitProperty_CocoaUI)` fails outright, so
/// the host never reaches the bundle load or the factory. If it leaked through
/// as `Ok` with a null `view`, the host would hand a null `NSView*` to
/// `addSubview:` and take down the process — and `Drop` would then `release` a
/// null pointer.
///
/// The exact status is asserted, not just "some error": the host reports the
/// AU's own `kAudioUnitErr_InvalidProperty` rather than inventing one, so the
/// caller sees what AudioToolbox would have produced. Measured -10879 on both
/// units.
fn an_au_without_a_cocoa_view_is_refused_cleanly() {
    let _g = lock();
    for unit in WITHOUT_COCOA_VIEW {
        let au = ready(unit);
        assert!(
            !AuEditor::has_editor(au.raw_unit()),
            "{}: this unit is in the corpus precisely because it advertises no \
             Cocoa view; if that changed the corpus needs updating, because the \
             no-editor path is now untested",
            unit.label
        );

        // SAFETY: as above.
        let err = unsafe { open_headless(&au) }
            .err()
            .unwrap_or_else(|| panic!("{}: open must fail with no Cocoa view", unit.label));
        assert!(
            matches!(
                err,
                AuError::OsStatus {
                    code: K_AUDIO_UNIT_ERR_INVALID_PROPERTY,
                    ..
                }
            ),
            "{}: expected kAudioUnitErr_InvalidProperty \
             ({K_AUDIO_UNIT_ERR_INVALID_PROPERTY}), got {err:?}",
            unit.label
        );
    }
}
}

gui_test! {
/// Dropping an open editor without calling `close()` must be safe.
///
/// `Drop` routes through `close`, which is the single teardown path. A host
/// that lets an editor fall out of scope — an error path, a `?` in the middle
/// of window setup, or simply closing a project — relies on that. If `Drop`
/// skipped the release the plugin's whole GUI object graph would leak; if it
/// released twice (once in `close`, once in `Drop`) the second would be an
/// over-release crash, which is why `close` nulls the pointer.
///
/// Both orders are exercised: dropped while open, and dropped after an
/// explicit close. Reaching the end of the loop is the result — an
/// over-release aborts the process rather than failing an assertion.
fn dropping_without_closing_is_safe() {
    let _g = lock();
    for (unit, _, _) in WITH_COCOA_VIEW {
        let au = ready(unit);

        // Dropped while open: `Drop` must do the release.
        {
            // SAFETY: as above.
            let editor = unsafe { open_headless(&au) }
                .unwrap_or_else(|e| panic!("{}: open failed: {e:?}", unit.label));
            assert!(!editor.view_ptr().is_null());
        }

        // Dropped after an explicit close: `Drop` must not release again.
        {
            // SAFETY: as above.
            let mut editor = unsafe { open_headless(&au) }
                .unwrap_or_else(|e| panic!("{}: open failed: {e:?}", unit.label));
            editor.close();
            assert!(editor.view_ptr().is_null());
        }

        // And the unit is still usable afterwards — the editor teardown must
        // not have disturbed the AU itself.
        assert!(
            au.is_initialized(),
            "{}: the AU was left uninitialized by editor teardown",
            unit.label
        );
    }
}
}

gui_test! {
/// Two editors open on the same AU at once must both be valid and independent.
///
/// A host can legitimately have this: a plugin window and a detached "always on
/// top" copy, or simply a reopen racing a close animation. Each `open` calls
/// the factory afresh and retains what it gets, so the two must be separate
/// views — if the host cached one view and handed it out twice, closing either
/// would release the other's pointer out from under it.
fn two_editors_on_one_unit_are_independent() {
    let _g = lock();
    let au = ready(&DELAY);

    // SAFETY: as above.
    let mut first = unsafe { open_headless(&au) }.expect("AUDelay opens an editor");
    // SAFETY: as above.
    let mut second = unsafe { open_headless(&au) }.expect("AUDelay opens a second editor");

    assert!(!first.view_ptr().is_null());
    assert!(!second.view_ptr().is_null());
    assert_ne!(
        first.view_ptr(),
        second.view_ptr(),
        "both editors point at the same NSView, so closing one would release \
         the other's view out from under it"
    );

    // Closing one must leave the other entirely untouched.
    let second_size = second.editor_size();
    first.close();
    assert!(first.view_ptr().is_null());
    assert!(
        !second.view_ptr().is_null(),
        "closing the first editor released the second's view"
    );
    assert_eq!(
        second.editor_size(),
        second_size,
        "closing the first editor changed the second's reported geometry"
    );
    second.close();
}
}
