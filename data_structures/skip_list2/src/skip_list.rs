use std::{
    alloc::Layout,
    cmp::Ordering::*,
    mem,
    ops::Bound,
    ptr::{self, NonNull, addr_of_mut, null_mut},
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicUsize, Ordering::*},
    },
};

use crate::{arena::MemAllocator, comparator::Comparator};

const MAX_HEIGHT: usize = 20;

#[repr(C)]
pub struct Node<K, V> {
    key: K,
    value: V,
    tower: [AtomicPtr<Self>; MAX_HEIGHT],
}

impl<K, V> Node<K, V> {
    fn get_next(&self, level: usize) -> *mut Self {
        self.tower[level].load(SeqCst)
    }

    fn set_next(&self, level: usize, node: *mut Self) {
        self.tower[level].store(node, SeqCst);
    }

    fn get_layout(height: usize) -> Layout {
        assert!(height > 0);
        let size =
            mem::size_of::<Self>() - mem::size_of::<AtomicPtr<Self>>() * (MAX_HEIGHT - height);
        let align = mem::align_of::<Self>();
        Layout::from_size_align(size, align)
            .expect(format!("Layout error, size: {size}, align: {align}").as_str())
    }

    fn new_in(key: K, value: V, height: usize, allocator: &impl MemAllocator) -> *mut Self {
        let layout = Self::get_layout(height);
        unsafe {
            let p = allocator.allocate(layout) as *mut Self;
            assert!(!p.is_null() && p.is_aligned());

            let node = &mut *p;
            ptr::write(addr_of_mut!(node.key), key);
            ptr::write(addr_of_mut!(node.value), value);
            ptr::write_bytes(node.tower.as_mut_ptr(), 0, height);
            p
        }
    }

    fn new_head(allocator: &impl MemAllocator) -> *mut Self {
        unsafe { Self::new_in(mem::zeroed(), mem::zeroed(), MAX_HEIGHT, allocator) }
    }
}

pub struct SkipList<K, V, C, A> {
    height: AtomicUsize,
    head: NonNull<Node<K, V>>,
    c: C,
    a: A,
}

impl<K, V, C, A> SkipList<K, V, C, A>
where
    C: Comparator<Item = K>,
    A: MemAllocator,
{
    pub fn new(c: C, a: A) -> Self {
        let height = 1;
        let head = Node::new_head(&a);
        SkipList {
            height: AtomicUsize::new(height),
            head: NonNull::new(head).unwrap(),
            c,
            a,
        }
    }

    fn height(&self) -> usize {
        self.height.load(SeqCst)
    }

    fn find_near(&self, key: Bound<&K>, reverse: bool) -> *mut Node<K, V> {
        unsafe {
            let head = self.head.as_ptr();
            let mut cur = head;
            let mut level = self.height() - 1;

            macro_rules! down_level {
                () => {
                    if level > 0 {
                        level -= 1;
                        continue;
                    }
                };
            }

            let (key, includeed) = match key {
                Bound::Included(key) => (Some(key), true),
                Bound::Excluded(key) => (Some(key), false),
                Bound::Unbounded => {
                    // find head
                    if reverse {
                        return (*head).get_next(0);
                    }

                    // find last
                    (None, false)
                }
            };

            loop {
                let next_ptr = (*cur).get_next(level);
                if next_ptr.is_null() {
                    // 如果还在高层，那么就下一层
                    down_level!();
                    // 如果没有后续了，如果是往前或者往后，那么直接结束
                    if cur == head || !reverse {
                        return null_mut();
                    }
                    // 最接近的是这个
                    return cur;
                }

                // find last
                let key = if let Some(ref key) = key {
                    key
                } else {
                    cur = next_ptr;
                    continue;
                };

                let next = &*next_ptr;
                match self.c.compare(key, &next.key) {
                    Less => {
                        down_level!();
                        if !reverse {
                            return next_ptr;
                        }
                        if cur == head {
                            return null_mut();
                        }
                        return cur;
                    }

                    Equal => {
                        if includeed {
                            return next_ptr;
                        }
                        if !reverse {
                            return next.get_next(0);
                        }
                        down_level!();
                        if cur == head {
                            return null_mut();
                        }
                        return cur;
                    }

                    Greater => {
                        cur = next_ptr;
                        continue;
                    }
                }
            }
        }
    }

    fn find_last(&self) -> *mut Node<K, V> {
        self.find_near(Bound::Unbounded, false)
    }

    fn find_first(&self) -> *mut Node<K, V> {
        self.find_near(Bound::Unbounded, true)
    }

    pub fn insert(&self, key: K, value: V) {
        let mut prev_height = self.height();
        let mut prev = [null_mut(); MAX_HEIGHT + 1];
        let mut next = [null_mut(); MAX_HEIGHT + 1];

        prev[prev_height] = self.head.as_ptr();
        for level in (0..prev_height).rev() {
            (prev[level], next[level]) = self.find_node_prev_next(&key, prev[level + 1], level);
            assert_ne!(prev[level], next[level]);
        }

        let height = random_height();
        let new_node_ptr = Node::new_in(key, value, height, &self.a);
        while height > prev_height {
            match self
                .height
                .compare_exchange(prev_height, height, SeqCst, SeqCst)
            {
                Ok(_) => break,
                Err(cur_h) => prev_height = cur_h,
            }
        }

        unsafe {
            let new_node = &*new_node_ptr;

            for level in 0..height {
                loop {
                    if prev[level].is_null() {
                        // level >= prev_height
                        (prev[level], next[level]) =
                            self.find_node_prev_next(&new_node.key, self.head.as_ptr(), level);
                    }

                    new_node.set_next(level, next[level]);

                    match (*prev[level]).tower[level].compare_exchange(
                        next[level],
                        new_node_ptr,
                        SeqCst,
                        SeqCst,
                    ) {
                        Ok(_) => break,
                        Err(_) => {
                            // re calculate prev[level] and next[level]
                            (prev[level], next[level]) =
                                self.find_node_prev_next(&new_node.key, prev[level], level);
                        }
                    }
                }
            }
        }
    }

    fn find_node_prev_next(
        &self,
        key: &K,
        start: *mut Node<K, V>,
        level: usize,
    ) -> (*mut Node<K, V>, *mut Node<K, V>) {
        let mut cur = start;
        unsafe {
            loop {
                let next = (*cur).get_next(level);
                if next.is_null() {
                    return (cur, null_mut());
                }

                match self.c.compare(&(*next).key, key) {
                    Less => cur = next,
                    Equal => return (next, next),
                    Greater => return (cur, next),
                }
            }
        }
    }

    pub fn mem_usage(&self) -> usize {
        self.a.mem_usage()
    }

    pub fn iter(self: &Arc<Self>) -> SkipListIter<K, V, C, A> {
        SkipListIter::new(self.clone())
    }
}

