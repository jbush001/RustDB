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

mod btree;
mod page_cache;
mod util;
mod page_allocator;
mod file_store;

use std::rc::Rc;
use std::cell::RefCell;
use crate::file_store::{FileStore};
use crate::page_cache::*;
use crate::page_allocator::*;

fn main() {
    let file_store: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(FileStore::open("main.db").unwrap()));
    let page_cache = PageCache::new(1000, Rc::clone(&file_store));
    let mut allocator = PageAllocator::new(&page_cache);

    allocator.alloc();
}
