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
use std::any::Any;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};
use std::io::{Result};

pub struct FileStore {
    file: File,
    length: PageIndex
}

impl FileStore {
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let length = PageIndex(file.metadata().unwrap().len() / PAGE_SIZE as u64);

        Ok(Self {
            file,
            length
        })
    }
}

impl PersistentStore for FileStore {
    fn read(&mut self, page_index: PageIndex, page: &mut PageData) {
        if page_index >= self.length {
            page.fill(0);
            return;
        }

        self.file.seek(SeekFrom::Start(page_index.0 * PAGE_SIZE as u64)).expect("seek failed");
        self.file.read_exact(page).expect("read failed");
    }

    fn write(&mut self, page_index: PageIndex, page: &PageData) {
        self.file.seek(SeekFrom::Start(page_index.0 * PAGE_SIZE as u64)).expect("seek failed");
        self.file.write_all(page).expect("write failed");
        self.length = std::cmp::max(self.length, PageIndex(page_index.0 + 1));
    }

    fn sync(&mut self) {
        let _ = self.file.sync_data();
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
    use crate::page_cache::*;
    use std::fs;
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_write() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut temp1: PageData = [0; PAGE_SIZE];
        let test_string1 = "abcdefghiklmnopqrstuvwxyz0123456789-";
        for (dest, src) in temp1.iter_mut().zip(test_string1.bytes().cycle()) {
            *dest = src;
        }

        let mut temp2: PageData = [0; PAGE_SIZE];
        let test_string2 = "zqwertyuiopasdffhkgklfkgjf9876543210!";
        for (dest, src) in temp2.iter_mut().zip(test_string2.bytes().cycle()) {
            *dest = src;
        }

        // Write out of order to ensure seek works correctly
        store.write(PageIndex(1), &temp2);
        store.write(PageIndex(0), &temp1);

        let bytes = fs::read(file.path().to_str().unwrap()).unwrap();
        assert_eq!(bytes[..PAGE_SIZE], temp1);
        assert_eq!(bytes[PAGE_SIZE..], temp2);
    }

    #[test]
    fn test_read() {
        let file = NamedTempFile::new().unwrap();
        let mut source_buf: [u8; _] = [0; PAGE_SIZE * 2];
        let test_string1 = "abcdefghiklmnopqrstuvwxyz0123456789-";
        for (dest, src) in source_buf.iter_mut().zip(test_string1.bytes().cycle()) {
            *dest = src;
        }

        fs::write(file.path().to_str().unwrap(), &source_buf).expect("write failed");

        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut temp2: PageData = [0; PAGE_SIZE];
        store.read(PageIndex(1), &mut temp2);
        let mut temp1: PageData = [0; PAGE_SIZE];
        store.read(PageIndex(0), &mut temp1);

        assert_eq!(temp1, source_buf[..PAGE_SIZE]);
        assert_eq!(temp2, source_buf[PAGE_SIZE..]);
    }

    #[test]
    fn test_sync() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut temp1: PageData = [0; PAGE_SIZE];
        let test_string1 = "abcdefghiklmnopqrstuvwxyz0123456789";
        for (dest, src) in temp1.iter_mut().zip(test_string1.bytes().cycle()) {
            *dest = src;
        }

        store.write(PageIndex(0), &temp1);
        store.sync();

        let bytes = fs::read(file.path().to_str().unwrap()).unwrap();
        assert_eq!(bytes.len(), PAGE_SIZE);
        assert_eq!(bytes, temp1);
    }

    #[test]
    fn test_read_past_end() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut page: PageData = [0; PAGE_SIZE];
        store.read(PageIndex(2), &mut page);
        assert_eq!(page, [0u8; PAGE_SIZE]);
    }

    #[test]
    fn test_as_any() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();
        let any = store.as_any();
        assert!(any.downcast_ref::<FileStore>().is_some());

        let any = store.as_any_mut();
        assert!(any.downcast_mut::<FileStore>().is_some());
    }

    #[test]
    fn test_open_failure() {
        // A path whose parent directory doesn't exist
        let result = FileStore::open("/nonexistent/dir/test.db");
        assert!(result.is_err());
    }

    #[test]
    #[should_panic]
    fn test_read_failure() {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null").unwrap();
        let mut store = FileStore {
            file,
            length: PageIndex(0)
        };

        let mut temp1: PageData = [0; PAGE_SIZE];
        store.write(PageIndex(0), &temp1);
        store.read(PageIndex(0), &mut temp1);
    }

    #[test]
    #[should_panic]
    fn test_write_failure() {
        let file = NamedTempFile::new().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(false)
            .open(file.path().to_str().unwrap()).unwrap();
        let mut store = FileStore {
            file,
            length: PageIndex(0)
        };

        let page: PageData = [0; PAGE_SIZE];
        store.write(PageIndex(0), &page);
    }
}
