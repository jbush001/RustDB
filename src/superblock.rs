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

// Block 0, the first block in the filesystem, has key information and
// links out to other file system structures. As such, many other places
// in the code access it.

use crate::page_cache::*;
use bytemuck::{Pod, Zeroable};
use std::mem;

const SUPERBLOCK_MAGIC: &[u8; 8] = b"RUSTDB00";

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct Superblock {
    pub magic: [u8; 8],
    pub free_list_head: u64,
    pub file_size: u64
}

pub const SUPERBLOCK_FPID: FilePageId = FilePageId(0);

pub fn init_superblock(page: &mut PageData) {
    let block = get_superblock_mut(page);
    block.magic.copy_from_slice(SUPERBLOCK_MAGIC);
    block.free_list_head = FilePageId::INVALID.0;
    block.file_size = LOG_PAGES as u64 + 2;
}

pub fn check_superblock(page: &PageData) -> Result<(), String> {
    let block = get_superblock(page);
    if block.magic != *SUPERBLOCK_MAGIC {
        return Err("Bad Magic".to_string());
    }

    Ok(())
}

pub fn get_superblock(page: &PageData) -> &Superblock {
    bytemuck::from_bytes(&page[0..mem::size_of::<Superblock>()])
}

pub fn get_superblock_mut(page: &mut PageData) -> &mut Superblock {
    bytemuck::from_bytes_mut(&mut page[0..mem::size_of::<Superblock>()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bad_magic() {
        let page: PageData = [0; PAGE_SIZE];
        assert_eq!(check_superblock(&page), Err("Bad Magic".to_string()));
    }

    #[test]
    fn test_good_magic() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_superblock(&mut page);
        assert_eq!(check_superblock(&page), Ok(()));
    }
}
