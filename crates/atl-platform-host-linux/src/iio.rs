// SPDX-License-Identifier: MIT
//! Accelerometers from Linux IIO sysfs (kernel Documentation/ABI/testing/sysfs-bus-iio) plus their udev properties, read on demand with std::fs: no D-Bus, no toolkit.
//! iio-sensor-proxy is not used: it publishes only orientation labels, no raw values. Its rules are followed exactly, so a machine its hwdb quirks fix reads right here too (iio-sensor-proxy ca0d5b0).
//! A device counts as an accelerometer when it has `in_accel_{x,y,z}_raw`. One axis value = (raw + offset) x scale, m/s² (ABI `in_accel_x_raw`: "Units after application of scale and offset are m/s^2"); per-axis `in_accel_<a>_scale` / `_offset` win over the shared ones, offset absent = 0.
//! Frame, in order:
//! 1. mount matrix -> Linux canonical frame. Source precedence of accel-mount-matrix.c `setup_mount_matrix`: udev `ACCEL_MOUNT_MATRIX` (systemd 60-sensor.hwdb; its header: x/y must follow the display) -> sysfs `mount_matrix` -> `in_accel_mount_matrix` -> `in_mount_matrix` -> identity; "" = identity.
//!    Text "x1, y1, z1; x2, y2, z2; x3, y3, z3", applied as rows: out r = row r . sensor vector (`apply_mount_matrix`, accel-mount-matrix.c:180-182).
//! 2. canonical -> Android, one fixed conversion [`to_android`].
//!
//! Location (accel-attributes.c `setup_accel_location`): udev `ACCEL_LOCATION` (""/display/lid = display, base) -> sysfs `label` (accel-display / accel-base) -> sysfs `location` (as the property) -> display. A `base` device is not listed: Android's
//! TYPE_ACCELEROMETER is the device's own frame (the display's, SensorEvent docs), and a laptop base's reading equals that only at one hinge angle; without a hinge-angle sensor there is nothing to compensate with, so no accelerometer beats a wrong one.
//! udev properties come from the udev database `/run/udev/data/c<major>:<minor>` (`E:KEY=VALUE` lines; the device number from sysfs `dev`), the file libudev itself reads: no libudev link, so this crate stays free of `unsafe`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use atl_platform_host_api::{Accelerometer, SensorError, Sensors};

