// SPDX-License-Identifier: MIT
//! The window's presentation samples from `wp_presentation` feedback, bound on GDK's own Wayland connection (the only one the window's `wl_surface` exists on): GdkFrameTimings carries the time and
//! period but drops the feedback's `kind` flags and `clock_id`, and those decide whether a time is a display vsync ([`PresentSource`]).
//! A second feedback object on a commit GDK also asks feedback for is legal (wp_presentation: every feedback object of a commit gets the event) and leaves GDK's own untouched.
//! Our objects live on wayland-rs's own queue of that connection (guest mode: GDK reads the socket, [`Presentation::take`] only dispatches what is queued): nothing here reads or blocks on the socket,
//! since GDK's main-loop source may hold a prepared read. Requests go out with GDK's next flush, before the commit they precede.
//! At most one feedback is outstanding: requested at the frame clock's before-paint, ahead of the paint's commit; a cycle that commits nothing leaves it for the next commit.
//! No Wayland display, no `wl_surface`, or no `wp_presentation` global: no samples, said once (the composer's clock stays free-running).

use std::cell::Cell;

use atl_platform_ui_api::{PresentSource, PresentTiming};
use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gtk4::gdk;
use gtk4::prelude::*;
use rustix::time::{ClockId, DynamicClockId, clock_gettime, clock_gettime_dynamic};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, Kind, WpPresentationFeedback,
};

/// Version 1 has everything read here (`clock_id`, `presented` with refresh and flags).
const PRESENTATION_VERSION: u32 = 1;

/// Linux clock ids as `wp_presentation.clock_id` sends them (uapi `linux/time.h`).
const CLOCK_REALTIME: u32 = 0;
const CLOCK_MONOTONIC: u32 = 1;
const CLOCK_MONOTONIC_RAW: u32 = 4;
const CLOCK_BOOTTIME: u32 = 7;

#[derive(Default)]
struct State {
    presentation: Option<WpPresentation>,
    clock_id: Option<u32>,
    /// The registry's initial globals are in (the sync after `get_registry` answered).
    globals_done: bool,
    outstanding: bool,
    samples: Vec<PresentTiming>,
    presented: u64,
    discarded: u64,
}

fn ns_of(t: rustix::time::Timespec) -> i64 {
    // hosts are 64-bit (arches.toml): tv_sec and tv_nsec are i64
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// Now on the host clock `clock_id`; None for a clock this process cannot read.
fn now_on(clock_id: u32) -> Option<i64> {
    let t = match clock_id {
        CLOCK_REALTIME => clock_gettime(ClockId::Realtime),
        CLOCK_MONOTONIC => clock_gettime(ClockId::Monotonic),
        CLOCK_MONOTONIC_RAW => clock_gettime(ClockId::MonotonicRaw),
        CLOCK_BOOTTIME => clock_gettime_dynamic(DynamicClockId::Boottime).ok()?,
        _ => return None,
    };
    Some(ns_of(t))
}

/// The sample of one `presented` event: `host_ns` on the host clock `clock_id` (None: not yet known) mapped onto CLOCK_MONOTONIC by the clocks' offset now, and what the host says it is.
fn sample(host_ns: i64, refresh_ns: i64, flags: Kind, clock_id: Option<u32>) -> PresentTiming {
    let monotonic = ns_of(clock_gettime(ClockId::Monotonic));
    let (time_ns, same_clock) = match clock_id {
        Some(CLOCK_MONOTONIC) => (host_ns, true),
        Some(id) => (
            now_on(id).map_or(monotonic, |now| host_ns + monotonic - now),
            false,
        ),
        None => (monotonic, false),
    };
    let source = match (flags.contains(Kind::Vsync), same_clock) {
        (false, _) => PresentSource::NotVsync,
        (true, true) => PresentSource::Vsync,
        (true, false) => PresentSource::OtherClock,
    };
    PresentTiming {
        time_ns,
        refresh_ns,
        source,
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == WpPresentation::interface().name
            && state.presentation.is_none()
        {
            state.presentation = Some(registry.bind::<WpPresentation, _, _>(
                name,
                version.min(PRESENTATION_VERSION),
                qh,
                (),
            ));
        }
    }
}

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.globals_done = true;
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            tracing::info!("atl-platform-ui: wp_presentation clock_id={clk_id}");
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                refresh,
                flags,
                ..
            } => {
                state.outstanding = false;
                state.presented += 1;
                let secs = (i64::from(tv_sec_hi) << 32) | i64::from(tv_sec_lo);
                let flags = match flags {
                    WEnum::Value(k) => k,
                    WEnum::Unknown(bits) => Kind::from_bits_truncate(bits),
                };
                state.samples.push(sample(
                    secs * 1_000_000_000 + i64::from(tv_nsec),
                    i64::from(refresh),
                    flags,
                    state.clock_id,
                ));
            }
            wp_presentation_feedback::Event::Discarded => {
                state.outstanding = false;
                state.discarded += 1;
            }
            _ => {}
        }
    }
}

