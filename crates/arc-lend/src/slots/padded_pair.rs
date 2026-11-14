/// One cache line (two on x86-64 and aarch64, whose adjacent-line prefetcher
/// pulls lines in pairs; the same choice as `crossbeam-utils`' `CachePadded`).
/// Each reader's slot and the `current` pointer get one of their own, so a
/// lend writes a line no other reader reads or writes.
#[repr(align(128))]
pub(crate) struct Padded<T>(pub(crate) T);
