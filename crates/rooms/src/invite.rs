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
}
