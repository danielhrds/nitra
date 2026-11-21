//! Contacts: the keys this profile trusts, with the name each one goes by,
//! in `contacts.json` in the profile. A contact connects without asking;
//! any other key needs the user's yes first (then it is saved here).
//! No addresses are kept: they change every session.

use std::io;
use std::path::{Path, PathBuf};

use identity::PublicKey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contact {
    /// Key code.
    pub key: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    contacts: Vec<Contact>,
}

pub struct Contacts {
    path: PathBuf,
    list: Vec<Contact>,
}

impl Contacts {
    /// Reads `path` (none yet: no contacts). A file that cannot be read is
    /// an error, not an empty list that would be saved over it.
    pub fn load(path: &Path) -> io::Result<Self> {
        let list = match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice::<File>(&bytes)
                    .map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("contacts file {} is damaged ({e})", path.display()),
                        )
                    })?
                    .contacts
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: path.to_owned(),
            list,
        })
    }

    pub fn find(&self, key: &PublicKey) -> Option<&Contact> {
        let code = key.to_string();
        self.list.iter().find(|c| c.key == code)
    }

    /// Adds (or renames) a contact and saves the file.
    pub fn add(&mut self, key: &PublicKey, name: &str) -> io::Result<()> {
        let code = key.to_string();
        self.list.retain(|c| c.key != code);
        self.list.push(Contact {
            key: code,
            name: name.to_owned(),
        });
        let json = serde_json::to_vec_pretty(&File {
            contacts: self.list.clone(),
        })
        .map_err(io::Error::other)?;
        let temporary = self.path.with_extension("tmp");
        std::fs::write(&temporary, json)?;
        std::fs::rename(&temporary, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity::Identity;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ziz-contacts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn contacts_are_saved_and_found_again() {
        let path = temp_file("contacts.json");
        let alice = *Identity::generate().unwrap().public_key();
        let bob = *Identity::generate().unwrap().public_key();
        let mut contacts = Contacts::load(&path).unwrap();
        assert!(contacts.find(&alice).is_none());
        contacts.add(&alice, "alice").unwrap();
        contacts.add(&alice, "alice (new pc)").unwrap();
        let again = Contacts::load(&path).unwrap();
        assert_eq!(again.find(&alice).unwrap().name, "alice (new pc)");
        assert!(again.find(&bob).is_none());
        assert_eq!(again.list.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_damaged_file_is_an_error() {
        let path = temp_file("damaged.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let error = Contacts::load(&path).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&path);
    }
}
