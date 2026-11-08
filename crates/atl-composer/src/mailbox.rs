// SPDX-License-Identifier: MIT
//! UI mode's hand-off between the composer's clock thread and the toolkit thread (design: composer threading S2). Toolkit-neutral: no GTK here.
//! - One slot, latest wins: the clock publishes each new scene over the last; a scene the toolkit never took is dropped, and the core released its buffers at the latch that replaced them (AOSP:
//!   SurfaceFlinger latches every vsync whatever HWC shows; a replaced, never-shown buffer goes back with its acquire fence, Layer.cpp:852-870).
//! - Hold set per buffer: a buffer is held from the take of a scene that shows it until the toolkit reports it released (after its paint, with the read fence). The core releases a replaced buffer
//!   only once it is not held ([`atl_compositor::model::Compositor::set_held`]).
//! - Lock rule: the toolkit thread takes only this mutex, never the core's; the clock takes it inside the core lock ([`Mailbox::latch`]: order core -> mailbox), so a take and a latch never interleave.
//!   Nobody holds it across a blocking call: the longest hold is one latch (fence polls with no wait).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use atl_compositor::scene::{Quad, Scene};

/// Counts for the composer's counter line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxStats {
    /// Scenes published.
    pub published: u64,
    /// Scenes the toolkit took.
    pub taken: u64,
    /// Scenes replaced before the toolkit took them (latest wins).
    pub dropped: u64,
    /// Taken scenes the toolkit could not present (their buffers are not held).
    pub refused: u64,
    /// Release reports from the toolkit.
    pub reports: u64,
    /// Most buffers held on one layer right after a report (between frame cycles): 1 = only the one on screen.
    pub max_held_per_layer: u64,
}

#[derive(Debug, Default)]
struct Inner {
    scene: Option<Scene>,
    /// Held buffer id -> its layer id.
    held: BTreeMap<u64, i32>,
    /// Read fences of the buffers reported released since the clock last looked, by buffer id.
    fences: HashMap<u64, OwnedFd>,
    /// A report arrived since the clock last looked.
    reported: bool,
    stats: MailboxStats,
}

/// A scene the toolkit took, and the buffers that take added to the hold set (removed again if it is refused).
#[derive(Debug)]
pub struct Taken {
    pub scene: Scene,
    added: Vec<u64>,
}

/// The mailbox.
#[derive(Debug, Default)]
pub struct Mailbox {
    inner: Mutex<Inner>,
}

/// The clock's hold of the mailbox during one latch.
pub struct Latch<'a> {
    inner: MutexGuard<'a, Inner>,
}

impl Latch<'_> {
    /// The buffers held now.
    pub fn held(&self) -> BTreeSet<u64> {
        self.inner.held.keys().copied().collect()
    }

    /// The read fences reported since the last look (buffer id -> sync_file), or None when no report arrived.
    pub fn take_report(&mut self) -> Option<HashMap<u64, OwnedFd>> {
        if !std::mem::take(&mut self.inner.reported) {
            return None;
        }
        Some(std::mem::take(&mut self.inner.fences))
    }

    /// `scene` replaces the slot's.
    pub fn publish(&mut self, scene: Scene) {
        let s = &mut self.inner;
        s.stats.published += 1;
        if s.scene.replace(scene).is_some() {
            s.stats.dropped += 1;
        }
    }

    /// The slot's scene names a buffer released outside a latch (a destroyed layer): it is dropped, and the next tick publishes the scene without it.
    pub fn clear(&mut self) {
        self.inner.scene = None;
    }
}

fn buffers(scene: &Scene) -> impl Iterator<Item = (u64, i32)> + '_ {
    scene
        .quads
        .iter()
        .filter_map(|q| Quad::buffer(q).map(|b| (b.buffer.id, q.layer_id)))
}

