// SPDX-License-Identifier: MIT
//! The arguments a display capture may ask for and the checks that refuse the rest (AOSP CaptureArgs / DisplayCaptureArgs). Plain values, so the table runs without binder types:
//! the converter build maps the AIDL structs to [`Args`] (service.rs), and the reply is capture_reply.rs.

use atl_gralloc::format::RGBA_8888;

/// The capture arguments the composer reads, as plain values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Args {
    pub pixel_format: i32,
    pub frame_scale: (f32, f32),
    /// sourceCrop is set and not empty.
    pub cropped: bool,
    pub secure_layers: bool,
    pub dataspace: i32,
    pub grayscale: bool,
    /// excludeHandles names at least one layer.
    pub excludes_layers: bool,
    /// DisplayCaptureArgs width and height; (0, 0) = the display's.
    pub size: (i32, i32),
}

/// Checks `args` against what this composer captures, for a display of `display` size. Refuses what it cannot honour instead of ignoring it.
/// Returns the reason a refusal gives in the log.
pub fn check(args: &Args, display: (i32, i32)) -> Result<(), &'static str> {
    if args.pixel_format != RGBA_8888 {
        return Err("pixelFormat is not RGBA_8888");
    }
    if args.frame_scale != (1.0, 1.0) {
        return Err("frameScale is not 1.0");
    }
    if args.cropped {
        return Err("sourceCrop is not empty");
    }
    if args.secure_layers {
        return Err("captureSecureLayers is not supported");
    }
    if args.dataspace != 0 {
        return Err("dataspace is not the default");
    }
    if args.grayscale {
        return Err("grayscale is not supported");
    }
    if args.excludes_layers {
        return Err("excludeHandles is not supported");
    }
    if args.size != (0, 0) && args.size != display {
        return Err("width x height is neither 0 nor the display's size");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display this table checks against: any size; the defaults and the one matching size pass.
    const DISPLAY: (i32, i32) = (1080, 2400);

    /// CaptureArgs.aidl's defaults: pixelFormat RGBA_8888, frameScale 1.0, sourceCrop empty, no secure layers, dataspace 0, no grayscale, no excluded layers.
    fn defaults() -> Args {
        Args {
            pixel_format: RGBA_8888,
            frame_scale: (1.0, 1.0),
            cropped: false,
            secure_layers: false,
            dataspace: 0,
            grayscale: false,
            excludes_layers: false,
            size: (0, 0),
        }
    }

    #[test]
    fn checks_each_argument() {
        let cases: [(&str, Args, Result<(), &str>); 10] = [
            ("defaults", defaults(), Ok(())),
            (
                "display size",
                Args {
                    size: DISPLAY,
                    ..defaults()
                },
                Ok(()),
            ),
            (
                "other size",
                Args {
                    size: (1, 1),
                    ..defaults()
                },
                Err("width x height is neither 0 nor the display's size"),
            ),
            (
                "pixel format",
                Args {
                    pixel_format: 2,
                    ..defaults()
                },
                Err("pixelFormat is not RGBA_8888"),
            ),
            (
                "frame scale",
                Args {
                    frame_scale: (0.5, 1.0),
                    ..defaults()
                },
                Err("frameScale is not 1.0"),
            ),
            (
                "crop",
                Args {
                    cropped: true,
                    ..defaults()
                },
                Err("sourceCrop is not empty"),
            ),
            (
                "secure",
                Args {
                    secure_layers: true,
                    ..defaults()
                },
                Err("captureSecureLayers is not supported"),
            ),
            (
                "dataspace",
                Args {
                    dataspace: 1 << 16,
                    ..defaults()
                },
                Err("dataspace is not the default"),
            ),
            (
                "grayscale",
                Args {
                    grayscale: true,
                    ..defaults()
                },
                Err("grayscale is not supported"),
            ),
            (
                "excludes",
                Args {
                    excludes_layers: true,
                    ..defaults()
                },
                Err("excludeHandles is not supported"),
            ),
        ];
        for (name, args, want) in cases {
            assert_eq!(check(&args, DISPLAY), want, "{name}");
        }
    }
}
