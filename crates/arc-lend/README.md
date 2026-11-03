# arc-lend

Sharing an [`Arc`] with threads that must never free.

The [`ArcLend`] type holds an `Arc` that one thread can replace while others
read it. In that respect it is similar to [`ArcSwap`], or to
`RwLock<Arc<T>>` without the lock. The difference is in who lets go of the
old value: with `ArcLend` it is always the writer, never a reader. It is
meant for threads that run against a deadline, such as an audio callback, a
game's fixed tick or a control loop.

## Why it exists

Picture an audio callback. It has about a millisecond to fill each buffer,
and while it runs, the user keeps turning knobs, so the filter settings it
reads are replaced every so often. The settings are too big for an atomic,
so they live behind an `Arc` that the writer swaps out.

That works until the moment a swap lands while the callback is still
reading the old settings. Then the callback holds the last reference, and
when it lets go, the old settings are freed right there, inside the
millisecond.

We wanted reads that are **wait-free**, **allocation-free** and
**free-free** (a reader never runs a destructor), whatever the timing, and a
writer that **never waits on a reader**.

## The idea: lend, don't share

With an `Arc`, every reader is a co-owner, and whichever owner is last
cleans up. `ArcLend` keeps a single owner, the cell itself, and *lends* the
value to readers instead. A reader marks what it has borrowed while it reads,
and clears the mark when it is done. Before freeing an old value, the writer
checks the marks. Anything still on loan is kept and freed on a later pass.

So a read is wait-free, allocation-free and free-free, and all the cleanup
happens on the writer's thread. The price is paid on the writer's side too: each reading thread
registers once, ahead of time, and the writer's work grows with the number
of registered readers.

## Using it

1. Wrap the value in an `ArcLend`, and share the cell through an `Arc`.
2. Before each reading thread starts, register a [`Reader`] for it. This
   is the one step that may allocate, so it belongs off the deadline.
3. On the reading thread, call [`lend`] once per iteration. The [`Loan`]
   it returns derefs to the value, and dropping it never frees.
4. On the writing thread, [`publish`] each new version. Old versions are
   freed there, once no reader still has them.

```rust
use std::sync::Arc;
use arc_lend::ArcLend;

struct Settings { gains: Vec<f32> }

// 1. The cell, shared through an `Arc`.
let cell = Arc::new(ArcLend::new(Settings { gains: vec![1.0; 4] }));

// 2. Register the reader on the writer's side, then move it to its thread.
let mut reader = cell.reader();
let reader_thread = std::thread::spawn(move || {
    for _iteration in 0..1000 {
        // 3. One lend per iteration: wait-free, allocation-free, free-free.
        let settings = reader.lend();
        let total: f32 = settings.gains.iter().sum();
    }
});

// 4. The writer publishes new versions, and frees old ones here.
for n in 1..100 {
    cell.publish(Settings { gains: vec![0.5; 4 + n % 3] });
}
reader_thread.join().unwrap();
```

The writer can also read the value back with [`current`], which is handy
for deriving the next version from the last.

## Is it the right fit?

Probably not, if none of your threads has a deadline. [`ArcSwap`] is then
the more general tool: any thread can read without registering first, a
reader can keep what it reads as its own `Arc`, and it offers more ways to
update.
`ArcLend` gives those up so that its reads can be free-free, and earns its
keep when a stray free on the reading thread would actually hurt.

It also asks a little of you: a loan should last one iteration, not be kept
around, and a reader paused at an unlucky moment can make the writer hold on
to old versions for longer. The [guide] goes through these.

## Further reading

- [Guide][guide]: how to use it well, and what to watch for.
- [Benchmarks][benchmarks]: what it costs, measured against the
  alternatives.
- [Design notes][design]: how it works, and why we believe it is safe.

## License

Licensed under either of the Apache License, Version 2.0 or the MIT
license, at your option.

[`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
[`RwLock`]: https://doc.rust-lang.org/std/sync/struct.RwLock.html
[`ArcSwap`]: https://docs.rs/arc-swap
[`ArcLend`]: https://docs.rs/arc-lend/latest/arc_lend/struct.ArcLend.html
[`Reader`]: https://docs.rs/arc-lend/latest/arc_lend/struct.Reader.html
[`Loan`]: https://docs.rs/arc-lend/latest/arc_lend/struct.Loan.html
[`publish`]: https://docs.rs/arc-lend/latest/arc_lend/struct.ArcLend.html#method.publish
[`current`]: https://docs.rs/arc-lend/latest/arc_lend/struct.ArcLend.html#method.current
[`lend`]: https://docs.rs/arc-lend/latest/arc_lend/struct.Reader.html#method.lend
[guide]: https://docs.rs/arc-lend/latest/arc_lend/docs/guide/index.html
[benchmarks]: https://docs.rs/crate/arc-lend/latest/source/BENCHMARKS.md
[design]: https://docs.rs/arc-lend/latest/arc_lend/docs/design/index.html