/// One window's presentation feedback.
pub(crate) struct Presentation {
    queue: EventQueue<State>,
    state: State,
    surface: WlSurface,
    /// "No wp_presentation" was said.
    absent_logged: Cell<bool>,
}

impl Presentation {
    /// Binds on the connection of `surface` (a realised window's); Err names why there are no samples.
    pub(crate) fn bind(surface: &gdk::Surface) -> Result<Self, String> {
        let wayland = surface
            .downcast_ref::<gdk4_wayland::WaylandSurface>()
            .ok_or("the window's surface is not a Wayland surface")?;
        let wl_surface = wayland
            .wl_surface()
            .ok_or("the Wayland surface has no wl_surface")?;
        let backend = wl_surface
            .backend()
            .upgrade()
            .ok_or("GDK's Wayland connection is gone")?;
        let conn = Connection::from_backend(backend);
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let display = conn.display();
        display.get_registry(&qh, ());
        display.sync(&qh, ());
        Ok(Presentation {
            queue,
            state: State::default(),
            surface: wl_surface,
            absent_logged: Cell::new(false),
        })
    }

    /// Before a paint: asks feedback for the next commit, unless one is still outstanding.
    pub(crate) fn request(&mut self) {
        self.dispatch();
        if self.state.outstanding {
            return;
        }
        if let Some(p) = self.state.presentation.as_ref() {
            p.feedback(&self.surface, &self.queue.handle(), ());
            self.state.outstanding = true;
        }
    }

    /// The samples that came since the last call, in order, and the running `(presented, discarded)` totals.
    pub(crate) fn take(&mut self) -> (Vec<PresentTiming>, (u64, u64)) {
        self.dispatch();
        (
            std::mem::take(&mut self.state.samples),
            (self.state.presented, self.state.discarded),
        )
    }

    fn dispatch(&mut self) {
        if let Err(e) = self.queue.dispatch_pending(&mut self.state) {
            tracing::warn!("atl-platform-ui: presentation feedback: {e}");
        }
        if self.state.globals_done
            && self.state.presentation.is_none()
            && !self.absent_logged.replace(true)
        {
            tracing::info!(
                "atl-platform-ui: no presentation samples: the compositor has no wp_presentation"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host time on CLOCK_MONOTONIC passes unchanged; the kind decides the source: vsync -> Vsync, none -> NotVsync (headless wlroots and WSLg send flags 0, measured).
    #[test]
    fn monotonic_samples_keep_their_time() {
        let s = sample(
            123,
            16_666_666,
            Kind::Vsync | Kind::HwClock,
            Some(CLOCK_MONOTONIC),
        );
        assert_eq!(
            (s.time_ns, s.refresh_ns, s.source),
            (123, 16_666_666, PresentSource::Vsync)
        );
        let s = sample(123, 0, Kind::empty(), Some(CLOCK_MONOTONIC));
        assert_eq!((s.time_ns, s.source), (123, PresentSource::NotVsync));
    }

    /// A vsync on CLOCK_MONOTONIC_RAW (WSLg's clock) is OtherClock and lands on CLOCK_MONOTONIC: a time read on the raw clock just before maps to at most a moment before monotonic now. Kernel clocks:
    /// both advance together within NTP slew, so the mapped time is within 1 ms below the monotonic time read after it.
    #[test]
    fn other_clock_samples_are_converted() {
        let raw = now_on(CLOCK_MONOTONIC_RAW).expect("CLOCK_MONOTONIC_RAW reads");
        let s = sample(raw, 16_666_666, Kind::Vsync, Some(CLOCK_MONOTONIC_RAW));
        let after = ns_of(clock_gettime(ClockId::Monotonic));
        assert_eq!(s.source, PresentSource::OtherClock);
        assert!(
            (0..1_000_000).contains(&(after - s.time_ns)),
            "{} vs {after}",
            s.time_ns
        );
    }
}
