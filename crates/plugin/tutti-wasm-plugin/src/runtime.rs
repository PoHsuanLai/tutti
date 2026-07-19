//! Process-wide wasmtime [`Engine`] for in-process audio plugins.
//!
//! One Engine is shared by every loaded WASM audio plugin in the
//! process. Configuration choices target real-time safety:
//!
//! - **Pooling allocator.** Instance memory + tables are pre-reserved
//!   so steady-state instantiation is an `madvise`, not `mmap` /
//!   `malloc`. This is what makes the audio path's per-block
//!   `call_process` allocation-bounded (modulo the host-side lift of
//!   `list<list<f32>>` returns which is a separate v0.2 concern).
//! - **Epoch interruption.** A single watchdog thread bumps the engine
//!   epoch at a fixed cadence. Each `call_process` is bracketed by a
//!   `set_epoch_deadline` so a runaway guest traps within bounded time
//!   rather than wedging the audio thread.
//! - **Component Model on**, async support off, fuel off.
//!
//! The Engine is also where bindgen-generated host imports would
//! attach, but the audio-plugin world deliberately has none (mirrors
//! AudioWorkletGlobalScope — see `wit/audio-plugin.wit`).

use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use wasmtime::{Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig};

/// How many WASM audio plugin instances we pre-reserve pool slots for.
/// Hit on the 257th instance load: instantiation will fail with a clear
/// error rather than fall back to on-demand allocation. The number is
/// deliberately generous — sessions with 100+ plugins are common in a
/// DAW; double that for headroom.
const POOL_CAPACITY: u32 = 256;

/// Maximum linear-memory pages we pre-reserve per instance. 64 KiB per
/// page × 4096 = 256 MiB. Most audio plugins use 1-8 MiB; this is
/// upper bound for an unusually large sample-based plugin embedded in
/// the WASM. Set so a guest cannot grow memory mid-block — the
/// `LinearMemory` is pinned at this size.
const MAX_MEMORY_PAGES: u64 = 4096;

/// Maximum table elements per instance. 8 KiB indirect-call table
/// covers any realistic plugin's function-pointer table.
const MAX_TABLE_ELEMENTS: usize = 8192;

/// Epoch tick interval. The audio thread sets a per-process deadline of
/// `EPOCH_DEADLINE_TICKS` before each `call_process`; the watchdog
/// increments the engine epoch every `WATCHDOG_PERIOD`. Worst-case
/// preemption is `(EPOCH_DEADLINE_TICKS + 1) × WATCHDOG_PERIOD`.
const WATCHDOG_PERIOD: Duration = Duration::from_millis(2);

/// Epoch ticks a `call_process` is allowed before being preempted. With
/// a 2 ms watchdog period, this is ~4 ms worst-case — generous enough
/// for healthy block processing at sane buffer sizes (1.33 ms at 64
/// samples / 48 kHz), tight enough to bound runaway loops.
pub(super) const EPOCH_DEADLINE_TICKS: u64 = 2;

static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();

/// Get-or-init the process-wide [`Engine`]. First call also spawns the
/// watchdog thread that bumps the epoch on a schedule.
pub(super) fn engine() -> Result<Arc<Engine>, String> {
    if let Some(engine) = ENGINE.get() {
        return Ok(Arc::clone(engine));
    }
    // Build the engine outside the OnceLock so a failure doesn't poison
    // it. Multiple racing builds are wasted work, not corruption — only
    // one wins set_once.
    let engine = Arc::new(build_engine()?);
    let engine = match ENGINE.set(Arc::clone(&engine)) {
        Ok(()) => engine,
        Err(_) => Arc::clone(ENGINE.get().expect("just set above")),
    };
    spawn_watchdog(Arc::clone(&engine));
    Ok(engine)
}

fn build_engine() -> Result<Engine, String> {
    let mut pool = PoolingAllocationConfig::default();
    pool.total_component_instances(POOL_CAPACITY);
    pool.total_core_instances(POOL_CAPACITY);
    pool.total_memories(POOL_CAPACITY);
    pool.total_tables(POOL_CAPACITY);
    pool.max_memories_per_component(8);
    pool.max_tables_per_component(8);
    pool.max_memory_size((MAX_MEMORY_PAGES as usize) * 64 * 1024);
    pool.table_elements(MAX_TABLE_ELEMENTS);

    let mut config = Config::new();
    config.async_support(false);
    config.wasm_component_model(true);
    config.consume_fuel(false);
    config.epoch_interruption(true);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));

    Engine::new(&config).map_err(|e| format!("wasmtime engine init failed: {e}"))
}

/// Spawn the watchdog thread for the given engine. Idempotent across
/// `engine()` calls because the engine itself is one-shot.
fn spawn_watchdog(engine: Arc<Engine>) {
    // Weak reference so the thread doesn't keep the engine alive past
    // its natural lifetime. Engine drop will cause `upgrade` to fail
    // and the thread exits on the next tick.
    let weak = Arc::downgrade(&engine);
    thread::Builder::new()
        .name("tutti-wasm-watchdog".to_string())
        .spawn(move || loop {
            thread::sleep(WATCHDOG_PERIOD);
            match weak.upgrade() {
                Some(engine) => engine.increment_epoch(),
                None => return,
            }
        })
        .expect("failed to spawn wasmtime epoch watchdog");
}
