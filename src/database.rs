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

use crate::collection::*;
use crate::page_allocator::PageAllocator;
use crate::page_cache::{PageCache, PersistentStore, TransactionGuard};
use crate::superblock::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const PAGE_CACHE_SIZE: usize = 128;

pub struct Database {
    meta_collection: Collection,  // On-disk storage of collection metadata
    collections: HashMap<String, (DocId, Rc<RefCell<Collection>>)>, // Collection objects
    page_cache: PageCache,
    page_allocator: PageAllocator
}

impl Database {
    pub fn open(file_store: Rc<RefCell<dyn PersistentStore>>) -> Database {
        let page_cache = PageCache::new(PAGE_CACHE_SIZE, Rc::clone(&file_store));
        let page_allocator = PageAllocator::new(&page_cache);

        let meta_collection = Collection::open(
            &json!({"indices": [], "root_page_fpid": 1, "name": "_meta"}));
        let mut collections = HashMap::new();
        let iter = SequentialScan::new(&meta_collection, &page_cache);
        for (docid, document) in iter {
            let name = document["name"].as_str().unwrap().to_string();
            let collection = Rc::new(RefCell::new(Collection::open(&document)));
            collections.insert(name, (docid, collection));
        }

        Database {
            meta_collection,
            collections,
            page_cache,
            page_allocator
        }
    }

    pub fn create(file_store: Rc<RefCell<dyn PersistentStore>>) -> Database {
        let page_cache = PageCache::new(PAGE_CACHE_SIZE, Rc::clone(&file_store));

        let _transaction = page_cache.begin_transaction();
        let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
        init_superblock(&mut page);

        let mut page_allocator = PageAllocator::new(&page_cache);

        Database {
            meta_collection: Collection::create("_meta", &page_cache, &mut page_allocator),
            collections: HashMap::new(),
            page_cache,
            page_allocator
        }
    }

    pub fn begin_transaction(&self) -> TransactionGuard {
        self.page_cache.begin_transaction()
    }

    pub fn create_collection(&mut self, name: &str) -> Result<DocId, String> {
        if self.collections.contains_key(name) {
            return Err("collection already exists".to_string());
        }

        let new_collection = Collection::create(name, &self.page_cache, &mut self.page_allocator);
        let metadata = &new_collection.get_metadata();
        println!("{:?}", metadata);
        let id = self.meta_collection.insert(metadata, &self.page_cache,
            &mut self.page_allocator);
        self.collections.insert(name.to_string(),
            (id, Rc::new(RefCell::new(new_collection))));

        Ok(id)
    }

    pub fn create_index(&mut self, collection_name: &str, field_name: &str) -> Result<(), String> {
        let (collection_id, collection) = self.collections.get(collection_name)
            .ok_or("collection not found".to_string())?;

        let mut collection = collection.borrow_mut();
        collection.create_index(&FieldPath::new(field_name)?, &self.page_cache,
            &mut self.page_allocator);

        // Update metadata.
        let metadata = collection.get_metadata();
        self.meta_collection.update(*collection_id, &metadata,
            &self.page_cache, &mut self.page_allocator);

        Ok(())
    }

    pub fn insert_document(&mut self, collection_name: &str, document: Value) -> Result<DocId, String> {
        let (_collection_id, collection) = self.collections.get(collection_name)
            .ok_or("collection not found".to_string())?;

        let mut collection = collection.borrow_mut();
        let docid = collection.insert(&document, &self.page_cache, &mut self.page_allocator);

        Ok(docid)
    }

    pub fn get_collection_list(&self) -> Vec<String> {
        self.collections.keys().cloned().collect()
    }

    pub fn query(&self, collection_name: &str) -> Result<impl Iterator<Item = (DocId, Value)>, String> {
        let (_collection_id, collection) = self.collections.get(collection_name)
            .ok_or("collection not found".to_string())?;
        Ok(SequentialScan::new(&collection.borrow(), &self.page_cache))
    }
}

#[cfg(test)]
mod tests {
    use crate::mocks::MockPersistentStore;
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::*;

    #[test]
    fn test_db_create_open() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        {
            let mut db = Database::create(mock_io.clone());
            let _transaction = db.begin_transaction();
            println!("create collection");
            assert!(db.create_collection("people").is_ok());
            println!("create index");
            db.create_index("people", "name").unwrap();
            println!("insert records");
            db.insert_document("people", json!({"name": "Alice", "age": 30})).unwrap();
            db.insert_document("people", json!({"name": "Bob", "age": 25})).unwrap();
            db.insert_document("people", json!({"name": "Charlie", "age": 35})).unwrap();
        }

        // Reopen the database.
        let db = Database::open(mock_io);
        assert_eq!(db.get_collection_list(), vec!["people".to_string()]);

        let mut iter = db.query("people").expect("error in query");
        assert_eq!(iter.next(), Some((DocId(1), json!({"name": "Alice", "age": 30}))));
        assert_eq!(iter.next(), Some((DocId(2), json!({"name": "Bob", "age": 25}))));
        assert_eq!(iter.next(), Some((DocId(3), json!({"name": "Charlie", "age": 35}))));
        assert!(iter.next().is_none());
    }
}
