// Persistent, structurally shared map keyed by `EntryId`.
//
// Why this exists: `EntryStore` publishes every mutation as a brand-new
// immutable `StoreSnapshot` (clone → mutate → `ArcSwap::store`), and readers
// keep whichever snapshot they loaded for as long as they like. With
// `BTreeMap` fields that clone copied every entry, every child list and every
// summary on every publication, so filling a store one listing at a time cost
// O(entries) per call — hydration was quadratic. Measured on mille 0.3.4:
// 5,500 entries in 0.45 s, 22,000 in 15 s, a 43,310-entry home folder in 44 s
// at 494 MB (petition M8).
//
// `IdMap` makes the clone O(1) — one `Arc` bump on the root — and a mutation
// O(depth), copying only the nodes on the path to the key it touches (each at
// most 32 slots). A publication that touches k keys therefore costs O(k), not
// O(n), and a retained older snapshot shares every untouched node with the
// newer one instead of owning a full copy.
//
// Shape: a bitmap-compressed radix trie on the raw id, five bits per level.
// `EntryId`s come from a monotonic counter, so live ids are dense and
// in-order iteration is a plain depth-first walk — the same ascending order
// `BTreeMap` iterated in, so nothing that walks a map (the resume writer, the
// reconfigure pass) sees a different sequence. Nodes store only their
// occupied slots (a 32-bit occupancy bitmap plus a packed `Vec`), so a sparse
// key set — directories only, say — costs what it holds rather than 32 slots
// per key, and an emptied node is pruned.
//
// The API mirrors the subset of `BTreeMap` the snapshot used, including
// `entry(..).or_insert(..)`, so call sites read the same. One difference:
// iteration yields keys by value (`EntryId` is `Copy`), because the trie
// stores no keys to lend a reference to — they are the path to the slot.

use std::fmt;
use std::ops::Index;
use std::sync::Arc;

use crate::entry::EntryId;

const BITS: u32 = 5;
const MASK: u64 = (1 << BITS) - 1;

/// Index of `key`'s slot in a node `level` levels above the leaves.
#[inline]
fn slot_of(key: u64, level: u32) -> u32 {
    ((key >> (BITS * level)) & MASK) as u32
}

/// Whether a trie `height` levels tall (0 = the root is a leaf) can hold `key`.
#[inline]
fn fits(key: u64, height: u32) -> bool {
    let bits = BITS * (height + 1);
    bits >= u64::BITS || key >> bits == 0
}

/// Occupied slots of one node, packed in slot order.
#[derive(Clone)]
struct Slots<T> {
    bitmap: u32,
    items: Vec<T>,
}

impl<T> Slots<T> {
    fn single(slot: u32, item: T) -> Self {
        Self {
            bitmap: 1 << slot,
            items: vec![item],
        }
    }

    /// `Ok(index)` when `slot` is occupied, else `Err(insertion index)`.
    #[inline]
    fn position(&self, slot: u32) -> Result<usize, usize> {
        let bit = 1u32 << slot;
        let index = (self.bitmap & (bit - 1)).count_ones() as usize;
        if self.bitmap & bit != 0 {
            Ok(index)
        } else {
            Err(index)
        }
    }

    #[inline]
    fn get(&self, slot: u32) -> Option<&T> {
        self.position(slot).ok().map(|index| &self.items[index])
    }

    #[inline]
    fn get_mut(&mut self, slot: u32) -> Option<&mut T> {
        match self.position(slot) {
            Ok(index) => Some(&mut self.items[index]),
            Err(_) => None,
        }
    }

    fn insert(&mut self, slot: u32, item: T) -> Option<T> {
        match self.position(slot) {
            Ok(index) => Some(std::mem::replace(&mut self.items[index], item)),
            Err(index) => {
                self.items.insert(index, item);
                self.bitmap |= 1 << slot;
                None
            }
        }
    }

    fn remove(&mut self, slot: u32) -> Option<T> {
        let index = self.position(slot).ok()?;
        self.bitmap &= !(1u32 << slot);
        Some(self.items.remove(index))
    }

    fn is_empty(&self) -> bool {
        self.bitmap == 0
    }
}

