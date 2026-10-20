//! The instrument behind `arc-lend`'s comparison benchmark
//! (`crates/arc-lend/BENCHMARKS.md`; the harness is
//! `benches/compare/main.rs`).
//!
//! Nothing here names a contender. It holds what the harness measures with:
//!
//! - [`CountingAlloc`], a global allocator that counts allocations and frees
//!   made by the *current thread*, but only while that thread has called
//!   [`begin_counting`]. The harness calls it on reader threads, after their
//!   setup, so a count is what the reader's own loop did.
//! - [`Payload`], the published value: a small heap array to sum, with a
//!   process-wide count of live values ([`live_payloads`]) and a per-thread
//!   count of payloads dropped while counting ([`Counts::payload_drops`]).
//! - [`Tail`], percentiles read from an HDR histogram ([`new_histogram`]).
//! - [`machine`], the hardware and toolchain the numbers came from.
//!
//! `tests/instruments.rs` checks the instrument itself, including that it sees
//! a free on a reader thread when one happens.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};

pub use hdrhistogram::Histogram;

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

// `const` thread-locals of `Cell`s of plain integers: no destructor, so no
// TLS-destructor registration, and no lazy initialization. That matters
// because the allocator reads them: a thread-local whose first access
// allocated would recurse into the allocator.
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static FREES: Cell<u64> = const { Cell::new(0) };
    static PAYLOAD_DROPS: Cell<u64> = const { Cell::new(0) };
}

/// What one thread did between [`begin_counting`] and [`end_counting`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Calls to `alloc`, `alloc_zeroed` and `realloc` on this thread.
    pub allocs: u64,
    /// Calls to `dealloc` and `realloc` on this thread (a `realloc` is counted
    /// as both: it may move, which frees the old block).
    pub frees: u64,
    /// [`Payload`]s whose destructor ran on this thread: a published value
    /// freed on the reader.
    pub payload_drops: u64,
}

impl Counts {
    /// Field-wise sum.
    #[must_use]
    pub fn plus(self, other: Counts) -> Counts {
        Counts {
            allocs: self.allocs + other.allocs,
            frees: self.frees + other.frees,
            payload_drops: self.payload_drops + other.payload_drops,
        }
    }
}

/// Start counting this thread's allocations, frees and payload drops, from
/// zero.
pub fn begin_counting() {
    // Mutation (recorded in `tests/instruments.rs`): delete this line, so no
    // thread is ever counted. Every reader row then reads 0 allocations and 0
    // frees, including arc-swap's naive store, and
    // `a_free_on_a_counting_thread_is_counted`,
    // `a_payload_counts_itself_and_its_drop` and
    // `a_promoted_arc_swap_guard_frees_on_the_reader` fail.
    COUNTING.with(|c| c.set(true));
    ALLOCS.with(|c| c.set(0));
    FREES.with(|c| c.set(0));
    PAYLOAD_DROPS.with(|c| c.set(0));
}

/// Stop counting on this thread and return what was counted since
/// [`begin_counting`].
pub fn end_counting() -> Counts {
    COUNTING.with(|c| c.set(false));
    Counts {
        allocs: ALLOCS.with(Cell::get),
        frees: FREES.with(Cell::get),
        payload_drops: PAYLOAD_DROPS.with(Cell::get),
    }
}

/// Whether this thread is counting. `try_with`: during thread teardown the
/// thread-locals may be gone, and a destructor that frees then is not counted.
#[inline]
fn counting() -> bool {
    COUNTING.try_with(Cell::get).unwrap_or(false)
}

#[inline]
fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    let _ = counter.try_with(|c| c.set(c.get() + 1));
}

/// The system allocator, counting calls made on threads that are
/// [counting](begin_counting).
///
/// It counts what goes through Rust's global allocator. Allocations a C
/// library makes for itself (glibc's bookkeeping for a thread-local
/// destructor, for example) do not pass through it and are not seen.
pub struct CountingAlloc;

// SAFETY: every method forwards to `System` with the same arguments, and the
// counting touches only this thread's `Cell`s, which never allocate.
unsafe impl GlobalAlloc for CountingAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if counting() {
            bump(&ALLOCS);
        }
        // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if counting() {
            bump(&ALLOCS);
        }
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if counting() {
            bump(&FREES);
        }
        // SAFETY: as in `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if counting() {
            bump(&ALLOCS);
            bump(&FREES);
        }
        // SAFETY: as in `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

// ---------------------------------------------------------------------------
// The published value
// ---------------------------------------------------------------------------

static LIVE: AtomicI64 = AtomicI64::new(0);

/// How many elements a [`Payload`] holds: a "small array" a reader sums per
/// block, on the heap so that freeing one is a real `free`.
pub const PAYLOAD_LEN: usize = 16;

