// This Source Code is covered under the terms of the Mozilla Public License, v.2.0.
// A copy of this license can be found in the root directory of this project.  If
// no copy was found, you can obtain one at http://mozilla.org/MPL/2.0/.

//! # An LRU Cache with weighted ejection criteria.
//!
//! A Least-Recently-Used cache that uses an arbitrary size attribute to
//! determine when the cache is "full."
//!
//! ## Say what?
//!
//! This code is based on an early [reference implementation of Rust's
//! LRU
//! cache](https://doc.rust-lang.org/0.12.0/std/collections/lru_cache/struct.LruCache.html).
//! It provides a simple LRU cache, but unlike that version, uses an
//! arbitrary criteria to determine when the cache is full.  This is
//! useful if the client code is caching many values that are not of
//! the same size, such as documents.
//!
//! ## What's it useful for?
//!
//! The simplest explanation is that it's useful for limiting the
//! memory usage of the cache, in those cases where the value objects
//! of the cache contain heap objects (objects that are Box'd or or
//! Rc'd, like strings).
//!
//!
//! For example, if `String.len()` is your weight, your `max_weight` is
//! 20, and your `max_count` is 5, then the total weight of the cache
//! is 100: the cache could hold 5 strings of length 20, but it could
//! also hold 10 strings of length 10, or 25 strings of length 4, and
//! so on.  It could not, however, hold 4 strings of length 25: the
//! `insert()` method will *reject* an object above the `max_weight`.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::mem::MaybeUninit;
use std::ptr;

struct LruCacheKey<K> {
    key: *const K,
}

impl<K: Hash> Hash for LruCacheKey<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe { (*self.key).hash(state) }
    }
}

impl<K: PartialEq> PartialEq for LruCacheKey<K> {
    fn eq(&self, other: &LruCacheKey<K>) -> bool {
        unsafe { (*self.key).eq(&*other.key) }
    }
}

impl<K: Eq> Eq for LruCacheKey<K> {}

pub trait Weighted {
    fn weight(&self) -> usize;
}

struct LruCacheItem<K, V> {
    key: MaybeUninit<K>,
    value: MaybeUninit<V>,
    prev: *mut LruCacheItem<K, V>,
    next: *mut LruCacheItem<K, V>,
}

