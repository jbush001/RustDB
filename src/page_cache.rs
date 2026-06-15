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

// This module mediates all disk access, keeping recently used pages
// in memory to optimize I/O.

use crate::util::*;
use crate::wal::*;
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

pub const LOG_PAGES: usize = 10;

pub const PAGE_SIZE: usize = 0x2000;
pub type PageData = [u8; PAGE_SIZE];

// This uniquely identifies a page size chunk inside the file.
#[derive(PartialEq, Ord, PartialOrd, Eq, Debug, Clone, Copy, Hash, Default)]
pub struct PageNum(u64);

impl PageNum {
    // This value is stored on disk to delimit linked lists or indicate a field
    // doesn't have a value.
    const INVALID: u64 = u64::MAX;

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: [u8; 8] = bytes[..8].try_into().expect("Invalid page num in from_bytes");
        let val = u64::from_le_bytes(bytes);
        if val == Self::INVALID { None } else { Some(Self(val)) }
    }

    pub fn to_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    pub const fn from_u64(val: u64) -> Self {
        // The INVALID pattern is only stored on disk, and is represented
        // internally with Option.
        assert!(val != Self::INVALID, "Invalid page num");
        PageNum(val)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

pub trait PageNumOptionExt {
    fn to_bytes(&self) -> [u8; 8];
}

impl PageNumOptionExt for Option<PageNum> {
    fn to_bytes(&self) -> [u8; 8] {
        match self {
            Some(PageNum(val)) => val.to_le_bytes(),
            None => PageNum::INVALID.to_le_bytes(),
        }
    }
}

// This is the interface to the underlying storage, called by this module
// to read and write pages.
pub trait PersistentStore: Any {
    fn read(&mut self, page_num: PageNum, page: &mut PageData);
    fn write(&mut self, page_num: PageNum, page: &PageData);
    fn sync(&mut self);
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

// Metadata and underlying storage for cached data.
#[derive(Debug, Clone)]
struct CachedPage {
    page_num: Option<PageNum>, // Which disk block this is from
    ref_count: u32,
    dirty: bool, // This differs from what is on disk and need to be written.
    data: Box<PageData>
}

// This is never constructed directly by users of the API, but is created
// implicitly when locking a page.
pub struct PageGuard {
    cache_slot: usize, // Index into array of cached pages
    data: *const PageData,
    cache: Rc<RefCell<PageCacheInner>>,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        self.cache.borrow_mut().unlock_page(self.cache_slot);
    }
}

impl Deref for PageGuard {
    type Target = PageData;
    fn deref(&self) -> &PageData {
        unsafe { &*self.data }
    }
}

pub struct PageGuardMut {
    cache_slot: usize,
    data: *mut PageData,
    cache: Rc<RefCell<PageCacheInner>>,
}

impl Drop for PageGuardMut {
    fn drop(&mut self) {
        self.cache.borrow_mut().unlock_page(self.cache_slot);
    }
}

impl Deref for PageGuardMut {
    type Target = PageData;
    fn deref(&self) -> &PageData {
        unsafe { &*self.data }
    }
}

impl DerefMut for PageGuardMut {
    fn deref_mut(&mut self) -> &mut PageData {
        unsafe { &mut *(self.data as *mut PageData) }
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

// This acts as the wrapper for an inner object. The latter is shared between
// multiple PageGuards, which allow RAII automatic release.
impl PageCache {
    pub fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        PageCache {
            inner: Rc::new(RefCell::new(PageCacheInner::new(size, persistent_store)))
        }
    }

    pub fn lock_page(&self, page_num: PageNum) -> PageGuard {
        let mut inner = self.inner.borrow_mut();
        let cache_slot = inner.lock_page_internal(page_num, false);
        let data: *const PageData = &*inner.pages[cache_slot].data;
        PageGuard {
            cache_slot,
            data,
            cache: Rc::clone(&self.inner)
        }
    }

    // If a page is locked mutable, it will be written back when
    // the transaction is complete.
    pub fn lock_page_mut(&self, page_num: PageNum) -> PageGuardMut {
        let mut inner = self.inner.borrow_mut();
        let cache_slot = inner.lock_page_internal(page_num, true);
        let data: *mut PageData = &mut *inner.pages[cache_slot].data;

        PageGuardMut {
            cache_slot,
            data,
            cache: Rc::clone(&self.inner)
        }
    }

    // A transaction must be active in order to write pages.
    pub fn begin_transaction(&self) -> TransactionGuard {
        let mut inner = self.inner.borrow_mut();
        inner.begin_transaction();

        TransactionGuard {
            cache: Rc::clone(&self.inner)
        }
    }

    pub fn replay(&self) {
        self.inner.borrow_mut().write_ahead_log.replay();
    }
}

struct PageCacheInner {
    page_map: HashMap<PageNum, usize>,
    pages: Vec<CachedPage>,
    lru: IndexQueue,
    dirty_page_list: IndexQueue,
    persistent_store: Rc<RefCell<dyn PersistentStore>>,
    transaction_active: bool,
    write_ahead_log: WriteAheadLog
}

impl PageCacheInner {
    fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        let mut lru = IndexQueue::new(size);
        for i in 0..size {
            lru.push_head(i);
        }

        PageCacheInner {
            page_map: HashMap::new(),
            pages: vec![CachedPage{page_num: None, ref_count: 0, dirty: false, data: Box::new([0u8; PAGE_SIZE])}; size],
            lru,
            persistent_store: persistent_store.clone(),
            transaction_active: false,
            dirty_page_list: IndexQueue::new(size),
            write_ahead_log: WriteAheadLog::new(PageNum::from_u64(1), LOG_PAGES, &persistent_store)
        }
    }

    fn lock_page_internal(&mut self, page_num: PageNum, writable: bool) -> usize {
        assert!(!writable || self.transaction_active);
        assert!(page_num.0 == 0 || page_num.0 > LOG_PAGES as u64,
            "Attempt to lock page in write ahead log");
        assert!(page_num.0 != u64::MAX, "Attempt to lock invalid page");

        let entry = self.page_map.get(&page_num);
        match entry {
            Some(cache_slot) => {
                // This page is already cached, return it.
                let cache_slot = *cache_slot;
                let cp = &mut self.pages[cache_slot];
                if cp.ref_count == 0 {
                    // We maintain an invariant that pages are never in one of the
                    // lists unless they have a ref count of zero.
                    if cp.dirty {
                        self.dirty_page_list.remove(cache_slot);
                    } else {
                        self.lru.remove(cache_slot);
                    }
                }

                cp.dirty |= writable;
                cp.ref_count += 1;
                cache_slot
            },
            None => {
                // This data is not resident, find an unused page and recycle it
                match self.lru.pop_tail() {
                    Some(index) => {
                        // The cache is write-through, so we never have dirty pages
                        // sitting around.
                        let cp = &mut self.pages[index];
                        match cp.page_num {
                            Some(page_num) => self.page_map.remove(&page_num),
                            None => None // this page has never been loaded, nothing to remove
                        };
                        self.page_map.insert(page_num, index);
                        cp.page_num = Some(page_num);
                        cp.ref_count = 1;
                        cp.dirty = writable;

                        // Read data into page
                        self.persistent_store.borrow_mut().read(page_num,
                            &mut self.pages[index].data);

                        index
                    }

                    None => panic!("cache full!")
                }
            }
        }
    }

    fn unlock_page(&mut self, cache_slot: usize) {
        let cp = &mut self.pages[cache_slot];
        assert!(!cp.dirty || self.transaction_active);
        cp.ref_count -= 1;
        if cp.ref_count == 0 {
            if cp.dirty {
                self.dirty_page_list.push_head(cache_slot);
            } else if self.pages[cache_slot].ref_count == 0 {
                // Put back in LRU
                self.lru.push_head(cache_slot);
            }
        }
    }

    fn begin_transaction(&mut self) {
        assert!(!self.transaction_active);
        self.transaction_active = true;
    }

    fn end_transaction(&mut self) {
        assert!(self.transaction_active);

        if self.dirty_page_list.empty() {
            self.transaction_active = false;
            return;
        }

        // Write to journal
        let pages: Vec<(PageNum, PageData)> = self.dirty_page_list
            .into_iter()
            .map(|index| (self.pages[index].page_num.unwrap(), *self.pages[index].data))
            .collect();

        self.write_ahead_log.log_transaction(&pages);
        self.persistent_store.borrow_mut().sync();

        // Write to backing store.
        while !self.dirty_page_list.empty() {
            let index = self.dirty_page_list.pop_tail().unwrap();
            let cached_page = &mut self.pages[index];
            self.persistent_store.borrow_mut().write(cached_page.page_num.unwrap(), &self.pages[index].data);
            self.pages[index].dirty = false;
            self.lru.push_head(index);
        }

        self.transaction_active = false;
        self.persistent_store.borrow_mut().sync();
        self.write_ahead_log.blocks_written();
    }
}

#[cfg(test)]
mod tests {
    use crate::mocks::*;
    use rand::rngs::{SmallRng};
    use rand::{SeedableRng, RngExt};
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::*;

    fn setup_cache(capacity: usize) -> (Rc<RefCell<dyn PersistentStore>>, PageCache) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(capacity, Rc::clone(&mock_io));
        (mock_io, page_cache)
    }

    #[test]
    fn test_pc_lock_two_pages() {
        let (mock_io, page_cache) = setup_cache(5);

        // Fill the first page
        mock_io.borrow_mut().write(PageNum::from_u64(100), &[0xcc; PAGE_SIZE]);

        {
            let guard = page_cache.lock_page(PageNum::from_u64(100));
            assert_eq!(*guard, [0xcc; PAGE_SIZE]);
        }

        // Read a different page
        mock_io.borrow_mut().write(PageNum::from_u64(101), &[0xdd; PAGE_SIZE]);
        {
            let guard = page_cache.lock_page(PageNum::from_u64(101));
            assert_eq!(*guard, [0xdd; PAGE_SIZE]);
        }

        // Now read the original page. It's cached, so it will not need to be re-read
        {
            let guard = page_cache.lock_page(PageNum::from_u64(100));
            assert_eq!(*guard, [0xcc; PAGE_SIZE]);
        }
    }

    #[test]
    fn test_pc_lock_write_dirty_cache_miss() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0x55;

        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the wriable bit
            let mut guard = page_cache.lock_page_mut(PageNum::from_u64(100));
            *guard = [0xcc; PAGE_SIZE];
        }

        // Unlocking will cause a writeback. Ensure the backing store is correct.
        let mut readback: PageData = [0; PAGE_SIZE];
        mock_io.borrow_mut().read(PageNum::from_u64(100), &mut readback);
        assert_eq!(readback, [0xcc; PAGE_SIZE]);
    }

    // Cache hit is a different code path. This is a regression test.
    #[test]
    fn test_pc_lock_write_dirty_cache_hit() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0x55;

        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the writable bit
            let _guard = page_cache.lock_page_mut(PageNum::from_u64(100));
        }

        // Now lock the page again.
        {
            let _transaction = page_cache.begin_transaction();

            // Read a page, set the writable bit
            let mut guard = page_cache.lock_page_mut(PageNum::from_u64(100));
            *guard = [0xcc; PAGE_SIZE];
        }

        // Unlocking will cause a writeback.
        // We only check the first byte, but ensure it is updated correctly.
        let mut readback: PageData = [0; PAGE_SIZE];
        mock_io.borrow_mut().read(PageNum::from_u64(100), &mut readback);
        assert_eq!(readback, [0xcc; PAGE_SIZE]);
    }

    // it's possible within a transaction we will lock the same page twice for
    // modification. Ensure this doesn't assert.
    #[test]
    fn test_pc_dirty_relock() {
        let (mock_io, page_cache) = setup_cache(5);
        let transaction = page_cache.begin_transaction();
        let guard1 = page_cache.lock_page_mut(PageNum::from_u64(100));
        drop(guard1);
        let mut guard2 = page_cache.lock_page_mut(PageNum::from_u64(100));
        *guard2 = [0xcc; PAGE_SIZE];
        drop(guard2);

        drop(transaction);

        let mut readback: PageData = [0; PAGE_SIZE];
        mock_io.borrow_mut().read(PageNum::from_u64(100), &mut readback);
        assert_eq!(readback, [0xcc; PAGE_SIZE]);
    }

    #[test]
    #[should_panic = "cache full!"]
    fn test_cache_full() {
        let (_mock_io, page_cache) = setup_cache(5);

        let _transaction = page_cache.begin_transaction();
        let _guard1 = page_cache.lock_page_mut(PageNum::from_u64(100));
        let _guard2 = page_cache.lock_page_mut(PageNum::from_u64(101));
        let _guard3 = page_cache.lock_page_mut(PageNum::from_u64(102));
        let _guard4 = page_cache.lock_page_mut(PageNum::from_u64(103));
        let _guard5 = page_cache.lock_page_mut(PageNum::from_u64(104));
        let _guard6 = page_cache.lock_page_mut(PageNum::from_u64(105));
    }

    #[test]
    fn test_empty_transaction() {
        let (_mock_io, page_cache) = setup_cache(5);
        {
            let _transaction = page_cache.begin_transaction();
        }
    }

    #[test]
    #[should_panic="assertion failed: !self.transaction_active"]
    fn test_multiple_transaction() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _transaction1 = page_cache.begin_transaction();
        let _transaction2 = page_cache.begin_transaction();
    }

    #[test]
    #[should_panic="assertion failed: !writable || self.transaction_active"]
    fn test_op_outside_transaction() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _guard = page_cache.lock_page_mut(PageNum::from_u64(3));
    }

    // Lock the same page twice for write in the same transaction
    // Regression test, this would hang previously because it would
    // reinsert the page into the dirty list, creating a loop.
    #[test]
    fn test_page_lock_write_then_write() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();
        let _guard1 = page_cache.lock_page_mut(PageNum::from_u64(100));
        let _guard2 = page_cache.lock_page_mut(PageNum::from_u64(100));
    }

    // Lock first read, then write, ensure it gets written back
    // Regression test, would not write back previously; it would
    // only set the writable flag when the reference count was zero.
    #[test]
    fn test_page_lock_read_then_write() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0xcc;
        {
            let _transaction = page_cache.begin_transaction();
            let _guard1 = page_cache.lock_page(PageNum::from_u64(100));
            let mut guard = page_cache.lock_page_mut(PageNum::from_u64(100));
            *guard = [0xcc; PAGE_SIZE];
        }

        let mut readback: PageData = [0; PAGE_SIZE];
        mock_io.borrow_mut().read(PageNum::from_u64(100), &mut readback);
        assert_eq!(readback, [0xcc; PAGE_SIZE]);
    }

    // Ensure we don't clear the writable flag when relocking for read.
    #[test]
    fn test_page_lock_write_then_read() {
        let (mock_io, page_cache) = setup_cache(5);

        const WRITE_VAL: u8 = 0xcc;
        {
            let _transaction = page_cache.begin_transaction();
            let mut guard1 = page_cache.lock_page_mut(PageNum::from_u64(100));
            *guard1 = [0xcc; PAGE_SIZE];

            let _guard2 = page_cache.lock_page(PageNum::from_u64(100));
        }

        let mut readback: PageData = [0; PAGE_SIZE];
        mock_io.borrow_mut().read(PageNum::from_u64(100), &mut readback);
        assert_eq!(readback, [0xcc; PAGE_SIZE]);
    }

    #[test]
    #[should_panic = "Attempt to lock page in write ahead log"]
    fn test_write_ahead_page() {
        let (_mock_io, page_cache) = setup_cache(5);
        let _guard = page_cache.lock_page(PageNum::from_u64(2));
    }

    #[test]
    fn test_page_cache_stress() {
        const TOTAL_PAGES: usize = 30;
        const CACHE_PAGES: usize = 10;

        let mut rng = SmallRng::seed_from_u64(0x12345);
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let page_cache = PageCache::new(CACHE_PAGES, Rc::clone(&mock_io));
        let mut oracle: Vec<PageData> = vec![[0u8; PAGE_SIZE]; TOTAL_PAGES + LOG_PAGES + 1];

        for _ in 0..10000 {
            let _transaction = page_cache.begin_transaction();
            let mut guards: Vec<PageGuard> = Vec::new();
            let mut mut_guards: Vec<PageGuardMut> = Vec::new();

            // Lock up to 5 pages.
            for _ in 0..rng.random_range(1..5) {
                // Note the same file page may be locked multiple times (3.33% chance)
                let page_num = rng.random_range(LOG_PAGES + 1..TOTAL_PAGES + LOG_PAGES + 1);
                // Randomly decide if to lock for read or write
                if rng.random_bool(0.5) {
                    let mut guard = page_cache.lock_page_mut(PageNum::from_u64(page_num as u64));
                    assert_eq!(*guard, oracle[page_num]);
                    rng.fill(&mut oracle[page_num]);
                    guard.copy_from_slice(oracle[page_num].as_slice());
                    mut_guards.push(guard);
                } else {
                    let guard = page_cache.lock_page(PageNum::from_u64(page_num as u64));
                    assert_eq!(*guard, oracle[page_num]);
                    guards.push(guard);
                }
            }

            // Note: guard are dropped here
        }

        // Check all pages
        for page_num in LOG_PAGES + 1..LOG_PAGES + 1 + TOTAL_PAGES {
            let guard = page_cache.lock_page(PageNum(page_num as u64));
            assert_eq!(*guard, oracle[page_num]);
        }
    }
}