/// The value every contender publishes.
///
/// Each one counts itself into [`live_payloads`] while it exists, so the
/// retained-memory runs can read how many published values are alive without
/// asking the contender. Its destructor notes whether it ran on a counting
/// thread ([`Counts::payload_drops`]).
#[derive(Debug)]
pub struct Payload {
    data: Vec<u64>,
}

impl Payload {
    /// A payload whose elements start at `seed`. Allocates.
    #[must_use]
    pub fn new(seed: u64) -> Payload {
        LIVE.fetch_add(1, Ordering::Relaxed);
        Payload {
            data: (0..PAYLOAD_LEN as u64)
                .map(|i| seed.wrapping_add(i))
                .collect(),
        }
    }

    /// The reader's work for one block: sum the array.
    #[inline]
    #[must_use]
    pub fn touch(&self) -> u64 {
        self.data.iter().fold(0u64, |a, &b| a.wrapping_add(b))
    }
}

impl Clone for Payload {
    fn clone(&self) -> Payload {
        LIVE.fetch_add(1, Ordering::Relaxed);
        Payload {
            data: self.data.clone(),
        }
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        if counting() {
            bump(&PAYLOAD_DROPS);
        }
    }
}

/// How many [`Payload`]s exist right now, process-wide.
#[must_use]
pub fn live_payloads() -> i64 {
    LIVE.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Percentiles
// ---------------------------------------------------------------------------

/// A histogram of nanoseconds from 1 ns to 100 s at three significant digits.
///
/// Made before a thread starts measuring: recording into it never allocates,
/// and a value past the top is clamped (`saturating_record`), never resized.
#[must_use]
pub fn new_histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 100_000_000_000, 3).expect("valid histogram bounds")
}

/// The tail of a latency distribution, in nanoseconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tail {
    pub count: u64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub p9999: u64,
    pub max: u64,
}

impl Tail {
    /// Reads the percentiles out of `h`.
    #[must_use]
    pub fn of(h: &Histogram<u64>) -> Tail {
        if h.is_empty() {
            return Tail::default();
        }
        Tail {
            count: h.len(),
            p50: h.value_at_quantile(0.5),
            p99: h.value_at_quantile(0.99),
            p999: h.value_at_quantile(0.999),
            p9999: h.value_at_quantile(0.9999),
            max: h.max(),
        }
    }

    /// `p50 | p99 | p99.9 | p99.99 | max`, as table cells.
    #[must_use]
    pub fn cells(&self) -> String {
        if self.count == 0 {
            return "– | – | – | – | –".into();
        }
        format!(
            "{} | {} | {} | {} | {}",
            ns(self.p50),
            ns(self.p99),
            ns(self.p999),
            ns(self.p9999),
            ns(self.max)
        )
    }
}

/// A duration in nanoseconds, in the unit that keeps it short.
#[must_use]
pub fn ns(v: u64) -> String {
    if v < 10_000 {
        format!("{v} ns")
    } else if v < 10_000_000 {
        format!("{:.1} µs", v as f64 / 1e3)
    } else {
        format!("{:.1} ms", v as f64 / 1e6)
    }
}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// The hardware and software the numbers came from, as `(what, value)` rows.
#[must_use]
pub fn machine() -> Vec<(&'static str, String)> {
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    let mut rows = vec![
        ("cpu", cpu_model()),
        ("logical cores", cores.to_string()),
        ("virtual machine", virtualization()),
        (
            "os",
            format!(
                "{} ({} {})",
                os_name(),
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        ),
        ("kernel", kernel()),
        ("rustc", env!("ARC_LEND_COMPARE_RUSTC").to_owned()),
        ("profile", profile().to_owned()),
    ];
    if let Some(clock) =
        read_trimmed("/sys/devices/system/clocksource/clocksource0/current_clocksource")
    {
        rows.push(("clocksource", clock));
    }
    rows
}

fn profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug (numbers are not meaningful)"
    } else {
        "release"
    }
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn cpuinfo_field(name: &str) -> Option<String> {
    let info = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    info.lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().to_owned())
}

fn cpu_model() -> String {
    cpuinfo_field("model name")
        .or_else(|| cpuinfo_field("Model"))
        .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn virtualization() -> String {
    let hypervisor_flag =
        cpuinfo_field("flags").is_some_and(|f| f.split(' ').any(|x| x == "hypervisor"));
    let vendor = read_trimmed("/sys/class/dmi/id/sys_vendor");
    let product = read_trimmed("/sys/class/dmi/id/product_name");
    let dmi = match (vendor, product) {
        (Some(v), Some(p)) => format!(" (DMI: {v} {p})"),
        (Some(v), None) | (None, Some(v)) => format!(" (DMI: {v})"),
        (None, None) => String::new(),
    };
    if hypervisor_flag {
        format!("yes: the CPU reports the `hypervisor` flag{dmi}")
    } else if cfg!(target_os = "linux") {
        format!("no `hypervisor` flag{dmi}")
    } else {
        "unknown (only detected on Linux)".into()
    }
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_owned())
}

fn kernel() -> String {
    read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into())
}
