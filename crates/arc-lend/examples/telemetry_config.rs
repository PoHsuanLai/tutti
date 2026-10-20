//! A sampling configuration that several worker threads consult on every
//! batch, updated live by a control thread.
//!
//! The workers sit on a hot path (think of a packet or request pipeline) and
//! read the config once per batch of events. The control thread changes it
//! now and then, and polls the cell's observables the way a host would.
//!
//! ```text
//! cargo run -p arc-lend --example telemetry_config
//! ```

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use arc_lend::ArcLend;

/// Which events to keep, per category: too big and too irregular for an
/// atomic, and read far more often than it is written.
struct Sampling {
    version: u32,
    /// Keep one event in `n`, per category. A category not listed is dropped.
    keep_one_in: HashMap<&'static str, u64>,
}

const CATEGORIES: [&str; 3] = ["http", "db", "cache"];
const WORKERS: usize = 4;

fn main() {
    let config = Arc::new(ArcLend::new(Sampling {
        version: 0,
        keep_one_in: HashMap::from([("http", 1), ("db", 10)]),
    }));
    let kept = Arc::new(AtomicU64::new(0));

    let workers: Vec<_> = (0..WORKERS)
        .map(|w| {
            let (config, kept) = (Arc::clone(&config), Arc::clone(&kept));
            thread::spawn(move || {
                let mut seen_version = 0;
                for batch in 0..2_000u64 {
                    // One lend per batch, passed down as `&Sampling`.
                    let sampling = config.lend();
                    seen_version = sampling.version;
                    let mut batch_kept = 0;
                    for event in 0..64u64 {
                        let category = CATEGORIES[((batch + event) as usize + w) % 3];
                        if let Some(&n) = sampling.keep_one_in.get(category) {
                            if (batch * 64 + event).is_multiple_of(n) {
                                batch_kept += 1;
                            }
                        }
                    }
                    kept.fetch_add(batch_kept, Ordering::Relaxed);
                } // the last loan drops here
                seen_version
            })
        })
        .collect();

    // The control thread: publish new configs while the workers run.
    for version in 1..=20 {
        let mut keep_one_in = HashMap::from([("http", 1 + version as u64 % 5)]);
        if version % 2 == 0 {
            keep_one_in.insert("cache", 100);
        }
        config.publish(Arc::new(Sampling {
            version,
            keep_one_in,
        }));
        thread::yield_now();
    }

    let versions: Vec<u32> = workers.into_iter().map(|h| h.join().unwrap()).collect();

    // What a host polls, with its own policy: here, report it. A loan held
    // only within a batch keeps both near zero.
    println!(
        "kept {} events; workers last saw versions {versions:?}",
        kept.load(Ordering::Relaxed)
    );
    println!(
        "retired values still held: {}, publishes that could not advance an epoch: {}",
        config.retired_len(),
        config.epoch_stalls()
    );
    assert_eq!(config.epoch_stalls(), 0);
}
