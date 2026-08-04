//! Host-conformance harness for **`clap_version` compatibility** (C-10) — the
//! two version claims a CLAP binary makes, and the host's obligation to read
//! them before it reads anything else.
//!
//! `clap_plugin_entry.clap_version` (`entry.h:61`) and
//! `clap_plugin_descriptor.clap_version` (`plugin.h:13`) are both initialized
//! to `CLAP_VERSION` by the plugin. `version.h:38-40` supplies
//! `clap_version_is_compatible()` for exactly this check and documents why the
//! `0.X.Y` line is excluded: "API and ABI are not stable" there.
//!
//! What an unchecked load costs is not a missing feature but a **struct-layout
//! misread**. `descriptor_to_info` reads seven `*const c_char` at 1.x offsets;
//! against a 0.x or 2.x layout those offsets name different fields, and the
//! host dereferences whatever is there. The failure is a crash or garbage
//! metadata in a plugin scan, not an error message.
//!
//! The probe's two declared versions are fields of `static`s the host reads
//! directly — by `dlsym` for the entry, off the factory's returned pointer for
//! the descriptor — so unlike every other switch in this fixture they are set
//! by rewriting the loaded image. That makes them process-global in the
//! strongest sense: every test here holds [`VERSION_LOCK`] and restores the
//! defaults before releasing it.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{CLAP_VERSION_TARGET_DESCRIPTOR, CLAP_VERSION_TARGET_ENTRY};

/// Serializes whole scenarios: override → load → restore. An override left
/// standing would make every other suite in this binary load a plugin the host
/// refuses.
static VERSION_LOCK: Mutex<()> = Mutex::new(());

/// A held [`VERSION_LOCK`] whose `Drop` restores both declared versions.
///
/// Restoring on drop rather than at the end of each test is what makes a
/// failing assertion safe: a panic still unwinds through this.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = VERSION_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_clap_version();
        Probe { _lock: lock }
    }

    /// Rewrite one of the probe's declared versions, for the next load.
    fn declare(&self, target: u32, major: u32, minor: u32, revision: u32) {
        set_clap_version(target, major, minor, revision);
    }

    /// Attempt a load through the real host.
    fn load(&self) -> tutti_clap_host::Result<ClapLoaded> {
        let path = Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
    }

    /// The lighter of the two entry points. Covered separately because it does
    /// not share `load`'s code past `load_descriptor`, and a scan is where an
    /// unreadable plugin is met first.
    fn probe_info(&self) -> tutti_clap_host::Result<tutti_clap_host::PluginInfo> {
        let path = Path::new(probe_path());
        ClapLoaded::probe(path, Some(path))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        reset_clap_version();
    }
}

// --- the exported C symbols, reached across the dlopen seam -----------------
//
// Opening the same path a second time shares the already-loaded image, so a
// write here lands in exactly the `static` the host's load will read.

/// The probe image, opened once and kept mapped for the life of the test
/// binary. Held open deliberately: dropping it `dlclose`s the image, and the
/// host's next load would map a fresh copy with the declared versions back at
/// their initializers — silently undoing the override under test.
fn probe_lib() -> &'static libloading::Library {
    static LIB: std::sync::OnceLock<libloading::Library> = std::sync::OnceLock::new();
    LIB.get_or_init(|| unsafe {
        libloading::Library::new(probe_path()).expect("re-open reference plugin")
    })
}

fn set_clap_version(target: u32, major: u32, minor: u32, revision: u32) {
    type F = unsafe extern "C" fn(u32, u32, u32, u32);
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_set_clap_version\0")
            .expect("set-clap-version symbol present");
        f(target, major, minor, revision);
    }
}

fn reset_clap_version() {
    type F = unsafe extern "C" fn();
    unsafe {
        let f: libloading::Symbol<F> = probe_lib()
            .get(b"tutti_test_plugin_reset_clap_version\0")
            .expect("reset-clap-version symbol present");
        f();
    }
}

