//! Which of a camera's formats to use: the one closest to what was asked
//! for (size first, then frame rate), and among equals the cheapest pixel
//! format to bring to the GPU.

use serde::Serialize;

/// How a camera lays out a frame's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum PixelFormat {
    /// 4:2:0: a luma plane, then interleaved chroma at half size.
    Nv12,
    /// 4:2:2 packed: Y0 U Y1 V for each two pixels.
    Yuy2,
    /// 8-bit B, G, R and an unused byte.
    Rgb32,
    /// 8-bit B, G, R.
    Rgb24,
    /// Compressed; Media Foundation decodes it on the CPU (the costly one).
    Mjpeg,
}

impl PixelFormat {
    /// Preference among formats of the same size and rate: fewer bytes to
    /// copy, and nothing to decode on the CPU.
    fn rank(self) -> u8 {
        match self {
            PixelFormat::Nv12 => 0,
            PixelFormat::Yuy2 => 1,
            PixelFormat::Rgb32 => 2,
            PixelFormat::Rgb24 => 3,
            PixelFormat::Mjpeg => 4,
        }
    }
}

/// One way a camera can deliver frames.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CameraMode {
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// What we would like from a camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraRequest {
    /// 720p30: enough for a face, and cheap.
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
        }
    }
}

/// The mode closest to `request`, or `None` if there are none.
///
/// Size comes first: the smallest at least as big as asked, or else the
/// biggest below it. Then the rate: the lowest at least the one asked for
/// (a rate within 1 fps counts, since cameras say 29.97), or else the
/// highest below it. Then the pixel format (see `PixelFormat::rank`). So a
/// camera that only gives 720p at 30 fps as MJPEG and 10 fps as YUY2 gets
/// MJPEG: the rate matters more than the decoding cost.
pub fn choose(modes: &[CameraMode], request: CameraRequest) -> Option<CameraMode> {
    let wanted_area = u64::from(request.width) * u64::from(request.height);
    let wanted_fps = f64::from(request.fps) - 1.0;
    modes
        .iter()
        .copied()
        .filter(|m| m.width > 0 && m.height > 0 && m.fps > 0.0)
        .min_by_key(|m| {
            let area = u64::from(m.width) * u64::from(m.height);
            let big_enough = m.width >= request.width && m.height >= request.height;
            let size = if big_enough {
                (0, area - wanted_area.min(area))
            } else {
                (1, u64::MAX - area)
            };
            // Hundredths of a frame per second, as an integer to sort by.
            let centi = |fps: f64| (fps * 100.0).round() as u64;
            let rate = if m.fps >= wanted_fps {
                (0, centi(m.fps))
            } else {
                (1, u64::MAX - centi(m.fps))
            };
            (size, rate, m.format.rank())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(format: PixelFormat, width: u32, height: u32, fps: f64) -> CameraMode {
        CameraMode {
            format,
            width,
            height,
            fps,
        }
    }

    const HD: CameraRequest = CameraRequest {
        width: 1280,
        height: 720,
        fps: 30,
    };

    #[test]
    fn the_iriun_gives_720p30_yuy2() {
        // What the Iriun Webcam reported (G1a): YUY2 and RGB24 at several
        // sizes, 30 and 60 fps.
        let mut modes = Vec::new();
        for (w, h) in [(640, 360), (1280, 720), (1920, 1080), (3840, 2160)] {
            for fps in [30.0, 60.0] {
                for format in [PixelFormat::Rgb24, PixelFormat::Yuy2] {
                    modes.push(mode(format, w, h, fps));
                }
            }
        }
        assert_eq!(
            choose(&modes, HD),
            Some(mode(PixelFormat::Yuy2, 1280, 720, 30.0))
        );
    }

    #[test]
    fn a_bigger_size_beats_a_smaller_one_and_the_closest_wins() {
        let modes = [
            mode(PixelFormat::Yuy2, 640, 480, 30.0),
            mode(PixelFormat::Yuy2, 3840, 2160, 30.0),
            mode(PixelFormat::Yuy2, 1920, 1080, 30.0),
        ];
        assert_eq!(choose(&modes, HD).unwrap().width, 1920);
        // Nothing big enough: the biggest there is.
        let small = [
            mode(PixelFormat::Yuy2, 320, 240, 30.0),
            mode(PixelFormat::Yuy2, 640, 480, 30.0),
        ];
        assert_eq!(choose(&small, HD).unwrap().width, 640);
    }

    #[test]
    fn the_rate_matters_more_than_decoding() {
        let modes = [
            mode(PixelFormat::Yuy2, 1280, 720, 10.0),
            mode(PixelFormat::Mjpeg, 1280, 720, 30.0),
        ];
        assert_eq!(choose(&modes, HD).unwrap().format, PixelFormat::Mjpeg);
        // At the same rate, no decoding.
        let modes = [
            mode(PixelFormat::Mjpeg, 1280, 720, 30.0),
            mode(PixelFormat::Rgb24, 1280, 720, 30.0),
            mode(PixelFormat::Nv12, 1280, 720, 30.0),
        ];
        assert_eq!(choose(&modes, HD).unwrap().format, PixelFormat::Nv12);
    }

    #[test]
    fn rates_close_to_the_request_count_and_too_slow_takes_the_fastest() {
        let modes = [
            mode(PixelFormat::Yuy2, 1280, 720, 60.0),
            mode(PixelFormat::Yuy2, 1280, 720, 29.97),
        ];
        assert_eq!(choose(&modes, HD).unwrap().fps, 29.97);
        let slow = [
            mode(PixelFormat::Yuy2, 1280, 720, 5.0),
            mode(PixelFormat::Yuy2, 1280, 720, 15.0),
        ];
        assert_eq!(choose(&slow, HD).unwrap().fps, 15.0);
    }

    #[test]
    fn nothing_to_choose_from() {
        assert_eq!(choose(&[], HD), None);
        assert_eq!(choose(&[mode(PixelFormat::Yuy2, 0, 0, 30.0)], HD), None);
    }
}
