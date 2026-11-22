//! Room invites as short texts to paste in a chat:
//! `zizroom:<room id>.<token>` (about 62 characters). The token works for a
//! limited time and number of uses; the room server stores only its hash.

use crate::protocol::is_id;

pub const PREFIX: &str = "zizroom:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoomInvite {
    pub room: String,
    pub token: String,
}

impl RoomInvite {
    pub fn text(&self) -> String {
        format!("{PREFIX}{}.{}", self.room, self.token)
    }

    /// Reads an invite, forgiving spaces and case around it (chats add
    /// them); `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_lowercase();
        let (room, token) = text.strip_prefix(PREFIX)?.split_once('.')?;
        (is_id(room) && is_id(token)).then(|| Self {
            room: room.to_owned(),
            token: token.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOM: &str = "abcdefghijklmnopqrstuvwxyz";
    const TOKEN: &str = "234567abcdefghijklmnopqrst";

    #[test]
    fn invites_round_trip() {
        let invite = RoomInvite {
            room: ROOM.into(),
            token: TOKEN.into(),
        };
        let text = invite.text();
        assert_eq!(text.len(), 61);
        assert_eq!(RoomInvite::parse(&text), Some(invite.clone()));
        assert_eq!(
            RoomInvite::parse(&format!("  {}\n", text.to_uppercase())),
            Some(invite)
        );
    }

    #[test]
    fn other_texts_are_refused() {
        assert_eq!(RoomInvite::parse(""), None);
        assert_eq!(RoomInvite::parse(&format!("ziz1:{ROOM}.{TOKEN}")), None);
        assert_eq!(RoomInvite::parse(&format!("{PREFIX}{ROOM}")), None);
        assert_eq!(RoomInvite::parse(&format!("{PREFIX}{ROOM}.short")), None);
        assert_eq!(RoomInvite::parse(&format!("{PREFIX}{ROOM}.{TOKEN}x")), None);
        assert_eq!(
            RoomInvite::parse(&format!("{PREFIX}{ROOM}.{}", "1".repeat(26))),
            None
        );
    }

    /// Deterministic mutations (xorshift; the same seed gives the same
    /// inputs, so a failure can be replayed): flipped bits, cuts, inserted
    /// bytes, repeated runs, stray JSON punctuation.
    struct Mutator(u64);

    impl Mutator {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }

        fn mutate(&mut self, seed: &[u8]) -> Vec<u8> {
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

    #[test]
    fn mutated_invites_never_panic() {
        let text = RoomInvite {
            room: ROOM.to_owned(),
            token: TOKEN.to_owned(),
        }
        .text();
        let mut mutator = Mutator(0x5EED_1234_ABCD_0005);
        for _ in 0..30_000 {
            let _ = RoomInvite::parse(&String::from_utf8_lossy(&mutator.mutate(text.as_bytes())));
        }
    }
}
