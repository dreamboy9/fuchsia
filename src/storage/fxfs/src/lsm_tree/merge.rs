// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::lsm_tree::types::{
        Item, ItemRef, Key, Layer, LayerIterator, LayerIteratorMut, NextKey, OrdLowerBound, Value,
    },
    anyhow::Error,
    async_trait::async_trait,
    futures::try_join,
    std::{
        cmp::Ordering, collections::BinaryHeap, convert::From, fmt::Debug, fmt::Write, ops::Bound,
    },
};

#[derive(Debug, Eq, PartialEq)]
pub enum ItemOp<K, V> {
    /// Keeps the item to be presented to the merger subsequently with a new merge pair.
    Keep,

    /// Discards the item and moves on to the next item in the respective layer.
    Discard,

    /// Replaces the item with something new which will be presented to the merger subsequently with
    /// a new pair.
    Replace(Item<K, V>),
}

#[derive(Debug, Eq, PartialEq)]
pub enum MergeResult<K, V> {
    /// Emits the left item unchanged. Keeps the right item. This is the common case. Once an item
    /// has been emitted, it will never be seen again by the merge function.
    EmitLeft,

    /// All other merge results are covered by the following. Take care when replacing items
    /// that you replace the correct item. The merger will never merge two items together from
    /// the same layer. Consider the following scenario:
    ///
    ///        +-----------+              +-----------+
    /// 0:     |    A      |              |    C      |
    ///        +-----------+--------------+-----------+
    /// 1:                 |      B       |
    ///                    +--------------+
    ///
    /// Let's say that all three items can be merged together. The merge function will first be
    /// presented with items A and B, at which point it has the option of replacing the left item
    /// (i.e. A, in layer 0) or the right item (i.e. B in layer 1). However, if you replace the left
    /// item, the merge function will not then be given the opportunity to merge it with C, so the
    /// correct thing to do in this case is to replace the right item B in layer 1, and discard the
    /// left item. A rule you can use is that you should avoid replacing an item with another item
    /// whose upper bound exceeds that of the item you are replacing.
    ///
    /// There are some combinations that might lead to infinite loops (e.g. None, Keep, Keep) and
    /// should obviously be avoided.
    Other { emit: Option<Item<K, V>>, left: ItemOp<K, V>, right: ItemOp<K, V> },
}

/// Users must provide a merge function which will take pairs of items, left and right, and return a
/// merge result. The left item's key will either be less than the right item's key, or if they are
/// the same, then the left item will be in a lower layer index (lower layer indexes indicate more
/// recent entries). The last remaining item is always emitted.
pub type MergeFn<K, V> =
    fn(&MergeLayerIterator<'_, K, V>, &MergeLayerIterator<'_, K, V>) -> MergeResult<K, V>;

pub enum MergeItem<K, V> {
    None,
    Item(Item<K, V>),
    Iter,
}

enum RawIterator<'a, K, V> {
    None,
    Const(Box<dyn LayerIterator<K, V> + 'a>),
    Mut(Box<dyn LayerIteratorMut<K, V> + 'a>),
}

// An iterator that keeps track of where we are for each of the layers. We push these onto a
// min-heap.
pub struct MergeLayerIterator<'a, K, V> {
    layer: Option<&'a dyn Layer<K, V>>,

    // The underlying iterator.
    iter: RawIterator<'a, K, V>,

    // The index of the layer this is for.
    pub layer_index: u16,

    // The item we are currently pointing at.
    item: MergeItem<K, V>,
}

impl<'a, K, V> MergeLayerIterator<'a, K, V> {
    fn new(layer_index: u16, layer: &'a dyn Layer<K, V>) -> Self {
        MergeLayerIterator {
            layer: Some(layer),
            iter: RawIterator::None,
            layer_index,
            item: MergeItem::None,
        }
    }

    pub fn new_with_item(layer_index: u16, item: MergeItem<K, V>) -> Self {
        MergeLayerIterator { layer: None, iter: RawIterator::None, layer_index, item }
    }