impl<K, V, C, A> Drop for SkipList<K, V, C, A> {
    fn drop(&mut self) {
        unsafe {
            let head = self.head.as_ptr();
            let mut cur = (*head).get_next(0);
            while cur.is_null() {
                let next = (*cur).get_next(0);
                ptr::drop_in_place(cur);
                cur = next;
            }
        }
    }
}

// [1, MAX_HEIGHT]
fn random_height() -> usize {
    const UPGRADE: usize = 4;
    let mut h = 1;
    while h < MAX_HEIGHT && (rand::random::<u32>() as usize % UPGRADE) == 0 {
        h += 1;
    }
    h
}

pub struct SkipListIter<K, V, C, A> {
    list: Arc<SkipList<K, V, C, A>>,
    cur: *mut Node<K, V>,
}

impl<K, V, C, A> SkipListIter<K, V, C, A>
where
    C: Comparator<Item = K>,
    A: MemAllocator,
{
    pub fn new(list: Arc<SkipList<K, V, C, A>>) -> Self {
        SkipListIter {
            list,
            cur: null_mut(),
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.cur.is_null()
    }

    pub fn key(&self) -> Option<&K> {
        if self.is_valid() {
            unsafe { Some(&(*self.cur).key) }
        } else {
            None
        }
    }

    pub fn value(&self) -> Option<&V> {
        if self.is_valid() {
            unsafe { Some(&(*self.cur).value) }
        } else {
            None
        }
    }

    pub fn next(&mut self) {
        assert!(self.is_valid());
        self.cur = unsafe { (*self.cur).get_next(0) };
    }

    pub fn prev(&mut self) {
        assert!(self.is_valid());
        self.cur = self
            .list
            .find_near(Bound::Excluded(self.key().unwrap()), true);
    }

    pub fn seek_to_first(&mut self) {
        self.cur = self.list.find_first();
    }

    pub fn seek_to_last(&mut self) {
        self.cur = self.list.find_last();
    }

    pub fn seek(&mut self, key: &K) {
        self.cur = self.list.find_near(Bound::Included(key), false);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{arena::BlockArena, comparator::DefaultComparator};

    use super::SkipList;

    #[test]
    fn insert_some() {
        const TEST_COUNT: usize = 1_000_000;

        let list = Arc::new(SkipList::new(
            DefaultComparator::default(),
            BlockArena::default(),
        ));

        for i in 0..TEST_COUNT {
            list.insert(i, i + 1);
        }

        let mut iter = list.iter();
        iter.seek_to_first();
        for i in 0..TEST_COUNT {
            assert_eq!(iter.key().unwrap(), &i);
            assert_eq!(iter.value().unwrap(), &(i + 1));
            iter.next();
        }
    }

    #[test]
    fn iterator() {
        const TEST_COUNT: usize = 1_000_000;

        let list = Arc::new(SkipList::new(
            DefaultComparator::default(),
            BlockArena::default(),
        ));

        for i in 0..TEST_COUNT {
            list.insert(i, i);
        }

        let mut iter = list.iter();
        iter.seek_to_last();
        iter.seek_to_first();
        for i in 0..TEST_COUNT {
            assert_eq!(iter.key().unwrap(), &i);
            assert_eq!(iter.value().unwrap(), &i);
            iter.next();
        }
        assert!(!iter.is_valid());

        for i in 0..TEST_COUNT {
            iter.seek(&i);
            assert_eq!(iter.key().unwrap(), &i);
            assert_eq!(iter.value().unwrap(), &i);
        }
    }
}
