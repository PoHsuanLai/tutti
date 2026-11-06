use super::*;
use tutti_graph::Harmony;

/// **Lanes a block still held when `set` replaced them are freed by the
/// next `reclaim`**, with no further publish.
///
/// Mutation (run): `HarmonyControls::reclaim` returning 0 without
/// reclaiming → the first lanes are still alive, fails.
#[test]
fn reclaim_frees_lanes_a_block_held_across_their_replacement() {
    let c_major = Harmony::chord(60, 60, 0b1001_0001);
    let mut node = HarmonyNode::new([TimedHarmony::new(Beat(0.0), c_major)]);
    let controls = HarmonyControls {
        shared: Arc::clone(&node.shared),
    };
    let gone = Arc::downgrade(&node.shared.current());
    {
        // A block in progress across the replacement.
        let _held = node.lanes.lend();
        controls.clear();
    }
    assert!(gone.upgrade().is_some(), "retired, pinned until a scan");
    assert_eq!(controls.reclaim(), 0);
    assert!(gone.upgrade().is_none(), "freed by the reclaim");
}
