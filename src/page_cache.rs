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

#[derive(Debug, Clone, Copy)]
struct CachedPage {
    file_page: Option<FilePageId>,
    ref_count: u32,
    dirty: bool
}

pub struct PageCache {
    page_map: HashMap<FilePageId, CachePageId>,
    pages: Vec<CachedPage>,
    eviction_policy: LRUEvictionPolicy,
    data: Box<[u8]>,
    persistent_store: Rc<RefCell<dyn PersistentStore>>
}

impl PageCache {
    pub fn new(size: usize, persistent_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        let mut eviction_policy = LRUEvictionPolicy::new(size);
        for i in 0..size {
            eviction_policy.insert(CachePageId(i));
        }

        PageCache {
            page_map: HashMap::new(),
            pages: vec![CachedPage{file_page: None, ref_count: 0, dirty: false}; size],
            eviction_policy,
            data: vec![0u8; size * PAGE_SIZE].into_boxed_slice(),
            persistent_store
        }
    }

    pub fn lock_page(&mut self, fpid: FilePageId, writable: bool) -> CachePageId {
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
                        let cache_offset = cpid.0 * PAGE_SIZE;
                        self.persistent_store.borrow_mut().read(file_offset,
                            &mut self.data[cache_offset..cache_offset + PAGE_SIZE]);

                        cpid
                    }

                    None => panic!("cache full!")
                }
            }
        }
    }

    pub fn unlock_page(&mut self, cpid: CachePageId) {
        let cp = &mut self.pages[cpid.0];
        cp.ref_count -= 1;
        if cp.dirty {
            // Write to backing store
            let file_offset = cp.file_page.unwrap().0 * PAGE_SIZE as u64;
            let cache_offset = cpid.0 * PAGE_SIZE;
            self.persistent_store.borrow_mut().write(file_offset,
                &self.data[cache_offset..cache_offset + PAGE_SIZE]);
            cp.dirty = false;
        }

        if cp.ref_count == 0 {
            // Put back in LRU
            self.eviction_policy.insert(cpid);
        }
    }

    pub fn get_page_data(&self, cpid: CachePageId) -> &[u8] {
        let offset = cpid.0 * PAGE_SIZE;
        &self.data[offset..offset + PAGE_SIZE]
    }

    pub fn get_page_data_mut(&mut self, cpid: CachePageId) -> &mut [u8] {
        let offset = cpid.0 * PAGE_SIZE;
        &mut self.data[offset..offset + PAGE_SIZE]
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
        let mut page_cache = super::PageCache::new(5, Rc::clone(&mock_io));

        // Read a page
        with_mock_mut(&mock_io, |m| {
            m.reset();
            m.read_data = 3;
        });

        let cpid1 = page_cache.lock_page(super::FilePageId(3), false);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x3000);
        });

        let pt1 = page_cache.get_page_data(cpid1);
        assert_eq!(pt1[0], 3);

        // Read a different page
        with_mock_mut(&mock_io, |m| {
            m.reset();
            m.read_data = 4;
        });
        let cpid2 = page_cache.lock_page(super::FilePageId(4), false);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x4000);
        });

        let pt2 = page_cache.get_page_data(cpid2);
        assert_eq!(pt2[0], 4);

        // Now read the original page. It's cached, so it will not need to be re-read
        with_mock_mut(&mock_io, |m| m.reset());
        let cpid3 = page_cache.lock_page(super::FilePageId(3), false);
        with_mock(&mock_io, |m| {
            assert!(!m.read_called);
        });

        let pt3 = page_cache.get_page_data(cpid3);
        assert_eq!(pt3[0], 3);

        // A few other checks
        assert_eq!(cpid1, cpid3);
        assert_ne!(cpid2, cpid3);
        page_cache.unlock_page(cpid1);
        page_cache.unlock_page(cpid2);
        page_cache.unlock_page(cpid3);

        // Now read other pages until we evict the oldest page, which should be 4
        with_mock_mut(&mock_io, |m| {
            m.reset();
        });
        let cpid4 = page_cache.lock_page(super::FilePageId(5), false);

        page_cache.unlock_page(cpid4);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x5000);
        });

        with_mock_mut(&mock_io, |m| m.reset());
        let cpid5 = page_cache.lock_page(super::FilePageId(6), false);
        page_cache.unlock_page(cpid5);
        with_mock(&mock_io, |m| {
            assert_eq!(m.read_address, 0x6000);
        });

        with_mock_mut(&mock_io, |m| m.reset());
        let cpid6 = page_cache.lock_page(super::FilePageId(7), false);
        page_cache.unlock_page(cpid6);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x7000);
        });

        // This will evict page 4...
        with_mock_mut(&mock_io, |m| m.reset());
        let cpid7 = page_cache.lock_page(super::FilePageId(8), false);
        page_cache.unlock_page(cpid7);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x8000);
        });

        // ...let's prove it by reading back
        with_mock_mut(&mock_io, |m| m.reset());
        let cpid8 = page_cache.lock_page(super::FilePageId(4), false);
        page_cache.unlock_page(cpid8);
        with_mock(&mock_io, |m| {
            assert!(m.read_called);
            assert_eq!(m.read_address, 0x4000);
        });

        // But ensure that page 5 is still okay
        with_mock_mut(&mock_io, |m| m.reset());
        let cpid9 = page_cache.lock_page(super::FilePageId(5), false);
        page_cache.unlock_page(cpid9);
        with_mock(&mock_io, |m| {
            assert!(!m.read_called);
        });
    }

    #[test]
    fn test_pc_lock_write_dirty() {
        let mock_io: Rc<RefCell<dyn super::PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let mut page_cache = super::PageCache::new(5, Rc::clone(&mock_io));

        // Read a page, set the wriable bit
        let cpid1 = page_cache.lock_page(super::FilePageId(3), true);
        let pt1 = page_cache.get_page_data_mut(cpid1);
        const WRITE_VAL: u8 = 0x55;
        pt1[0] = WRITE_VAL;

        // Unlocking will cause a writeback.
        page_cache.unlock_page(cpid1);
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
        let mut page_cache = super::PageCache::new(5, Rc::clone(&mock_io));
        page_cache.lock_page(super::FilePageId(0), true);
        page_cache.lock_page(super::FilePageId(1), true);
        page_cache.lock_page(super::FilePageId(2), true);
        page_cache.lock_page(super::FilePageId(3), true);
        page_cache.lock_page(super::FilePageId(4), true);
        page_cache.lock_page(super::FilePageId(5), true);
    }
}