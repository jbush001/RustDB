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
mod collection;
mod database;
mod file_store;
mod page_allocator;
mod page_cache;
mod query;
mod superblock;
mod util;
mod vararray;
#[cfg(test)] mod mocks;

use crate::database::Database;
use crate::file_store::FileStore;
use crate::page_cache::PersistentStore;
use std::cell::RefCell;
use std::rc::Rc;

fn main() {
    // TODO need to check if main.db exists, create if if not
    let file_store: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(FileStore::open("main.db").unwrap()));
    let _db = Database::create(file_store);
}
