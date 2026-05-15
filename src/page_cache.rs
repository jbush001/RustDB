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

const PAGE_SIZE: usize = 4096;

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
                self.prev[next_id.0] = self.prev[id.0];
            }
            None => {
                self.tail = self.prev[id.0];
            }
        }

        match self.prev[id.0] {
            Some(prev_id) => {
                self.next[prev_id.0] = self.next[id.0];
            }
            None => {
                self.tail = self.next[id.0];
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

    #[derive(Default)]
    struct MockIO {
        read_called: bool,
        read_address: u64,
        read_data: u8,
        write_called: bool,
        write_address: u64,
        write_data: u8
    }

    impl MockIO {
        fn reset(&mut self) {
            self.read_called = false;
            self.read_address = 0;
            self.read_data = 0;
            self.write_called = false;
            self.write_address = 0;
            self.write_data = 0;
        }
    }

    impl super::PersistentStore for MockIO {
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

    fn with_mock<F>(io: &Rc<RefCell<dyn super::PersistentStore>>, f: F)
    where F: FnOnce(&MockIO) {
        let borrowed = io.borrow();
        f(borrowed.as_any().downcast_ref::<MockIO>().unwrap());
    }

    fn with_mock_mut<F>(io: &Rc<RefCell<dyn super::PersistentStore>>, f: F)
    where F: FnOnce(&mut MockIO) {
        let mut borrowed = io.borrow_mut();
        f(borrowed.as_any_mut().downcast_mut::<MockIO>().unwrap());
    }

    // Eviction policy tests
    #[test]
    fn test_ep_evict_empty() {
        let mut policy = super::LRUEvictionPolicy::new(10);
        assert_eq!(policy.evict(), None);
    }

    #[test]
    fn test_ep_evict_one() {
        let mut policy = super::LRUEvictionPolicy::new(10);
        policy.insert(super::CachePageId(1));
        assert_eq!(policy.evict(), Some(super::CachePageId(1)));
        assert_eq!(policy.evict(), None);
    }

    #[test]
    fn test_ep_evict_many() {
        let mut policy = super::LRUEvictionPolicy::new(10);
        policy.insert(super::CachePageId(1));
        policy.insert(super::CachePageId(2));
        policy.insert(super::CachePageId(3));
        policy.insert(super::CachePageId(4));
        assert_eq!(policy.evict(), Some(super::CachePageId(1)));
        assert_eq!(policy.evict(), Some(super::CachePageId(2)));
        assert_eq!(policy.evict(), Some(super::CachePageId(3)));
        assert_eq!(policy.evict(), Some(super::CachePageId(4)));
        assert_eq!(policy.evict(), None);
    }

    #[test]
    fn test_ep_insert_remove() {
        let mut policy = super::LRUEvictionPolicy::new(10);
        policy.insert(super::CachePageId(1));
        policy.insert(super::CachePageId(2));
        policy.insert(super::CachePageId(3));
        policy.remove(super::CachePageId(2));
        assert_eq!(policy.evict(), Some(super::CachePageId(1)));
        assert_eq!(policy.evict(), Some(super::CachePageId(3)));
    }

    // Page cache tests
    #[test]
    fn test_pc_lock_unlock() {
        let mock_io: Rc<RefCell<dyn super::PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let page_cache = super::PageCache::new(5, Rc::clone(&mock_io));

        // Read a page
        with_mock_mut(&mock_io, |m| {
            m.reset();
            m.read_data = 3;
        });

        {
            let guard = page_cache.lock_page(super::FilePageId(3));
            with_mock(&mock_io, |m| {
                assert!(m.read_called);
                assert_eq!(m.read_address, 0x3000);
            });

            assert_eq!((*guard)[0], 3);

            // Read a different page
            with_mock_mut(&mock_io, |m| {
                m.reset();
                m.read_data = 4;
            });
        }

        {
            let guard = page_cache.lock_page(super::FilePageId(4));
            with_mock(&mock_io, |m| {
                assert!(m.read_called);
                assert_eq!(m.read_address, 0x4000);
            });

            assert_eq!((*guard)[0], 4);
        }

        // Now read the original page. It's cached, so it will not need to be re-read
        with_mock_mut(&mock_io, |m| m.reset());
        {
            let guard = page_cache.lock_page(super::FilePageId(3));
            with_mock(&mock_io, |m| {
                assert!(!m.read_called);
            });

            assert_eq!((*guard)[0], 3);
        }

        // Now read other pages until we evict the oldest page, which should be 4
        with_mock_mut(&mock_io, |m| {
            m.reset();
        });
        page_cache.lock_page(super::FilePageId(5));

        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x5000);
        });

        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(super::FilePageId(6));
        with_mock(&mock_io, |m| {
            assert_eq!(m.read_address, 0x6000);
        });

        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(super::FilePageId(7));
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x7000);
        });

        // This will evict page 4...
        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(super::FilePageId(8));
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x8000);
        });

        // ...let's prove it by reading back
        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(super::FilePageId(4));
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x4000);
        });

        // But ensure that page 5 is still okay
        with_mock_mut(&mock_io, |m| m.reset());
        page_cache.lock_page(super::FilePageId(5));
        with_mock(&mock_io, |m| {
            assert!(!m.read_called);
        });
    }

    #[test]
    fn test_pc_lock_write_dirty() {
        let mock_io: Rc<RefCell<dyn super::PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let page_cache = super::PageCache::new(5, Rc::clone(&mock_io));

        const WRITE_VAL: u8 = 0x55;

        // Read a page, set the wriable bit
        {
            let mut guard = page_cache.lock_page_mut(super::FilePageId(3));
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
    #[should_panic]
    fn test_cache_full() {
        let mock_io: Rc<RefCell<dyn super::PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let page_cache = super::PageCache::new(5, Rc::clone(&mock_io));
        let _guard1 = page_cache.lock_page_mut(super::FilePageId(0));
        let _guard2 = page_cache.lock_page_mut(super::FilePageId(1));
        let _guard3 = page_cache.lock_page_mut(super::FilePageId(2));
        let _guard4 = page_cache.lock_page_mut(super::FilePageId(3));
        let _guard5 = page_cache.lock_page_mut(super::FilePageId(4));
        let _guard6 = page_cache.lock_page_mut(super::FilePageId(5));
    }
}