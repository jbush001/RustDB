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
use crate::util::*;

pub const PAGE_SIZE: usize = 4096;

#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct FilePageId(pub u64);

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub struct CachePageId(pub usize);

pub trait PersistentStore: Any {
    fn read(&mut self, offset: u64, slice: &mut [u8]);
    fn write(&mut self, offset: u64, slice: &[u8]);
    fn sync(&mut self);
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

pub struct TransactionGuard {
    cache: Rc<RefCell<PageCacheInner>>,
}

impl Drop for TransactionGuard {
    fn drop(&mut self) {
        self.cache.borrow_mut().end_transaction();
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

    pub fn begin_transaction(&self) -> TransactionGuard {
        let mut inner = self.inner.borrow_mut();
        inner.begin_transaction();

        TransactionGuard {
            cache: Rc::clone(&self.inner)
        }
    }
}

struct Journal {
}

impl Journal {
    fn log_page_write(&self, _fpid: FilePageId, _data: &[u8]) {
    }

    fn committed(&self) {
    }

    fn replay(&self) {
    }
}

struct PageCacheInner {
    page_map: HashMap<FilePageId, CachePageId>,
    pages: Vec<CachedPage>,
    lru: IndexQueue,
    dirty_page_list: IndexQueue,
    persistent_store: Rc<RefCell<dyn PersistentStore>>,
    transaction_active: bool,
    journal: Journal
}

impl PageCacheInner {
    fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        let mut lru = IndexQueue::new(size);
        for i in 0..size {
            lru.push_head(i);
        }

        PageCacheInner {
            page_map: HashMap::new(),
            pages: vec![CachedPage{file_page: None, ref_count: 0, dirty: false, data: Box::new([0u8; PAGE_SIZE])}; size],
            lru,
            persistent_store,
            transaction_active: false,
            dirty_page_list: IndexQueue::new(size),
            journal: Journal {}
        }
    }

    fn lock_page_internal(&mut self, fpid: FilePageId, writable: bool) -> CachePageId {
        assert!(!writable || self.transaction_active);

        let entry = self.page_map.get(&fpid);
        match entry {
            Some(cpid) => {
                // This page is already cached, return it.
                let cpid = *cpid;
                let cp = &mut self.pages[cpid.0];
                if cp.ref_count == 0 {
                    // We maintain an invariant that pages are never in one of the
                    // lists unless they have a ref count of zero.
                    if cp.dirty {
                        self.dirty_page_list.remove(cpid.0);
                    } else {
                        self.lru.remove(cpid.0);
                        cp.dirty = writable;
                    }
                }

                cp.ref_count += 1;
                cpid
            },
            None => {
                // This data is not resident, find an unused page and recycle it
                match self.lru.pop_tail() {
                    Some(index) => {
                        // The cache is write-through, so we never have dirty pages
                        // sitting around.
                        let cp = &mut self.pages[index];
                        match cp.file_page {
                            Some(fpid) => self.page_map.remove(&fpid),
                            None => None // this page has never been loaded, nothing to remove
                        };
                        self.page_map.insert(fpid, CachePageId(index));
                        cp.file_page = Some(fpid);
                        cp.ref_count = 1;
                        cp.dirty = writable;

                        // Read data into page
                        let file_offset = fpid.0 * PAGE_SIZE as u64;
                        self.persistent_store.borrow_mut().read(file_offset,
                            &mut *self.pages[index].data);

                        CachePageId(index)
                    }

                    None => panic!("cache full!")
                }
            }
        }
    }

    fn unlock_page(&mut self, cpid: CachePageId) {
        let cp = &mut self.pages[cpid.0];
        assert!(!cp.dirty || self.transaction_active);
        cp.ref_count -= 1;
        if cp.dirty {
            self.dirty_page_list.push_head(cpid.0);
        } else if self.pages[cpid.0].ref_count == 0 {
            // Put back in LRU
            self.lru.push_head(cpid.0);
        }
    }

    fn begin_transaction(&mut self) {
        assert!(!self.transaction_active);
        self.transaction_active = true;
    }

