How arc-lend keeps every free off its readers, and why we believe the
protocol is sound.

## Abstract

arc-lend is a publish cell built for threads that run against a deadline
and must not free memory while they do. Its readers never own what they
read. Each borrows through a slot of its own. The publisher, which alone
holds the values, frees each one on its own thread once no slot can
refer to it. In this note we describe the protocol, argue that it is
sound under the C++20 memory model that Rust adopts, bound the memory it
can retain, and say how we checked it.

## 1. Motivation

Audio callbacks, simulation ticks and control loops often read state that
is too large for an atomic, and that another thread replaces now and then.
The usual tool in Rust is an `Arc`, swapped atomically. With it, whichever
thread drops the last count frees the old value. When that thread is the
one with the deadline, a free of arbitrary size runs inside the deadline,
at a moment no test chose.

Existing designs avoid this in a few ways, which the README compares: the
writer waits for its readers, drops are routed to a collector thread, or
users follow a discipline that keeps the last count off the reader. We
wanted a cell whose readers *cannot* free, as a property of the types, and
whose writer never waits on a reader.

## 2. Overview

Our approach borrows from hazard pointers. Each reader registers once,
ahead of time, and receives a slot: one word on a cache line of its own.
To read, the reader writes the address of the value it is looking at into
its slot, and clears the slot when it is done. It never touches a
reference count. The cell keeps the only owning handle to every value.
When the publisher swaps a new value in, it keeps the old one in a
retirement list until a scan of the slots shows that no reader can still
hold it.

The delicate part is the gap between a reader loading the pointer and
writing it into its slot. Classic hazard pointers close it on the reader,
with a loop that re-reads the pointer until it is stable. We wanted the
reader to be wait-free, so we close the gap on the publisher's side. Before
it loads, the reader writes a *claim*. A publisher that sees a claim does
not wait for it. It keeps every value the reader might be about to load,
and looks again at the next scan. §4 makes this precise, and §5 argues that
it is enough.

## 3. Definitions

- **`current`**: an atomic pointer to the published value. It was made
  from an `Arc`, so the cell owns one strong count of it.
- **Slot list**: one *slot* per registered reader, each on a cache line of
  its own. The list grows by doubling: a first block of one slot, then
  blocks as large as the list so far. Blocks never move, so a reader can
  keep a pointer to its slot. The list never shrinks.
- **Slot word**: what a slot holds: `FREE`, a *claim* or an
  *announcement* (the table below).
- **`owned`**: a flag beside each slot's word, true while a live reader
  owns the slot.
- **`seq`**: a counter of publishes. A value's *sequence number* is what
  `seq` was while the value was current.
- **Retired list**: the values swapped out and not yet freed, each with its
  sequence number. To *retire* a value is to move it here.
- **`scanned_at`**: what `seq` was at the last scan.
- **Claim record**: for each slot, the claim the last scan saw there (if
  any) and that claim's **floor**: the lowest sequence number of a value
  the claiming reader could still load.
- A retired value is **pinned** while some slot announces it, or some claim
  has a floor at or below its sequence number. A scan frees exactly the
  retired values that are not pinned.

We call the thread that writes the *publisher*; the README and the API docs
call it the writer. Everything but `current` and the slots lives in a
*control block* behind a mutex. The publisher and registration take that
mutex; no reader does.

### 3.1. A slot's life

Each lend takes its reader's slot once around `FREE` → claim →
announcement → `FREE`.

| Word | Written by | Means | A scan that reads it |
|---|---|---|---|
| `FREE` (0) | the loan's drop (`Release`), or registration | no loan | pins nothing |
| claim: `(generation << 1) \| 1`, odd | the reader, starting a lend (`Release`) | mid-lend: what it will load is not known yet | pins every retired value from the claim's floor on |
| announcement: the value's address, even and nonzero | the reader, once it has loaded (`Relaxed`) | holds that value | pins that one value |

Each reader counts its lends in its *generation*, kept on its slot, so two claims by one
reader differ unless the counter has wrapped (§5.1). An `Arc`'s data
pointer is even and nonzero, so an announcement is never mistaken for the
other two.

## 4. The algorithm

Each operation below is written as simplified Rust: the orderings are
the real ones, but the types and helpers are made up for clarity. The
shipped code is in `src/cell.rs`, `src/reader.rs` and `src/slots.rs`.

### 4.1. Registering a reader