    pub fn item(&self) -> ItemRef<'_, K, V> {
        match &self.item {
            MergeItem::None => panic!("No item!"),
            MergeItem::Item(ref item) => ItemRef::from(item),
            MergeItem::Iter => self.iter().get().unwrap(),
        }
    }

    pub fn key(&self) -> &K {
        return self.item().key;
    }

    pub fn value(&self) -> &V {
        return self.item().value;
    }

    fn iter(&self) -> &dyn LayerIterator<K, V> {
        match &self.iter {
            RawIterator::None => panic!("No iterator!"),
            RawIterator::Const(iter) => iter.as_ref(),
            RawIterator::Mut(iter) => iter.as_iterator(),
        }
    }

    fn iter_mut(&mut self) -> &mut dyn LayerIterator<K, V> {
        match &mut self.iter {
            RawIterator::None => panic!("No iterator!"),
            RawIterator::Const(iter) => iter.as_mut(),
            RawIterator::Mut(iter) => iter.as_iterator_mut(),
        }
    }

    fn set_item_from_iter(&mut self) {
        self.item = {
            if self.iter().get().is_none() {
                MergeItem::None
            } else {
                match self.iter {
                    RawIterator::None => unreachable!(),
                    RawIterator::Const(_) => MergeItem::Iter,
                    RawIterator::Mut(_) => MergeItem::Iter,
                }
            }
        }
    }

    fn take_item(&mut self) -> Option<Item<K, V>> {
        if let MergeItem::Item(_) = self.item {
            let mut item = MergeItem::None;
            std::mem::swap(&mut self.item, &mut item);
            if let MergeItem::Item(item) = item {
                Some(item)
            } else {
                unreachable!();
            }
        } else {
            None
        }
    }

    async fn advance(&mut self) -> Result<(), Error> {
        self.iter_mut().advance().await?;
        self.set_item_from_iter();
        Ok(())
    }

    fn replace(&mut self, item: Item<K, V>) {
        self.item = MergeItem::Item(item);
    }

    fn is_some(&self) -> bool {
        match self.item {
            MergeItem::None => false,
            _ => true,
        }
    }

    // This function exists so that we can advance multiple iterators concurrently using, say,
    // try_join!.
    async fn maybe_discard(&mut self, op: &ItemOp<K, V>) -> Result<(), Error> {
        if let ItemOp::Discard = op {
            self.advance().await?;
        }
        Ok(())
    }

    fn erase(&mut self) {
        if let RawIterator::Mut(iter) = &mut self.iter {
            iter.erase();
        } else {
            panic!("No iterator!");
        }
    }

    fn insert(&mut self, item: Item<K, V>) {
        if let RawIterator::Mut(iter) = &mut self.iter {
            iter.insert(item);
        } else {
            panic!("No iterator!");
        }
    }
}

// -- Ord and friends --
impl<K: OrdLowerBound, V> Ord for MergeLayerIterator<'_, K, V> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering because we want min-heap not max-heap.
        other.key().cmp_lower_bound(self.key()).then(other.layer_index.cmp(&self.layer_index))
    }
}
impl<K: OrdLowerBound, V> PartialOrd for MergeLayerIterator<'_, K, V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        return Some(self.cmp(other));
    }
}
impl<K: OrdLowerBound, V> PartialEq for MergeLayerIterator<'_, K, V> {
    fn eq(&self, other: &Self) -> bool {
        return self.cmp(other) == Ordering::Equal;
    }
}
impl<K: OrdLowerBound, V> Eq for MergeLayerIterator<'_, K, V> {}

// As we merge items, the current item can be an item that has been replaced (and later emitted) by
// the merge function, or an item referenced by an iterator, or nothing.
enum CurrentItem<'a, 'b, K, V> {
    None,
    Item(Item<K, V>),
    Iterator(&'a mut MergeLayerIterator<'b, K, V>),
}

impl<'a, 'b, K, V> CurrentItem<'a, 'b, K, V> {
    // Takes the iterator if one is present and replaces the current item with None; otherwise,
    // leaves the current item untouched.
    fn take_iterator(&mut self) -> Option<&'a mut MergeLayerIterator<'b, K, V>> {
        if let CurrentItem::Iterator(_) = self {
            let mut result = CurrentItem::None;
            std::mem::swap(self, &mut result);
            if let CurrentItem::Iterator(iter) = result {
                Some(iter)
            } else {
                unreachable!();
            }
        } else {
            None
        }
    }
}

impl<'a, K, V> From<&'a CurrentItem<'_, '_, K, V>> for Option<ItemRef<'a, K, V>> {
    fn from(iter: &'a CurrentItem<'_, '_, K, V>) -> Option<ItemRef<'a, K, V>> {
        match iter {
            CurrentItem::None => None,
            CurrentItem::Iterator(iterator) => Some(iterator.item()),
            CurrentItem::Item(item) => Some(item.into()),
        }
    }
}

