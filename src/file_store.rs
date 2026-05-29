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
use std::io::{Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};

pub struct FileStore {
    file: File,
    length: u64
}

impl FileStore {
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let length = file.metadata().unwrap().len();

        Ok(Self {
            file,
            length
        })
    }
}

impl PersistentStore for FileStore {
    fn read(&mut self, offset: u64, page: &mut Page) {
        assert!(offset % PAGE_SIZE as u64 == 0);
        if offset >= self.length {
            page.fill(0);
            return;
        }

        self.file.seek(SeekFrom::Start(offset)).expect("seek failed");
        let available = (self.length - offset) as usize;
        if available < page.len() {
            // Partially past EOF — read what exists, zero the rest
            // TODO should this be an error or something?
            self.file.read_exact(&mut page[..available]).expect("read failed");
            page[available..].fill(0);
        } else {
            self.file.read_exact(page).expect("read failed");
        }
    }

    fn write(&mut self, offset: u64, page: &Page) {
        assert!(offset % PAGE_SIZE as u64 == 0);
        self.file.seek(SeekFrom::Start(offset)).expect("seek failed");
        self.file.write_all(page).expect("write failed");
        self.length = std::cmp::max(self.length, offset + page.len() as u64);
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
    use tempfile::NamedTempFile;
    use std::fs;
    use crate::page_cache::*;
    use super::*;

    #[test]
    fn test_read_write() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut temp1: Page = [0; PAGE_SIZE];
        let test_string1 = "abcdefghiklmnopqrstuvwxyz0123456789";
        for (dest, src) in temp1.iter_mut().zip(test_string1.bytes().cycle()) {
            *dest = src;
        }

        store.write(0, &temp1);

        let mut temp2: Page = [0; PAGE_SIZE];
        store.read(0, &mut temp2);
        assert_eq!(&temp2[..test_string1.len()], test_string1.as_bytes());

        let bytes = fs::read(file.path().to_str().unwrap()).unwrap();
        assert_eq!(bytes, temp1);
    }

    #[test]
    fn test_sync() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut temp1: Page = [0; PAGE_SIZE];
        let test_string1 = "abcdefghiklmnopqrstuvwxyz0123456789";
        for (dest, src) in temp1.iter_mut().zip(test_string1.bytes().cycle()) {
            *dest = src;
        }

        store.write(0, &temp1);
        store.sync();

        let bytes = fs::read(file.path().to_str().unwrap()).unwrap();
        assert_eq!(bytes.len(), PAGE_SIZE);
        assert_eq!(bytes, temp1);
    }

    #[test]
    fn test_read_past_end() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut page: Page = [0; PAGE_SIZE];
        store.read(0x2000, &mut page);
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
            length: 0
        };

        let mut temp1: Page = [0; PAGE_SIZE];
        store.write(0, &temp1);
        store.read(0, &mut temp1);
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
            length: 0
        };

        let page: Page = [0; PAGE_SIZE];
        store.write(0, &page);
    }
}
