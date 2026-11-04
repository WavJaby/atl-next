// SPDX-License-Identifier: MIT
//! The binders the compositor has met, named by keys. The core identifies a binder by a number (`BinderKey::Local(cookie)`); the front-end keeps the other direction: which real binder a key stands for,
//! to call a listener back or recognise a layer handle in a transaction. Generic over the binder type `B` so the bookkeeping is tested without libbinder; the front-end uses `WpIBinder`
//! (weak: a layer handle's last strong reference lives in the client and its end is how the compositor learns the layer is gone, and a listener that died simply fails to promote).

use std::collections::{BTreeMap, HashMap};

/// Binders by key and keys by binder. Keys are never reused; 0 is not a key (a null binder's cookie).
#[derive(Debug)]
pub struct ObjectMap<B: Ord + Clone> {
    next: u64,
    by_key: HashMap<u64, B>,
    by_binder: BTreeMap<B, u64>,
}

impl<B: Ord + Clone> Default for ObjectMap<B> {
    fn default() -> Self {
        Self {
            next: 1,
            by_key: HashMap::new(),
            by_binder: BTreeMap::new(),
        }
    }
}

impl<B: Ord + Clone> ObjectMap<B> {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// The key of `binder`: the one it has, or a new one.
    pub fn key_for(&mut self, binder: &B) -> u64 {
        if let Some(&key) = self.by_binder.get(binder) {
            return key;
        }
        let key = self.next;
        self.next += 1;
        self.by_key.insert(key, binder.clone());
        self.by_binder.insert(binder.clone(), key);
        key
    }

    /// The key of `binder` if it has one: unlike [`ObjectMap::key_for`], it makes none.
    pub fn find(&self, binder: &B) -> Option<u64> {
        self.by_binder.get(binder).copied()
    }

    /// The binder behind `key`.
    pub fn get(&self, key: u64) -> Option<&B> {
        self.by_key.get(&key)
    }

    /// Forgets `key` (a layer handle whose client dropped it); returns its binder.
    pub fn remove(&mut self, key: u64) -> Option<B> {
        let binder = self.by_key.remove(&key)?;
        self.by_binder.remove(&binder);
        Some(binder)
    }

    /// How many binders are known.
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same binder keeps its key, a new one gets the next, a removed one is gone in both directions and its key is not reused.
    #[test]
    fn keys_are_stable_and_not_reused() {
        let mut m = ObjectMap::new();
        assert_eq!(m.find(&"a"), None);
        let a = m.key_for(&"a");
        let b = m.key_for(&"b");
        assert_eq!((a, b), (1, 2));
        assert_eq!(m.key_for(&"a"), a);
        assert_eq!(m.find(&"a"), Some(a));
        assert_eq!(m.get(b), Some(&"b"));
        assert_eq!(m.remove(a), Some("a"));
        assert_eq!(m.get(a), None);
        assert_eq!(m.len(), 1);
        // "a" meets again: a new key
        assert_eq!(m.key_for(&"a"), 3);
        assert_eq!(m.remove(99), None);
    }
}