#[derive(Clone)]
enum Node<V> {
    /// Level ≥ 1: children one level down.
    Branch(Slots<Arc<Node<V>>>),
    /// Level 0: the values themselves.
    Leaf(Slots<V>),
}

impl<V> Node<V> {
    fn is_empty(&self) -> bool {
        match self {
            Node::Branch(slots) => slots.is_empty(),
            Node::Leaf(slots) => slots.is_empty(),
        }
    }

    /// A fresh chain from `level` down to the leaf holding `key`.
    fn path_to(level: u32, key: u64, value: V) -> Arc<Self> {
        let mut node = Arc::new(Node::Leaf(Slots::single(slot_of(key, 0), value)));
        for above in 1..=level {
            node = Arc::new(Node::Branch(Slots::single(slot_of(key, above), node)));
        }
        node
    }
}

/// Persistent map from `EntryId` to `V`. `clone` is O(1); see the module
/// comment for the cost model.
pub struct IdMap<V> {
    root: Option<Arc<Node<V>>>,
    /// Levels above the leaves; the root covers ids below 32^(height + 1).
    height: u32,
    len: usize,
}

impl<V> Clone for IdMap<V> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            height: self.height,
            len: self.len,
        }
    }
}

impl<V> Default for IdMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: fmt::Debug> fmt::Debug for IdMap<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<V> IdMap<V> {
    pub fn new() -> Self {
        Self {
            root: None,
            height: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    pub fn get(&self, key: &EntryId) -> Option<&V> {
        let key = key.0;
        if !fits(key, self.height) {
            return None;
        }
        let mut node = self.root.as_deref()?;
        let mut level = self.height;
        loop {
            match node {
                Node::Branch(slots) => {
                    node = slots.get(slot_of(key, level))?;
                    level -= 1;
                }
                Node::Leaf(slots) => return slots.get(slot_of(key, 0)),
            }
        }
    }

    #[inline]
    pub fn contains_key(&self, key: &EntryId) -> bool {
        self.get(key).is_some()
    }

    /// Ascending-id iteration, like `BTreeMap::iter`.
    pub fn iter(&self) -> Iter<'_, V> {
        let mut stack = Vec::with_capacity(self.height as usize + 1);
        if let Some(root) = self.root.as_deref() {
            stack.push(Frame::new(root, self.height, 0));
        }
        Iter {
            stack,
            remaining: self.len,
        }
    }

    pub fn keys(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.iter().map(|(_, value)| value)
    }
}

impl<V: Clone> IdMap<V> {
    pub fn insert(&mut self, key: EntryId, value: V) -> Option<V> {
        let key = key.0;
        let Some(root) = self.root.as_mut() else {
            let mut height = 0;
            while !fits(key, height) {
                height += 1;
            }
            self.height = height;
            self.root = Some(Node::path_to(height, key, value));
            self.len = 1;
            return None;
        };
        if !fits(key, self.height) {
            // Grow upward: the old root becomes slot 0 of a taller root. Every
            // key it held has zeros in the new top digit.
            let mut grown = root.clone();
            while !fits(key, self.height) {
                self.height += 1;
                grown = Arc::new(Node::Branch(Slots::single(0, grown)));
            }
            *root = grown;
        }

        let mut node = root;
        let mut level = self.height;
        let previous = loop {
            match Arc::make_mut(node) {
                Node::Leaf(slots) => break slots.insert(slot_of(key, 0), value),
                Node::Branch(slots) => {
                    let slot = slot_of(key, level);
                    if slots.get(slot).is_none() {
                        slots.insert(slot, Node::path_to(level - 1, key, value));
                        break None;
                    }
                    node = slots.get_mut(slot).expect("slot checked above");
                    level -= 1;
                }
            }
        };
        if previous.is_none() {
            self.len += 1;
        }
        previous
    }

    /// Mutable access to one value. Copies only the nodes on the path to the
    /// key, and nothing at all when the key is absent.
    pub fn get_mut(&mut self, key: &EntryId) -> Option<&mut V> {
        if !self.contains_key(key) {
            return None;
        }
        let key = key.0;
        let mut node = self.root.as_mut()?;
        let mut level = self.height;
        loop {
            match Arc::make_mut(node) {
                Node::Leaf(slots) => return slots.get_mut(slot_of(key, 0)),
                Node::Branch(slots) => {
                    node = slots.get_mut(slot_of(key, level))?;
                    level -= 1;
                }
            }
        }
    }

