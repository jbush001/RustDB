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
use crate::file_store::FileStore;
use crate::page_allocator::PageAllocator;
use crate::page_cache::{PageCache, PersistentStore, TransactionGuard, LOG_PAGES, PageNum};
use crate::superblock::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;

const PAGE_CACHE_SIZE: usize = 128;
const META_COLLECTION_FPID: PageNum = PageNum::from_u64(LOG_PAGES as u64 + 1);

pub struct Database {
    meta_collection: Collection,  // On-disk storage of collection metadata
    collections: HashMap<String, (DocId, Rc<RefCell<Collection>>)>, // Collection objects
    page_cache: PageCache,
    page_allocator: PageAllocator
}

impl Database {
    pub fn open(file_path: &str) -> Result<Self, String> {
        let exists = fs::exists(file_path).map_err(|e| e.to_string())?;
        let file_store: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(FileStore::open(file_path).map_err(|e| e.to_string())?));
        if exists {
            Self::open_filestore(file_store.clone())
        } else {
            Ok(Self::create(file_store.clone()))
        }
    }

    fn open_filestore(file_store: Rc<RefCell<dyn PersistentStore>>) -> Result<Self, String> {
        // TODO validate superblock

        let page_cache = PageCache::new(PAGE_CACHE_SIZE, Rc::clone(&file_store));

        // Replay before accessing other data structures to ensure they are consistent.
        page_cache.replay();

        let page_allocator = PageAllocator::new(&page_cache);

        let meta_collection = Collection::open(
            &json!({"indices": [], "root_page_pnum": META_COLLECTION_FPID.as_u64(), "name": "_meta"}),
            &page_cache);
        let mut collections = HashMap::new();
        let iter = SequentialScan::new(&meta_collection, &page_cache);
        for (docid, document) in iter {
            let name = document["name"].as_str().unwrap().to_string();
            let collection = Rc::new(RefCell::new(Collection::open(&document, &page_cache)));
            collections.insert(name, (docid, collection));
        }

        Ok(Self {
            meta_collection,
            collections,
            page_cache,
            page_allocator
        })
    }

    fn create(file_store: Rc<RefCell<dyn PersistentStore>>) -> Self {
        let page_cache = PageCache::new(PAGE_CACHE_SIZE, Rc::clone(&file_store));

        let _transaction = page_cache.begin_transaction();
        let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
        init_superblock(&mut page);

        let page_allocator = PageAllocator::new(&page_cache);

        Self {
            meta_collection: Collection::create_at("_meta", &page_cache, META_COLLECTION_FPID),
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
            return Err("Collection already exists".to_string());
        }

        let new_collection = Collection::create(name, &self.page_cache, &mut self.page_allocator);
        let metadata = &new_collection.get_metadata();
        let id = self.meta_collection.insert(metadata, &self.page_cache,
            &mut self.page_allocator);
        self.collections.insert(name.to_string(),
            (id, Rc::new(RefCell::new(new_collection))));

        Ok(id)
    }

    pub fn create_index(&mut self, collection_name: &str, field_name: &str) -> Result<(), String> {
        let (collection_id, collection) = self.collections.get(collection_name)
            .ok_or("Collection not found".to_string())?;

        let mut collection = collection.borrow_mut();
        collection.create_index(&FieldPath::new(field_name)?, &self.page_cache,
            &mut self.page_allocator);

        // Update metadata.
        let metadata = collection.get_metadata();
        self.meta_collection.update(*collection_id, &metadata,
            &self.page_cache, &mut self.page_allocator);

        Ok(())
    }

    pub fn insert_document(&mut self, collection_name: &str, document: &Value) -> Result<DocId, String> {
        let (_collection_id, collection) = self.collections.get(collection_name)
            .ok_or("Collection not found".to_string())?;

        let mut collection = collection.borrow_mut();
        let docid = collection.insert(&document, &self.page_cache, &mut self.page_allocator);

        Ok(docid)
    }

    pub fn get_collection_list(&self) -> Vec<String> {
        self.collections.keys().cloned().collect()
    }

    pub fn seq_iter(&self, collection_name: &str) -> Result<impl Iterator<Item = (DocId, Value)>, String> {
        let (_collection_id, collection) = self.collections.get(collection_name)
            .ok_or("Collection not found".to_string())?;
        Ok(SequentialScan::new(&collection.borrow(), &self.page_cache))
    }

    pub fn index_iter(&self, collection_name: &str, index_name: &str,
        low_key: Option<Value>, high_key: Option<Value>, reverse: bool)
        -> Result<impl Iterator<Item = (DocId, Value)>, String> {
        let (_collection_id, collection) = self.collections.get(collection_name)
            .ok_or("Collection not found".to_string())?;
        let index = {
            collection.borrow().find_index(index_name)
        };

        if let Some(index) = index {
            Ok(IndexScan::new(collection.clone(), index,
                low_key, high_key, reverse, &self.page_cache)?)
        } else {
            Err("Unknown index".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::mocks::MockPersistentStore;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tempfile::NamedTempFile;
    use super::*;

    #[test]
    fn test_db_create_open() {
        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_str().unwrap();

        // This will remove the file (so the open funtion will create a new one
        // below), but will also automatically delete the file after the test runs.
        std::fs::remove_file(&path).unwrap();

        {
            let mut db = Database::open(&path).expect("failed to open database");
            let _transaction = db.begin_transaction();
            assert!(db.create_collection("people").is_ok());
            db.create_index("people", "name").unwrap();
            db.insert_document("people", &json!({"name": "Alice", "age": 30})).unwrap();
            db.insert_document("people", &json!({"name": "Bob", "age": 25})).unwrap();
            db.insert_document("people", &json!({"name": "Charlie", "age": 35})).unwrap();
        }

        // Reopen the database.
        let db = Database::open(&path).expect("failed to open database");
        assert_eq!(db.get_collection_list(), vec!["people".to_string()]);

        let mut iter = db.seq_iter("people").expect("error in query");
        assert_eq!(iter.next(), Some((DocId(1), json!({"name": "Alice", "age": 30}))));
        assert_eq!(iter.next(), Some((DocId(2), json!({"name": "Bob", "age": 25}))));
        assert_eq!(iter.next(), Some((DocId(3), json!({"name": "Charlie", "age": 35}))));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_create_collection_existing() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut db = Database::create(mock_io.clone());
        let _transaction = db.begin_transaction();
        assert!(db.create_collection("people").is_ok());
        assert_eq!(db.create_collection("people"), Err("Collection already exists".to_string()));
    }

    #[test]
    fn test_create_index_bad_collection() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut db = Database::create(mock_io.clone());
        let _transaction = db.begin_transaction();
        assert_eq!(db.create_index("orders", "invoice_num"), Err("Collection not found".to_string()));
    }

    #[test]
    fn test_insert_doc_bad_collection() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(
            MockPersistentStore::default()));
        let mut db = Database::create(mock_io.clone());
        let _transaction = db.begin_transaction();
        assert_eq!(db.insert_document("employees", &json!({"name": "Alice", "age": 30})),
            Err("Collection not found".to_string()));
    }

    #[test]
    fn test_query_bad_collection() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(
            MockPersistentStore::default()));
        let db = Database::create(mock_io.clone());
        assert!(db.seq_iter("employees").is_err());
    }

    #[test]
    fn test_index_iter() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(
            MockPersistentStore::default()));

        let records = [
            json!({"name": "Alice", "age": 7}),
            json!({"name": "Jim", "age": 12}),
            json!({"name": "Joe", "age": 25}),
            json!({"name": "Mike", "age": 37}),
            json!({"name": "Ed", "age": 82})
        ];

        let mut db = Database::create(mock_io.clone());
        {
            let _transaction = db.begin_transaction();
            db.create_collection("people").expect("failed to create collection");
            db.create_index("people", "age").unwrap();
            for rec in &records {
                db.insert_document("people", rec).unwrap();
            }
        }

        let mut iter = db.index_iter("people", "age", Some(json!(10)), Some(json!(40)), false).expect("error in query");
        for i in 1..4 {
            assert_eq!(iter.next(), Some((DocId(i + 1), records[i as usize].clone())));
        }

        assert!(iter.next().is_none());
    }
}
