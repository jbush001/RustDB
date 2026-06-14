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

// This is a simplistic implementation that is a bit of a placeholder. There
// are two places to get pages: virgin pages can be carved off the end of the
// file (the frontier) or any previously freed pages are stored in a on-disk
// linked list structure.

use crate::page_cache::*;
use crate::superblock::*;
use crate::util::*;

pub struct PageAllocator {
    page_cache: PageCache,
    next_frontier: PageNum,
    free_list_head: Option<PageNum>,
    pub total_allocs: usize,
    pub total_frees: usize
}

impl PageAllocator {
    pub fn new(page_cache: &PageCache) -> Self {
        let page = page_cache.lock_page(SUPERBLOCK_FPID);
        let superblock = get_superblock(&page);

        assert!(superblock.file_size > 0);

        let page_cache = page_cache.clone();
        PageAllocator {
            page_cache,
            next_frontier: PageNum(superblock.file_size),
            free_list_head: PageNum::from_disk(superblock.free_list_head),
            total_allocs: 0,
            total_frees: 0
        }
    }

    pub fn alloc(&mut self) -> PageNum {
        self.total_allocs += 1;

        if let Some(page_num) = self.free_list_head {
            {
                let page = self.page_cache.lock_page(page_num);
                self.free_list_head = PageNum::from_disk(get_u64(&page[..], 0));
            }

            let mut page = self.page_cache.lock_page_mut(SUPERBLOCK_FPID);
            let superblock = get_superblock_mut(&mut page);
            superblock.free_list_head = PageNum::to_disk(self.free_list_head);

            assert!(page_num.0 >= LOG_PAGES as u64 + 2);

            page_num
        } else {
            // Carve off frontier
            let page_num = self.next_frontier;
            self.next_frontier.0 += 1;
            let mut page = self.page_cache.lock_page_mut(SUPERBLOCK_FPID);
            let superblock = get_superblock_mut(&mut page);
            superblock.file_size = self.next_frontier.0;

            assert!(page_num.0 >= LOG_PAGES as u64 + 2);

            page_num
        }
    }

    pub fn free(&mut self, page_num: PageNum) {
        assert!(page_num.0 >= LOG_PAGES as u64 + 2);

        self.total_frees += 1;

        {
            let mut page = self.page_cache.lock_page_mut(page_num);
            set_u64(&mut page[..], 0, PageNum::to_disk(self.free_list_head));
            self.free_list_head = Some(page_num);
        }

        let mut page = self.page_cache.lock_page_mut(SUPERBLOCK_FPID);
        let superblock = get_superblock_mut(&mut page);
        superblock.free_list_head = PageNum::to_disk(self.free_list_head);
    }
}

#[cfg(test)]
mod tests {
    use crate::mocks::{MockPersistentStore};
    use crate::page_cache::*;
    use crate::superblock::*;
    use more_asserts::{assert_gt};
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::*;

    #[test]
    fn test_page_allocator() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();

        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);

        // Allocate two frontier blocks
        let p0 = allocator.alloc();
        let p1 = allocator.alloc();
        assert_ne!(p0, p1);
        allocator.free(p0);

        // Alloc after free should return the same block
        let p3 = allocator.alloc();
        assert_eq!(p3, p0);

        // Should come from the frontier
        let p4 = allocator.alloc();
        assert_gt!(p4.0, p1.0);
    }

    #[test]
    fn test_page_allocator_persistence() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();

        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);

        // Allocate two frontier blocks, free one
        let p0 = allocator.alloc();
        let p1 = allocator.alloc();
        allocator.free(p1);

        // Create a new allocator to read the state back from the superblock
        let mut allocator2 = PageAllocator::new(&mut page_cache);

        // Alloc after free should return the same block
        let p3 = allocator2.alloc();
        assert_eq!(p3, p1);
        assert_ne!(p3, p0);

        // Should come from the frontier
        let p4 = allocator2.alloc();
        assert_gt!(p4, p1);
        assert_ne!(p4, p0);
        assert_ne!(p4, p3);
    }
}
