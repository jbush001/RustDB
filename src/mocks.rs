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

use crate::page_cache::*;
use std::collections::HashMap;
use std::any::Any;

#[derive(Default)]
pub struct MockPersistentStore {
    saved_pages: HashMap<u64, [u8; PAGE_SIZE]>
}

impl MockPersistentStore {
    pub fn default() -> Self {
        Self {
            saved_pages: HashMap::new()
        }
    }
}

impl PersistentStore for MockPersistentStore {
    fn read(&mut self, offset: u64, slice: &mut [u8]) {
        assert_eq!((offset as usize % PAGE_SIZE), 0, "Only page aligned IO supported");
        assert_eq!(slice.len(), PAGE_SIZE, "Only page size IO supported");

        if self.saved_pages.contains_key(&offset) {
            slice.copy_from_slice(self.saved_pages.get(&offset).unwrap().as_slice());
        } else {
            slice.fill(0);
        }
    }

    fn write(&mut self, offset: u64, slice: &[u8]) {
        assert_eq!((offset as usize % PAGE_SIZE), 0, "Only page aligned IO supported");
        assert_eq!(slice.len(), PAGE_SIZE, "Only page size IO supported");

        self.saved_pages.insert(offset, *slice.first_chunk::<PAGE_SIZE>().unwrap());
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_zeroes() {
        let mut mock = MockPersistentStore::default();
        let mut temp: [u8; 0x1000] = [0; 0x1000];
        mock.read(0x1000u64, &mut temp);
        assert_eq!(&temp, &[0; 0x1000]);
    }

    #[test]
    fn test_readback() {
        let mut mock = MockPersistentStore::default();
        let mut temp1: [u8; 0x1000] = [0xcc; 0x1000];
        mock.write(0x1000u64, &mut temp1);

        let mut temp2: [u8; 0x1000] = [0; 0x1000];
        mock.read(0x1000u64, &mut temp2);
        assert_eq!(&temp1, &temp2);

        // Ensure other blocks are zero
        let mut temp3: [u8; 0x1000] = [0; 0x1000];
        mock.read(0x2000u64, &mut temp3);
        assert_eq!(&[0; 0x1000], &temp3);
    }

    #[test]
    #[should_panic = "Only page aligned IO supported"]
    fn test_unaligned_read() {
        let mut mock = MockPersistentStore::default();
        let mut temp: [u8; 0x1000] = [0; 0x1000];
        mock.read(0x1001u64, &mut temp);
    }

    #[test]
    #[should_panic = "Only page aligned IO supported"]
    fn test_unaligned_write() {
        let mut mock = MockPersistentStore::default();
        let mut temp: [u8; 0x1000] = [0; 0x1000];
        mock.write(0x1001u64, &mut temp);
    }

    #[test]
    #[should_panic = "Only page size IO supported"]
    fn test_bad_read_size() {
        let mut mock = MockPersistentStore::default();
        let mut temp: [u8; 0x1001] = [0; 0x1001];
        mock.read(0x1000u64, &mut temp);
    }

    #[test]
    #[should_panic = "Only page size IO supported"]
    fn test_bad_write_size() {
        let mut mock = MockPersistentStore::default();
        let mut temp: [u8; 0x1001] = [0; 0x1001];
        mock.write(0x1000u64, &mut temp);
    }

    #[test]
    fn test_any() {
        let mut mock = MockPersistentStore::default();

        let any_ref: &dyn std::any::Any = mock.as_any();
        assert!(any_ref.is::<MockPersistentStore>(), "Failed to downcast as_any()");

        let any_mut: &mut dyn std::any::Any = mock.as_any_mut();
        assert!(any_mut.is::<MockPersistentStore>(), "Failed to downcast as_any_mut()");
    }
}