```rust,ignore
// Under the control lock.
fn register(&mut self) -> Reader {
    // Search from where the last registration stopped.
    let i = match self.slots.find(|s| !s.owned.load(Acquire)) {
        Some(i) => i,
        // Every slot is owned: double the list (one slot the first time).
        None => self.slots.grow(),
    };
    self.slots[i].owned.store(true, Relaxed);
    // A forgotten loan may have left an announcement behind.
    self.slots[i].word.store(FREE, Relaxed);
    Reader::on(i)
}

fn drop(reader: &mut Reader) {
    reader.slot.owned.store(false, Release);
}
```

Registration runs on the control side, under the lock, and may allocate a
block. Its `Acquire` read of `owned` pairs with the previous owner's
`Release` when it dropped, so that owner's last store to the slot happens
before the new owner's first. Dropping a reader is a single store, so it
is as cheap as a lend.

### 4.2. A lend

```rust,ignore
fn lend(&mut self) -> Loan {
    self.slot.generation += 1; // on the slot, so a lend writes only the slot
    self.slot.word.store(claim(self.slot.generation), Release);
    fence(SeqCst);
    let value = current.load(Acquire);
    self.slot.word.store(announcement(value), Relaxed);
    Loan { slot: &self.slot, value }
}

fn drop(loan: &mut Loan) {
    loan.slot.word.store(FREE, Release);
}
```

The reader side is three stores to a line no other reader touches, one
fence and one load. It takes no lock, touches no reference count, never
loops or retries, and never frees. So a lend is wait-free and does not
allocate. The `Acquire` load pairs with the publisher's `Release` swap, so
the reader sees the value fully built. We made the announcement `Relaxed`
because the publisher only compares the address, and the fence before it
already orders the reader's earlier accesses (§5, case (b)).

### 4.3. A publish

```rust,ignore
// Under the control lock.
fn publish(&mut self, value: Arc<T>) {
    // The only allocations, before anything changes.
    self.retired.reserve(1);
    let mut freeable = take(&mut self.spare);
    let old = self.current.swap(value, Release);
    self.retired.push((old, self.seq));
    self.seq += 1;
    self.scan(&mut freeable);
    // After unlocking: drop `freeable`, and keep its buffer for next time.
}
```

The publish does its only allocations first, so a panic there leaves the
cell untouched. It then swaps, retires the outgoing value and scans
(§4.5). The values the scan collects are dropped after the lock is
released, since a destructor is arbitrary code and may itself publish to
the cell.

The publisher never waits on a reader. It does not spin on a claim: the
claim pins what the reader might load, and the next scan looks again. A
held loan pins only its own value. We tried the alternatives before
settling on this. A publish that waited on a held loan would deadlock a
thread that lends and then publishes. One that waited on a claim, as an
earlier version of this crate did, could be stalled by a reader
descheduled between two of its instructions.

### 4.4. A reclaim

```rust,ignore
// Under the control lock.
fn reclaim(&mut self) {
    if self.retired.is_empty() {
        return; // no fence, no scan
    }
    let mut freeable = take(&mut self.spare);
    self.scan(&mut freeable);
    // After unlocking: as in a publish.
}
```

A reclaim is a publish without the swap. With nothing retired it returns
at once, which lets a host call it every tick for every cell it owns. We
argue that skipping the scan then keeps exactly what scanning would. Every
publish ends with a scan that sets `scanned_at = seq`, so here `scanned_at`
already equals `seq`. A claim that this scan would record for the first
time would get `scanned_at` as its floor, and the next scan, which reads
`scanned_at` before advancing it, gives it the same floor. A claim the
last scan recorded keeps its floor either way. The only other thing a scan
could change is the records of slots that are free or announcing, which it
clears, and the next scan clears them in the same way.

### 4.5. A scan

```rust,ignore
fn scan(&mut self, freeable: &mut Vec<Arc<T>>) {
    fence(SeqCst);
    let mut floor = u64::MAX;
    let mut announced = Set::new();
    for (slot, record) in self.slots.iter().zip(&mut self.records) {
        match slot.word.load(Acquire) {
            Free => *record = None,
            Announced(addr) => {
                *record = None;
                announced.insert(addr);
            }
            Claim(word) => {
                // The same claim the last scan saw keeps the floor it got then.
                let since = match record {
                    Some(seen) if seen.word == word => seen.floor,
                    _ => self.scanned_at,
                };
                *record = Some(Record { word, floor: since });
                floor = floor.min(since);
            }
        }
    }
    self.scanned_at = self.seq;
    freeable.extend(
        self.retired.extract_if(|(value, seq)| *seq < floor && !announced.contains(value)),
    );
}
```