/// Where the kernel lists IIO devices; the one product path (a test environment supplies another sysfs by mount namespace, not by a parameter).
const IIO_DEVICES: &str = "/sys/bus/iio/devices";
/// systemd-udevd's device database; same rule as [`IIO_DEVICES`].
const UDEV_DATA: &str = "/run/udev/data";
const AXES: [&str; 3] = ["x", "y", "z"];
/// Read interval when the device states no sampling frequency: 100 Hz, Android's SENSOR_DELAY_FASTEST class for a phone accelerometer, and 3 small sysfs reads per sample stay cheap at it.
const DEFAULT_PERIOD: Duration = Duration::from_millis(10);
/// Storage bits assumed when `scan_elements/in_accel_x_type` is absent: 16 is the widest common accelerometer word, so the range is an over-estimate, never a clip.
const DEFAULT_REAL_BITS: u32 = 16;
const IDENTITY: [[f64; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Linux canonical frame -> Android SensorEvent frame. Per axis, gravity as each frame reports it:
/// - upright: canonical {0, -g, 0} (iio-sensor-proxy test-orientation.c:27 "normal"), Android y = +g -> y = -y;
/// - right edge up: canonical {-g, 0, 0} (test-orientation.c:28), Android x = +g -> x = -x;
/// - flat face-up: canonical z > 0 (orientation.c:110-120), Android z = +g -> z = z.
fn to_android(c: [f64; 3]) -> [f64; 3] {
    [-c[0], -c[1], c[2]]
}

/// The machine's IIO accelerometers.
#[derive(Debug, Clone)]
pub struct IioSensors {
    root: PathBuf,
    udev: PathBuf,
}

impl Default for IioSensors {
    fn default() -> Self {
        IioSensors {
            root: PathBuf::from(IIO_DEVICES),
            udev: PathBuf::from(UDEV_DATA),
        }
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

fn read_f64(path: &Path) -> Result<Option<f64>, SensorError> {
    match read_text(path) {
        None => Ok(None),
        Some(t) => t
            .parse()
            .map(Some)
            .map_err(|_| SensorError(format!("{}: not a number: {t:?}", path.display()))),
    }
}

/// `x1, y1, z1; x2, y2, z2; x3, y3, z3` as rows; "" is the identity (accel-mount-matrix.c `parse_mount_matrix`).
fn parse_matrix(text: &str) -> Option<[[f64; 3]; 3]> {
    if text.is_empty() {
        return Some(IDENTITY);
    }
    let mut m = [[0.0; 3]; 3];
    let rows: Vec<&str> = text.split(';').collect();
    if rows.len() != 3 {
        return None;
    }
    for (r, row) in rows.iter().enumerate() {
        let cols: Vec<&str> = row.split(',').collect();
        if cols.len() != 3 {
            return None;
        }
        for (c, v) in cols.iter().enumerate() {
            m[r][c] = v.trim().parse().ok()?;
        }
    }
    Some(m)
}

/// Real bits of a scan element type `[be|le]:[s|u]bits/storagebits[>>shift]`.
fn real_bits(text: &str) -> Option<u32> {
    let (_, rest) = text.split_once(':')?;
    let bits = rest.get(1..)?.split('/').next()?;
    bits.parse().ok()
}

/// Where an accelerometer sits (iio-sensor-proxy `AccelLocation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Display,
    Base,
}

/// `ACCEL_LOCATION` / sysfs `location` (accel-attributes.c `parse_accel_location`); `None` = unparsable, the next source decides.
fn parse_location(text: &str) -> Option<Location> {
    match text {
        "" | "display" | "lid" => Some(Location::Display),
        "base" => Some(Location::Base),
        _ => None,
    }
}

/// sysfs `label` (`parse_accel_label`).
fn parse_label(text: &str) -> Option<Location> {
    match text {
        "accel-display" => Some(Location::Display),
        "accel-base" => Some(Location::Base),
        _ => None,
    }
}

impl IioSensors {
    fn device(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn is_accel(dir: &Path) -> bool {
        AXES.iter()
            .all(|a| dir.join(format!("in_accel_{a}_raw")).is_file())
    }

    /// The device's udev properties (`E:` lines of its database entry); empty when udev has none or the device has no `dev` number.
    fn udev_properties(&self, dir: &Path) -> HashMap<String, String> {
        let Some(dev) = read_text(&dir.join("dev")) else {
            return HashMap::new();
        };
        let Some(db) = read_text(&self.udev.join(format!("c{dev}"))) else {
            return HashMap::new();
        };
        db.lines()
            .filter_map(|l| l.strip_prefix("E:"))
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    fn location(&self, dir: &Path) -> Location {
        let props = self.udev_properties(dir);
        props
            .get("ACCEL_LOCATION")
            .and_then(|v| parse_location(v))
            .or_else(|| read_text(&dir.join("label")).and_then(|v| parse_label(&v)))
            .or_else(|| read_text(&dir.join("location")).and_then(|v| parse_location(&v)))
            .unwrap_or(Location::Display)
    }

    /// The per-axis attribute, else the shared one.
    fn axis_attr(dir: &Path, axis: &str, attr: &str) -> Result<Option<f64>, SensorError> {
        match read_f64(&dir.join(format!("in_accel_{axis}_{attr}")))? {
            Some(v) => Ok(Some(v)),
            None => read_f64(&dir.join(format!("in_accel_{attr}"))),
        }
    }

    /// The first source that is present and parses wins; one that is present but unparsable is skipped, as iio-sensor-proxy warns and falls through.
    fn matrix(&self, dir: &Path) -> [[f64; 3]; 3] {
        let udev = self.udev_properties(dir).remove("ACCEL_MOUNT_MATRIX");
        let sysfs = ["mount_matrix", "in_accel_mount_matrix", "in_mount_matrix"]
            .iter()
            .map(|n| read_text(&dir.join(n)));
        std::iter::once(udev)
            .chain(sysfs)
            .flatten()
            .find_map(|t| parse_matrix(&t))
            .unwrap_or(IDENTITY)
    }

    fn scale(dir: &Path, axis: &str) -> Result<f64, SensorError> {
        Self::axis_attr(dir, axis, "scale")?
            .ok_or_else(|| SensorError(format!("{}: no in_accel_scale", dir.display())))
    }

    fn describe(&self, id: &str, dir: &Path) -> Result<Accelerometer, SensorError> {
        let scale = Self::scale(dir, "x")?;
        let bits = read_text(&dir.join("scan_elements/in_accel_x_type"))
            .and_then(|t| real_bits(&t))
            .unwrap_or(DEFAULT_REAL_BITS);
        let hz = match read_f64(&dir.join("in_accel_sampling_frequency"))? {
            Some(f) => Some(f),
            None => read_f64(&dir.join("sampling_frequency"))?,
        };
        let min_period = hz
            .filter(|f| *f > 0.0)
            .map(|f| Duration::from_secs_f64(1.0 / f))
            .unwrap_or(DEFAULT_PERIOD);
        Ok(Accelerometer {
            id: id.to_owned(),
            name: read_text(&dir.join("name")).unwrap_or_else(|| id.to_owned()),
            max_range: (scale * f64::from(1u32 << (bits.clamp(1, 32) - 1))) as f32,
            resolution: scale as f32,
            min_period,
        })
    }
}

impl Sensors for IioSensors {
    /// Display devices in name order; one that fails to describe is left out (it cannot be read either).
    fn accelerometers(&self) -> Vec<Accelerometer> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("iio:device"))
            .filter(|n| Self::is_accel(&self.device(n)))
            .filter(|n| self.location(&self.device(n)) == Location::Display)
            .collect();
        ids.sort();
        ids.iter()
            .filter_map(|id| self.describe(id, &self.device(id)).ok())
            .collect()
    }

    fn read_accel(&self, id: &str) -> Result<[f32; 3], SensorError> {
        let dir = self.device(id);
        let mut v = [0.0f64; 3];
        for (i, a) in AXES.iter().enumerate() {
            let path = dir.join(format!("in_accel_{a}_raw"));
            let raw = read_f64(&path)?
                .ok_or_else(|| SensorError(format!("{}: unreadable", path.display())))?;
            let offset = Self::axis_attr(&dir, a, "offset")?.unwrap_or(0.0);
            v[i] = (raw + offset) * Self::scale(&dir, a)?;
        }
        let m = self.matrix(&dir);
        let mut canonical = [0.0f64; 3];
        for (r, row) in m.iter().enumerate() {
            canonical[r] = row[0] * v[0] + row[1] * v[1] + row[2] * v[2];
        }
        Ok(to_android(canonical).map(|x| x as f32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake sysfs device directory per `(name, files)` plus a fake udev database; device `iio:device<n>` gets `dev` = `250:<n>`.
    struct Fake(PathBuf);

    /// One device's sysfs files and its udev entry.
    type Device<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>);

    impl Fake {
        fn new(tag: &str, devices: &[Device<'_>]) -> Fake {
            let root = std::env::temp_dir().join(format!("atl-iio-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("udev")).unwrap();
            for (n, (files, udev)) in devices.iter().enumerate() {
                let dev = root.join(format!("sys/iio:device{n}"));
                std::fs::create_dir_all(dev.join("scan_elements")).unwrap();
                std::fs::write(dev.join("dev"), format!("250:{n}\n")).unwrap();
                for (f, t) in *files {
                    std::fs::write(dev.join(f), format!("{t}\n")).unwrap();
                }
                if let Some(db) = udev {
                    std::fs::write(root.join(format!("udev/c250:{n}")), db).unwrap();
                }
            }
            Fake(root)
        }
        fn sensors(&self) -> IioSensors {
            IioSensors {
                root: self.0.join("sys"),
                udev: self.0.join("udev"),
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const RAW: [(&str, &str); 4] = [
        ("in_accel_x_raw", "100"),
        ("in_accel_y_raw", "-200"),
        ("in_accel_z_raw", "1000"),
        ("in_accel_scale", "0.01"),
    ];

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn identity_without_matrix() {
        let f = Fake::new("id", &[(&RAW, None)]);
        let s = f.sensors();
        let list = s.accelerometers();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].min_period, DEFAULT_PERIOD);
        // raw x 0.01 = canonical (1, -2, 10); Android negates x and y
        assert!(close(
            s.read_accel("iio:device0").unwrap(),
            [-1.0, 2.0, 10.0]
        ));
    }

    #[test]
    fn matrix_rows_map_sensor_to_device() {
        let mut files = RAW.to_vec();
        // canonical x = sensor y, canonical y = -sensor x
        files.push(("in_accel_mount_matrix", "0, 1, 0; -1, 0, 0; 0, 0, 1"));
        files.push(("in_accel_offset", "10"));
        files.push(("in_accel_sampling_frequency", "50"));
        files.push(("scan_elements/in_accel_x_type", "le:s12/16>>4"));
        let f = Fake::new("mm", &[(&files, None)]);
        let s = f.sensors();
        // sensor = ((100+10), (-200+10), (1000+10)) x 0.01 = (1.1, -1.9, 10.1) -> canonical (-1.9, -1.1, 10.1) -> Android (1.9, 1.1, 10.1)
        assert!(close(
            s.read_accel("iio:device0").unwrap(),
            [1.9, 1.1, 10.1]
        ));
        let a = &s.accelerometers()[0];
        assert_eq!(a.min_period, Duration::from_millis(20));
        // 2^(12-1) steps of 0.01
        assert!((a.max_range - 20.48).abs() < 1e-4);
    }

    /// The owner's laptop (TP3607SH, HID accel_3d), measured 2026-11-06, with hwdb `ACCEL_MOUNT_MATRIX=1, 0, 0; 0, 1, 0; 0, 0, -1`. Raw readings at scale 0.01 and the Android values by hand:
    /// canonical = (x, y, -z), Android = (-x, -y, -z) of the raw. Upright: y = +g, z > 0 (the lid leans back). Flat screen-up: z = +g. Left edge down = right edge up: x = +g.
    #[test]
    fn laptop_readings_reach_the_android_frame() {
        let cases: [([&str; 3], [f32; 3]); 3] = [
            (["-6", "-871", "-374"], [0.06, 8.71, 3.74]),
            (["14", "63", "-1019"], [-0.14, -0.63, 10.19]),
            (["-980", "62", "-50"], [9.80, -0.62, 0.50]),
        ];
        let db = "E:ACCEL_MOUNT_MATRIX=1, 0, 0; 0, 1, 0; 0, 0, -1\nE:ACCEL_LOCATION=display\n";
        for (raw, want) in cases {
            let files = [
                ("in_accel_x_raw", raw[0]),
                ("in_accel_y_raw", raw[1]),
                ("in_accel_z_raw", raw[2]),
                ("in_accel_scale", "0.01"),
                // the hwdb property beats every sysfs matrix
                ("in_accel_mount_matrix", "0, 1, 0; 1, 0, 0; 0, 0, 1"),
            ];
            let f = Fake::new("laptop", &[(&files, Some(db))]);
            let got = f.sensors().read_accel("iio:device0").unwrap();
            assert!(close(got, want), "{raw:?}: got {got:?}, want {want:?}");
        }
    }

    #[test]
    fn matrix_precedence_follows_iio_sensor_proxy() {
        // only in_mount_matrix (a swap of x and y): canonical (-2, 1, 10) -> Android (2, -1, 10)
        let mut files = RAW.to_vec();
        files.push(("in_mount_matrix", "0, 1, 0; 1, 0, 0; 0, 0, 1"));
        let f = Fake::new("inmm", &[(&files, None)]);
        assert!(close(
            f.sensors().read_accel("iio:device0").unwrap(),
            [2.0, -1.0, 10.0]
        ));
        // mount_matrix beats in_accel_mount_matrix and in_mount_matrix; an unparsable udev property falls through
        files.push(("in_accel_mount_matrix", "0, 1, 0; 1, 0, 0; 0, 0, 1"));
        files.push(("mount_matrix", "1, 0, 0; 0, 1, 0; 0, 0, -1"));
        let f = Fake::new("mm2", &[(&files, Some("E:ACCEL_MOUNT_MATRIX=bogus\n"))]);
        assert!(close(
            f.sensors().read_accel("iio:device0").unwrap(),
            [-1.0, 2.0, -10.0]
        ));
    }

    #[test]
    fn only_display_accelerometers_are_listed() {
        let label_base = [RAW.as_slice(), &[("label", "accel-base")]].concat();
        let location_base = [RAW.as_slice(), &[("location", "base")]].concat();
        // the property beats the label
        let prop_lid = [RAW.as_slice(), &[("label", "accel-base")]].concat();
        let f = Fake::new(
            "loc",
            &[
                (&RAW, Some("E:ACCEL_LOCATION=base\n")),
                (&RAW, Some("E:ACCEL_LOCATION=display\n")),
                (&label_base, None),
                (&location_base, None),
                (&prop_lid, Some("E:ACCEL_LOCATION=lid\n")),
                (&RAW, None),
            ],
        );
        let ids: Vec<String> = f
            .sensors()
            .accelerometers()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, ["iio:device1", "iio:device4", "iio:device5"]);
    }

    #[test]
    fn no_devices_is_empty() {
        let s = IioSensors {
            root: PathBuf::from("/nonexistent/atl-iio"),
            udev: PathBuf::from("/nonexistent/atl-udev"),
        };
        assert!(s.accelerometers().is_empty());
    }
}
