//! `bench stamps <file>`: reads the visual frame stamp of every frame in a
//! raw 8-bit luma file (e.g. a decoded video cropped to the stamp region:
//! `ffmpeg -i in.h264 -vf crop=1024:32:0:0 -pix_fmt gray -f rawvideo out.gray`)
//! and checks the sequence. Diagnostics only: frames are read on the CPU.

use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use telemetry::frame_code::{self, read_frame_stamp};

/// What the stamps of a decoded stream say about it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StampCheck {
    pub frames: u64,
    pub unreadable: u64,
    /// Expected frame ids that never appeared.
    pub missing: u64,
    /// Frames whose id was not larger than the previous one.
    pub out_of_order: u64,
    pub first_id: Option<u32>,
    pub last_id: Option<u32>,
}

/// Checks a sequence of decoded stamp ids (`None` = unreadable frame).
pub fn check_sequence(ids: impl IntoIterator<Item = Option<u32>>) -> StampCheck {
    let mut check = StampCheck::default();
    let mut previous: Option<u32> = None;
    for id in ids {
        check.frames += 1;
        let Some(id) = id else {
            check.unreadable += 1;
            continue;
        };
        check.first_id.get_or_insert(id);
        check.last_id = Some(id);
        match previous {
            Some(previous) if id <= previous => check.out_of_order += 1,
            Some(previous) => check.missing += u64::from(id - previous - 1),
            None => {}
        }
        previous = Some(id.max(previous.unwrap_or(0)));
    }
    check
}

pub fn run(path: &Path, width: u32, height: u32) -> Result<(), Box<dyn Error>> {
    if width < frame_code::CODE_WIDTH || height < frame_code::CODE_HEIGHT {
        return Err(format!(
            "frames must be at least {}x{} (the stamp region)",
            frame_code::CODE_WIDTH,
            frame_code::CODE_HEIGHT
        )
        .into());
    }
    let mut reader = BufReader::new(File::open(path)?);
    let mut frame = vec![0_u8; (width * height) as usize];
    let mut ids = Vec::new();
    loop {
        match reader.read_exact(&mut frame) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error.into()),
        }
        let stamp = read_frame_stamp(|x, y| frame[(y * width + x) as usize]);
        ids.push(stamp.ok().map(|stamp| stamp.frame_id));
    }
    let check = check_sequence(ids);
    println!(
        "stamps: {} frames, {} unreadable, {} missing, {} out of order, ids {:?}..{:?}",
        check.frames,
        check.unreadable,
        check.missing,
        check.out_of_order,
        check.first_id,
        check.last_id
    );
    if check.unreadable + check.missing + check.out_of_order > 0 {
        return Err("stamp sequence has problems".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_sequence() {
        let check = check_sequence((0..10).map(Some));
        assert_eq!(
            check,
            StampCheck {
                frames: 10,
                first_id: Some(0),
                last_id: Some(9),
                ..Default::default()
            }
        );
    }

    #[test]
    fn gaps_repeats_and_unreadable_frames_are_counted() {
        let check = check_sequence([Some(0), Some(1), Some(4), None, Some(4), Some(3), Some(5)]);
        assert_eq!(check.frames, 7);
        assert_eq!(check.unreadable, 1);
        assert_eq!(check.missing, 2);
        assert_eq!(check.out_of_order, 2);
        assert_eq!((check.first_id, check.last_id), (Some(0), Some(5)));
    }
}
