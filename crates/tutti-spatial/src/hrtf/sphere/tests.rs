use super::*;

/// The resampler reads a response's first and last samples. One vertex
/// whose left response is a unit delta on its first sample and right a
/// unit delta on its last, neither decayed, is doubled from 8 to 16 kHz.
/// Each response is resampled alone, so an output tap at `t` is
/// `step · kernel(t − m)` for its delta at `m`, and the tap that lands on
/// the delta (`n = 2m`) takes the kernel's centre: sinc and window are 1
/// there, so it is `step · cutoff = 0.5 · 0.95 = 0.475`, the delta's DC
/// sum of 1 at the kernel's peak. No libm call is involved but `cos(0)`,
/// which is exact; the tolerance covers only the `f32` rounding.
///
/// Mutations (run):
/// - `.min(len)` → `.min(len - 1)` on the window's `end` (drops every
///   response's last sample) → the right ear's tap is 0, not 0.475.
/// - `.max(0.0)` → `.max(1.0)` on its `lo` (drops every response's first
///   sample) → the left ear's tap is 0.
/// - emitting the right response as index 0 (swap the two) → the left
///   ear's tap is 0.
#[test]
fn resampling_reads_a_responses_edge_taps() {
    const LEN: usize = 8;
    let mut responses = vec![0.0f32; 2 * LEN];
    responses[0] = 1.0;
    responses[2 * LEN - 1] = 1.0;
    let out_len = 2 * LEN;
    let mut got = Vec::new();
    resample_into(&responses, LEN, 8_000, 16_000, out_len, |r, response| {
        assert_eq!(r, got.len(), "emitted in index order");
        got.push(response.to_vec());
    });
    assert_eq!(got.len(), 2);

    let first = got[0][0];
    let last = got[1][2 * (LEN - 1)];
    for (ear, tap) in [("left, first sample", first), ("right, last sample", last)] {
        assert!(
            (tap - 0.475).abs() <= 1e-6,
            "{ear}: the tap on the delta is {tap}, expected 0.475"
        );
    }
}

/// Resampling in small batches gives every response the same samples as
/// one batch, and the right index. Fifteen responses of 8 taps go from 8 to
/// 16 kHz in batches of 3, so four boundaries fall, some between a
/// vertex's two ears. Response `r` is a delta of
/// amplitude `r + 1` on tap `r % 8`, so its output tap `2 (r % 8)` is
/// `0.475 (r + 1)` (see `resampling_reads_a_responses_edge_taps`), which
/// pins each emitted index to its own response, and the whole output is
/// bit-identical to the single-batch run.
///
/// Mutations (run):
/// - `emit(first + r, ..)` -> `emit(r, ..)` -> the order assertion fails
///   at the first response of the second batch.
/// - `first = batch * per_batch` -> `batch * per_batch + 1` -> same.
/// - dropping the `r` offset in the tap-major copy (`taps[m * count]`)
///   -> the analytic tap of response 1 is wrong.
#[test]
fn resampling_in_batches_matches_one_batch() {
    const LEN: usize = 8;
    const OUT_LEN: usize = 16;
    const RESPONSES: usize = 15;
    let mut responses = vec![0.0f32; RESPONSES * LEN];
    for r in 0..RESPONSES {
        responses[r * LEN + r % LEN] = (r + 1) as f32;
    }
    let run = |batch_samples: usize| {
        let mut got = Vec::new();
        resample_batched(
            &responses,
            LEN,
            8_000,
            16_000,
            OUT_LEN,
            batch_samples,
            |r, response| {
                assert_eq!(r, got.len(), "emitted in index order");
                got.push(response.to_vec());
            },
        );
        got
    };
    let whole = run(RESAMPLE_BATCH);
    let small = run(3 * OUT_LEN);
    assert_eq!(whole.len(), RESPONSES);
    assert_eq!(small, whole, "batching changes no sample");
    for (r, response) in small.iter().enumerate() {
        let tap = response[2 * (r % LEN)];
        let want = 0.475 * (r + 1) as f32;
        assert!(
            (tap - want).abs() <= 1e-5 * want,
            "response {r}: tap {tap}, expected {want}"
        );
    }
}
