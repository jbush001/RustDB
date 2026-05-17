//
//   Copyright 2026 Jeff Bush
//
//   Licensed under the Apache License, Version 2.0 (the "License");
//   you may not use this file except in compliance with the License.
//   You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
//   Unless required by applicable law or agreed to in writing, software
//   distributed under the License is distributed on an "AS IS" BASIS,
//   WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//   See the License for the specific language governing permissions and
//   limitations under the License.
//

use std::collections::HashMap;
use std::rc::Rc;
use std::cell::RefCell;
use std::any::Any;
use std::ops::{Deref, DerefMut};

pub const PAGE_SIZE: usize = 4096;

#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct FilePageId(pub u64);

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct CachePageId(pub usize);

struct LRUEvictionPolicy {
    next: Vec<Option<CachePageId>>,
    prev: Vec<Option<CachePageId>>,
    head: Option<CachePageId>,
    tail: Option<CachePageId>
}

impl LRUEvictionPolicy {
    fn new(num_pages: usize) -> Self {
        Self {
            next: vec![None; num_pages],
            prev: vec![None; num_pages],
            head: None,
            tail: None,
        }
    }

    fn remove(&mut self, id: CachePageId) {
        match self.next[id.0] {
            Some(next_id) => {
                assert!(self.tail != Some(id));
                self.prev[next_id.0] = self.prev[id.0];
            }
            None => {
                assert!(self.tail.unwrap() == id);
                self.tail = self.prev[id.0];
            }
        }

        match self.prev[id.0] {
            Some(prev_id) => {
                assert!(self.head != Some(id));
                self.next[prev_id.0] = self.next[id.0];
            }
            None => {
                assert!(self.head.unwrap() == id);
                self.head = self.next[id.0];
            }
        }
    }

    fn insert(&mut self, id: CachePageId) {
        match self.head {
            Some(head) => {
                self.prev[head.0] = Some(id);
                self.next[id.0] = Some(head);
                self.head = Some(id);
                self.prev[id.0] = None;
            }

            None => {
                self.head = Some(id);
                self.tail = Some(id);
                self.next[id.0] = None;
                self.prev[id.0] = None;
            }
        }
    }

    fn evict(&mut self) -> Option<CachePageId> {
        match self.tail {
            Some(id) => {
                self.remove(id);
                Some(id)
            }
            None => None
        }
    }
}

pub trait PersistentStore: Any {
    fn read(&mut self, offset: u64, slice: &mut [u8]);
    fn write(&mut self, offset: u64, slice: &[u8]);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

#[derive(Debug, Clone)]
struct CachedPage {
    file_page: Option<FilePageId>,
    ref_count: u32,
    dirty: bool,
    data: Box<[u8; PAGE_SIZE]>
}

pub struct PageGuard {
    cpid: CachePageId,
    data: *const [u8; PAGE_SIZE],
    cache: Rc<RefCell<PageCacheInner>>,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.cache.borrow_mut().unlock_page(self.cpid);
    }
}

impl Deref for PageGuard {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { (*self.data).as_slice() }
    }
}

pub struct PageGuardMut {
    cpid: CachePageId,
    data: *mut [u8; PAGE_SIZE],
    cache: Rc<RefCell<PageCacheInner>>,
}

impl Drop for PageGuardMut {
    fn drop(&mut self) {
        self.cache.borrow_mut().unlock_page(self.cpid);
    }
}

impl Deref for PageGuardMut {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { (*self.data).as_slice() }
    }
}

impl DerefMut for PageGuardMut {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { (*self.data).as_mut_slice() }
    }
}

#[derive(Clone)]
pub struct PageCache {
    inner: Rc<RefCell<PageCacheInner>>
}

impl PageCache {
    pub fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        PageCache {
            inner: Rc::new(RefCell::new(PageCacheInner::new(size, persistent_store)))
        }
    }

    pub fn lock_page(&self, fpid: FilePageId) -> PageGuard {
        let mut inner = self.inner.borrow_mut();
        let cpid = inner.lock_page_internal(fpid, false);
        let data: *const [u8; PAGE_SIZE] = &*inner.pages[cpid.0].data;
        PageGuard {
            cpid,
            data,
            cache: Rc::clone(&self.inner)
        }
    }

    pub fn lock_page_mut(&self, fpid: FilePageId) -> PageGuardMut {
        let mut inner = self.inner.borrow_mut();
        let cpid = inner.lock_page_internal(fpid, true);
        let data: *mut [u8; PAGE_SIZE] = &mut *inner.pages[cpid.0].data;

        PageGuardMut {
            cpid,
            data,
            cache: Rc::clone(&self.inner)
        }
    }
}

