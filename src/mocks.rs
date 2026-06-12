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

// Test utilities that are shared between files

use crate::page_cache::*;
use std::any::Any;
use std::collections::HashMap;

#[derive(Default)]
pub struct MockPersistentStore {
    saved_pages: HashMap<FilePageId, PageData>,
    write_limit: usize
}

impl MockPersistentStore {
    pub fn default() -> Self {
        Self {
            saved_pages: HashMap::new(),
            write_limit: usize::MAX
        }
    }

    pub fn set_write_limit(&mut self, limit: usize) {
        self.write_limit = limit;
    }

    pub fn hit_write_limit(&self) -> bool {
        self.write_limit == 0
    }
}

impl PersistentStore for MockPersistentStore {
    fn read(&mut self, fpid: FilePageId, page: &mut PageData) {
        if self.saved_pages.contains_key(&fpid) {
            page.copy_from_slice(self.saved_pages.get(&fpid).unwrap().as_slice());
        } else {
            page.fill(0);
        }
    }

    fn write(&mut self, fpid: FilePageId, page: &PageData) {
        if self.write_limit != usize::MAX && self.write_limit > 0 {
            self.write_limit -= 1;
        }

        if self.write_limit == 0 {
            return;
        }

        self.saved_pages.insert(fpid, *page);
    }

    fn sync(&mut self) {
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[test]
fn test_read_zeroes() {
    let mut mock = MockPersistentStore::default();
    let mut temp: PageData = [0; PAGE_SIZE];
    mock.read(FilePageId(1), &mut temp);
    assert_eq!(&temp, &[0; PAGE_SIZE]);
}

#[test]
fn test_readback() {
    let mut mock = MockPersistentStore::default();
    let mut temp1: PageData = [0xcc; PAGE_SIZE];
    mock.write(FilePageId(1), &mut temp1);

    let mut temp2: PageData = [0; PAGE_SIZE];
    mock.read(FilePageId(1), &mut temp2);
    assert_eq!(&temp1, &temp2);

    // Ensure other blocks are zero
    let mut temp3: PageData = [0; PAGE_SIZE];
    mock.read(FilePageId(2), &mut temp3);
    assert_eq!(&[0; PAGE_SIZE], &temp3);
}

#[test]
fn test_any() {
    let mut mock = MockPersistentStore::default();

    let any_ref: &dyn std::any::Any = mock.as_any();
    assert!(any_ref.is::<MockPersistentStore>(), "Failed to downcast as_any()");

    let any_mut: &mut dyn std::any::Any = mock.as_any_mut();
    assert!(any_mut.is::<MockPersistentStore>(), "Failed to downcast as_any_mut()");
}
