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


//
// the 0th page of the database file contains information
// magic: u32
// free_list_head: u64   Page number of free list
// file_size: u64        File size in pages
//

use crate::page_cache::*;
use crate::util::*;

const FREE_LIST_HEAD_OFFS: usize = 4;
const FILE_SIZE_OFFS: usize = 4;
const FREE_LIST_END: u64 = 0; // Since page 0 is the superblock, can't be freed.

pub struct PageAllocator {
    page_cache: PageCache,
    next_frontier: u64,
    free_list_head: u64
}

impl PageAllocator {
    pub fn new(page_cache: &PageCache) -> Self {
        let page = page_cache.lock_page(FilePageId(0));
        let next_frontier = std::cmp::max(get_u64(&page, FILE_SIZE_OFFS), 1);
        let free_list_head = get_u64(&page, FREE_LIST_HEAD_OFFS);

        let page_cache = page_cache.clone();
        PageAllocator {
            page_cache,
            next_frontier,
            free_list_head
        }
    }

    // Returns a 64 bit page number
    pub fn alloc(&mut self) -> u64 {
        if self.free_list_head != FREE_LIST_END {
            let result = self.free_list_head;
            {
                let page = self.page_cache.lock_page(FilePageId(self.free_list_head));
                self.free_list_head = get_u64(&page, 0);
            }

            let mut page = self.page_cache.lock_page_mut(FilePageId(0));
            set_u64(&mut page, FREE_LIST_HEAD_OFFS, self.free_list_head);

            result
        } else {
            // Carve off frontier
            let result = self.next_frontier;
            self.next_frontier += 1;
            let mut page = self.page_cache.lock_page_mut(FilePageId(0));
            set_u64(&mut page, FILE_SIZE_OFFS, self.next_frontier);

            result
        }
    }

    pub fn free(&mut self, fpid: u64) {
        {
            let mut page = self.page_cache.lock_page_mut(FilePageId(fpid));
            set_u64(&mut page, 0, self.free_list_head);
            self.free_list_head = fpid;
        }

        let mut page = self.page_cache.lock_page_mut(FilePageId(0));
        set_u64(&mut page, FREE_LIST_HEAD_OFFS, self.free_list_head);
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::cell::RefCell;
    use crate::page_cache::*;
    use more_asserts::{assert_gt};
    use crate::mocks::{MockPersistentStore};
    use super::*;

    #[test]
    fn test_page_allocator() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
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
        assert_gt!(p4, p1);
    }
}
