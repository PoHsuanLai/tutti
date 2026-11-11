//! The auto traits the design rests on, asserted at compile time.

use super::*;

/// Autoref specialization: the inherent const on `Wrap<T>` wins over the
/// trait's default only when the bound holds, so `IS_*` resolves to `false`
/// exactly when the type does *not* have the trait. A plain
/// `assert_not_send::<T>()` cannot express this (there is no stable negative
/// bound).
///
/// `const` blocks rather than `assert!`: both operands are compile-time
/// constants, so the property fails the *build*, not just a test run.
struct Wrap<T>(PhantomData<T>);
trait NotSend {
    const IS_SEND: bool = false;
}
impl<T> NotSend for Wrap<T> {}
impl<T: Send> Wrap<T> {
    const IS_SEND: bool = true;
}
trait NotSync {
    const IS_SYNC: bool = false;
}
impl<T> NotSync for Wrap<T> {}
impl<T: Sync> Wrap<T> {
    const IS_SYNC: bool = true;
}

/// A loan must not escape the thread that took it: that is what makes "moved
/// to another thread" a compile error rather than a convention.
#[test]
fn a_loan_is_not_send() {
    const {
        assert!(!<Wrap<Loan<'static, Vec<u32>>>>::IS_SEND);
    }
    // Control: the same machinery reports `true` for a type that IS `Send`,
    // so the assertion above measures something.
    const {
        assert!(<Wrap<Vec<u32>>>::IS_SEND);
    }
}

/// A reader moves to the thread that lends (it is made on the control side),
/// and is not shared: one reader, one owner, one slot.
#[test]
fn a_reader_is_send_but_not_sync() {
    const {
        assert!(<Wrap<Reader<Vec<u32>>>>::IS_SEND);
        assert!(!<Wrap<Reader<Vec<u32>>>>::IS_SYNC);
        // Controls.
        assert!(<Wrap<Vec<u32>>>::IS_SYNC);
        assert!(!<Wrap<Cell<u32>>>::IS_SYNC);
    }
    // And only for a payload the cell itself may share: a reader of a
    // `!Sync` payload would hand `&T` to another thread.
    const {
        assert!(!<Wrap<Reader<Cell<u32>>>>::IS_SEND);
    }
}

/// The cell itself must stay shareable: every reader holds an
/// `Arc<ArcLend<_>>` across threads. The raw pointer inside would make it
/// neither `Send` nor `Sync` by default; `PhantomData<Arc<T>>` plus the
/// atomics is what gives it `Arc<T>`'s auto traits back without an
/// `unsafe impl`.
#[test]
fn the_cell_is_send_and_sync_for_send_sync_payloads() {
    fn assert_send_sync<X: Send + Sync>() {}
    assert_send_sync::<ArcLend<Vec<u32>>>();
}