A scan reads every slot once, after the fence, and then filters the
retired list in one order-preserving pass, so values are freed oldest
first. The announced addresses are sorted and each retired value is looked
up by binary search. A scan neither allocates nor drops, so nothing in it
can unwind with a value half-moved.

## 5. Correctness

We argue in C++20's terms, which are Rust's, and name the rule each step
rests on.

**The guarantee.** If a reader's lend loads a value `V`, then everything the
reader does through that loan happens before any free of `V`.

Fix such a lend. Call its claim store `C`, its fence `Fr` and its load of
`current` `R`. Let `S` be a scan, by a publish or a reclaim, that frees
`V`, and call its fence `Fp`. `V` was retired by a swap `X` that happens
before `Fp`: `X` is sequenced before `Fp` in the same publish, or reaches
it through the mutex from an earlier publish. `SeqCst` fences are totally
ordered, so either `Fp` precedes `Fr` or `Fr` precedes `Fp`.

| Reader (one lend) | Publisher (one publish) |
|---|---|
| `C`: store the claim (`Release`) | `X`: swap `current` (`Release`) |
| `Fr`: `fence(SeqCst)` | `Fp`: `fence(SeqCst)` |
| `R`: load `current` (`Acquire`), getting `V` | `S`: read every slot (`Acquire`) |
| announce `V` (`Relaxed`) | free what is not pinned |

Each column is in program order. For a reclaim, `X` is the last publish's
swap, and reaches `Fp` through the mutex. Step 1 rules out `Fp` coming
first, and the cases after it cover `Fr` coming first.

**Step 1.** `Fp` does not precede `Fr`.

*Why:* suppose it does. `X` happens before `Fp`, and `R` is sequenced
after `Fr`, so `R` reads `X` or a later write of `current`
([atomics.order]/4.4: had `R` been coherence-ordered before `X`, then with
`Fr` happening before `R` and `X` happening before `Fp`, `Fr` would
precede `Fp` in the total order). Every write of `current` from `X` on
stores a value other than `V`, so `R` does not load `V`, which contradicts
our choice of lend.

So `Fr` precedes `Fp`. By the same clause, with `C` sequenced before `Fr`,
`S`'s read of the slot sees `C` or a later write to the slot. We take each
possibility in turn.

- **(a) The announcement of `V`.** Then `V` is announced, so `S` does not
  free it.
- **(b) A later write by the same reader.** This is `FREE` from the loan's
  drop or the next lend's claim, both `Release`, or a later announcement,
  which the next lend's fence precedes (a `SeqCst` fence is also a release
  fence). `S`'s `Acquire` read synchronizes with that write, so the
  reader's accesses through its loan of `V` happen before the scan, and so
  before the free. This is why the claim is `Release`: without it, the
  loom model's `claim_relaxed` mutation frees `V` under the previous loan.
- **(c) A write by a later owner of the slot.** The registration that
  handed the slot on read the reader's `Release` drop of `owned` with
  `Acquire`, and ran under the mutex before `S`. The new owner's stores
  come after that registration. Either way, the reader's loan happens
  before the free.
- **(d) `C` itself.** The reader is mid-lend, and `S` keeps every retired
  value whose sequence number is at or above the claim's floor. Step 2
  shows that `V`'s is, so `S` does not free `V`.

A read of `FREE` from before `C` is not among the cases: since `Fr`
precedes `Fp`, `S`'s read is at `C` or later in the slot's modification
order. With Step 2, that covers every case, so the guarantee holds.

**Step 2.** In case (d), the floor `S` gives the claim is at most `V`'s
sequence number.

*Why:* let `S'` be the scan before `S`, with fence `Fp'`.

- *`S'` saw a different word in this slot, or the slot did not exist yet.*
  The floor is `scanned_at`, the sequence number of the value current at
  `S'`. `S'` happens before `S` through the mutex, and `S` read `C`, so by
  read-read coherence `S'` read `C` or an earlier write. It did not read
  `C`, so it read an earlier one. Had `Fr` preceded `Fp'`, `S'` would have
  read `C` or later, so `Fp'` precedes `Fr`. (If the slot was registered
  after `S'`, then `S'` happens before the registration, which happens
  before `C`, with the same result.) Every swap up to `S'` happens before
  `Fp'`, so by the argument of Step 1, `R` loads the value current at
  `S'` or a later one. Its sequence number is at least `scanned_at`.
