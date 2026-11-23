//! Test helpers (built only for tests).

/// Deterministic mutations (xorshift; the same seed gives the same
/// inputs, so a failure can be replayed): flipped bits, cuts, inserted
/// bytes, repeated runs, stray JSON punctuation.
pub struct Mutator(pub u64);

impl Mutator {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    pub fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
        let mut v = seed.to_vec();
        for _ in 0..=self.below(4) {
            match self.below(5) {
                0 if !v.is_empty() => {
                    let i = self.below(v.len());
                    v[i] ^= 1 << self.below(8);
                }
                1 => {
                    let i = self.below(v.len() + 1);
                    v.truncate(i);
                }
                2 => {
                    let (i, b) = (self.below(v.len() + 1), self.next() as u8);
                    v.insert(i, b);
                }
                3 if !v.is_empty() => {
                    let i = self.below(v.len());
                    let j = (i + 1 + self.below(32)).min(v.len());
                    let run = v[i..j].to_vec();
                    v.splice(i..i, run);
                }
                _ => {
                    let i = self.below(v.len() + 1);
                    v.insert(i, b"{}[]\":,\\0e-"[self.below(11)]);
                }
            }
        }
        v
    }
}
