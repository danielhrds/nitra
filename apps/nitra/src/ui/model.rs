//! What the window shows, built from the saved rooms and the room events:
//! each room's connection, participants and presence, which room is
//! selected and which one we are in. Pure: events in, state and effects
//! out, so every case is unit tested; the window only draws it.

use std::collections::HashMap;

use rooms::{Member, Presence, Role, RoomEvent, Screen};

use super::store::SavedRoom;
use crate::host::Watcher;
use crate::viewer::LiveStats;

#[derive(Clone, Debug, PartialEq)]
pub enum Connection {
    Connecting,
    Online,
    /// Lost; trying again in this many seconds.
    Reconnecting(u64),
    Ended(String),
}

#[derive(Clone, Debug)]
pub struct RoomState {
    pub id: String,
    pub name: String,
    pub server: String,
    pub connection: Connection,
    pub members: Vec<Member>,
    /// Who has the app open, by key code (only those online).
    pub presence: HashMap<String, Presence>,
    /// The latest still picture of each live screen, by sharer key.
    pub previews: HashMap<String, PreviewImage>,
}

/// A preview the app turned into a texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewImage {
    /// The texture as Dear ImGui draws it (a shader resource view pointer).
    pub texture: u64,
    pub width: u32,
    pub height: u32,
}

impl RoomState {
    fn new(saved: &SavedRoom) -> Self {
        Self {
            id: saved.id.clone(),
            name: saved.name.clone(),
            server: saved.server.clone(),
            connection: Connection::Connecting,
            members: Vec::new(),
            presence: HashMap::new(),
            previews: HashMap::new(),
        }
    }

    /// Screens live now: (key, name of who shares it, screen), oldest first.
    pub fn live_screens(&self) -> Vec<(String, String, Screen)> {
        let mut screens: Vec<_> = self
            .presence
            .values()
            .filter_map(|p| Some((p.key.clone(), self.member_name(&p.key), p.screen.clone()?)))
            .collect();
        screens.sort_by_key(|(_, _, s)| s.started_at);
        screens
    }

    pub fn in_room_count(&self) -> usize {
        self.presence.values().filter(|p| p.entered).count()
    }

    pub fn member_name(&self, key: &str) -> String {
        self.members
            .iter()
            .find(|m| m.key == key)
            .map_or_else(|| "?".to_owned(), |m| m.name.clone())
    }

    pub fn role_of(&self, key: &str) -> Option<Role> {
        self.members.iter().find(|m| m.key == key).map(|m| m.role)
    }

    pub fn is_live(&self) -> bool {
        self.presence.values().any(|p| p.screen.is_some())
    }
}

/// What the app must do after an event (the model never does I/O).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Save this room's (new) name.
    SaveRoom {
        id: String,
        name: String,
    },
    /// We are out of this room for good: forget it.
    ForgetRoom(String),
    CopyInvite(String),
    Notice(String),
}

