// SPDX-License-Identifier: MIT
//! Key tilt (atld `--tilt-keys`, delivered as `ro.atl.tilt`): on a desktop with no accelerometer, held Left / Right (or A / D) tilt the device for accelerometer games. The keys still go to the app; the tilt is injected as accelerometer data through AOSP's own path
//! (sensorservice HAL bypass data injection, atl-services sensor_wire `Injector`), never a channel of ours.
//! * Angle: a held key moves it towards +-max at max/ramp per ms, release (or both keys) back to level at the same rate. Left = left edge down = theta > 0.
//! * Values (Android device frame, SensorEvent docs: x right, y up, z out of the screen; a device at rest reads +g against gravity): screen frame x = g sin theta, y = 0, z = g cos theta, so left edge down gives x > 0 (the up direction leans to +x).
//!   Rotated into the device frame by the display rotation (SensorManager.remapCoordinateSystem's inverse: ROTATION_90 -> device (x, y) = (-sy, sx)).
//! * Focus: any change (a window of this process gains or loses it) releases the keys and levels the device at once. Only a process with a focused window injects, so two apps' composers never interleave events (injected events go to every app).
//! * The service decides whether injection runs at all (it refuses the mode with a real accelerometer present); a process whose service says no logs it once and injects nothing.

use std::time::Duration;

use atl_services::compositor_registry::TiltConfig;

/// SensorManager.GRAVITY_EARTH.
const GRAVITY: f32 = 9.806_65;
/// KeyEvent.KEYCODE_DPAD_LEFT / _RIGHT / _A / _D: the keys that tilt (arrows, and the WASD side keys).
const KEYCODE_DPAD_LEFT: i32 = 21;
const KEYCODE_DPAD_RIGHT: i32 = 22;
const KEYCODE_A: i32 = 29;
const KEYCODE_D: i32 = 32;

/// Which way a key tilts: +1 left edge down, -1 right edge down.
fn direction(key_code: i32) -> Option<f32> {
    match key_code {
        KEYCODE_DPAD_LEFT | KEYCODE_A => Some(1.0),
        KEYCODE_DPAD_RIGHT | KEYCODE_D => Some(-1.0),
        _ => None,
    }
}

/// The tilt state: held tilt keys, the angle, whether this process has a focused window.
#[derive(Debug)]
pub struct Tilt {
    config: TiltConfig,
    /// Held tilt keycodes.
    held: Vec<i32>,
    theta_deg: f32,
    focused: bool,
    /// A window of this process had focus once: ends the probe ([`TiltConfig::probe_unfocused`]).
    was_focused: bool,
    /// A level event is owed after losing focus (the last one this process sends).
    owe_level: bool,
}

/// The probe's reading: the left edge straight down (90 deg), beyond any configured angle, so an app that receives it shows the focus check failed.
const PROBE: [f32; 3] = [GRAVITY, 0.0, 0.0];

impl Tilt {
    pub fn new(config: TiltConfig) -> Tilt {
        Tilt {
            config,
            held: Vec::new(),
            theta_deg: 0.0,
            focused: false,
            was_focused: false,
            owe_level: false,
        }
    }

    pub fn period(&self) -> Duration {
        Duration::from_millis(u64::from(self.config.period_ms))
    }

    /// A key the focused window got: tilt keys update the held set, others are ignored.
    pub fn key(&mut self, key_code: i32, down: bool) {
        if direction(key_code).is_none() {
            return;
        }
        self.held.retain(|k| *k != key_code);
        if down {
            self.held.push(key_code);
        }
    }

    /// The focus changed: keys released, level at once; `focused` = a window of this process has focus now.
    pub fn focus(&mut self, focused: bool) {
        self.held.clear();
        self.theta_deg = 0.0;
        self.owe_level = self.focused && !focused;
        self.focused = focused;
        self.was_focused |= focused;
    }

    /// The angle the held keys aim at: one direction's keys -> +-max, none or both -> level.
    fn target(&self) -> f32 {
        let sum: f32 = self.held.iter().filter_map(|k| direction(*k)).sum();
        sum.signum() * if sum == 0.0 { 0.0 } else { self.config.max_deg }
    }

    /// Advances the angle by `dt`; the values to inject, None when this process injects nothing now. Before the first focus with the probe on: [`PROBE`], every step.
    pub fn step(&mut self, dt: Duration, rotation: i32) -> Option<[f32; 3]> {
        if self.config.probe_unfocused && !self.was_focused {
            return Some(PROBE);
        }
        if !self.focused {
            return std::mem::take(&mut self.owe_level).then(|| self.values(rotation));
        }
        let target = self.target();
        let rate = if self.config.ramp_ms == 0 {
            f32::INFINITY
        } else {
            self.config.max_deg / self.config.ramp_ms as f32
        };
        let max_move = rate * dt.as_secs_f32() * 1000.0;
        let d = target - self.theta_deg;
        self.theta_deg += d.clamp(-max_move, max_move);
        Some(self.values(rotation))
    }

    /// The accelerometer reading at the current angle, device frame for `rotation` (Surface.ROTATION_*).
    pub fn values(&self, rotation: i32) -> [f32; 3] {
        let t = self.theta_deg.to_radians();
        let (sx, sy, z) = (GRAVITY * t.sin(), 0.0, GRAVITY * t.cos());
        let (x, y) = match rotation.rem_euclid(4) {
            1 => (-sy, sx),
            2 => (-sx, -sy),
            3 => (sy, -sx),
            _ => (sx, sy),
        };
        [x, y, z]
    }
}

