//! Processes for app audio: the process behind a window, the process tree,
//! and which processes to capture when sharing everything but a few apps.
//!
//! Process loopback capture always takes a process together with all its
//! descendants. So, sharing "everything except Discord and ziz":
//! - a process inside an excluded tree is never captured;
//! - a process whose tree holds an excluded one is not captured as a whole
//!   (that would capture the excluded one too); its other descendants that
//!   play sound are captured one by one instead;
//! - a process whose ancestor is already captured is skipped (its sound is
//!   already in the ancestor's capture: no doubling).

use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};
use windows::core::BOOL;

/// Apps whose sound is left out when sharing the whole desktop: the voice
/// chat friends are in (they must not hear themselves).
pub const DEFAULT_EXCLUDED: [&str; 3] = ["Discord.exe", "DiscordPTB.exe", "DiscordCanary.exe"];

/// Walking up the tree stops after this many steps (process ids are reused,
/// so parent links can form cycles).
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub parent: u32,
    /// Executable file name, e.g. `Discord.exe`.
    pub name: String,
}

pub type ProcessTable = HashMap<u32, ProcessInfo>;

/// Every running process, by id.
pub fn processes() -> std::io::Result<ProcessTable> {
    // SAFETY: a snapshot handle owned here and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(std::io::Error::other)?;
    let mut table = ProcessTable::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry` is a writable PROCESSENTRY32W with `dwSize` set, as
    // the walk requires; the snapshot is valid until closed.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let length = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        table.insert(
            entry.th32ProcessID,
            ProcessInfo {
                parent: entry.th32ParentProcessID,
                name: String::from_utf16_lossy(&entry.szExeFile[..length]),
            },
        );
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    // SAFETY: closes the snapshot opened above, once.
    let _ = unsafe { CloseHandle(snapshot) };
    Ok(table)
}

/// The ancestors of `pid`, nearest first (stops at unknown ids, cycles and
/// `MAX_DEPTH`).
fn ancestors(pid: u32, table: &ProcessTable) -> Vec<u32> {
    let mut out = Vec::new();
    let mut current = pid;
    while let Some(info) = table.get(&current) {
        let parent = info.parent;
        if parent == 0 || parent == pid || out.contains(&parent) || out.len() >= MAX_DEPTH {
            break;
        }
        out.push(parent);
        current = parent;
    }
    out
}

/// Whether `pid` or one of its ancestors is in `set`.
fn in_tree_of(pid: u32, set: &HashSet<u32>, table: &ProcessTable) -> bool {
    set.contains(&pid) || ancestors(pid, table).iter().any(|a| set.contains(a))
}

/// Which processes to capture (each with its tree) for "every sound except
/// `excluded` names and the `own` process": from the processes that have an
/// audio session (`sounding`).
pub fn capture_targets(
    sounding: &[u32],
    table: &ProcessTable,
    own: u32,
    excluded: &[String],
) -> Vec<u32> {
    // Roots of the trees that must never be captured.
    let mut banned: HashSet<u32> = table
        .iter()
        .filter(|(_, info)| {
            excluded
                .iter()
                .any(|name| name.eq_ignore_ascii_case(&info.name))
        })
        .map(|(pid, _)| *pid)
        .collect();
    banned.insert(own);
    // Processes whose tree holds a banned one: capturing them would take it.
    let mut holds_banned: HashSet<u32> = HashSet::new();
    for pid in &banned {
        holds_banned.extend(ancestors(*pid, table));
    }
    let candidates: HashSet<u32> = sounding
        .iter()
        .copied()
        .filter(|pid| *pid != 0 && table.contains_key(pid))
        .filter(|pid| !in_tree_of(*pid, &banned, table))
        .filter(|pid| !holds_banned.contains(pid))
        .collect();
    let mut targets: Vec<u32> = candidates
        .iter()
        .copied()
        .filter(|pid| {
            !ancestors(*pid, table)
                .iter()
                .any(|a| candidates.contains(a))
        })
        .collect();
    targets.sort_unstable();
    targets
}

/// The process to capture for a window. Store apps' windows belong to
/// `ApplicationFrameHost.exe`; the app itself owns a child window.
pub fn window_process(window: isize, table: &ProcessTable) -> Option<u32> {
    let hwnd = HWND(window as *mut _);
    let mut pid = 0_u32;
    // SAFETY: plain query; an invalid handle leaves `pid` at 0.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let is_frame_host = table
        .get(&pid)
        .is_some_and(|p| p.name.eq_ignore_ascii_case("ApplicationFrameHost.exe"));
    if !is_frame_host {
        return Some(pid);
    }

    struct Search {
        host: u32,
        found: Option<u32>,
    }
    unsafe extern "system" fn visit(child: HWND, state: LPARAM) -> BOOL {
        // SAFETY: `state` is the address of the `Search` below, alive during
        // the synchronous EnumChildWindows call.
        let search = unsafe { &mut *(state.0 as *mut Search) };
        let mut pid = 0_u32;
        // SAFETY: plain query on a handle supplied by EnumChildWindows.
        unsafe { GetWindowThreadProcessId(child, Some(&mut pid)) };
        if pid != 0 && pid != search.host {
            search.found = Some(pid);
            return false.into();
        }
        true.into()
    }
    let mut search = Search {
        host: pid,
        found: None,
    };
    // SAFETY: the callback only dereferences `search` during this call; the
    // result only says whether the walk stopped early.
    let _ = unsafe {
        EnumChildWindows(
            Some(hwnd),
            Some(visit),
            LPARAM(&mut search as *mut Search as isize),
        )
    };
    Some(search.found.unwrap_or(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table from (pid, parent, name) rows.
    fn table(rows: &[(u32, u32, &str)]) -> ProcessTable {
        rows.iter()
            .map(|&(pid, parent, name)| {
                (
                    pid,
                    ProcessInfo {
                        parent,
                        name: name.into(),
                    },
                )
            })
            .collect()
    }

    fn discord() -> Vec<String> {
        DEFAULT_EXCLUDED.iter().map(|s| (*s).to_owned()).collect()
    }

    /// explorer (10) started a game (20), Chrome (30) with an audio child
    /// (31), Discord (40) with its voice child (41), and ziz (50) which
    /// plays a friend's stream.
    fn desktop() -> ProcessTable {
        table(&[
            (4, 0, "System"),
            (10, 4, "explorer.exe"),
            (20, 10, "game.exe"),
            (30, 10, "chrome.exe"),
            (31, 30, "chrome.exe"),
            (40, 10, "Discord.exe"),
            (41, 40, "Discord.exe"),
            (50, 10, "ziz.exe"),
        ])
    }

    #[test]
    fn excluded_apps_and_ziz_are_never_captured() {
        let t = desktop();
        assert_eq!(
            capture_targets(&[20, 31, 41, 50], &t, 50, &discord()),
            vec![20, 31]
        );
    }

    #[test]
    fn names_match_without_case() {
        let t = table(&[(1, 0, "x"), (2, 1, "DISCORD.EXE"), (3, 1, "app.exe")]);
        assert_eq!(capture_targets(&[2, 3], &t, 99, &discord()), vec![3]);
    }

    #[test]
    fn a_process_whose_tree_holds_an_excluded_one_is_split() {
        // explorer itself plays a sound: capturing its tree would take
        // Discord and ziz, so its sounding descendants go one by one.
        let t = desktop();
        assert_eq!(
            capture_targets(&[10, 20, 31, 41], &t, 50, &discord()),
            vec![20, 31]
        );
    }

    #[test]
    fn a_descendant_of_a_captured_process_is_not_captured_twice() {
        let t = desktop();
        assert_eq!(capture_targets(&[30, 31], &t, 50, &discord()), vec![30]);
    }

    #[test]
    fn children_of_excluded_apps_stay_out_whatever_their_name() {
        let t = table(&[
            (1, 0, "explorer.exe"),
            (2, 1, "Discord.exe"),
            (3, 2, "helper.exe"),
        ]);
        assert_eq!(capture_targets(&[3], &t, 99, &discord()), Vec::<u32>::new());
    }

    #[test]
    fn children_of_ziz_stay_out() {
        let t = table(&[
            (1, 0, "explorer.exe"),
            (2, 1, "ziz.exe"),
            (3, 2, "child.exe"),
            (4, 1, "app.exe"),
        ]);
        assert_eq!(capture_targets(&[3, 4], &t, 2, &discord()), vec![4]);
    }

    #[test]
    fn gone_processes_and_the_system_session_are_skipped() {
        let t = desktop();
        assert_eq!(capture_targets(&[0, 999, 20], &t, 50, &discord()), vec![20]);
    }

    #[test]
    fn parent_cycles_from_reused_ids_end() {
        let t = table(&[(1, 2, "a.exe"), (2, 1, "b.exe"), (3, 1, "c.exe")]);
        assert_eq!(ancestors(3, &t), vec![1, 2]);
        assert_eq!(capture_targets(&[1, 3], &t, 99, &[]), vec![1]);
    }

    #[test]
    fn extra_exclusions_are_honoured() {
        let t = desktop();
        let mut excluded = discord();
        excluded.push("chrome.exe".into());
        assert_eq!(capture_targets(&[20, 31], &t, 50, &excluded), vec![20]);
    }

    #[test]
    fn this_pc_lists_this_process() {
        let t = processes().unwrap();
        let me = t.get(&std::process::id()).expect("this process is listed");
        assert!(me.name.to_lowercase().ends_with(".exe"));
    }
}
