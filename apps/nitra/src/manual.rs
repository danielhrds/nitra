//! Manual invites, for connecting without a signaling server: the host's
//! signed offer and the viewer's signed answer travel as `nitra1:` texts the
//! two people copy and paste (WhatsApp, Discord...). Each invite is
//! answered once and lasts ten minutes. A key that is not a contact is
//! shown and needs a yes before it connects, then it is saved.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use identity::PublicKey;
use signaling::invite;
use signaling::signed::{Envelope, MANUAL_LIFETIME_SECS};

use crate::clipboard;
use crate::contacts::Contacts;

/// Where the texts go and come from: the clipboard (people), or files
/// (scripts and tests).
#[derive(Clone, Debug, Default)]
pub struct ManualArgs {
    /// Host: write the invite here. Viewer: read it from here.
    pub invite_file: Option<PathBuf>,
    /// Host: read the answer from here. Viewer: write it here.
    pub answer_file: Option<PathBuf>,
}

/// Gives our text to the user: on the clipboard, or into `file`.
pub fn hand_over(envelope: &Envelope, file: Option<&Path>, what: &str) -> io::Result<()> {
    let text = invite::encode(envelope)?;
    tracing::info!(what, characters = text.len(), "manual text ready");
    match file {
        Some(path) => {
            // Written next to it, then renamed: the other side never reads half.
            let temporary = path.with_extension("tmp");
            std::fs::write(&temporary, &text)?;
            std::fs::rename(&temporary, path)?;
            println!(
                "{what} written to {} ({} characters)",
                path.display(),
                text.len()
            );
        }
        None => {
            clipboard::set_text(&text)?;
            println!(
                "{what} copied to the clipboard ({} characters): paste it to the other person",
                text.len()
            );
        }
    }
    Ok(())
}

/// The other side's text, decoded: waits for `file`, or for the user to copy
/// it and press Enter. Retries on text that is not an invite (the user may
/// have copied something else) until the invite's lifetime runs out.
pub fn take(file: Option<&Path>, what: &str) -> io::Result<Envelope> {
    let deadline = Instant::now() + Duration::from_secs(MANUAL_LIFETIME_SECS);
    loop {
        if Instant::now() > deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no {what} within {} minutes", MANUAL_LIFETIME_SECS / 60),
            ));
        }
        let text = match file {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Err(e) => return Err(e),
            },
            None => {
                print!("copy the {what} you received, then press Enter: ");
                io::stdout().flush()?;
                if read_line()?.is_none() {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no input"));
                }
                clipboard::get_text()?.unwrap_or_default()
            }
        };
        match invite::decode(&text) {
            Ok(envelope) => return Ok(envelope),
            Err(error) if file.is_some() => return Err(io::Error::other(error)),
            Err(error) => println!("{error}"),
        }
    }
}

/// Asks before trusting a key that is not a contact (saving it on a yes).
/// `preset` keys (from the command line) count as a yes.
pub fn trust(
    contacts: &mut Contacts,
    preset: &[PublicKey],
    key: &PublicKey,
    claimed_name: &str,
) -> io::Result<bool> {
    let name = clean_name(claimed_name);
    if let Some(contact) = contacts.find(key) {
        println!("'{}' is a contact (key {key})", contact.name);
        return Ok(true);
    }
    if !preset.contains(key) {
        println!("'{name}' is not a contact yet. Key: {key}");
        println!("If you can, compare the start of the key with them (call or message).");
        print!("Trust this key? [y/N] ");
        io::stdout().flush()?;
        let answer = read_line()?.unwrap_or_default();
        if !matches!(
            answer.trim().to_lowercase().as_str(),
            "y" | "yes" | "s" | "sim"
        ) {
            return Ok(false);
        }
    }
    contacts.add(key, &name)?;
    println!("saved '{name}' as a contact");
    Ok(true)
}

/// The name a peer gives itself is just text it chose: shortened and
/// stripped of control characters before it is shown or saved.
pub fn clean_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    if clean.trim().is_empty() {
        "unnamed".to_owned()
    } else {
        clean.trim().to_owned()
    }
}

/// One line from the console; `None` at end of input.
fn read_line() -> io::Result<Option<String>> {
    let mut line = String::new();
    let n = io::stdin().lock().read_line(&mut line)?;
    Ok((n > 0).then_some(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  daniel "), "daniel");
        assert_eq!(clean_name("evil\x1b[2Jname\r\n"), "evil[2Jname");
        assert_eq!(clean_name(""), "unnamed");
        assert_eq!(clean_name(&"x".repeat(200)).len(), 64);
    }
}