/// Merger is the main entry point to merging.
pub struct Merger<'a, K, V> {
    // A buffer containing all the MergeLayerIterator objects.
    iterators: Vec<MergeLayerIterator<'a, K, V>>,

    // The function to be used for merging items.
    merge_fn: MergeFn<K, V>,

    // If true, additional logging is enabled.
    trace: bool,
}

impl<'a, K: Key + NextKey + OrdLowerBound, V: Value> Merger<'a, K, V> {
    pub(super) fn new(layers: &[&'a dyn Layer<K, V>], merge_fn: MergeFn<K, V>) -> Merger<'a, K, V> {
        Merger {
            iterators: layers
                .iter()
                .enumerate()
                .map(|(index, layer)| MergeLayerIterator::new(index as u16, *layer))
                .collect(),
            merge_fn: merge_fn,
            trace: false,
        }
    }

    /// Seek searches for |bound|.  If |bound| is Bound::Unbounded, the iterator is positioned on
    /// the first item.  If |bound| is Bound::Included(key), the iterator is positioned on an item
    /// such that item.key >= key.  In the latter case, a full merge might not occur; only the
    /// layers that need to be consulted to satisfy the query will occur.
    pub async fn seek(&mut self, bound: Bound<&K>) -> Result<MergerIterator<'_, 'a, K, V>, Error> {
        let pending_iterators = self.iterators.iter_mut().rev().collect();
        let mut merger_iter = MergerIterator {
            merge_fn: self.merge_fn,
            pending_iterators,
            heap: BinaryHeap::new(),
            item: CurrentItem::None,
            trace: self.trace,
            history: String::new(),
        };
        merger_iter.seek(bound).await?;
        Ok(merger_iter)
    }

    pub fn set_trace(&mut self, v: bool) {
        self.trace = v;
    }
}

/// This is an iterator that will allow iteration over merged layers.  The primary interface is via
/// the LayerIterator trait.
pub struct MergerIterator<'a, 'b, K, V> {
    merge_fn: MergeFn<K, V>,

    // Iterators that we have not yet pushed onto the heap.
    pending_iterators: Vec<&'a mut MergeLayerIterator<'b, K, V>>,

    // A heap with the merge iterators.
    heap: BinaryHeap<&'a mut MergeLayerIterator<'b, K, V>>,

    // The current item.
    item: CurrentItem<'a, 'b, K, V>,

    // If true, logs regarding merger behaviour are appended to history.
    trace: bool,

    // Holds trace information if trace is true.
    pub history: String,
}

impl<'a, 'b, K: Key + NextKey + OrdLowerBound, V: Value> MergerIterator<'a, 'b, K, V> {
    async fn seek(&mut self, bound: Bound<&K>) -> Result<(), Error> {
        match bound {
            Bound::Unbounded => self.advance_impl(None, Bound::Unbounded).await,
            Bound::Included(key) => self.advance_impl(Some(key), bound).await,
            Bound::Excluded(_) => panic!("Excluded bounds not supported!"),
        }
    }

    // Merges items from an array of layers using the provided merge function. The merge function is
    // repeatedly provided the lowest and the second lowest element, if one exists. In cases where
    // the two lowest elements compare equal, the element with the lowest layer (i.e. whichever
    // comes first in the layers array) will come first.  If next_key is set, the merger will stop
    // querying more layers as soon as a key is encountered that equals or precedes it.  If next_key
    // is None, then all layers are queried.  next_key_bound is the bound to search for if another
    // layer does need to be consulted.  See the comment for the NextKey trait.
    async fn advance_impl(
        &mut self,
        next_key: Option<&K>,
        next_key_bound: Bound<&K>,
    ) -> Result<(), Error> {
        if let Some(iterator) = self.item.take_iterator() {
            iterator.advance().await?;
            if iterator.is_some() {
                self.heap.push(iterator);
            }
        }
        while !self.pending_iterators.is_empty()
            && (self.heap.is_empty()
                || next_key.is_none()
                || self.heap.peek().unwrap().key().cmp_lower_bound(next_key.as_ref().unwrap())
                    == Ordering::Greater)
        {
            let iter = self.pending_iterators.pop().unwrap();
            let sub_iter = iter.layer.as_ref().unwrap().seek(next_key_bound).await?;
            if self.trace {
                writeln!(
                    self.history,
                    "merger: search for {:?}, found {:?}",
                    next_key_bound,
                    sub_iter.get()
                )
                .unwrap();
            }
            iter.iter = RawIterator::Const(sub_iter);
            iter.set_item_from_iter();
            if iter.is_some() {
                self.heap.push(iter);
            }
        }
        while !self.heap.is_empty() {
            let lowest = self.heap.pop().unwrap();
            let maybe_second_lowest = self.heap.pop();
            if let Some(second_lowest) = maybe_second_lowest {
                let result = (self.merge_fn)(&lowest, &second_lowest);
                if self.trace {
                    writeln!(
                        self.history,
                        "merge {:?}, {:?} -> {:?}",
                        lowest.item(),
                        second_lowest.item(),
                        result
                    )
                    .unwrap();
                }
                match result {
                    MergeResult::EmitLeft => {
                        self.heap.push(second_lowest);
                        self.item = CurrentItem::Iterator(lowest);
                        return Ok(());
                    }
                    MergeResult::Other { emit, left, right } => {
                        try_join!(
                            lowest.maybe_discard(&left),
                            second_lowest.maybe_discard(&right)
                        )?;
                        self.update_item(lowest, left);
                        self.update_item(second_lowest, right);
                        if let Some(emit) = emit {
                            self.item = CurrentItem::Item(emit);
                            return Ok(());
                        }
                    }
                }
            } else {
                self.item = CurrentItem::Iterator(lowest);
                return Ok(());
            }
        }
        self.item = CurrentItem::None;
        Ok(())
    }

    // Updates the merge iterator depending on |op|. If discarding, the iterator should have already
    // been advanced.
    fn update_item(&mut self, item: &'a mut MergeLayerIterator<'b, K, V>, op: ItemOp<K, V>) {
        match op {
            ItemOp::Keep => self.heap.push(item),
            ItemOp::Discard => {
                // The iterator should have already been advanced.
                if item.is_some() {
                    self.heap.push(item);
                }
            }
            ItemOp::Replace(replacement) => {
                item.replace(replacement);
                self.heap.push(item);
            }
        }
    }
}

#[async_trait]
impl<'a, K: Key + NextKey + OrdLowerBound, V: Value> LayerIterator<K, V>
    for MergerIterator<'a, '_, K, V>
{
    async fn advance(&mut self) -> Result<(), Error> {
        let current_key;
        let mut next_key = None;
        let mut next_key_bound = Bound::Unbounded;
        if !self.pending_iterators.is_empty() {
            if let Some(ItemRef { key, .. }) = self.get() {
                next_key = key.next_key();
                match next_key {
                    None => {
                        // If there is no next key, we must now query all layers and the key we
                        // search for is the immediate successor of the current key.
                        current_key = Some(key.clone());
                        next_key_bound = Bound::Excluded(current_key.as_ref().unwrap());
                    }
                    Some(ref key) => next_key_bound = Bound::Included(key),
                }
            }
        }
        self.advance_impl(next_key.as_ref(), next_key_bound).await
    }

    fn get(&self) -> Option<ItemRef<'_, K, V>> {
        (&self.item).into()
    }
}

// Merges the given item into a mutable layer.
pub(super) async fn merge_into<K: Debug + OrdLowerBound, V: Debug>(
    mut_iter: Box<dyn LayerIteratorMut<K, V> + '_>,
    item: Item<K, V>,
    merge_fn: MergeFn<K, V>,
) -> Result<(), Error> {
    let merge_item = if mut_iter.get().is_some() { MergeItem::Iter } else { MergeItem::None };
    let mut mut_merge_iter = MergeLayerIterator {
        layer: None,
        iter: RawIterator::Mut(mut_iter),
        layer_index: 1,
        item: merge_item,
    };
    let mut item_merge_iter = MergeLayerIterator::new_with_item(0, MergeItem::Item(item));
    while mut_merge_iter.is_some() && item_merge_iter.is_some() {
        if mut_merge_iter > item_merge_iter {
            // In this branch the mutable layer is left and the item we're merging-in is right.
            let merge_result = merge_fn(&mut_merge_iter, &item_merge_iter);
            log::debug!(
                "(1) merge for {:?} {:?} -> {:?}",
                mut_merge_iter.key(),
                item_merge_iter.key(),
                merge_result
            );
            match merge_result {
                MergeResult::EmitLeft => {
                    if let Some(item) = mut_merge_iter.take_item() {
                        mut_merge_iter.insert(item);
                        mut_merge_iter.set_item_from_iter();
                    } else {
                        mut_merge_iter.advance().await?;
                    }
                }
                MergeResult::Other { emit, left, right } => {
                    if let Some(emit) = emit {
                        mut_merge_iter.insert(emit);
                    }
                    match left {
                        ItemOp::Keep => {}
                        ItemOp::Discard => {
                            if let MergeItem::Iter = mut_merge_iter.item {
                                mut_merge_iter.erase();
                            }
                            mut_merge_iter.set_item_from_iter();
                        }
                        ItemOp::Replace(item) => {
                            if let MergeItem::Iter = mut_merge_iter.item {
                                mut_merge_iter.erase();
                            }
                            mut_merge_iter.item = MergeItem::Item(item)
                        }
                    }
                    match right {
                        ItemOp::Keep => {}
                        ItemOp::Discard => item_merge_iter.item = MergeItem::None,
                        ItemOp::Replace(item) => item_merge_iter.item = MergeItem::Item(item),
                    }
                }
            }
        } else {
            // In this branch, the item we're merging-in is left and the mutable layer is right.
            let merge_result = merge_fn(&item_merge_iter, &mut_merge_iter);
            log::debug!(
                "(2) merge for {:?} {:?} -> {:?}",
                item_merge_iter.key(),
                mut_merge_iter.key(),
                merge_result
            );
            match merge_result {
                MergeResult::EmitLeft => break, // Item is inserted outside the loop
                MergeResult::Other { emit, left, right } => {
                    if let Some(emit) = emit {
                        mut_merge_iter.insert(emit);
                    }
                    match left {
                        ItemOp::Keep => {}
                        ItemOp::Discard => item_merge_iter.item = MergeItem::None,
                        ItemOp::Replace(item) => item_merge_iter.item = MergeItem::Item(item),
                    }
                    match right {
                        ItemOp::Keep => {}
                        ItemOp::Discard => {
                            if let MergeItem::Iter = mut_merge_iter.item {
                                mut_merge_iter.erase();
                            }
                            mut_merge_iter.set_item_from_iter();
                        }
                        ItemOp::Replace(item) => {
                            if let MergeItem::Iter = mut_merge_iter.item {
                                mut_merge_iter.erase();
                            }
                            mut_merge_iter.item = MergeItem::Item(item)
                        }
                    }
                }
            }
        }
    } // while ...
      // The only way we could get here with both items is via the break above, so we know the
      // correct order required here.
    if let MergeItem::Item(item) = item_merge_iter.item {
        mut_merge_iter.insert(item);
    }
    if let Some(item) = mut_merge_iter.take_item() {
        mut_merge_iter.insert(item);
    }
    if let RawIterator::Mut(mut iter) = mut_merge_iter.iter {
        iter.commit_and_wait().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::{
            ItemOp::{Discard, Keep, Replace},
            MergeResult, Merger,
        },
        crate::lsm_tree::{
            skip_list_layer::SkipListLayer,
            types::{
                IntoLayerRefs, Item, ItemRef, Key, Layer, LayerIterator, MutableLayer, NextKey,
                OrdLowerBound, OrdUpperBound,
            },
        },
        fuchsia_async as fasync,
        rand::Rng,
        std::ops::{Bound, Range},
    };

    #[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
    struct TestKey(Range<u64>);

    impl NextKey for TestKey {
        fn next_key(&self) -> Option<Self> {
            Some(TestKey(self.0.end..self.0.end + 1))
        }
    }

    impl OrdUpperBound for TestKey {
        fn cmp_upper_bound(&self, other: &TestKey) -> std::cmp::Ordering {
            self.0.end.cmp(&other.0.end)
        }
    }

    impl OrdLowerBound for TestKey {
        fn cmp_lower_bound(&self, other: &Self) -> std::cmp::Ordering {
            self.0.start.cmp(&other.0.start)
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_emit_left() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::EmitLeft);
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_other_emit() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::Other {
                emit: Some(Item::new(TestKey(3..3), 3)),
                left: Discard,
                right: Discard,
            });
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(3..3), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_replace_left() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::Other {
                emit: None,
                left: Replace(Item::new(TestKey(3..3), 3)),
                right: Discard,
            });
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        // The merger should replace the left item and then after discarding the right item, it
        // should emit the replacement.
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(3..3), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_replace_right() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::Other {
                emit: None,
                left: Discard,
                right: Replace(Item::new(TestKey(3..3), 3)),
            });
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        // The merger should replace the right item and then after discarding the left item, it
        // should emit the replacement.
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(3..3), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_left_less_than_right() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |left, right| {
            assert_eq!((left.key(), left.value()), (&TestKey(1..1), &1));
            assert_eq!((right.key(), right.value()), (&TestKey(2..2), &2));
            MergeResult::EmitLeft
        });
        merger.seek(Bound::Unbounded).await.expect("seek failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_left_equals_right() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let item = Item::new(TestKey(1..1), 1);
        skip_lists[0].insert(item.clone()).await;
        skip_lists[1].insert(item.clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |left, right| {
            assert_eq!((left.key(), left.value()), (&TestKey(1..1), &1));
            assert_eq!((left.key(), left.value()), (&TestKey(1..1), &1));
            assert_eq!(left.layer_index, 0);
            assert_eq!(right.layer_index, 1);
            MergeResult::EmitLeft
        });
        merger.seek(Bound::Unbounded).await.expect("seek failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_keep() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |left, right| {
            if left.key() == &TestKey(1..1) {
                MergeResult::Other {
                    emit: None,
                    left: Replace(Item::new(TestKey(3..3), 3)),
                    right: Keep,
                }
            } else {
                assert_eq!(left.key(), &TestKey(2..2));
                assert_eq!(right.key(), &TestKey(3..3));
                MergeResult::Other { emit: None, left: Discard, right: Keep }
            }
        });
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        // The merger should first replace left and then it should call the merger again with 2 & 3
        // and end up just keeping 3.
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(3..3), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_10_layers() {
        let skip_lists: Vec<_> = (0..10).map(|_| SkipListLayer::new(100)).collect();
        let mut rng = rand::thread_rng();
        for i in 0..100 {
            skip_lists[rng.gen_range(0, 10) as usize].insert(Item::new(TestKey(i..i), i)).await;
        }
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::EmitLeft);
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        for i in 0..100 {
            let ItemRef { key, value } = iter.get().expect("missing item");
            assert_eq!((key, value), (&TestKey(i..i), &i));
            iter.advance().await.unwrap();
        }
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_uses_cmp_lower_bound() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..10), 1), Item::new(TestKey(2..3), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::EmitLeft);
        let mut iter = merger.seek(Bound::Unbounded).await.expect("seek failed");

        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_emit_left() {
        let skip_list = SkipListLayer::new(100);
        let items =
            [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2), Item::new(TestKey(3..3), 3)];
        skip_list.insert(items[0].clone()).await;
        skip_list.insert(items[2].clone()).await;
        skip_list
            .merge_into(items[1].clone(), &items[0].key, |_left, _right| MergeResult::EmitLeft)
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[2].key, &items[2].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_emit_last_after_replacing() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_list.insert(items[0].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |left, right| {
                if left.key() == &TestKey(1..1) {
                    assert_eq!(right.key(), &TestKey(2..2));
                    MergeResult::Other {
                        emit: None,
                        left: Replace(Item::new(TestKey(3..3), 3)),
                        right: Keep,
                    }
                } else {
                    assert_eq!(left.key(), &TestKey(2..2));
                    assert_eq!(right.key(), &TestKey(3..3));
                    MergeResult::EmitLeft
                }
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(3..3), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_emit_left_after_replacing() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(3..3), 3)];
        skip_list.insert(items[0].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |left, right| {
                if left.key() == &TestKey(1..1) {
                    assert_eq!(right.key(), &TestKey(3..3));
                    MergeResult::Other {
                        emit: None,
                        left: Replace(Item::new(TestKey(2..2), 2)),
                        right: Keep,
                    }
                } else {
                    assert_eq!(left.key(), &TestKey(2..2));
                    assert_eq!(right.key(), &TestKey(3..3));
                    MergeResult::EmitLeft
                }
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(2..2), &2));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    // This tests emitting in both branches of merge_into, and most of the discard paths.
    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_emit_other_and_discard() {
        let skip_list = SkipListLayer::new(100);
        let items =
            [Item::new(TestKey(1..1), 1), Item::new(TestKey(3..3), 3), Item::new(TestKey(5..5), 3)];
        skip_list.insert(items[0].clone()).await;
        skip_list.insert(items[2].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |left, right| {
                if left.key() == &TestKey(1..1) {
                    // This tests the top branch in merge_into.
                    assert_eq!(right.key(), &TestKey(3..3));
                    MergeResult::Other {
                        emit: Some(Item::new(TestKey(2..2), 2)),
                        left: Discard,
                        right: Keep,
                    }
                } else {
                    // This tests the bottom branch in merge_into.
                    assert_eq!(left.key(), &TestKey(3..3));
                    assert_eq!(right.key(), &TestKey(5..5));
                    MergeResult::Other {
                        emit: Some(Item::new(TestKey(4..4), 4)),
                        left: Discard,
                        right: Discard,
                    }
                }
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(2..2), &2));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(4..4), &4));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    // This tests replacing the item and discarding the right item (the one remaining untested
    // discard path) in the top branch in merge_into.
    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_replace_and_discard() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(3..3), 3)];
        skip_list.insert(items[0].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |_left, _right| MergeResult::Other {
                emit: Some(Item::new(TestKey(2..2), 2)),
                left: Replace(Item::new(TestKey(4..4), 4)),
                right: Discard,
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(2..2), &2));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(4..4), &4));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    // This tests replacing the right item in the top branch of merge_into and the left item in the
    // bottom branch of merge_into.
    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_replace_merge_item() {
        let skip_list = SkipListLayer::new(100);
        let items =
            [Item::new(TestKey(1..1), 1), Item::new(TestKey(3..3), 3), Item::new(TestKey(5..5), 5)];
        skip_list.insert(items[0].clone()).await;
        skip_list.insert(items[2].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |_left, right| {
                if right.key() == &TestKey(3..3) {
                    MergeResult::Other {
                        emit: None,
                        left: Discard,
                        right: Replace(Item::new(TestKey(2..2), 2)),
                    }
                } else {
                    assert_eq!(right.key(), &TestKey(5..5));
                    MergeResult::Other {
                        emit: None,
                        left: Replace(Item::new(TestKey(4..4), 4)),
                        right: Discard,
                    }
                }
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(4..4), &4));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    // This tests replacing the right item in the bottom branch of merge_into.
    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_replace_existing() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(3..3), 3)];
        skip_list.insert(items[1].clone()).await;

        skip_list
            .merge_into(items[0].clone(), &items[0].key, |_left, right| {
                if right.key() == &TestKey(3..3) {
                    MergeResult::Other {
                        emit: None,
                        left: Keep,
                        right: Replace(Item::new(TestKey(2..2), 2)),
                    }
                } else {
                    MergeResult::EmitLeft
                }
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(2..2), &2));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_discard_last() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_list.insert(items[0].clone()).await;

        skip_list
            .merge_into(items[1].clone(), &items[0].key, |_left, _right| MergeResult::Other {
                emit: None,
                left: Discard,
                right: Keep,
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into_empty() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1..1), 1)];

        skip_list
            .merge_into(items[0].clone(), &items[0].key, |_left, _right| {
                panic!("Unexpected merge!");
            })
            .await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_uses_minimum_number_of_iterators() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(1..1), 2)];
        skip_lists[0].insert(items[0].clone()).await;
        skip_lists[1].insert(items[1].clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |_left, _right| {
            MergeResult::Other { emit: None, left: Discard, right: Keep }
        });
        let iter = merger.seek(Bound::Included(&items[0].key)).await.expect("seek failed");

        // Seek should only search in the first skip list, so no merge should take place, and we'll
        // know if it has because we'll see a different value (2 rather than 1).
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
    }

    // Checks that merging the given layers produces |expected| sequence of items starting from
    // |start|.
    async fn test_advance<K: Eq + Key + NextKey + OrdLowerBound>(
        layers: &[&[(K, i64)]],
        start: Bound<&K>,
        expected: &[(K, i64)],
    ) {
        let mut skip_lists = Vec::new();
        for &layer in layers {
            let skip_list = SkipListLayer::new(100);
            for (k, v) in layer {
                skip_list.insert(Item::new(k.clone(), *v)).await;
            }
            skip_lists.push(skip_list);
        }
        let mut merger =
            Merger::new(&skip_lists.into_layer_refs(), |_left, _right| MergeResult::EmitLeft);
        let mut iter = merger.seek(start).await.expect("seek failed");
        for (k, v) in expected {
            let ItemRef { key, value } = iter.get().expect("get failed");
            assert_eq!((key, value), (k, v));
            iter.advance().await.expect("advance failed");
        }
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_skips_replaced_items() {
        // The 1..2 and the 2..3 items are overwritten and merging them should be skipped.
        test_advance(
            &[
                &[(TestKey(1..2), 1), (TestKey(2..3), 2)],
                &[(TestKey(1..2), 3), (TestKey(2..3), 4), (TestKey(3..4), 5)],
            ],
            Bound::Included(&TestKey(1..2)),
            &[(TestKey(1..2), 1), (TestKey(2..3), 2), (TestKey(3..4), 5)],
        )
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_advance_skips_replaced_items_at_end() {
        // Like the last test, the 1..2 item is overwritten and seeking for it should skip the merge
        // but this time, the keys are at the end.
        test_advance(
            &[&[(TestKey(1..2), 1)], &[(TestKey(1..2), 2)]],
            Bound::Included(&TestKey(1..2)),
            &[(TestKey(1..2), 1)],
        )
        .await;
    }

    #[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
    struct TestKeyWithDefaultNextKey(Range<u64>);

    impl NextKey for TestKeyWithDefaultNextKey {}

    impl OrdUpperBound for TestKeyWithDefaultNextKey {
        fn cmp_upper_bound(&self, other: &TestKeyWithDefaultNextKey) -> std::cmp::Ordering {
            self.0.end.cmp(&other.0.end)
        }
    }

    impl OrdLowerBound for TestKeyWithDefaultNextKey {
        fn cmp_lower_bound(&self, other: &Self) -> std::cmp::Ordering {
            self.0.start.cmp(&other.0.start)
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_skips_replaced_items_with_default_next_key() {
        // This differs from the earlier test because with a default next key implementation, the
        // overwritten 2..3 key will get merged because the merger is unable to know whether the
        // 2..3 is the immediate successor to 1..2.
        test_advance(
            &[
                &[(TestKeyWithDefaultNextKey(1..2), 1), (TestKeyWithDefaultNextKey(2..3), 2)],
                &[
                    (TestKeyWithDefaultNextKey(1..2), 3),
                    (TestKeyWithDefaultNextKey(2..3), 4),
                    (TestKeyWithDefaultNextKey(3..4), 5),
                ],
            ],
            Bound::Included(&TestKeyWithDefaultNextKey(1..2)),
            &[
                (TestKeyWithDefaultNextKey(1..2), 1),
                (TestKeyWithDefaultNextKey(2..3), 2),
                (TestKeyWithDefaultNextKey(2..3), 4), // <-- This is the difference.
                (TestKeyWithDefaultNextKey(3..4), 5),
            ],
        )
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_advance_skips_replaced_items_at_end_with_default_next_key() {
        // Like the last test, the 1..2 item is overwritten and seeking for it should skip the merge
        // but this time, the keys are at the end.
        test_advance(
            &[&[(TestKeyWithDefaultNextKey(1..2), 1)], &[(TestKeyWithDefaultNextKey(1..2), 2)]],
            Bound::Included(&TestKeyWithDefaultNextKey(1..2)),
            &[(TestKeyWithDefaultNextKey(1..2), 1)],
        )
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_less_than() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[0].clone()).await;
        skip_lists[1].insert(items[1].clone()).await;
        // Search for a key before 1..1.
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |_left, _right| {
            MergeResult::Other { emit: None, left: Discard, right: Keep }
        });
        let iter = merger.seek(Bound::Included(&TestKey(0..0))).await.expect("seek failed");

        // This should find the 2..2 key because of our merge function.
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_to_end() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[0].clone()).await;
        skip_lists[1].insert(items[1].clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |_left, _right| {
            MergeResult::Other { emit: None, left: Discard, right: Keep }
        });
        let iter = merger.seek(Bound::Included(&TestKey(3..3))).await.expect("seek failed");

        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_all_discarded() {
        let skip_lists = [SkipListLayer::new(100), SkipListLayer::new(100)];
        let items = [Item::new(TestKey(1..1), 1), Item::new(TestKey(2..2), 2)];
        skip_lists[0].insert(items[1].clone()).await;
        skip_lists[1].insert(items[0].clone()).await;
        let mut merger = Merger::new(&skip_lists.into_layer_refs(), |_left, _right| {
            MergeResult::Other { emit: None, left: Discard, right: Discard }
        });
        let iter = merger.seek(Bound::Unbounded).await.expect("seek failed");
        assert!(iter.get().is_none());
    }
}