pub struct Model {
    /// Our key code, canonical (no dashes).
    pub me: String,
    pub rooms: Vec<RoomState>,
    pub selected: Option<String>,
    /// The room we entered (one at a time).
    pub entered: Option<String>,
    /// What this app shares now (one screen at a time), from the moment it
    /// was picked: the room's presence follows once the host is up.
    pub sharing: Option<LocalShare>,
    /// Screens this app watches (one per sharer and room).
    pub watching: Vec<LocalWatch>,
    /// The watched screen shown large (a sharer's key), if any.
    pub focus: Option<String>,
    /// The watched screen filling the window, if any.
    pub fullscreen: Option<String>,
    /// Live numbers of each watched screen, by sharer key (the app refreshes
    /// them before drawing).
    pub watch_stats: HashMap<String, LiveStats>,
    /// Who watches our screen, with the link to each (the app refreshes it).
    pub audience: Vec<Watcher>,
    /// The name others see (from the settings).
    pub my_name: String,
    /// The saved settings (the room panel shows and changes how a share is
    /// sent; the picker shows it).
    pub settings: super::settings::Settings,
    /// The GPU everything runs on, and whether nitra was tested on it.
    pub gpu: Option<crate::hardware::Gpu>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalWatch {
    /// Which watching thread.
    pub id: u64,
    pub room: String,
    /// The sharer's key code (canonical).
    pub key: String,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalShare {
    /// Which sharing thread (a stopped one may still report back).
    pub id: u64,
    pub room: String,
    /// What is shared, as the picker named it.
    pub label: String,
}

impl Model {
    pub fn new(me: String, saved: &[SavedRoom]) -> Self {
        Self {
            me,
            rooms: saved.iter().map(RoomState::new).collect(),
            selected: saved.first().map(|r| r.id.clone()),
            entered: None,
            sharing: None,
            watching: Vec::new(),
            focus: None,
            fullscreen: None,
            watch_stats: HashMap::new(),
            audience: Vec::new(),
            my_name: String::new(),
            settings: super::settings::Settings::default(),
            gpu: None,
        }
    }

    pub fn room(&self, id: &str) -> Option<&RoomState> {
        self.rooms.iter().find(|r| r.id == id)
    }

    fn room_mut(&mut self, id: &str) -> Option<&mut RoomState> {
        self.rooms.iter_mut().find(|r| r.id == id)
    }

    pub fn selected_room(&self) -> Option<&RoomState> {
        self.room(self.selected.as_deref()?)
    }

    /// A room we just created or joined: shown and selected at once.
    pub fn add_room(&mut self, saved: &SavedRoom) {
        if self.room(&saved.id).is_none() {
            self.rooms.push(RoomState::new(saved));
        }
        self.selected = Some(saved.id.clone());
    }

    pub fn remove_room(&mut self, id: &str) {
        self.rooms.retain(|r| r.id != id);
        if self.selected.as_deref() == Some(id) {
            self.selected = self.rooms.first().map(|r| r.id.clone());
        }
        if self.entered.as_deref() == Some(id) {
            self.entered = None;
        }
    }

    pub fn apply(&mut self, id: &str, event: RoomEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(room) = self.room_mut(id) else {
            return effects;
        };
        match event {
            RoomEvent::Connected(welcome) => {
                room.connection = Connection::Online;
                room.members = welcome.members;
                room.presence = welcome
                    .presence
                    .into_iter()
                    .filter(|p| p.online)
                    .map(|p| (p.key.clone(), p))
                    .collect();
                if !welcome.room.name.is_empty() && welcome.room.name != room.name {
                    room.name = welcome.room.name;
                    effects.push(Effect::SaveRoom {
                        id: id.to_owned(),
                        name: room.name.clone(),
                    });
                }
            }
            RoomEvent::MemberJoined(member) => {
                room.members.retain(|m| m.key != member.key);
                room.members.push(member);
            }
            RoomEvent::MemberLeft(key) | RoomEvent::MemberRemoved { key, .. } => {
                room.members.retain(|m| m.key != key);
                room.presence.remove(&key);
            }
            RoomEvent::OwnerChanged(key) => {
                for member in &mut room.members {
                    member.role = if member.key == key {
                        Role::Owner
                    } else {
                        Role::Member
                    };
                }
            }
            RoomEvent::MemberRenamed { key, name } => {
                if let Some(member) = room.members.iter_mut().find(|m| m.key == key) {
                    member.name = name;
                }
            }
            RoomEvent::RoomRenamed(name) => {
                room.name = name.clone();
                effects.push(Effect::SaveRoom {
                    id: id.to_owned(),
                    name,
                });
            }
            RoomEvent::Presence(presence) => {
                // A screen that ended takes its preview with it.
                if presence.screen.is_none() {
                    room.previews.remove(&presence.key);
                }
                if presence.online {
                    room.presence.insert(presence.key.clone(), presence);
                } else {
                    room.presence.remove(&presence.key);
                }
            }
            // Opened and turned into a texture by the app (`set_preview`).
            RoomEvent::Preview { .. } => {}
            RoomEvent::InviteCreated { invite, .. } => {
                effects.push(Effect::CopyInvite(invite.text()))
            }
            RoomEvent::ServerError(reason) => {
                effects.push(Effect::Notice(format!("A sala recusou: {reason}")))
            }
            RoomEvent::Disconnected { retry_in, .. } => {
                room.connection = Connection::Reconnecting(retry_in.as_secs().max(1));
                // Unknown while away; the next welcome brings it back.
                room.presence.clear();
                room.previews.clear();
            }
            RoomEvent::Ended { reason } => {
                room.presence.clear();
                room.previews.clear();
                room.connection = Connection::Ended(reason.clone());
                let notice = if reason.contains("(1000") {
                    Some(format!("Você saiu de \"{}\".", room.name))
                } else if reason.contains("(4004") {
                    Some(format!("Você foi removido de \"{}\".", room.name))
                } else if reason.contains("(4003") {
                    Some(format!(
                        "\"{}\" não aceitou a entrada (convite inválido ou você não é participante).",
                        room.name
                    ))
                } else {
                    None
                };
                if let Some(notice) = notice {
                    effects.push(Effect::Notice(notice));
                    effects.push(Effect::ForgetRoom(id.to_owned()));
                }
            }
            RoomEvent::Signal { .. } | RoomEvent::SignalFailed { .. } => {}
        }
        effects
    }

    /// Our status for the "you" card.
    /// Keeps `key`'s new preview in `room` (only while their screen is live).
    pub fn set_preview(&mut self, room: &str, key: &str, image: PreviewImage) -> bool {
        let Some(room) = self.room_mut(room) else {
            return false;
        };
        let live = room.presence.get(key).is_some_and(|p| p.screen.is_some());
        if live {
            room.previews.insert(key.to_owned(), image);
        }
        live
    }

    /// Every texture a preview uses (the app frees the others).
    pub fn preview_textures(&self) -> Vec<u64> {
        self.rooms
            .iter()
            .flat_map(|r| r.previews.values().map(|p| p.texture))
            .collect()
    }

    /// Our watch of `key`'s screen in `room`, if any.
    pub fn watch_of(&self, room: &str, key: &str) -> Option<&LocalWatch> {
        self.watching
            .iter()
            .find(|w| w.room == room && w.key == key)
    }

    /// Watches whose screen is gone from the room (the sharer stopped or
    /// left): to stop.
    pub fn watches_without_screen(&self) -> Vec<u64> {
        self.watching
            .iter()
            .filter(|w| {
                self.room(&w.room)
                    .is_none_or(|room| room.presence.get(&w.key).is_none_or(|p| p.screen.is_none()))
            })
            .map(|w| w.id)
            .collect()
    }

    /// Forgets a watch (stopped or ended), with the focus or full screen on it.
    pub fn forget_watch(&mut self, id: u64) {
        if let Some(index) = self.watching.iter().position(|w| w.id == id) {
            let watch = self.watching.remove(index);
            if self.focus.as_deref() == Some(watch.key.as_str()) {
                self.focus = None;
            }
            if self.fullscreen.as_deref() == Some(watch.key.as_str()) {
                self.fullscreen = None;
            }
            self.watch_stats.remove(&watch.key);
        }
    }

    pub fn my_status(&self) -> String {
        if let Some(share) = &self.sharing
            && let Some(room) = self.room(&share.room)
        {
            return format!("Transmitindo em {}", room.name);
        }
        match self.entered.as_deref().and_then(|id| self.room(id)) {
            Some(room) => {
                let sharing = room
                    .presence
                    .get(&self.me)
                    .is_some_and(|p| p.screen.is_some());
                if sharing {
                    format!("Transmitindo em {}", room.name)
                } else {
                    format!("Na sala {}", room.name)
                }
            }
            None => "Fora de sala".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rooms::{RoomInvite, ScreenInfo, ScreenKind, Welcome};

    use super::*;

    const ME: &str = "me";

    fn saved(id: &str, name: &str) -> SavedRoom {
        SavedRoom {
            id: id.into(),
            name: name.into(),
            server: "wss://s".into(),
        }
    }

    fn member(key: &str, name: &str, role: Role) -> Member {
        Member {
            key: key.into(),
            name: name.into(),
            role,
            joined_at: 1,
        }
    }

    fn presence(key: &str, entered: bool, screen: Option<&str>) -> Presence {
        Presence {
            key: key.into(),
            online: true,
            entered,
            screen: screen.map(|name| Screen {
                info: ScreenInfo {
                    kind: ScreenKind::Window,
                    name: name.into(),
                    width: 1920,
                    height: 1080,
                    fps: 60,
                },
                started_at: 5,
            }),
        }
    }

    fn welcome(name: &str) -> RoomEvent {
        RoomEvent::Connected(Welcome {
            room: rooms::protocol::RoomInfo { name: name.into() },
            you: member(ME, "Eu", Role::Owner),
            members: vec![
                member(ME, "Eu", Role::Owner),
                member("ana", "Ana", Role::Member),
            ],
            presence: vec![
                presence(ME, false, None),
                presence("ana", true, Some("Valorant")),
            ],
        })
    }

    fn model() -> Model {
        Model::new(ME.into(), &[saved("r1", "Sexta"), saved("r2", "Trabalho")])
    }

    #[test]
    fn a_picked_share_shows_at_once() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.entered = Some("r1".into());
        assert_eq!(m.my_status(), "Na sala Sexta");
        // Picked: the status says so before the room's presence catches up.
        m.sharing = Some(LocalShare {
            id: 1,
            room: "r1".into(),
            label: "Tela 1 · 1920×1080".into(),
        });
        assert_eq!(m.my_status(), "Transmitindo em Sexta");
        m.sharing = None;
        assert_eq!(m.my_status(), "Na sala Sexta");
    }

    #[test]
    fn a_new_name_shows_at_once() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.apply(
            "r1",
            RoomEvent::MemberRenamed {
                key: "ana".into(),
                name: "Ana Paula".into(),
            },
        );
        assert_eq!(m.room("r1").unwrap().member_name("ana"), "Ana Paula");
    }

    #[test]
    fn a_preview_lives_as_long_as_its_screen() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        let image = PreviewImage {
            texture: 42,
            width: 320,
            height: 180,
        };
        // Ana shares (welcome): kept. Nobody else: refused.
        assert!(m.set_preview("r1", "ana", image));
        assert!(!m.set_preview("r1", ME, image), "not sharing");
        assert_eq!(m.preview_textures(), vec![42]);
        // Her screen ends: the preview goes, and its texture can be freed.
        m.room_mut("r1")
            .unwrap()
            .presence
            .get_mut("ana")
            .unwrap()
            .screen = None;
        let stopped = m.room("r1").unwrap().presence["ana"].clone();
        m.apply("r1", RoomEvent::Presence(stopped));
        assert!(m.preview_textures().is_empty());
        // A lost connection forgets them too.
        m.apply("r1", welcome("Sexta"));
        assert!(m.set_preview("r1", "ana", image));
        m.apply(
            "r1",
            RoomEvent::Disconnected {
                reason: "test".into(),
                retry_in: Duration::from_secs(1),
            },
        );
        assert!(m.preview_textures().is_empty());
    }

    #[test]
    fn a_watch_stops_when_its_screen_is_gone() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.watching.push(LocalWatch {
            id: 7,
            room: "r1".into(),
            key: "ana".into(),
            muted: false,
        });
        m.focus = Some("ana".into());
        m.fullscreen = Some("ana".into());
        assert!(m.watch_of("r1", "ana").is_some());
        // Ana is sharing: nothing to stop.
        assert!(m.watches_without_screen().is_empty());
        // She stops sharing: the watch goes, with the focus and full screen.
        m.room_mut("r1")
            .unwrap()
            .presence
            .get_mut("ana")
            .unwrap()
            .screen = None;
        assert_eq!(m.watches_without_screen(), vec![7]);
        m.forget_watch(7);
        assert!(m.watching.is_empty() && m.focus.is_none() && m.fullscreen.is_none());
    }