/// The injector thread: connects as [`atl_services::sensor::TILT_INJECTOR_PACKAGE`], then every period steps the tilt and injects. Ends (logged) when the service is not in injection mode or the connection fails.
#[cfg(feature = "converter")]
pub fn spawn(tilt: std::sync::Arc<std::sync::Mutex<Tilt>>, rotation: i32) -> Result<(), String> {
    use atl_services::sensor_wire::Injector;
    let run = move || {
        let injector = match Injector::connect(atl_services::sensor::TILT_INJECTOR_PACKAGE) {
            Ok(Some(i)) => i,
            Ok(None) => {
                tracing::info!("tilt: sensorservice not in injection mode, no tilt");
                return;
            }
            Err(e) => {
                tracing::warn!("tilt: {e}; no tilt");
                return;
            }
        };
        tracing::info!("tilt: injecting");
        let mut last = std::time::Instant::now();
        loop {
            let period = tilt.lock().unwrap_or_else(|e| e.into_inner()).period();
            std::thread::sleep(period);
            let now = std::time::Instant::now();
            let values = tilt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .step(now - last, rotation);
            last = now;
            let Some(v) = values else { continue };
            let ts = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
            if let Err(e) = injector.inject(ts.tv_sec * 1_000_000_000 + ts.tv_nsec, &v) {
                tracing::warn!("tilt: {e}");
            }
        }
    };
    std::thread::Builder::new()
        .name("atl-tilt".to_owned())
        .spawn(run)
        .map(|_| ())
        .map_err(|e| format!("tilt thread: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TiltConfig {
        TiltConfig {
            max_deg: 30.0,
            ramp_ms: 300,
            period_ms: 20,
            probe_unfocused: false,
        }
    }

    #[test]
    fn probe_injects_only_before_the_first_focus() {
        let mut t = Tilt::new(TiltConfig {
            probe_unfocused: true,
            ..cfg()
        });
        assert_eq!(t.step(MS(20), 0), Some(PROBE));
        t.focus(true);
        assert!(close(t.step(MS(20), 0).unwrap(), [0.0, 0.0, GRAVITY]));
        t.focus(false);
        assert!(close(t.step(MS(20), 0).unwrap(), [0.0, 0.0, GRAVITY]));
        assert_eq!(
            t.step(MS(20), 0),
            None,
            "after a focus: as without the probe"
        );
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn held_left_ramps_to_max_and_back() {
        let mut t = Tilt::new(cfg());
        t.focus(true);
        assert!(close(t.step(MS(20), 0).unwrap(), [0.0, 0.0, GRAVITY]));
        t.key(KEYCODE_DPAD_LEFT, true);
        // half the ramp: 15 deg
        let half = t.step(MS(150), 0).unwrap();
        let s15 = 15f32.to_radians();
        assert!(
            close(half, [GRAVITY * s15.sin(), 0.0, GRAVITY * s15.cos()]),
            "{half:?}"
        );
        let full = t.step(MS(1000), 0).unwrap();
        // by hand: 9.80665 sin 30 = 4.903325, cos 30 * g = 8.492829
        assert!(close(full, [4.903_325, 0.0, 8.492_829]), "{full:?}");
        t.key(KEYCODE_DPAD_LEFT, false);
        assert!(close(t.step(MS(1000), 0).unwrap(), [0.0, 0.0, GRAVITY]));
    }

    #[test]
    fn right_and_d_tilt_the_other_way_both_cancel_others_ignored() {
        let mut t = Tilt::new(cfg());
        t.focus(true);
        t.key(KEYCODE_D, true);
        assert!(t.step(MS(1000), 0).unwrap()[0] < -4.9);
        t.key(KEYCODE_A, true);
        assert!(close(t.step(MS(1000), 0).unwrap(), [0.0, 0.0, GRAVITY]));
        t.key(KEYCODE_A, false);
        t.key(KEYCODE_D, false);
        // KEYCODE_SPACE
        t.key(62, true);
        assert!(close(t.step(MS(1000), 0).unwrap(), [0.0, 0.0, GRAVITY]));
    }

    #[test]
    fn focus_change_levels_and_unfocused_sends_one_level_event() {
        let mut t = Tilt::new(cfg());
        assert_eq!(t.step(MS(20), 0), None, "no focus yet: nothing");
        t.focus(true);
        t.key(KEYCODE_DPAD_LEFT, true);
        t.step(MS(1000), 0);
        t.focus(false);
        assert!(close(t.step(MS(20), 0).unwrap(), [0.0, 0.0, GRAVITY]));
        assert_eq!(t.step(MS(20), 0), None);
        t.focus(true);
        assert!(
            close(t.step(MS(20), 0).unwrap(), [0.0, 0.0, GRAVITY]),
            "the key was released by the change"
        );
    }

    #[test]
    fn rotation_maps_screen_x_into_the_device_frame() {
        let mut t = Tilt::new(TiltConfig {
            ramp_ms: 0,
            ..cfg()
        });
        t.focus(true);
        t.key(KEYCODE_DPAD_LEFT, true);
        let s = t.step(MS(20), 0).unwrap()[0];
        assert!(close(t.values(1), [0.0, s, t.values(0)[2]]));
        assert!(close(t.values(2), [-s, 0.0, t.values(0)[2]]));
        assert!(close(t.values(3), [0.0, -s, t.values(0)[2]]));
    }
}