    pub fn remove(&mut self, key: &EntryId) -> Option<V> {
        if !self.contains_key(key) {
            return None;
        }
        let root = self.root.as_mut()?;
        let removed = remove_at(root, self.height, key.0);
        if removed.is_some() {
            self.len -= 1;
            if self.len == 0 {
                self.root = None;
                self.height = 0;
            }
        }
        removed
    }

    /// `BTreeMap::entry` for the two shapes the store uses:
    /// `entry(k).or_insert(v)` and `entry(k).or_default()`.
    pub fn entry(&mut self, key: EntryId) -> IdMapEntry<'_, V> {
        IdMapEntry { map: self, key }
    }

    /// Visit every value mutably, in ascending-id order. Copies every shared
    /// node — the cost of touching every value of a persistent map.
    pub fn for_each_value_mut(&mut self, mut visit: impl FnMut(&mut V)) {
        if let Some(root) = self.root.as_mut() {
            for_each_mut(root, &mut visit);
        }
    }
}

fn remove_at<V: Clone>(node: &mut Arc<Node<V>>, level: u32, key: u64) -> Option<V> {
    match Arc::make_mut(node) {
        Node::Leaf(slots) => slots.remove(slot_of(key, 0)),
        Node::Branch(slots) => {
            let slot = slot_of(key, level);
            let child = slots.get_mut(slot)?;
            let removed = remove_at(child, level - 1, key);
            if child.is_empty() {
                slots.remove(slot);
            }
            removed
        }
    }
}

fn for_each_mut<V: Clone>(node: &mut Arc<Node<V>>, visit: &mut impl FnMut(&mut V)) {
    match Arc::make_mut(node) {
        Node::Leaf(slots) => slots.items.iter_mut().for_each(visit),
        Node::Branch(slots) => {
            for child in &mut slots.items {
                for_each_mut(child, visit);
            }
        }
    }
}

impl<V> Index<&EntryId> for IdMap<V> {
    type Output = V;

    fn index(&self, key: &EntryId) -> &V {
        self.get(key).expect("no entry found for key")
    }
}

impl<'a, V> IntoIterator for &'a IdMap<V> {
    type Item = (EntryId, &'a V);
    type IntoIter = Iter<'a, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<V: Clone> FromIterator<(EntryId, V)> for IdMap<V> {
    fn from_iter<I: IntoIterator<Item = (EntryId, V)>>(iter: I) -> Self {
        let mut map = Self::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

pub struct IdMapEntry<'a, V> {
    map: &'a mut IdMap<V>,
    key: EntryId,
}

impl<'a, V: Clone> IdMapEntry<'a, V> {
    pub fn or_insert(self, default: V) -> &'a mut V {
        self.or_insert_with(|| default)
    }

    pub fn or_insert_with(self, default: impl FnOnce() -> V) -> &'a mut V {
        if !self.map.contains_key(&self.key) {
            self.map.insert(self.key, default());
        }
        self.map
            .get_mut(&self.key)
            .expect("entry was inserted above")
    }
}

impl<'a, V: Clone + Default> IdMapEntry<'a, V> {
    pub fn or_default(self) -> &'a mut V {
        self.or_insert_with(V::default)
    }
}

struct Frame<'a, V> {
    node: &'a Node<V>,
    level: u32,
    prefix: u64,
    /// Occupied slots not yet visited.
    pending: u32,
    /// Index of the next occupied slot's item.
    next: usize,
}

impl<'a, V> Frame<'a, V> {
    fn new(node: &'a Node<V>, level: u32, prefix: u64) -> Self {
        let pending = match node {
            Node::Branch(slots) => slots.bitmap,
            Node::Leaf(slots) => slots.bitmap,
        };
        Self {
            node,
            level,
            prefix,
            pending,
            next: 0,
        }
    }
}

pub struct Iter<'a, V> {
    stack: Vec<Frame<'a, V>>,
    remaining: usize,
}