    #[test]
    fn saved_rooms_show_before_the_network() {
        let m = model();
        assert_eq!(m.rooms.len(), 2);
        assert_eq!(m.selected.as_deref(), Some("r1"));
        assert_eq!(m.rooms[0].connection, Connection::Connecting);
        assert_eq!(m.my_status(), "Fora de sala");
    }

    #[test]
    fn welcome_fills_the_room_and_a_new_name_is_saved() {
        let mut m = model();
        let effects = m.apply("r1", welcome("Sexta à noite"));
        assert_eq!(
            effects,
            vec![Effect::SaveRoom {
                id: "r1".into(),
                name: "Sexta à noite".into()
            }]
        );
        let room = m.room("r1").unwrap();
        assert_eq!(room.connection, Connection::Online);
        assert_eq!(room.members.len(), 2);
        assert_eq!(room.in_room_count(), 1);
        let live = room.live_screens();
        assert_eq!(live.len(), 1);
        assert_eq!(
            (live[0].1.as_str(), live[0].2.info.name.as_str()),
            ("Ana", "Valorant")
        );
        assert!(
            m.apply("r1", welcome("Sexta à noite")).is_empty(),
            "same name: nothing to save"
        );
    }

    #[test]
    fn presence_changes_and_departures_update_the_room() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.apply(
            "r1",
            RoomEvent::Presence(Presence {
                online: false,
                ..presence("ana", false, None)
            }),
        );
        assert!(!m.room("r1").unwrap().is_live());
        m.apply(
            "r1",
            RoomEvent::MemberJoined(member("bia", "Bia", Role::Member)),
        );
        m.apply(
            "r1",
            RoomEvent::Presence(presence("bia", true, Some("Monitor 1"))),
        );
        assert_eq!(m.room("r1").unwrap().live_screens()[0].1, "Bia");
        m.apply(
            "r1",
            RoomEvent::MemberRemoved {
                key: "bia".into(),
                by: ME.into(),
            },
        );
        let room = m.room("r1").unwrap();
        assert!(room.presence.is_empty() || !room.presence.contains_key("bia"));
        assert_eq!(room.members.len(), 2);
    }

    #[test]
    fn owner_changes_and_renames_are_followed() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.apply("r1", RoomEvent::OwnerChanged("ana".into()));
        let room = m.room("r1").unwrap();
        assert_eq!(
            (room.role_of("ana"), room.role_of(ME)),
            (Some(Role::Owner), Some(Role::Member))
        );
        let effects = m.apply("r1", RoomEvent::RoomRenamed("Domingo".into()));
        assert_eq!(
            effects,
            vec![Effect::SaveRoom {
                id: "r1".into(),
                name: "Domingo".into()
            }]
        );
        assert_eq!(m.room("r1").unwrap().name, "Domingo");
    }

    #[test]
    fn reconnecting_hides_stale_presence() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.apply(
            "r1",
            RoomEvent::Disconnected {
                reason: "x".into(),
                retry_in: Duration::from_millis(1500),
            },
        );
        let room = m.room("r1").unwrap();
        assert_eq!(room.connection, Connection::Reconnecting(1));
        assert!(room.presence.is_empty());
    }

    #[test]
    fn refusals_forget_the_room_with_a_notice() {
        for (code, words) in [
            ("1000", "saiu"),
            ("4004", "removido"),
            ("4003", "não aceitou"),
        ] {
            let mut m = model();
            let effects = m.apply(
                "r2",
                RoomEvent::Ended {
                    reason: format!("closed by the room ({code} x)"),
                },
            );
            assert!(
                matches!(&effects[0], Effect::Notice(n) if n.contains(words)),
                "{effects:?}"
            );
            assert_eq!(effects[1], Effect::ForgetRoom("r2".into()));
        }
        let mut m = model();
        assert!(
            m.apply(
                "r2",
                RoomEvent::Ended {
                    reason: "stopped".into()
                }
            )
            .is_empty()
        );
    }

    #[test]
    fn invites_are_copied_and_refusals_shown() {
        let mut m = model();
        let invite = RoomInvite {
            room: "a".repeat(26),
            token: "b".repeat(26),
        };
        let effects = m.apply(
            "r1",
            RoomEvent::InviteCreated {
                invite: invite.clone(),
                expires_at: 0,
                uses_left: 5,
            },
        );
        assert_eq!(effects, vec![Effect::CopyInvite(invite.text())]);
        let effects = m.apply(
            "r1",
            RoomEvent::ServerError("only the owner can rename the room".into()),
        );
        assert!(matches!(&effects[0], Effect::Notice(n) if n.contains("only the owner")));
    }

    #[test]
    fn rooms_are_added_selected_and_removed() {
        let mut m = model();
        m.add_room(&saved("r3", "Nova"));
        assert_eq!(m.selected.as_deref(), Some("r3"));
        m.entered = Some("r3".into());
        assert_eq!(m.my_status(), "Na sala Nova");
        m.remove_room("r3");
        assert_eq!(
            (m.selected.as_deref(), m.entered.as_deref()),
            (Some("r1"), None)
        );
        assert!(m.apply("unknown", welcome("x")).is_empty());
    }

    #[test]
    fn my_status_says_when_i_share() {
        let mut m = model();
        m.apply("r1", welcome("Sexta"));
        m.entered = Some("r1".into());
        m.apply(
            "r1",
            RoomEvent::Presence(presence(ME, true, Some("Monitor 1"))),
        );
        assert_eq!(m.my_status(), "Transmitindo em Sexta");
    }
}