impl Mailbox {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The clock's latch: call with the core lock held, and keep the guard over the core's latch and the publish.
    pub fn latch(&self) -> Latch<'_> {
        Latch { inner: self.lock() }
    }

    /// Whether a release report waits for the clock.
    pub fn has_report(&self) -> bool {
        self.lock().reported
    }

    /// The toolkit takes the latest scene (None: nothing new); its buffers are held from now.
    pub fn take(&self) -> Option<Taken> {
        let mut s = self.lock();
        let scene = s.scene.take()?;
        s.stats.taken += 1;
        let mut added = Vec::new();
        for (id, layer) in buffers(&scene) {
            if s.held.insert(id, layer).is_none() {
                added.push(id);
            }
        }
        Some(Taken { scene, added })
    }

    /// The toolkit could not present `taken`: it keeps showing what it showed, so the buffers the take added are not held; the clock gets a report to act on.
    pub fn refused(&self, taken: Taken) {
        let mut s = self.lock();
        s.stats.refused += 1;
        for id in taken.added {
            s.held.remove(&id);
        }
        s.reported = true;
    }

    /// The toolkit stopped drawing these buffers (buffer id, read fence; None = no fence, reusable now).
    pub fn released(&self, releases: impl IntoIterator<Item = (u64, Option<OwnedFd>)>) {
        let mut s = self.lock();
        for (id, fence) in releases {
            s.held.remove(&id);
            if let Some(fence) = fence {
                s.fences.insert(id, fence);
            }
        }
        s.reported = true;
        s.stats.reports += 1;
        let mut per_layer: HashMap<i32, u64> = HashMap::new();
        for layer in s.held.values() {
            *per_layer.entry(*layer).or_default() += 1;
        }
        let most = per_layer.values().copied().max().unwrap_or(0);
        s.stats.max_held_per_layer = s.stats.max_held_per_layer.max(most);
    }

    /// The counts so far.
    pub fn stats(&self) -> MailboxStats {
        self.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use atl_compositor::scene::{BufferContent, Content, SceneBuffer};
    use atl_libgui_parcel::state::FloatRect;

    use super::*;

    fn scene(frame: u64, buffers: &[(i32, u64)]) -> Scene {
        let r = FloatRect {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        };
        Scene {
            frame,
            layer_stack: 0,
            quads: buffers
                .iter()
                .map(|&(layer_id, id)| Quad {
                    layer_id,
                    name: String::new(),
                    content: Content::Buffer(BufferContent {
                        buffer: Arc::new(SceneBuffer {
                            width: 1,
                            height: 1,
                            stride: 1,
                            format: 1,
                            usage: 0,
                            id,
                            fds: vec![],
                            ints: vec![],
                        }),
                        frame_number: frame,
                        acquire_fence: None,
                        src: r,
                        opaque: true,
                        buffer_transform: 0,
                    }),
                    dst: r,
                    clip: None,
                    alpha: 1.0,
                })
                .collect(),
        }
    }

    /// Latest wins: two publishes before a take leave the second; the first counts as dropped and none of its buffers is held.
    #[test]
    fn latest_scene_wins() {
        let m = Mailbox::default();
        m.latch().publish(scene(1, &[(7, 10)]));
        m.latch().publish(scene(2, &[(7, 11)]));
        let t = m.take().expect("a scene");
        assert_eq!(t.scene.frame, 2);
        assert_eq!(m.latch().held(), BTreeSet::from([11]));
        assert!(m.take().is_none(), "taken once");
        let s = m.stats();
        assert_eq!((s.published, s.dropped, s.taken), (2, 1, 1));
    }

    /// A buffer stays held from its take until its release report; the report's fence reaches the clock once, with the report flag.
    #[test]
    fn held_until_reported() {
        let m = Mailbox::default();
        m.latch().publish(scene(1, &[(7, 10)]));
        m.take().expect("first");
        m.latch().publish(scene(2, &[(7, 11)]));
        m.take().expect("second");
        assert_eq!(m.latch().held(), BTreeSet::from([10, 11]));
        assert!(!m.has_report());
        m.released([(10, None)]);
        assert!(m.has_report());
        let mut l = m.latch();
        assert_eq!(l.held(), BTreeSet::from([11]));
        assert_eq!(l.take_report().map(|f| f.len()), Some(0));
        assert_eq!(l.take_report().map(|f| f.len()), None);
        drop(l);
        assert_eq!(
            m.stats().max_held_per_layer,
            1,
            "one on screen per layer after the paint"
        );
    }

    /// A refused take un-holds only what it added: a buffer the window already showed stays held.
    #[test]
    fn refused_take_unholds_what_it_added() {
        let m = Mailbox::default();
        m.latch().publish(scene(1, &[(7, 10)]));
        m.take().expect("first");
        m.latch().publish(scene(2, &[(7, 10), (8, 20)]));
        let t = m.take().expect("second");
        m.refused(t);
        assert_eq!(m.latch().held(), BTreeSet::from([10]));
        assert!(m.has_report());
    }
}