impl<'a, V> Iterator for Iter<'a, V> {
    type Item = (EntryId, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if frame.pending == 0 {
                self.stack.pop();
                continue;
            }
            let slot = frame.pending.trailing_zeros() as u64;
            frame.pending &= frame.pending - 1;
            let index = frame.next;
            frame.next += 1;
            let key = frame.prefix | (slot << (BITS * frame.level));
            match frame.node {
                Node::Leaf(slots) => {
                    self.remaining -= 1;
                    return Some((EntryId(key), &slots.items[index]));
                }
                Node::Branch(slots) => {
                    let child = slots.items[index].as_ref();
                    let level = frame.level - 1;
                    self.stack.push(Frame::new(child, level, key));
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<V> ExactSizeIterator for Iter<'_, V> {}

/// `BTreeSet<EntryId>` counterpart of [`IdMap`], with the same cost model.
#[derive(Clone, Default)]
pub struct IdSet(IdMap<()>);

impl fmt::Debug for IdSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl IdSet {
    pub fn new() -> Self {
        Self(IdMap::new())
    }

    /// `true` when `id` was not already present, like `BTreeSet::insert`.
    pub fn insert(&mut self, id: EntryId) -> bool {
        self.0.insert(id, ()).is_none()
    }

    /// `true` when `id` was present, like `BTreeSet::remove`.
    pub fn remove(&mut self, id: &EntryId) -> bool {
        self.0.remove(id).is_some()
    }

    #[inline]
    pub fn contains(&self, id: &EntryId) -> bool {
        self.0.contains_key(id)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = EntryId> + '_ {
        self.0.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    fn id(raw: u64) -> EntryId {
        EntryId(raw)
    }

    #[test]
    fn empty_map_answers_nothing() {
        let map: IdMap<u32> = IdMap::new();
        assert!(map.is_empty());
        assert_eq!(map.get(&id(0)), None);
        assert_eq!(map.get(&id(u64::MAX)), None);
        assert_eq!(map.iter().count(), 0);
    }

    #[test]
    fn insert_get_replace_and_remove_round_trip() {
        let mut map = IdMap::new();
        assert_eq!(map.insert(id(7), "a"), None);
        assert_eq!(map.insert(id(7), "b"), Some("a"));
        assert_eq!(map.len(), 1);
        assert_eq!(map[&id(7)], "b");
        assert_eq!(map.remove(&id(7)), Some("b"));
        assert_eq!(map.remove(&id(7)), None);
        assert!(map.is_empty());
        assert!(map.root.is_none(), "an emptied map releases its nodes");
    }

    #[test]
    fn keys_across_the_whole_id_range_grow_the_trie_and_stay_ordered() {
        let keys = [
            0,
            1,
            31,
            32,
            1023,
            1024,
            (1 << 30) + 5,
            crate::entry::ENTRY_ID_MAX - 1,
            u64::MAX,
        ];
        let mut map = IdMap::new();
        for (index, &key) in keys.iter().enumerate().rev() {
            map.insert(id(key), index);
        }
        let iterated: Vec<u64> = map.keys().map(|key| key.0).collect();
        assert_eq!(iterated, keys);
        for (index, &key) in keys.iter().enumerate() {
            assert_eq!(map.get(&id(key)), Some(&index));
        }
        assert_eq!(map.get(&id(33)), None);
    }

    #[test]
    fn a_clone_is_a_stable_snapshot_while_the_original_mutates() {
        let mut map: IdMap<u64> = (0..5_000).map(|raw| (id(raw), raw)).collect();
        let retained = map.clone();
        for raw in (0..5_000).step_by(3) {
            map.remove(&id(raw));
        }
        *map.get_mut(&id(1)).unwrap() += 100;
        *map.entry(id(9_999)).or_insert(0) += 1;
        map.for_each_value_mut(|value| *value += 1);

        assert_eq!(retained.len(), 5_000);
        assert!(retained.iter().all(|(key, value)| key.0 == *value));
        assert_eq!(map.get(&id(1)), Some(&102));
        assert_eq!(map.get(&id(9_999)), Some(&2));
        assert_eq!(map.get(&id(3)), None);
    }

    #[test]
    fn a_mutation_copies_only_the_path_it_touches() {
        let mut map: IdMap<u64> = (0..32 * 32 * 4).map(|raw| (id(raw), raw)).collect();
        let retained = map.clone();
        map.insert(id(5), 0);
        let (Some(Node::Branch(old_top)), Some(Node::Branch(new_top))) =
            (retained.root.as_deref(), map.root.as_deref())
        else {
            panic!("a 4,096-key map has branch roots");
        };
        // Slot 0 holds the edited key: copied. Slots 1..3 are untouched: shared.
        assert!(!Arc::ptr_eq(&old_top.items[0], &new_top.items[0]));
        for index in 1..old_top.items.len() {
            assert!(Arc::ptr_eq(&old_top.items[index], &new_top.items[index]));
        }
    }

    #[test]
    fn absent_key_mutators_copy_nothing() {
        let mut map: IdMap<u64> = (0..100).map(|raw| (id(raw), raw)).collect();
        let retained = map.clone();
        assert!(map.get_mut(&id(1_000)).is_none());
        assert!(map.remove(&id(1_000)).is_none());
        assert!(Arc::ptr_eq(
            retained.root.as_ref().unwrap(),
            map.root.as_ref().unwrap()
        ));
    }

    #[test]
    fn id_set_mirrors_btree_set_results() {
        let mut set = IdSet::new();
        assert!(set.insert(id(4)));
        assert!(!set.insert(id(4)));
        assert!(set.insert(id(2)));
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![id(2), id(4)]);
        assert!(set.remove(&id(4)));
        assert!(!set.remove(&id(4)));
        assert!(set.contains(&id(2)));
        assert_eq!(set.len(), 1);
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(u64, u32),
        Remove(u64),
        Bump(u64),
        Upsert(u64),
        Snapshot,
    }

    fn key_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![
            0u64..64,
            0u64..5_000,
            Just(crate::entry::ENTRY_ID_MAX - 1),
            any::<u64>(),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (key_strategy(), any::<u32>()).prop_map(|(key, value)| Op::Insert(key, value)),
            2 => key_strategy().prop_map(Op::Remove),
            1 => key_strategy().prop_map(Op::Bump),
            1 => key_strategy().prop_map(Op::Upsert),
            1 => Just(Op::Snapshot),
        ]
    }

    proptest! {
        /// Every operation agrees with `BTreeMap`, and every clone taken along
        /// the way still equals the model as it was when the clone was taken.
        #[test]
        fn behaves_like_btree_map_and_clones_stay_frozen(ops in prop::collection::vec(op_strategy(), 1..400)) {
            let mut map: IdMap<u32> = IdMap::new();
            let mut model: BTreeMap<u64, u32> = BTreeMap::new();
            let mut frozen: Vec<(IdMap<u32>, BTreeMap<u64, u32>)> = Vec::new();
            for op in ops {
                match op {
                    Op::Insert(key, value) => {
                        prop_assert_eq!(map.insert(id(key), value), model.insert(key, value));
                    }
                    Op::Remove(key) => {
                        prop_assert_eq!(map.remove(&id(key)), model.remove(&key));
                    }
                    Op::Bump(key) => {
                        let ours = map.get_mut(&id(key)).map(|value| {
                            *value = value.wrapping_add(1);
                            *value
                        });
                        let theirs = model.get_mut(&key).map(|value| {
                            *value = value.wrapping_add(1);
                            *value
                        });
                        prop_assert_eq!(ours, theirs);
                    }
                    Op::Upsert(key) => {
                        *map.entry(id(key)).or_default() += 7;
                        *model.entry(key).or_default() += 7;
                    }
                    Op::Snapshot => frozen.push((map.clone(), model.clone())),
                }
                prop_assert_eq!(map.len(), model.len());
            }
            let ours: Vec<(u64, u32)> = map.iter().map(|(key, value)| (key.0, *value)).collect();
            let theirs: Vec<(u64, u32)> = model.iter().map(|(key, value)| (*key, *value)).collect();
            prop_assert_eq!(ours, theirs);
            for (key, value) in &model {
                prop_assert_eq!(map.get(&id(*key)), Some(value));
            }
            for (snapshot, expected) in frozen {
                let ours: Vec<(u64, u32)> =
                    snapshot.iter().map(|(key, value)| (key.0, *value)).collect();
                let theirs: Vec<(u64, u32)> =
                    expected.iter().map(|(key, value)| (*key, *value)).collect();
                prop_assert_eq!(snapshot.len(), expected.len());
                prop_assert_eq!(ours, theirs);
            }
        }
    }
}
