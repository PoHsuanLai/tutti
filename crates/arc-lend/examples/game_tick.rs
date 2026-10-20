//! A fixed-rate simulation tick that reads the level it simulates through an
//! `ArcLend`, while a loader thread swaps levels in.
//!
//! The tick must not stall on a lock, and must not pay for freeing a level it
//! has just finished with: a large level's `Vec`s can take long enough to
//! free that a tick would miss its slot. So the tick only *borrows* the level,
//! once per tick, and the loader (which built the level) is the one that frees
//! the old one.
//!
//! ```text
//! cargo run -p arc-lend --example game_tick
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arc_lend::ArcLend;

/// What the tick reads: built by the loader, never mutated after publishing.
struct Level {
    name: String,
    /// Solid tiles, one row per `Vec`: the kind of nested allocation that is
    /// slow to free.
    solid: Vec<Vec<bool>>,
    gravity: f32,
}

impl Level {
    fn generate(n: usize) -> Self {
        let size = 64 + 32 * n;
        Self {
            name: format!("level {n}"),
            solid: (0..size)
                .map(|y| {
                    (0..size)
                        .map(|x| (x * 7 + y * 13 + n).is_multiple_of(11))
                        .collect()
                })
                .collect(),
            gravity: 9.8 + n as f32,
        }
    }
}

#[derive(Default)]
struct Body {
    x: usize,
    y: f32,
    vy: f32,
}

/// One step of the simulation. Takes `&Level`: the loan stays in `tick`'s
/// loop, and nothing below it can keep the level.
fn step(level: &Level, body: &mut Body, dt: f32) {
    body.vy -= level.gravity * dt;
    body.y = (body.y + body.vy * dt).max(0.0);
    let row = (body.y as usize).min(level.solid.len() - 1);
    body.x = (body.x + 1) % level.solid[row].len();
    if level.solid[row][body.x] {
        body.vy = 5.0; // bounce off a solid tile
    }
}

fn main() {
    let level = Arc::new(ArcLend::new(Level::generate(0)));
    let running = Arc::new(AtomicBool::new(true));

    // The tick thread: the one with a deadline.
    let tick = {
        // Registered here, on the loader's side; the tick only lends.
        let mut reader = level.reader();
        let running = Arc::clone(&running);
        thread::spawn(move || {
            let period = Duration::from_millis(2);
            let mut body = Body::default();
            let (mut ticks, mut last_name) = (0u32, String::new());
            let mut next = Instant::now();
            while running.load(Ordering::Relaxed) {
                {
                    // One lend per tick, at the top. Dropped at the end of
                    // this block: a store, never a free.
                    let level = reader.lend();
                    step(&level, &mut body, period.as_secs_f32());
                    if level.name != last_name {
                        // Allocating here is for the demo's output only.
                        last_name.clone_from(&level.name);
                        println!("tick {ticks}: now simulating {last_name}");
                    }
                }
                ticks += 1;
                next += period;
                thread::sleep(next.saturating_duration_since(Instant::now()));
            }
            ticks
        })
    };

    // The loader: builds each level off the tick thread, publishes it, and
    // pays for freeing the one it replaced.
    for n in 1..=5 {
        thread::sleep(Duration::from_millis(30));
        let next = Arc::new(Level::generate(n));
        level.publish(next);
        // The loader's own health check: a loan parked across ticks would
        // show up here as values that never get freed.
        println!(
            "loader: published level {n}; {} retired value(s) still held",
            level.retired_len()
        );
    }

    running.store(false, Ordering::Relaxed);
    let ticks = tick.join().unwrap();
    // The tick thread has exited and dropped its reader: nothing is pinned.
    assert_eq!(level.reclaim(), 0);
    println!("{ticks} ticks; the tick thread only ever borrowed a level");
}