- *`S'` saw this same claim word.* The reader has not moved since then,
  and the floor is the one `S'` recorded. The same argument, applied one
  scan further back, shows it bounds what `R` can load.

### 5.1. Reused claim words

A claim word can repeat: the generation counter wraps, or a word recorded
for one owner of a slot is matched by the next. The scan then keeps the
older floor. `scanned_at` only grows, so an older floor pins a superset of
what the right one would. Mistaking one claim for another can only make
the scan keep more, never free early.

## 6. What a reader can pin

An announcement pins one value, so loans held across iterations pin at
most one value per registered reader. A claim pins every value retired
from its floor on. For a reader that announces promptly, that is the one
or two values current around its lend.

The case we find least comfortable is a reader preempted between its claim
and its announcement, two instructions apart. It pins everything retired
while it is preempted, and the first scan after it announces frees all of
that except the value it holds. The bound is the number of publishes
during the preemption, not a fixed count. We accepted it as the price of a
publisher that never waits on a reader. The cell reports the backlog
through `retired_len` and the count `reclaim` returns, and leaves the
policy to the host.

## 7. Cost

| Operation | Thread | What it does |
|---|---|---|
| `ArcLend::reader`, `Reader::clone` | control | the mutex; a walk for an unowned slot from where the last registration stopped; when every slot is owned, a new block that doubles the list (an allocation) |
| `Reader::lend` | reader | a `Release` store, a `SeqCst` fence, an `Acquire` load and a `Relaxed` store, all on the reader's own cache line except the load |
| drop a `Loan` | reader | one `Release` store |
| drop a `Reader` | any | one `Release` store, and its `Arc` of the cell |
| `ArcLend::publish` | publisher | the mutex; a swap and a `SeqCst` fence; one `Acquire` load per slot; a sort of the announced addresses; one pass over the retired list; the frees, after the lock. It allocates only when the retired list grows or the reused free buffer is too small |
| `ArcLend::reclaim` | publisher | a publish without the swap, or the mutex alone when nothing is retired |
| `ArcLend::current` | publisher | the mutex and a strong-count increment |
| `ArcLend::retired_len` | publisher | the mutex |

Memory is the pointer and one cache line per slot (128 bytes on x86-64 and
aarch64, 64 elsewhere), kept at the high-water mark of readers rounded up
to a power of two. On the publisher's side there are also the retired list
and one claim record per slot.

