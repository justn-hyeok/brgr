//! A deterministic byte source for property tests.
//!
//! brgr parses bytes it did not write: a Herdr plugin's bridge request, a
//! directory name sitting in the worktrees root. Example-based tests cover the
//! shapes someone thought of, which is the wrong set — the interesting inputs
//! are the ones nobody pictured. This walks a far wider set from a fixed seed,
//! so a failure names the case that produced it and re-running reproduces it.
//!
//! No dependency: `rand` would pull a crate into the tree for something a dozen
//! lines does, and a seeded generator here is reproducible in a way a
//! thread-local RNG is not.

/// xorshift64*, chosen because it is short enough to read and verify by eye.
pub struct Adversary(u64);

impl Adversary {
    pub const fn new(seed: u64) -> Self {
        // Zero is xorshift's fixed point: it would emit nothing but zero.
        Self(if seed == 0 { 1 } else { seed })
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value below `bound`, or zero when `bound` is zero.
    ///
    /// Scaled from the high half rather than `% bound`. The low bits of a
    /// multiply-based xorshift are its weakest, and `% bound` for a small bound
    /// reads only those: with modulo, a conjunction of three roughly even
    /// choices came out at 89 of 2,048 cases instead of the ~205 independence
    /// predicts. Scaling the high half also avoids modulo bias.
    pub fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        let draw = u128::from(self.next() >> 32);
        usize::try_from((draw * bound as u128) >> 32).unwrap_or(0)
    }

    /// One of `choices`.
    pub fn pick<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        &choices[self.below(choices.len())]
    }

    /// A string of up to `max` characters drawn from `alphabet`.
    ///
    /// Drawing from a named alphabet rather than arbitrary Unicode keeps the
    /// cases near the boundary that matters: a slug parser is far more likely to
    /// be wrong about `+`, `0`, and `r` than about an emoji.
    pub fn text(&mut self, alphabet: &str, max: usize) -> String {
        let letters: Vec<char> = alphabet.chars().collect();
        let length = self.below(max + 1);
        (0..length).map(|_| *self.pick(&letters)).collect()
    }
}
