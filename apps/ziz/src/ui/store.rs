//! The rooms this profile belongs to, saved in the profile (`rooms.json`),
//! so the window shows them at once, before any network. A damaged file is
//! set aside (renamed, never overwritten) and the list starts empty.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "rooms.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedRoom {
    pub id: String,
    /// Last name the room server gave it.
    pub name: String,
    /// `ws://` or `wss://` origin of its room server.
    pub server: String,
}

pub struct RoomStore {
    path: PathBuf,
    rooms: Vec<SavedRoom>,
}

impl RoomStore {
    /// The saved rooms, and a warning when the file had to be set aside.
    pub fn load(dir: &Path) -> io::Result<(Self, Option<String>)> {
        let path = dir.join(FILE);
        // Bytes, not text: a file that is not even UTF-8 (a disk error, a
        // cut-off save) is damaged like any other, not a failure to start.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok((
                    Self {
                        path,
                        rooms: Vec::new(),
                    },
                    None,
                ));
            }
            Err(e) => return Err(e),
        };
        match serde_json::from_slice::<Vec<SavedRoom>>(without_bom(&bytes)) {
            Ok(rooms) => Ok((Self { path, rooms }, None)),
            Err(error) => {
                let aside = dir.join(format!("{FILE}.damaged"));
                std::fs::rename(&path, &aside)?;
                let warning = format!(
                    "a lista de salas estava danificada ({error}); guardada em {}",
                    aside.display()
                );
                Ok((
                    Self {
                        path,
                        rooms: Vec::new(),
                    },
                    Some(warning),
                ))
            }
        }
    }

    pub fn rooms(&self) -> &[SavedRoom] {
        &self.rooms
    }

    /// Adds the room, or updates its name and server.
    pub fn upsert(&mut self, room: SavedRoom) -> io::Result<()> {
        match self.rooms.iter_mut().find(|r| r.id == room.id) {
            Some(existing) if *existing == room => return Ok(()),
            Some(existing) => *existing = room,
            None => self.rooms.push(room),
        }
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> io::Result<()> {
        let before = self.rooms.len();
        self.rooms.retain(|r| r.id != id);
        if self.rooms.len() == before {
            return Ok(());
        }
        self.save()
    }

    /// Written to a temporary file then renamed, so a crash never leaves a
    /// half-written list.
    fn save(&self) -> io::Result<()> {
        let json = serde_json::to_string_pretty(&self.rooms).map_err(io::Error::other)?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, json + "\n")?;
        std::fs::rename(&temporary, &self.path)
    }
}

/// The file's bytes without a UTF-8 byte order mark: editors and Windows
/// PowerShell 5.1 often write one, and JSON parsers refuse it.
pub fn without_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ziz-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn room(id: &str, name: &str) -> SavedRoom {
        SavedRoom {
            id: id.into(),
            name: name.into(),
            server: "wss://example".into(),
        }
    }

    #[test]
    fn rooms_are_saved_and_read_back() {
        let dir = dir("roundtrip");
        let (mut store, warning) = RoomStore::load(&dir).unwrap();
        assert!(store.rooms().is_empty() && warning.is_none());
        store.upsert(room("a", "Sexta")).unwrap();
        store.upsert(room("b", "Trabalho")).unwrap();
        store.upsert(room("a", "Sexta à noite")).unwrap();
        let (again, _) = RoomStore::load(&dir).unwrap();
        assert_eq!(
            again.rooms(),
            &[room("a", "Sexta à noite"), room("b", "Trabalho")]
        );
        let mut again = again;
        again.remove("a").unwrap();
        assert_eq!(
            RoomStore::load(&dir).unwrap().0.rooms(),
            &[room("b", "Trabalho")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mutated_files_load_without_panicking() {
        let dir = dir("mutated");
        let (mut store, _) = RoomStore::load(&dir).unwrap();
        store.upsert(room("a", "Sexta")).unwrap();
        store.upsert(room("b", "Trabalho")).unwrap();
        let good = std::fs::read(dir.join(FILE)).unwrap();
        let mut mutator = crate::testing::Mutator(0x5EED_1234_ABCD_000A);
        for _ in 0..1_000 {
            std::fs::write(dir.join(FILE), mutator.mutate(&good)).unwrap();
            let (store, _) = RoomStore::load(&dir).unwrap();
            assert!(store.rooms().len() <= 2);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_is_set_aside_not_overwritten() {
        let dir = dir("damaged");
        std::fs::write(dir.join(FILE), "{ not json").unwrap();
        let (store, warning) = RoomStore::load(&dir).unwrap();
        assert!(store.rooms().is_empty());
        assert!(warning.unwrap().contains("danificada"));
        assert_eq!(
            std::fs::read_to_string(dir.join("rooms.json.damaged")).unwrap(),
            "{ not json"
        );
        assert!(!dir.join(FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
