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

use serde_json::Value;
use crate::btree::*;
use crate::page_cache::*;
use crate::page_allocator::*;

#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
struct DocID(u64);

struct Collection {
    next_docid: u64,
    document_btree_root: u64,
}

impl Collection {
    fn insert_document(&mut self,
        document: &Value,
        page_cache: &PageCache,
        page_allocator: &mut PageAllocator) -> DocID {

        let docid = self.next_docid;
        self.next_docid += 1;
        let content = document.to_string().into_bytes();
        btree_insert(self.document_btree_root,
            &docid.to_be_bytes(), // Note: docid is stored bigendian so its in order.
            &content,
            page_cache,
            page_allocator);

        DocID(docid)
    }

    fn iterate(&mut self, page_cache: &PageCache) -> impl Iterator<Item = (DocID, Value)> {
        let doc_cursor = btree_iterate(self.document_btree_root, false, page_cache);
        doc_cursor.map(|(key, value)| {
            let docid = DocID(u64::from_be_bytes(key.try_into().expect("failed to convert docid")));
            let doc = serde_json::from_slice(&value).expect("Failed to parse JSON");

            (docid, doc)
        })
    }
}


#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::cell::RefCell;
    use crate::page_cache::*;
    use crate::mocks::{MockPersistentStore};
    use serde_json::{Value, json};
    use crate::btree::*;
    use super::*;

    fn create_document(index: usize) -> Value {
        json!({
            "index": index,
            "value": "abcdefgjiklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        })
    }

    #[test]
    fn test_insert() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let mut allocator = PageAllocator::new(&mut page_cache);
        let document_btree_root = allocator.alloc();
        {
            let mut node = page_cache.lock_page_mut(FilePageId(document_btree_root));
            init_btree_node(&mut node);
        }

        let mut collection = Collection {
            next_docid: 1,
            document_btree_root
        };

        let mut docids: Vec<DocID> = Vec::new();
        for i in 0..100 {
            docids.push(collection.insert_document(&create_document(i),
                &page_cache, &mut allocator));
        }

        let mut i = 0;
        for (key, value) in collection.iterate(&page_cache) {
            assert_eq!(docids[i], key);
            assert_eq!(create_document(i), value);
            i += 1;
        }
    }
}
