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
    fn read(&mut self, offset: u64, slice: &mut [u8]) {
        if offset >= self.length {
            slice.fill(0);
            return;
        }

        self.file.seek(SeekFrom::Start(offset)).expect("seek failed");
        let available = (self.length - offset) as usize;
        if available < slice.len() {
            // Partially past EOF — read what exists, zero the rest
            self.file.read_exact(&mut slice[..available]).expect("read failed");
            slice[available..].fill(0);
        } else {
            self.file.read_exact(slice).expect("read failed");
        }
    }

    fn write(&mut self, offset: u64, slice: &[u8]) {
        self.file.seek(SeekFrom::Start(offset)).expect("seek failed");
        self.file.write_all(slice).expect("write failed");
        self.length = std::cmp::max(self.length, offset + slice.len() as u64);
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

        store.write(0, &"abcdefghiklmnopqrstuvwxyz0123456789".as_bytes());

        let mut buf: [u8; 16] = [0; 16];
        store.read(8, &mut buf);
        assert_eq!(buf, "iklmnopqrstuvwxy".as_bytes());

        store.write(12, &"-!@#$%^&*()".as_bytes());

        let content = fs::read_to_string(file.path().to_str().unwrap()).unwrap();
        assert_eq!(content, "abcdefghiklm-!@#$%^&*()yz0123456789");
    }

    #[test]
    fn test_sync() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        store.write(0, &"abcdefgh".as_bytes());
        store.sync();

        let mut buf: [u8; 8] = [0; 8];
        store.read(0, &mut buf);
        assert_eq!(buf, "abcdefgh".as_bytes());
    }

    #[test]
    fn test_read_past_end() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        store.write(0, &[1u8, 2, 3, 4, 5, 6, 7, 8, 9]);

        let mut buf: [u8; 10] = [0xff; 10];
        store.read(5, &mut buf);
        assert_eq!(buf, [6u8, 7, 8, 9, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_read_completely_past_end() {
        let file = NamedTempFile::new().unwrap();
        let mut store = FileStore::open(file.path().to_str().unwrap()).unwrap();

        let mut buf: [u8; 10] = [0xff; 10];
        store.read(100, &mut buf);
        assert_eq!(buf, [0u8; 10]);

        // For coverage, read exactly at end
        let mut buf: [u8; 10] = [0xff; 10];
        store.read(110, &mut buf);
        assert_eq!(buf, [0u8; 10]);
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

        let mut buf: [u8; 16] = [0; 16];
        store.write(8, &buf);
        store.read(8, &mut buf);
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

        let buf: [u8; 16] = [0; 16];
        store.write(8, &buf);
    }
}
