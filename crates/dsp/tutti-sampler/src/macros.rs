//! Shared boilerplate for the crate's `AudioUnit` impls.

/// Expand the copy-pasted tail methods of an `AudioUnit` impl.
///
/// Invoked *inside* an `impl AudioUnit for T { .. }` block. Always emits the
/// identical trio (`as_any` / `as_any_mut` / `get_id`); `route` and `footprint`
/// are emitted only when asked, so a unit whose `route`/`footprint` genuinely
/// differ (e.g. time-stretch's latency-delayed route, the voice pool's
/// slot-sized footprint) simply omits them and hand-writes its own.
///
/// ```ignore
/// impl AudioUnit for Foo {
///     // ...inputs/outputs/tick/process...
///     audio_unit_boilerplate!(id = SOME_NODE_ID, outputs = 2);
/// }
/// ```
///
/// Forms:
/// - `id = <expr>` — emit only `as_any` / `as_any_mut` / `get_id`.
/// - `id = <expr>, outputs = <n>` — additionally emit the default `route`
///   (`SignalFrame::new(n)`) and `footprint` (`size_of::<Self>()`).
macro_rules! audio_unit_boilerplate {
    (id = $id:expr) => {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn get_id(&self) -> u64 {
            $id
        }
    };
    (id = $id:expr, outputs = $outputs:expr) => {
        audio_unit_boilerplate!(id = $id);
        fn route(
            &mut self,
            _input: &tutti_core::SignalFrame,
            _frequency: f64,
        ) -> tutti_core::SignalFrame {
            tutti_core::SignalFrame::new($outputs)
        }
        fn footprint(&self) -> usize {
            std::mem::size_of::<Self>()
        }
    };
}