    fn end_transaction(&mut self) {
        assert!(self.transaction_active);

        let mut store = self.persistent_store.borrow_mut();

        // Write to journal
        for index in &self.dirty_page_list {
            let cached_page = &mut self.pages[index];
            assert!(cached_page.ref_count == 0);
            self.journal.log_page_write(cached_page.file_page.unwrap(), &cached_page.data[..]);

            // TODO write to circular buffer

        }

        store.sync();

        // Write to backing store.
        while !self.dirty_page_list.empty() {
            let index = self.dirty_page_list.pop_tail().unwrap();
            let cached_page = &mut self.pages[index];
            let file_offset = cached_page.file_page.unwrap().0 * PAGE_SIZE as u64;
            store.write(file_offset, &*self.pages[index].data);
            self.pages[index].dirty = false;
            self.lru.push_head(index);
        }

        self.transaction_active = false;

        store.sync();

        self.journal.committed();
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::cell::RefCell;
    use std::any::Any;
    use rand::rngs::{SmallRng};
    use rand::{SeedableRng, RngExt, Rng};
    use rand::seq::SliceRandom;
    use rand::prelude::IndexedRandom;
    use crate::mocks::*;
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

        fn sync(&mut self) {}

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
    fn test_pc_pop_tail() {
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
    fn test_pc_lock_write_dirty_cache_miss() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0x55;

        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the wriable bit
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

    // Cache hit is a different code path. This is a regression test.
    #[test]
    fn test_pc_lock_write_dirty_cache_hit() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0x55;

        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the wriable bit
            let mut guard = page_cache.lock_page_mut(FilePageId(3));
        }

        // Now lock the page again.
        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the wriable bit
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


    // it's possible within a transaction we will lock the same page twice for
    // modification. Ensure this doesn't assert.
    #[test]
    fn test_pc_dirty_relock() {
        let (mock_io, page_cache) = setup_cache(5);
        let transaction = page_cache.begin_transaction();
        let guard1 = page_cache.lock_page_mut(FilePageId(3));
        drop(guard1);
        let guard2 = page_cache.lock_page_mut(FilePageId(3));
        drop(guard2);

        with_mock(&mock_io, |m| {
            assert!(!m.write_called);
        });

        drop(transaction);

        with_mock(&mock_io, |m| {
            assert!(m.write_called);
            assert_eq!(m.write_address, 0x3000);
        });
    }

    #[test]
    #[should_panic = "cache full!"]
    fn test_cache_full() {
        let (_mock_io, page_cache) = setup_cache(5);

        let _transaction = page_cache.begin_transaction();
        let _guard1 = page_cache.lock_page_mut(FilePageId(0));
        let _guard2 = page_cache.lock_page_mut(FilePageId(1));
        let _guard3 = page_cache.lock_page_mut(FilePageId(2));
        let _guard4 = page_cache.lock_page_mut(FilePageId(3));
        let _guard5 = page_cache.lock_page_mut(FilePageId(4));
        let _guard6 = page_cache.lock_page_mut(FilePageId(5));
    }

    #[test]
    fn test_empty_transaction() {
        let (mock_io, page_cache) = setup_cache(5);
        {
            let _transaction = page_cache.begin_transaction();
        }

        with_mock(&mock_io, |m| {
            assert!(!m.write_called);
        });

        // Now ensure we can do another write transaction with no issues.
        const WRITE_VAL: u8 = 0xcc;
        {
            let transaction = page_cache.begin_transaction();
            let mut guard = page_cache.lock_page_mut(FilePageId(3));
            (*guard)[0] = WRITE_VAL;
        }

        with_mock(&mock_io, |m| {
            assert!(m.write_called);
            assert_eq!(m.write_address, 0x3000);
            assert_eq!(m.write_data, WRITE_VAL);
        });
    }

    #[test]
    fn test_page_cache_stress() {
        const TOTAL_PAGES: usize = 30;
        const CACHE_PAGES: usize = 10;

        let seed: u64 = 0x12345;
        let mut rng = SmallRng::seed_from_u64(seed);
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(CACHE_PAGES, Rc::clone(&mock_io));
        let page_indices: Vec<usize> = (0..TOTAL_PAGES).collect();
        let mut oracle: Vec<[u8; PAGE_SIZE]> = vec![[0u8; PAGE_SIZE]; TOTAL_PAGES];

        for rep in 0..10000 {
            let _transaction = page_cache.begin_transaction();
            let num_pages = rng.random_range(1..5);
            let mut to_update: Vec<usize> = page_indices.sample(&mut rng, num_pages).copied().collect();
            to_update.shuffle(&mut rng);
            let mut guards: Vec<PageGuardMut> = Vec::new();
            for fpid in &to_update {
                let mut guard = page_cache.lock_page_mut(FilePageId(*fpid as u64));
                assert_eq!(*guard, oracle[*fpid]);
                rng.fill(&mut oracle[*fpid]);
                guard.copy_from_slice(oracle[*fpid].as_slice());
                guards.push(guard);
            }

            // Note: guard are dropped here, forcing writeback
        }

        // Check all pages
        for fpid in page_indices {
            let mut guard = page_cache.lock_page(FilePageId(fpid as u64));
            assert_eq!(*guard, oracle[fpid]);
        }
    }
}