/// Assert `result` is the rejection this fix produces, and that its message
/// names the version that was refused.
#[track_caller]
fn assert_rejected<T>(result: tutti_clap_host::Result<T>, declared: &str, what: &str) {
    let Err(err) = result else {
        panic!(
            "the host accepted a plugin declaring CLAP {declared} at its {what}. \
             It then reads that binary's structs at 1.x offsets, which is a \
             memory misread rather than a missing feature"
        );
    };
    let msg = format!("{err}");
    assert!(
        msg.contains(declared),
        "the error must name the version it refused, so a scan log says which \
         binary to rebuild — got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// The baseline: nothing here is satisfied by a host that refuses everything.
// ---------------------------------------------------------------------------

/// The probe declares `CLAP_VERSION` at both sites, and must load. Without
/// this, every rejection below would pass against a `load` that returned `Err`
/// unconditionally.
#[test]
fn a_current_version_plugin_still_loads() {
    let probe = Probe::acquire();

    probe.load().expect(
        "the unmodified probe declares CLAP_VERSION at both sites and must \
         still load — the version check is a floor, not a filter",
    );
    probe.probe_info().expect("and probe() agrees");
}

// ---------------------------------------------------------------------------
// clap_plugin_entry.clap_version
// ---------------------------------------------------------------------------

/// **C-10, entry.** A `0.x` entry is the case `clap_version_is_compatible`
/// exists for: `version.h`'s own comment says the development-stage versions
/// "aren't compatible", because API *and ABI* were unstable there.
///
/// The check must precede `init()`. A 0.x `clap_plugin_entry` gives no promise
/// that its `init` pointer sits where the 1.2 binding expects one, so calling
/// through it to find out is the misread this rejects.
#[test]
fn a_development_stage_entry_is_refused() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_ENTRY, 0, 9, 3);

    assert_rejected(probe.load(), "0.9.3", "clap_entry");
    assert_rejected(probe.probe_info(), "0.9.3", "clap_entry");
}

/// **C-10, entry, future major.** `clap_version_is_compatible` is written from
/// the plugin's side — it answers "is this host new enough?", so it has only a
/// floor. A host asks the mirror question and needs a ceiling too: a major bump
/// is the announcement that the layout changed, so a 2.0 binary is exactly as
/// unreadable here as a 0.9 one, in the opposite direction.
#[test]
fn a_future_major_entry_is_refused() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_ENTRY, 2, 0, 0);

    assert_rejected(probe.load(), "2.0.0", "clap_entry");
}

// ---------------------------------------------------------------------------
// clap_plugin_descriptor.clap_version
// ---------------------------------------------------------------------------

/// **C-10, descriptor.** A separate claim from the entry's, and separately
/// checked: `plugin.h:13` gives each descriptor its own `clap_version`, and a
/// factory may hand out several. A host that checks only the entry accepts a
/// descriptor it cannot read — which is the struct `descriptor_to_info`
/// immediately walks, dereferencing seven `*const c_char` at 1.x offsets.
///
/// Overriding *only* the descriptor is what makes this test distinguish the two
/// checks: the entry still declares `CLAP_VERSION`, so a host with one check in
/// the wrong place passes the entry gate and then reads this struct anyway.
#[test]
fn a_development_stage_descriptor_is_refused() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_DESCRIPTOR, 0, 4, 1);

    assert_rejected(probe.load(), "0.4.1", "plugin descriptor");
    assert_rejected(probe.probe_info(), "0.4.1", "plugin descriptor");
}

/// See [`a_future_major_entry_is_refused`] — the same ceiling, at the other
/// site.
#[test]
fn a_future_major_descriptor_is_refused() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_DESCRIPTOR, 3, 1, 0);

    assert_rejected(probe.load(), "3.1.0", "plugin descriptor");
}

// ---------------------------------------------------------------------------
// What must NOT be rejected
// ---------------------------------------------------------------------------

/// Minor and revision carry no gate. CLAP adds within a major by appending
/// fields and extension ids, so a plugin built against a *later* 1.x than this
/// host is readable: the host sees a prefix it understands, and every extension
/// it does not know about is a `get_extension` that returns null.
///
/// Pinning this is the point — a check written as "must equal our version", or
/// as `version >= CLAP_VERSION` on the whole triple, would reject most of the
/// shipping CLAP corpus, and would do it as a clean error that looked correct.
#[test]
fn a_later_minor_within_major_one_is_accepted() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_ENTRY, 1, 99, 7);
    probe.declare(CLAP_VERSION_TARGET_DESCRIPTOR, 1, 99, 7);

    probe.load().expect(
        "1.99.7 is ABI-compatible with a 1.x host: CLAP adds within a major by \
         appending, so the host reads a prefix it understands",
    );
}

/// And an *earlier* 1.x, which is most of what a real scan meets. A `1.0.0`
/// plugin predates several extensions this host queries; each of those is a
/// `get_extension` returning null, which the host already handles everywhere.
#[test]
fn an_earlier_minor_within_major_one_is_accepted() {
    let probe = Probe::acquire();
    probe.declare(CLAP_VERSION_TARGET_ENTRY, 1, 0, 0);
    probe.declare(CLAP_VERSION_TARGET_DESCRIPTOR, 1, 0, 0);

    probe
        .load()
        .expect("1.0.0 is the ABI floor of the stable line, not an incompatibility");
}
