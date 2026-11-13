use super::*;

fn taken(set: &DirtySet) -> Vec<usize> {
    set.take().map(DirtyIndex::get).collect()
}

/// **A flag is a level, not a count**: two sets of one index are one take, and
/// a second take finds nothing.
#[test]
fn two_sets_of_one_index_are_one_take() {
    let set = DirtySet::new(10);
    let three = set.index(3).unwrap();
    set.set(three);
    set.set(three);
    assert_eq!(taken(&set), [3]);
    assert!(taken(&set).is_empty());
}

/// **Indices on either side of a word boundary stay apart**, and the last
/// index of a set that ends mid-word and one that ends on a word are both
/// writable.
#[test]
fn word_boundaries_hold() {
    for len in [65, 128, 129] {
        let set = DirtySet::new(len);
        let marks = [0, 63, 64, len - 1];
        for i in marks {
            set.set(set.index(i).unwrap());
        }
        let mut want = marks.to_vec();
        want.sort_unstable();
        want.dedup();
        assert_eq!(taken(&set), want, "len {len}");
    }
    let set = DirtySet::new(128);
    set.set(set.index(63).unwrap());
    assert_eq!(taken(&set), [63], "63 does not leak into word 1");
    set.set(set.index(64).unwrap());
    assert_eq!(taken(&set), [64], "64 does not leak into word 0");
}

/// **An index past the end is refused where it is made**, for every length
/// including a full word and none.
#[test]
fn an_index_past_the_end_is_refused() {
    for len in [0, 1, 63, 64, 65] {
        let set = DirtySet::new(len);
        assert_eq!(set.len(), len);
        assert_eq!(set.is_empty(), len == 0);
        assert!(set.index(len).is_none(), "len {len}");
        assert!(set.index(usize::MAX).is_none());
        if len > 0 {
            assert!(set.index(len - 1).is_some());
        }
    }
}

/// **A take dropped early leaves the words it did not reach marked**, so the
/// next take yields them.
#[test]
fn a_take_dropped_early_leaves_the_rest() {
    let set = DirtySet::new(200);
    for i in [1, 70, 150] {
        set.set(set.index(i).unwrap());
    }
    assert_eq!(set.take().next().map(DirtyIndex::get), Some(1));
    assert_eq!(taken(&set), [70, 150]);
}