impl<K, V> LruCacheItem<K, V> {
    fn new(key: K, value: V) -> Self {
        LruCacheItem {
            key: MaybeUninit::new(key),
            value: MaybeUninit::new(value),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }

    fn new_sigil() -> Self {
        Self {
            key: MaybeUninit::uninit(),
            value: MaybeUninit::uninit(),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum LruError {
    ExceedsMaximumWeight,
    NonsenseParameters,
}

pub struct LruWeightedCache<K, V> {
    cache: HashMap<LruCacheKey<K>, Box<LruCacheItem<K, V>>>,
    max_item_weight: usize,
    max_total_weight: usize,
    current_weight: usize,
    head: *mut LruCacheItem<K, V>,
    tail: *mut LruCacheItem<K, V>,
}

impl<K: Hash + Eq, V: Weighted> LruWeightedCache<K, V> {
    /// Build a new LRU cache.
    ///
    /// The two values you have to supply, `max_count` and `max_weight`,
    /// are used both to determine if the cache will accept a new
    /// object and if the cache will eject an old object.  The maximum
    /// weight of the cache will be `max_count * max_weight`, but it's
    /// important to understand that `max_count` is the number of
    /// *maximal-weight* objects the cache can contain.
    pub fn new(
        max_count: usize,
        max_item_weight: usize,
    ) -> Result<LruWeightedCache<K, V>, LruError> {
        if max_count == 0 || max_item_weight == 0 {
            return Err(LruError::NonsenseParameters);
        }

        let max_total_weight = max_item_weight * max_count;
        let lrucache = LruWeightedCache {
            cache: HashMap::new(),
            max_item_weight,
            max_total_weight,
            current_weight: 0,
            head: Box::into_raw(Box::new(LruCacheItem::new_sigil())),
            tail: Box::into_raw(Box::new(LruCacheItem::new_sigil())),
        };

        // The Oroborous Condition!
        unsafe {
            (*lrucache.head).next = lrucache.tail;
            (*lrucache.tail).prev = lrucache.head;
        }

        Ok(lrucache)
    }

    /// Returns true if the [Weighted](trait.Weighted.html) object is less than
    /// the max weight.
    pub fn will_accept(&mut self, value: &V) -> bool {
        value.weight() <= self.max_item_weight
    }

    // From the oldest upward, discard objects until there's enough
    // room for the requested object.
    fn eject(&mut self, value: &V, node_ptr: &Option<*mut LruCacheItem<K, V>>) {
        // Must keep track of our own notion of current weight, because
        // we have not yet ejected this value from the cache.

        let mut current_weight = self.current_weight;
        if let Some(node_ptr) = *node_ptr {
            // Remove the size of the value for an existing candidate node.
            unsafe { current_weight -= (*node_ptr).value.assume_init_ref().weight() };
        }

        while current_weight + value.weight() > self.max_total_weight {
            let v = unsafe {
                self.remove((*(*self.tail).prev).key.assume_init_ref())
                    .unwrap()
            };
            current_weight -= v.weight();
        }
    }

    /// Put a key-value pair into the cache, ejecting older entries as
    /// necessary until the new value "fits" according to the
    /// [Weighted](trait.Weighted.html) function.  Will reject an
    /// object if its reported weight is above `max_item_weight`.
    pub fn insert(&mut self, key: K, value: V) -> Result<(), LruError> {
        if !self.will_accept(&value) {
            return Err(LruError::ExceedsMaximumWeight);
        }

        let node_ptr = self.cache.get_mut(&LruCacheKey { key: &key }).map(|node| {
            let node_ptr: *mut LruCacheItem<K, V> = &mut **node;
            node_ptr
        });

        // Eject until there's enough room to store the value. Pass the
        // node_ptr so eject can avoid over-ejection by knowing to
        // calculate the candidate node value size, if a candidate
        // node is already present.
        self.eject(&value, &node_ptr);

        match node_ptr {
            Some(node_ptr) => {
                unsafe {
                    self.current_weight = (self.current_weight
                        - (*node_ptr).value.assume_init_ref().weight())
                        + value.weight();
                    // This is still a move.
                    (*node_ptr).value = MaybeUninit::new(value);
                }
                self.promote(node_ptr);
            }
            None => {
                self.current_weight += value.weight();
                let mut node = Box::new(LruCacheItem::new(key, value));
                let node_ptr: *mut LruCacheItem<K, V> = &mut *node;
                self.attach(node_ptr);
                let keyref = unsafe { (*node_ptr).key.assume_init_ref() };
                self.cache.insert(LruCacheKey { key: keyref }, node);
            }
        }
        Ok(())
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        let lkey = LruCacheKey { key };
        self.cache
            .get(&lkey)
            .map(|v| unsafe { v.value.assume_init_ref() })
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let key = LruCacheKey { key };
        match self.cache.remove(&key) {
            None => None,
            Some(lru_entry) => {
                self.detach(&(*lru_entry));
                let value = unsafe { lru_entry.value.assume_init() };
                self.current_weight -= value.weight();
                Some(value)
            }
        }
    }

    /// Number of entries in the cache.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.len() == 0
    }

    /// Current weight of the cache.
    pub fn weight(&self) -> usize {
        self.current_weight
    }

    #[inline]
    fn detach(&mut self, node: *const LruCacheItem<K, V>) {
        unsafe {
            (*(*node).prev).next = (*node).next;
            (*(*node).next).prev = (*node).prev;
        }
    }

    #[inline]
    fn attach(&mut self, node: *mut LruCacheItem<K, V>) {
        unsafe {
            (*node).next = (*self.head).next;
            (*node).prev = self.head;
            (*self.head).next = node;
            (*(*node).next).prev = node;
        }
    }

    #[inline]
    fn promote(&mut self, node: *mut LruCacheItem<K, V>) {
        self.detach(node);
        self.attach(node);
    }

    pub fn contains_key(&self, key: &K) -> bool {
        let key = LruCacheKey { key };
        self.cache.contains_key(&key)
    }
}

#[doc(hidden)]
impl<K, V> Drop for LruWeightedCache<K, V> {
    fn drop(&mut self) {
        self.cache.values_mut().for_each(|e| unsafe {
            ptr::drop_in_place(e.key.as_mut_ptr());
            ptr::drop_in_place(e.value.as_mut_ptr());
        });

        // We rebox the head/tail, and because these are maybe-uninit
        // they do not have the absent k/v dropped.

        let _head = unsafe { *Box::from_raw(self.head) };
        let _tail = unsafe { *Box::from_raw(self.tail) };
    }
}

// The compiler does not automatically derive Send and Sync for LruCache because it contains
// raw pointers. The raw pointers are safely encapsulated by LruCache though so we can
// implement Send and Sync for it below.
unsafe impl<K: Send, V: Send> Send for LruWeightedCache<K, V> {}
unsafe impl<K: Sync, V: Sync> Sync for LruWeightedCache<K, V> {}

impl<K: Hash + Eq, V> std::fmt::Debug for LruWeightedCache<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("LruWeightedCache")
            .field("max_item_weight", &self.max_item_weight)
            .field("max_total_weight", &self.max_total_weight)
            .field("current_weight", &self.current_weight)
            .finish()
    }
}

impl Weighted for String {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl Weighted for str {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl<'a> Weighted for &'a str {
    fn weight(&self) -> usize {
        (*self).len()
    }
}

impl Weighted for Vec<u8> {
    fn weight(&self) -> usize {
        self.len()
    }
}

impl<'a> Weighted for &'a Vec<u8> {
    fn weight(&self) -> usize {
        self.len()
    }
}

#[cfg(test)]
mod tests {
    use super::LruCacheItem;
    use super::LruError;
    use super::LruWeightedCache;
    use pretty_assertions::assert_eq;

