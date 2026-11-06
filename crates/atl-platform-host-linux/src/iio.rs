// SPDX-License-Identifier: MIT
//! Accelerometers from Linux IIO sysfs (kernel Documentation/ABI/testing/sysfs-bus-iio), read on demand with std::fs: no D-Bus, no toolkit.
//! iio-sensor-proxy is not used: it publishes only orientation labels, no raw values.
//! A device counts as an accelerometer when it has `in_accel_{x,y,z}_raw`. One axis value = (raw + offset) x scale, m/s² (ABI `in_accel_x_raw`: "Units after application of scale and offset are m/s^2"); per-axis `in_accel_<a>_scale` / `_offset` win over the shared ones, offset absent = 0.
//! Then the mount matrix (`in_accel_mount_matrix`, else `mount_matrix`, else identity), text "x1, y1, z1; x2, y2, z2; x3, y3, z3": row r gives device axis r = row r . sensor vector -- the row form iio-sensor-proxy applies [recall; axis signs are checked on real hardware, plan 5c].
//! The ABI's main-hardware frame for a hand-held (y up the screen, x right, z out of the screen) is Android's SensorEvent frame, so the result needs no further mapping.

use std::path::{Path, PathBuf};
use std::time::Duration;

use atl_platform_host_api::{Accelerometer, SensorError, Sensors};

/// Where the kernel lists IIO devices; the one product path (a test environment supplies another sysfs by mount namespace, not by a parameter).
const IIO_DEVICES: &str = "/sys/bus/iio/devices";
const AXES: [&str; 3] = ["x", "y", "z"];
/// Read interval when the device states no sampling frequency: 100 Hz, Android's SENSOR_DELAY_FASTEST class for a phone accelerometer, and 3 small sysfs reads per sample stay cheap at it.
const DEFAULT_PERIOD: Duration = Duration::from_millis(10);
/// Storage bits assumed when `scan_elements/in_accel_x_type` is absent: 16 is the widest common accelerometer word, so the range is an over-estimate, never a clip.
const DEFAULT_REAL_BITS: u32 = 16;

/// The machine's IIO accelerometers.
#[derive(Debug, Clone)]
pub struct IioSensors {
    root: PathBuf,
}

impl Default for IioSensors {
    fn default() -> Self {
        IioSensors {
            root: PathBuf::from(IIO_DEVICES),
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

/// `x1, y1, z1; x2, y2, z2; x3, y3, z3` as rows.
fn parse_matrix(text: &str) -> Option<[[f64; 3]; 3]> {
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

impl IioSensors {
    fn device(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn is_accel(dir: &Path) -> bool {
        AXES.iter()
            .all(|a| dir.join(format!("in_accel_{a}_raw")).is_file())
    }

    /// The per-axis attribute, else the shared one.
    fn axis_attr(dir: &Path, axis: &str, attr: &str) -> Result<Option<f64>, SensorError> {
        match read_f64(&dir.join(format!("in_accel_{axis}_{attr}")))? {
            Some(v) => Ok(Some(v)),
            None => read_f64(&dir.join(format!("in_accel_{attr}"))),
        }
    }

    fn matrix(dir: &Path) -> Result<[[f64; 3]; 3], SensorError> {
        for name in ["in_accel_mount_matrix", "mount_matrix"] {
            let path = dir.join(name);
            if let Some(t) = read_text(&path) {
                return parse_matrix(&t).ok_or_else(|| {
                    SensorError(format!("{}: not a 3x3 matrix: {t:?}", path.display()))
                });
            }
        }
        Ok([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]])
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
    /// Devices in name order; one that fails to describe is left out (it cannot be read either).
    fn accelerometers(&self) -> Vec<Accelerometer> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("iio:device"))
            .filter(|n| Self::is_accel(&self.device(n)))
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
        let m = Self::matrix(&dir)?;
        let mut out = [0.0f32; 3];
        for (r, row) in m.iter().enumerate() {
            out[r] = (row[0] * v[0] + row[1] * v[1] + row[2] * v[2]) as f32;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(PathBuf);

    impl Fake {
        fn new(tag: &str, files: &[(&str, &str)]) -> Fake {
            let root = std::env::temp_dir().join(format!("atl-iio-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let dev = root.join("iio:device0");
            std::fs::create_dir_all(dev.join("scan_elements")).unwrap();
            for (n, t) in files {
                std::fs::write(dev.join(n), format!("{t}\n")).unwrap();
            }
            Fake(root)
        }
        fn sensors(&self) -> IioSensors {
            IioSensors {
                root: self.0.clone(),
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
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5)
    }

    #[test]
    fn identity_without_matrix() {
        let f = Fake::new("id", &RAW);
        let s = f.sensors();
        let list = s.accelerometers();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].min_period, DEFAULT_PERIOD);
        // raw x 0.01 per axis
        assert!(close(
            s.read_accel("iio:device0").unwrap(),
            [1.0, -2.0, 10.0]
        ));
    }

    #[test]
    fn matrix_rows_map_sensor_to_device() {
        let mut files = RAW.to_vec();
        // device x = sensor y, device y = -sensor x
        files.push(("in_accel_mount_matrix", "0, 1, 0; -1, 0, 0; 0, 0, 1"));
        files.push(("in_accel_offset", "10"));
        files.push(("in_accel_sampling_frequency", "50"));
        files.push(("scan_elements/in_accel_x_type", "le:s12/16>>4"));
        let f = Fake::new("mm", &files);
        let s = f.sensors();
        // sensor = ((100+10), (-200+10), (1000+10)) x 0.01 = (1.1, -1.9, 10.1)
        assert!(close(
            s.read_accel("iio:device0").unwrap(),
            [-1.9, -1.1, 10.1]
        ));
        let a = &s.accelerometers()[0];
        assert_eq!(a.min_period, Duration::from_millis(20));
        // 2^(12-1) steps of 0.01
        assert!((a.max_range - 20.48).abs() < 1e-4);
    }

    #[test]
    fn no_devices_is_empty() {
        let s = IioSensors {
            root: PathBuf::from("/nonexistent/atl-iio"),
        };
        assert!(s.accelerometers().is_empty());
    }
}
