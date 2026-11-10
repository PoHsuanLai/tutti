use crate::Law;

/// One piece of a trajectory: a value moving from `from` to `to` along `law`,
/// read at a progress `t` in `[0, 1]`. The pure part of a segment, without an
/// address or a position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    /// The value at progress 0.
    pub from: f32,
    /// The value at progress 1.
    pub to: f32,
    /// How the value moves between them.
    pub law: Law,
}

impl Piece {
    /// A piece from `from` to `to` along `law`.
    #[must_use]
    pub const fn new(from: f32, to: f32, law: Law) -> Self {
        Self { from, to, law }
    }

    /// The value `t` of the way along the piece: `from + (to - from) * g(t)`,
    /// with `g` the law. The one evaluation of a piece the graph, the lanes and
    /// the editor share.
    #[inline]
    #[must_use]
    pub fn value(self, t: f32) -> f32 {
        self.from + (self.to - self.from) * self.law.eval(t)
    }
}

#[cfg(test)]
mod tests;