    #[test]
    fn build_an_entry() {
        let entry = LruCacheItem::new("test", "value");
        assert_eq!(unsafe { entry.key.assume_init() }, "test");
        assert_eq!(unsafe { entry.value.assume_init() }, "value");
    }

    #[test]
    fn build_an_empty_cache() {
        let cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(5, 2).unwrap();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.weight(), 0);
    }

    #[test]
    fn add_to_the_cache() {
        let mut cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(5, 2).unwrap();
        let _ = cache.insert("foo", "aa");
        let _ = cache.insert("bar", "bb");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.weight(), 4);
    }

    #[test]
    fn replace_in_the_cache() {
        let mut cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(5, 2).unwrap();
        let _ = cache.insert("foo", "aa");
        let _ = cache.insert("bar", "bb");
        let _ = cache.insert("bar", "c");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.weight(), 3); // "bb" + "c", since "aa" should have been ejected.
    }

    #[test]
    fn eject_by_weight() {
        let mut cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(3, 4).unwrap();
        for i in &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"] {
            let _ = cache.insert(i, i);
        }
        let _ = cache.insert("z", "zzz");
        assert_eq!(cache.weight(), 12); // 3 * 4
        assert_eq!(cache.len(), 10); // three items should have been removed, then one added.
    }

    #[test]
    fn replace_by_weight() {
        let mut cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(3, 4).unwrap();
        for i in &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"] {
            let _ = cache.insert(i, i);
        }
        let _ = cache.insert("l", "zzz");
        assert_eq!(cache.weight(), 12); // 3 * 4
        assert_eq!(cache.len(), 10); // three items should have been removed, then one added.
    }

    #[test]
    fn delete_in_the_cache() {
        let mut cache: LruWeightedCache<&str, &str> = LruWeightedCache::new(5, 2).unwrap();
        let _ = cache.insert("foo", "aa");
        let _ = cache.insert("bar", "bb");
        cache.remove(&"bar");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.weight(), 2);
        assert!(cache.contains_key(&"foo"));
        assert!(!cache.contains_key(&"bar"));
        assert!(cache.get(&"foo") == Some(&"aa"));
        assert!(cache.get(&"bar").is_none());
    }

    #[test]
    fn catch_errant_nonsense() {
        let cache = LruWeightedCache::<&str, &str>::new(0, 0);
        match cache {
            Ok(_) => unreachable!(),
            Err(err) => assert_eq!(err, LruError::NonsenseParameters),
        }
    }
}