struct PageCacheInner {
    page_map: HashMap<FilePageId, CachePageId>,
    pages: Vec<CachedPage>,
    eviction_policy: LRUEvictionPolicy,
    persistent_store: Rc<RefCell<dyn PersistentStore>>
}

impl PageCacheInner {
    fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        let mut eviction_policy = LRUEvictionPolicy::new(size);
        for i in 0..size {
            eviction_policy.insert(CachePageId(i));
        }

        PageCacheInner {
            page_map: HashMap::new(),
            pages: vec![CachedPage{file_page: None, ref_count: 0, dirty: false, data: Box::new([0u8; PAGE_SIZE])}; size],
            eviction_policy,
            persistent_store
        }
    }

    fn lock_page_internal(&mut self, fpid: FilePageId, writable: bool) -> CachePageId {
        let entry = self.page_map.get(&fpid);
        match entry {
            Some(cpid) => {
                // This page is already cached, return it.
                let cpid = *cpid;
                let cp = &mut self.pages[cpid.0];
                if cp.ref_count == 0 {
                    // Remove from the LRU while the page is locked (it can't be
                    // evicted).
                    self.eviction_policy.remove(cpid);
                }

                cp.ref_count += 1;
                cpid
            },
            None => {
                // This data is not resident, find an unused page and recycle it
                match self.eviction_policy.evict() {
                    Some(cpid) => {
                        // The cache is write-through, so we never have dirty pages
                        // sitting around.
                        let cp = &mut self.pages[cpid.0];
                        match cp.file_page {
                            Some(fpid) => self.page_map.remove(&fpid),
                            None => None // this page has never been loaded, nothing to remove
                        };
                        self.page_map.insert(fpid, cpid);
                        cp.file_page = Some(fpid);
                        cp.ref_count = 1;
                        cp.dirty = writable;

                        // Read data into page
                        let file_offset = fpid.0 * PAGE_SIZE as u64;
                        self.persistent_store.borrow_mut().read(file_offset,
                            &mut *self.pages[cpid.0].data);

                        cpid
                    }

                    None => panic!("cache full!")
                }
            }
        }
    }

    fn unlock_page(&mut self, cpid: CachePageId) {
        let cp = &mut self.pages[cpid.0];
        cp.ref_count -= 1;
        if cp.dirty {
            // Write to backing store
            let file_offset = cp.file_page.unwrap().0 * PAGE_SIZE as u64;
            self.persistent_store.borrow_mut().write(file_offset,
                &*self.pages[cpid.0].data);
            self.pages[cpid.0].dirty = false;
        }

        if self.pages[cpid.0].ref_count == 0 {
            // Put back in LRU
            self.eviction_policy.insert(cpid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::cell::RefCell;
    use std::any::Any;
    use super::*;

    #[derive(Default)]
    struct MockIOChecker {
        read_called: bool,
        read_address: u64,
        read_data: u8,
        write_called: bool,
        write_address: u64,
        write_data: u8
    }

    impl MockIOChecker {
        fn reset(&mut self) {
            self.read_called = false;
            self.read_address = 0;
            self.read_data = 0;
            self.write_called = false;
            self.write_address = 0;
            self.write_data = 0;
        }
    }

    impl PersistentStore for MockIOChecker {
        fn read(&mut self, offset: u64, slice: &mut [u8]) {
            self.read_called = true;
            self.read_address = offset;
            for item in slice.iter_mut() {
                *item = self.read_data;
            }
        }

        fn write(&mut self, offset: u64, slice: &[u8]) {
            self.write_called = true;
            self.write_address = offset;
            self.write_data = slice[0];
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    fn with_mock<F>(io: &Rc<RefCell<dyn PersistentStore>>, f: F)
    where F: FnOnce(&MockIOChecker) {
        let borrowed = io.borrow();
        f(borrowed.as_any().downcast_ref::<MockIOChecker>().unwrap());
    }

    fn with_mock_mut<F>(io: &Rc<RefCell<dyn PersistentStore>>, f: F)
    where F: FnOnce(&mut MockIOChecker) {
        let mut borrowed = io.borrow_mut();
        f(borrowed.as_any_mut().downcast_mut::<MockIOChecker>().unwrap());
    }

    fn sanity_check_lru(lru: &LRUEvictionPolicy) {
        // You can't have a valid head pointer with valid tail and vice versa
        assert_eq!(lru.head.is_some(), lru.tail.is_some(), "Head/tail asymmetry");
        if lru.head.is_none() {
            return;
        }

        assert_eq!(lru.next.len(), lru.prev.len(), "Array size mismatch");

        // Forward traversal
        let mut node = lru.head;
        let mut forward_len: usize = 0;
        while let Some(_id) = node {
            forward_len += 1;
            assert!(forward_len <= lru.next.len(), "Cycle in next pointers");

            if lru.next[node.unwrap().0 as usize].is_none() {
                assert_eq!(node, lru.tail, "Forward traversal terminated early: not at tail");
                break;
            } else {
                node = lru.next[node.unwrap().0 as usize];
            }
        }

        // Backward traversal
        let mut node = lru.tail;
        let mut reverse_len: usize = 0;
        while let Some(_id) = node {
            reverse_len += 1;
            assert!(reverse_len <= lru.prev.len(), "Cycle in prev pointers");

            if lru.prev[node.unwrap().0 as usize].is_none() {
                assert_eq!(node, lru.head, "Backward traversal terminated early: not at head");
                break;
            } else {
                node = lru.prev[node.unwrap().0 as usize];
            }
        }
    }

    // Test that our validation tests catch bad lists
    #[test]
    #[should_panic = "Head/tail asymmetry"]
    fn test_head_tail_assym1() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), None],
            prev: vec![None, Some(CachePageId(0))],
            head: Some(CachePageId(0)),
            tail: None
        })
    }

    #[test]
    #[should_panic = "Head/tail asymmetry"]
    fn test_head_tail_assym2() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), None],
            prev: vec![None, Some(CachePageId(0))],
            head: None,
            tail: Some(CachePageId(0))
        })
    }

    #[test]
    #[should_panic = "Array size mismatch"]
    fn test_array_size_mismatch() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), None],
            prev: vec![Some(CachePageId(0))],
            head: Some(CachePageId(1)),
            tail: Some(CachePageId(0))
        })
    }

    #[test]
    #[should_panic = "Cycle in next pointers"]
    fn test_next_cycle() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), Some(CachePageId(1))],
            prev: vec![None, Some(CachePageId(0))],
            head: Some(CachePageId(0)),
            tail: Some(CachePageId(1))
        })
    }

    #[test]
    #[should_panic = "Cycle in prev pointers"]
    fn test_prev_cycle() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), None],
            prev: vec![Some(CachePageId(0)), Some(CachePageId(0))],
            head: Some(CachePageId(0)),
            tail: Some(CachePageId(1))
        })
    }

    #[test]
    #[should_panic = "Forward traversal terminated early: not at tail"]
    fn test_array_tail_bad() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), None, None],
            prev: vec![None, Some(CachePageId(0)), Some(CachePageId(1))],
            head: Some(CachePageId(0)),
            tail: Some(CachePageId(2))
        })
    }

    #[test]
    #[should_panic = "Backward traversal terminated early: not at head"]
    fn test_array_head_bad() {
        sanity_check_lru(&LRUEvictionPolicy {
            next: vec![Some(CachePageId(1)), Some(CachePageId(2)), None],
            prev: vec![None, None, Some(CachePageId(1))],
            head: Some(CachePageId(0)),
            tail: Some(CachePageId(2))
        })
    }

    // Eviction policy tests
    #[test]
    fn test_ep_evict_empty() {
        let mut policy = LRUEvictionPolicy::new(10);
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), None);
    }

    #[test]
    fn test_ep_evict_one() {
        let mut policy = LRUEvictionPolicy::new(10);
        policy.insert(CachePageId(1));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(1)));
        assert_eq!(policy.evict(), None);
    }

    #[test]
    fn test_ep_evict_many() {
        let mut policy = LRUEvictionPolicy::new(10);
        policy.insert(CachePageId(1));
        policy.insert(CachePageId(2));
        policy.insert(CachePageId(3));
        policy.insert(CachePageId(4));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(1)));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(2)));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(3)));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(4)));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), None);
        sanity_check_lru(&policy);
    }

    #[test]
    fn test_ep_insert_remove() {
        let mut policy = LRUEvictionPolicy::new(10);
        policy.insert(CachePageId(1));
        policy.insert(CachePageId(2));
        policy.insert(CachePageId(3));
        policy.remove(CachePageId(2));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(1)));
        assert_eq!(policy.evict(), Some(CachePageId(3)));
        sanity_check_lru(&policy);
    }

    // Regression test
    #[test]
    fn test_ep_remove_head() {
        let mut policy = LRUEvictionPolicy::new(10);
        policy.insert(CachePageId(1));
        sanity_check_lru(&policy);
        policy.insert(CachePageId(2));
        sanity_check_lru(&policy);
        policy.remove(CachePageId(2));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), Some(CachePageId(1)));
        sanity_check_lru(&policy);
        assert_eq!(policy.evict(), None);
        sanity_check_lru(&policy);
    }

    fn setup_cache(capacity: usize) -> (Rc<RefCell<dyn PersistentStore>>, PageCache) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockIOChecker::default()));
        let page_cache = PageCache::new(capacity, Rc::clone(&mock_io));
        (mock_io, page_cache)
    }

    // Page cache tests
    #[test]
    fn test_pc_lock_two_pages() {
        let (mock_io, page_cache) = setup_cache(5);

        // Read the first page
        with_mock_mut(&mock_io, |m| {
            m.reset();
            m.read_data = 3;
        });

        {
            let guard = page_cache.lock_page(FilePageId(3));
            with_mock(&mock_io, |m| {
                assert!(m.read_called);
                assert_eq!(m.read_address, 0x3000);
            });

            assert_eq!((*guard)[0], 3);
        }

        // Read a different page
        with_mock_mut(&mock_io, |m| {
            m.reset();
            m.read_data = 4;
        });

        let guard = page_cache.lock_page(FilePageId(4));
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x4000);
        });

        assert_eq!((*guard)[0], 4);

        // Now read the original page. It's cached, so it will not need to be re-read
        with_mock_mut(&mock_io, |m| m.reset());
        {
            let guard = page_cache.lock_page(FilePageId(3));
            with_mock(&mock_io, |m| {
                assert!(!m.read_called);
            });

            assert_eq!((*guard)[0], 3);
        }
    }

    #[test]
    fn test_pc_evict() {
        let (mock_io, page_cache) = setup_cache(5);

        page_cache.lock_page(FilePageId(3));
        page_cache.lock_page(FilePageId(4));
        page_cache.lock_page(FilePageId(5));
        page_cache.lock_page(FilePageId(6));
        page_cache.lock_page(FilePageId(7));

        // This will evict page 3...
        page_cache.lock_page(FilePageId(8));

        // ...let's prove it by relocking page 3
        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(FilePageId(3));
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x3000);
        });

        // Ensure page 5 is still in the cache and didn't get
        // evicted
        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(FilePageId(5));
        with_mock(&mock_io, |m| {
            assert!(!m.read_called);
        });
    }

    #[test]
    fn test_pc_lock_write_dirty() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0x55;

        // Read a page, set the wriable bit
        {
            let mut guard = page_cache.lock_page_mut(FilePageId(3));
            (*guard)[0] = WRITE_VAL;
        }

        // Unlocking will cause a writeback.
        with_mock(&mock_io, |m| {
            assert!(m.write_called);
            assert_eq!(m.write_address, 0x3000);

            // We only check the first byte, but ensure it is updated correctly.
            assert_eq!(m.write_data, WRITE_VAL);
        });
    }

    #[test]
    #[should_panic = "cache full!"]
    fn test_cache_full() {
        let (_mock_io, page_cache) = setup_cache(5);

        let _guard1 = page_cache.lock_page_mut(FilePageId(0));
        let _guard2 = page_cache.lock_page_mut(FilePageId(1));
        let _guard3 = page_cache.lock_page_mut(FilePageId(2));
        let _guard4 = page_cache.lock_page_mut(FilePageId(3));
        let _guard5 = page_cache.lock_page_mut(FilePageId(4));
        let _guard6 = page_cache.lock_page_mut(FilePageId(5));
    }
}