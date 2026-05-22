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

use bytemuck::{Pod, Zeroable};
use crate::page_cache::*;
use std::mem;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct Superblock {
    pub free_list_head: u64,
    pub file_size: u64
}

pub const SUPERBLOCK_FPID: FilePageId = FilePageId(0);

pub fn get_superblock(page_guard: &PageGuard) -> &Superblock {
    bytemuck::from_bytes(&page_guard[0..mem::size_of::<Superblock>()])
}

pub fn get_superblock_mut(page_guard: &mut PageGuardMut) -> &mut Superblock {
    bytemuck::from_bytes_mut(&mut page_guard[0..mem::size_of::<Superblock>()])
}