What these costs came to in practice, against `arc-swap`, `basedrop`,
`left-right` and the locks, is in
[BENCHMARKS.md](https://docs.rs/crate/arc-lend/latest/source/BENCHMARKS.md).

## 8. Verification

A test that a reader never allocates cannot establish the guarantee. The
hazard is a race between a reader and a publisher, and a single-threaded
test explores none of its schedules. We rely instead on three checks.

- **The types.** A `Loan` holds no owning handle, so dropping one cannot
  run a destructor. The compiler checks this.
- **loom.** Under `--cfg loom`, the cell's atomics, fences and mutex are
  loom's, so `tests/loom.rs` explores the shipped code rather than a model
  of it. Its mutation record lists, for each ordering in §5, the model
  that fails when that ordering is weakened.
- **miri.** Its weak-memory emulation runs the unit tests, including a
  multi-reader stress test, over many scheduling seeds.

We want to be clear about what these leave open. loom's `SeqCst` fences are
stronger than C++'s, and it does not explore load buffering. Its models
run two or three threads and a few publishes each. So we read a passing
model as support for the argument in §5, not a replacement for it, and
miri as a second, independent check.

## 9. Related work

The slots are hazard pointers (Michael, 2004) with the reader's validation
loop removed. The claim's floor is close in spirit to Hazard Eras
(Ramalhete and Correia, 2017), where a reader protects a range of eras
rather than one pointer. Here the reader writes no era: the publisher
infers the range from two consecutive scans. RCU and `left-right` keep the
reader cheap by making the writer wait for readers to leave. We chose to
make the writer keep memory instead.

Below we compare the cells a Rust user would choose between. Each row is
the crate's documented or source-visible behaviour, checked against the
version named; the sources are cited under [Sources](#sources).
"Waits" means the operation can loop on another thread's progress. The
claims about where a value is freed, and whether the writer waits, are
also checked by running each cell, in `compare/tests/claims.rs` and
`compare/tests/instruments.rs` (all but basedrop's spinning `replace`,
which no test can hold a reader inside).

| | Reader holds | Reader frees? | Reader allocates? | Reader waits? | Writer waits on readers? | A reader preempted mid-read pins | Readers |
|---|---|---|---|---|---|---|---|
| **arc-lend** | a borrow | never | never (registering may, on the control side) | no (wait-free) | never | what is retired while it is preempted, so as many values as the writer publishes meanwhile (§6) | one registered `Reader` each |
| `arc-swap` 1.9.3 `load` | a `Guard`, which a store can promote to an owned `Arc` | yes, if its guard was promoted and is the last owner | on a thread's first load, if no free debt node exists (`debt/list.rs:169`) | lock-free; wait-free almost always | no | the value it loaded | any |
| `arc-swap` 1.9.3 `Cache` | an owned `Arc`, kept until its next load | yes, the old cached `Arc` when a load replaces it, if it is the last owner | as `load`, on its first reload | no while unchanged (a pointer compare); as `load` on a change | no | the value it cached, until it next reads, even when idle | one `Cache` each |
| `basedrop` 0.1.3 `SharedCell` | an owned `Shared<T>` | no: the last drop queues it for a `Collector` | no | no | yes: `replace` spins until no reader is mid-`get` | nothing (the writer spins instead) | any |
| Firewheel 0.14 `ArcGc` | an owned `ArcGc<T>` | no: a registry keeps a count until `collect` | no | no | n/a: a pointer type, swapped by other means; registering takes a global mutex | the value it holds | any |
| `left-right` 0.11.8 | a borrow (`ReadGuard`) | no | no (a `ReadHandle` allocates once, when made) | no | yes: `publish` waits for every reader to leave the old copy | nothing (the writer waits instead) | one handle per reader |
| `crossbeam-epoch` 0.9.21 | a borrow, under a pinned `Guard` | yes: every 128th `pin()` collects the expired garbage, whoever retired it | on a thread's first `pin()`, which registers it | no | no | everything retired while it is pinned (the epoch cannot advance past it) | any |
| `haphazard` 0.1.8 | a borrow, protected by its `HazardPointer` | no | no (acquiring a `HazardPointer` may, when no record is free) | lock-free: a load retries until the pointer is stable | no | the value it protects | one `HazardPointer` each |
| `triple_buffer` 9.0.0 | a borrow of its own buffer | no | no | no | no | its own buffer | one |
| `rtrb` 0.4.0 + a return ring | an owned value it popped | only if it does not send it back | no | no | no | the value it popped | one |
| farbot `NonRealtimeMutatable` | a borrow | no | no | no | yes: the writer's CAS loops until the reader releases | nothing (the writer waits instead) | one |
| `Mutex` / `RwLock` | a lock guard | it can, if it drops the last `Arc` it cloned | no | yes: it blocks | yes | nothing (the writer blocks instead) | any |

### When we would choose something else

- **`arc-swap`** is the better general-purpose cell. Its loads cost about
  what a lend does (see
  [the benchmark](https://docs.rs/crate/arc-lend/latest/source/BENCHMARKS.md)),
  need no registration, and its writer's cost does not grow with the
  readers. A reader preempted mid-load pins only the value it loaded,
  where arc-lend's pins everything retired meanwhile: under a fast
  publisher and frequent preemption, arc-swap holds less memory. It has a
  far larger API (`rcu`, `compare_and_swap`, `Cache`, `Access`
  projections) and years of use. Read through a `Cache` per reader, an
  unchanged value is read with no fence, faster than a lend, and reads
  nest without a slot limit; each change then costs every cache an owned
  count on the one shared `Arc`, which the benchmark shows under a fast
  writer. If the reader may free, or a
  thread's first load may allocate, it is the right choice. It can also be
  made safe for a deadline by discipline: prewarm each reader thread, and
  have the writer keep retired `Arc`s until their count is one. arc-lend
  makes that a property of the type instead of a protocol for its users.
- **`triple_buffer`** is better for one reader and a `Clone` value that is
  cheap to overwrite in place: no `Arc` per publish, and its reader is a
  single swap.
- **`left-right`** is better when the writer applies small operations to a
  large structure (an oplog against two copies) and can afford to wait for
  readers, or use `try_publish`.
- **`basedrop`** and **Firewheel's `ArcGc`** are better when the deadline
  thread must *own* values (store them, pass them on, drop them whenever),
  and a collector thread is acceptable. arc-lend's readers cannot keep
  anything past their loan.
- **`crossbeam-epoch`** is better for lock-free data structures whose
  readers can afford to run a destructor now and then: pinning is cheap,
  and it reclaims whole structures, not one value. A deadline thread pays
  for other threads' frees on its collecting pins.
- **`haphazard`** is better when a reader must pin at most the one value
  it reads, even while preempted, and can afford to retry its load.
- **`rtrb`** with a return ring suits one reader that takes ownership of a
  stream of values and sends each back to be freed.
- **A seqlock** (crill's `seqlock_object`, P3825) suits a small
  trivially-copyable value written often. Its readers copy, and may retry
  against a concurrent write.
- **A plain atomic** (or `Mutex`, off the deadline thread) for anything that
  fits in one, or is never read on a deadline.

### Sources

The comparison was checked against these sources (line numbers at the
version named):

- `arc-swap` 1.9.3. A store pays every outstanding debt with an owned
  count (`src/debt/mod.rs:81`, `pay_all`, called from
  `src/strategy/hybrid.rs:205`); a promoted guard's drop then releases its
  `Arc` on the reader (`src/strategy/hybrid.rs:105-125`, the
  `ManuallyDrop::drop` at line 124). Its docs say the same
  (`src/docs/performance.rs:55-60`). A thread without a debt node gets
  one on its first load (`src/debt/list.rs:223-228`), allocating it if
  none is free (`src/debt/list.rs:169`, `Box::leak(Box::<Node>::default())`).
  Readers are lock-free and "most of the time" wait-free
  (`src/docs/performance.rs:7-9`).
- `basedrop` 0.1.3. `SharedCell::get` is a reader-count increment, a
  clone and a decrement (`src/shared_cell.rs:61-74`); `replace` spins
  until the count is zero (`src/shared_cell.rs:117`). `Shared::drop`
  queues the last count to the collector rather than freeing
  (`src/shared.rs:101-111`, `src/collector.rs:97-102`).
- Firewheel 0.14 (`firewheel-core`, commit `34b35cad`). `ArcGc::new`
  registers a strong count in a global `Mutex<Vec<_>>`
  (`src/collector.rs:59`, `:75`, `:239`); `collect` drops entries whose
  count is 1 (`src/collector.rs:93`); `ArcGc::drop` only flags
  (`src/collector.rs:312-316`, `:79-85`).
- `left-right` 0.11.8. `publish` waits in a spin/yield loop until every
  reader has moved past the swap (`src/write.rs:249-300`); `try_publish`
  returns instead (`src/write.rs:320-330`). Making a `ReadHandle`
  allocates and locks the epoch list (`src/read.rs:87-91`); `enter` is
  an increment (`src/read.rs:172`).
- `crossbeam-epoch` 0.9.21. Every 128th `pin()` on a thread collects
  (`src/internal.rs:450-453`), which runs expired deferred functions on
  that thread; a thread's first `pin()` registers it
  (`src/internal.rs:334`).
- `haphazard` 0.1.8. `protect_ptr` loads, protects and re-loads until the
  pointer is stable (`src/hazard.rs:116-149`); acquiring a hazard pointer
  allocates a record when none is free (`src/domain.rs:554-560`); only a
  retire (`src/domain.rs:643`) or `eager_reclaim` (`:619-622`) reclaims.
- `triple_buffer` 9.0.0. Single-producer single-consumer
  (`src/lib.rs:125`); `Output::read` takes `&mut self` (`src/lib.rs:432`).
- `rtrb` 0.4.0. A single-producer single-consumer ring (`src/lib.rs:1`).
- farbot (commit `d8f132c2`). The realtime side takes the pointer with an
  `exchange(nullptr)` (`include/farbot/detail/RealtimeObject.tcc:44`), so
  there is one realtime reader, and the writer's `compare_exchange_weak`
  loops until it is given back (`RealtimeObject.tcc:69-72`).
- crill (commit `895eaf2f`). `seqlock_object`'s readers "can block if
  there is a concurrent write" (`include/crill/seqlock_object.h:21-23`).

