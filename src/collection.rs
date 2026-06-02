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

// A collection is a set of documents, which are assumed to be of the same
// general schema and type. The documents themselves are stored inside a BTree,
// indexed by a unique 64-bit identifier (the DocID). There are ancilary BTrees
// for any indices fields within these documents to allow fast sorting and
// searching.

// TODO: collection metadata is not persisted.

use serde_json::Value;
use crate::btree::*;
use crate::page_cache::*;
use crate::page_allocator::*;
use regex::Regex;
use std::rc::Rc;
use std::cell::RefCell;
use crate::util::*;

const FLAG_OVERFLOW: u8 = 0x80;

// Unique identifier (within a collection) for a specific document.
#[derive(PartialEq, Eq, Ord, PartialOrd, Debug, Clone, Copy, Hash)]
pub struct DocID(pub u64);

struct Index {
    field: FieldPath,
    btree_root: FilePageId
}

pub struct Collection {
    next_docid: u64,
    document_btree_root: FilePageId,
    indices: Vec<Index>
}

impl Collection {
    pub fn insert_document(&mut self,
        document: &Value,
        page_cache: &PageCache,
        page_allocator: &mut PageAllocator) -> DocID {

        let docid = self.next_docid;
        self.next_docid += 1;
        let mut content = Vec::with_capacity(1024);
        content.push(0); // flag byte
        serde_json::to_writer(&mut content, &document).expect("serialization failed");
        if content.len() > MAX_RECORD_SIZE {
            // need to create overflow records

            // This goes inline into the page
            let mut pointer: [u8; 17] = [0; 17];
            pointer[0] = FLAG_OVERFLOW;
            set_u64(&mut pointer, 1, content.len() as u64 - 1);

            let mut offset = 1; // Skip the flag byte we speculatively added
            let mut fpid = page_allocator.alloc();
            set_u64(&mut pointer, 9, fpid.0);
            while offset < content.len() {
                let mut page = page_cache.lock_page_mut(fpid);
                let to_copy = std::cmp::min(content.len() - offset, PAGE_SIZE - 8);
                page[8..8 + to_copy].copy_from_slice(&content[offset..offset + to_copy]);
                fpid = if offset + to_copy < content.len() {
                    page_allocator.alloc()
                } else {
                    FilePageId(0)
                };

                set_u64(&mut page[..], 0, fpid.0);
                offset += to_copy;
            }

            btree_insert(self.document_btree_root,
                &docid.to_be_bytes(), // Note: docid is stored bigendian so its in order.
                &pointer,
                page_cache,
                page_allocator);
        } else {
            btree_insert(self.document_btree_root,
                &docid.to_be_bytes(), // Note: docid is stored bigendian so its in order.
                &content,
                page_cache,
                page_allocator);
        }

        // Update indices
        for index in &self.indices {
            if let Ok(val) = lookup_field(&index.field, document) {
                if let Ok(encoded) = encode_key(&val, DocID(docid)) {
                    btree_insert(index.btree_root,
                        &encoded,
                        &docid.to_be_bytes(),
                        page_cache,
                        page_allocator);
                }
            }
        }

        DocID(docid)
    }

    pub fn create_index(&mut self, path: &FieldPath, page_cache: &PageCache,
        page_allocator: &mut PageAllocator) {
        let index = Index {
            field: path.clone(),
            btree_root: btree_create(page_cache, page_allocator)
        };

        self.indices.push(index)

        // TODO: if there are documents in the collection, this does not scan to
        // reindex them.
    }

    pub fn get_document(&self, docid: DocID, page_cache: &PageCache) -> Option<Value> {
        let docid_key = &docid.0.to_be_bytes();
        let mut cursor = btree_find(self.document_btree_root, docid_key, false, page_cache);
        let entry = cursor.next();
        entry.as_ref()?; // Return if entry is None

        let (got_docid, document_bytes) = entry.unwrap();
        if got_docid != docid_key {
            return None;
        }

        get_document_body(&document_bytes, page_cache)
    }

    // TODO decide how to handle missing document. Should it be an error, or is it
    // fine to ignore it?
    fn delete(&mut self, docid: DocID, page_cache: &PageCache, page_allocator: &mut PageAllocator) {
        let docid_key = &docid.0.to_be_bytes();
        let mut cursor = btree_find(self.document_btree_root, docid_key, false, page_cache);
        let entry = cursor.next();
        if entry.is_none() {
            return;
        }

        let (got_docid, document_bytes) = entry.unwrap();
        if got_docid != docid_key {
            return;
        }

        let document = get_document_body(&document_bytes, page_cache)
            .expect("Failed to read document body");

        // Free overflow pages if present
        if (document_bytes[0] & FLAG_OVERFLOW) != 0 {
            let mut current_fpid = FilePageId(get_u64(&document_bytes, 9));
            while current_fpid != FilePageId(0) {
                let page = page_cache.lock_page(current_fpid);
                let next_page = FilePageId(get_u64(&page[..], 0));
                drop(page);
                page_allocator.free(current_fpid);
                current_fpid = next_page;
            }
        }

        btree_delete(self.document_btree_root,
            docid_key,
            page_cache,
            page_allocator);

        // Remove from indices
        for index in &self.indices {
            if let Ok(val) = lookup_field(&index.field, &document) {
                if let Ok(encoded) = encode_key(&val, docid) {
                    btree_delete(index.btree_root,
                        &encoded,
                        page_cache,
                        page_allocator);
                }
            }
        }
    }
}

fn get_document_body(document_bytes: &[u8], page_cache: &PageCache) -> Option<Value> {
    if (document_bytes[0] & FLAG_OVERFLOW) != 0 {
        // This is using overflow pages
        let mut length = get_u64(document_bytes, 1) as usize;
        let mut current_fpid = FilePageId(get_u64(document_bytes, 9));

        let mut content = Vec::with_capacity(length);
        while length > 0 {
            if current_fpid == FilePageId(0) {
                println!("Error: record truncated");
                break;
            }

            let page = page_cache.lock_page(current_fpid);
            current_fpid = FilePageId(get_u64(&page[..], 0));
            let to_copy = std::cmp::min(length, PAGE_SIZE - 8);
            content.extend_from_slice(&page[8..8 + to_copy]);
            length -= to_copy;
        }

        Some(serde_json::from_slice(&content).expect("Failed to parse JSON"))
    } else {
        // Stored inline
        Some(serde_json::from_slice(&document_bytes[1..]).expect("Failed to parse JSON"))
    }
}

//
// Converts a database field value into a byte array suitable for use as a
// B-tree key. B-tree keys must conform to two rules:
//   1. They must be unique.
//   2. Lexicographic byte order must match logical value order.
//
// To satisfy (1), we append the document ID as a tiebreaker. For (2), we
// convert non-lexicographic types: floats as sign-magnitude ordered binary,
// integers as unsigned offset binary. Strings are already in lexicographic
// order, but since they are variable-length, the document ID must be
// separated with a zero byte to preserve the sort order.
//
// Note: this encoding is one-way and not intended to be decoded.
//
pub fn encode_key(key: &Value, docid: DocID) -> Result<Vec<u8>, String> {
    // Prepend a tag in the event key types are mixed in an index.
    const TAG_BOOL: u8 = 1;
    const TAG_INT: u8 = 2;
    const TAG_FLOAT: u8 = 3;
    const TAG_STRING: u8 = 4;

    let mut encoded = match key {
        Value::Bool(b) => {
            vec![TAG_BOOL, if *b {1u8} else {0u8}]
        },
        Value::Number(n) => {
            // All encoded numbers are treated as bigendian
            if let Some(i) = n.as_i64() {
                let mut bytes = i.to_be_bytes();
                // Flip the sign to ensure negative values sort before positive
                // (the negative values already are in lexicographic order)
                bytes[0] ^= 0x80;
                let mut v = vec![TAG_INT];
                v.extend_from_slice(&bytes);
                v
            } else if let Some(f) = n.as_f64() {
                // Mask negative values so these will sort correctly.
                let bits = f.to_bits();
                let masked = if (bits & 0x80000000_00000000) != 0 {
                    // Negative, flip all bits
                    bits ^ 0xffffffff_ffffffff
                } else {
                    // Positive, flip sign bit
                    bits ^ 0x80000000_00000000
                };

                let mut v = vec![TAG_FLOAT];
                v.extend_from_slice(&masked.to_be_bytes());
                v
            } else {
                unreachable!();
            }
        },
        Value::String(s) => {
            let mut v = vec![TAG_STRING];
            v.extend_from_slice(s.as_bytes());
            v
        }
        _ => { return Err("Unindexable field type".to_string()); }
    };

    // The delimiter ensures sort order is preserved even for variable length
    // keys.
    encoded.push(0);

    // Append document ID to serve as a tie breaker.
    encoded.extend_from_slice(&docid.0.to_be_bytes());

    Ok(encoded)
}

pub struct SequentialScan {
    iterator: BTreeCursor,
    page_cache: PageCache
}

impl SequentialScan {
    fn new(collection: &Collection, page_cache: &PageCache) -> Self {
        Self {
            iterator: btree_iterate(collection.document_btree_root,
                false, page_cache),
            page_cache: page_cache.clone()
        }
    }
}

impl Iterator for SequentialScan {
    type Item = (DocID, Value);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((docid_bytes, value_bytes)) = self.iterator.next() {
            let docid = DocID(u64::from_be_bytes(docid_bytes.try_into().unwrap()));
            let doc = get_document_body(&value_bytes, &self.page_cache)?;
            Some((docid, doc))
        } else {
            None
        }
    }
}

pub struct IndexScan {
    iterator: BTreeCursor,
    reverse: bool,
    end_range: Option<Vec<u8>>,
    done: bool,
    collection: Rc<RefCell<Collection>>,
    page_cache: PageCache
}

impl IndexScan {
    fn new(collection_ref: Rc<RefCell<Collection>>, field_index: usize,
        start_range: Option<Value>, end_range: Option<Value>, reverse: bool,
        page_cache: &PageCache) -> Result<Self, String> {
        let collection = collection_ref.borrow();

        let iterator = if let Some(start_range) = start_range {
            let start_key = encode_key(&start_range, DocID(if reverse {u64::MAX} else {0}))?;
            btree_find(collection.indices[field_index].btree_root,
                &start_key,
                reverse, page_cache)
        } else {
            btree_iterate(collection.indices[field_index].btree_root,
                false, page_cache)
        };

        let end_key = if let Some(end_range) = end_range {
            Some(encode_key(&end_range, DocID(if reverse {0} else {u64::MAX}))?)
        } else {
            None
        };

        Ok(Self {
            iterator,
            reverse,
            end_range: end_key,
            done: false,
            collection: collection_ref.clone(),
            page_cache: page_cache.clone()
        })
    }
}

impl Iterator for IndexScan {
    type Item = (DocID, Value);

    fn next(&mut self) -> Option<Self::Item> {
        let docid_bytes = match self.iterator.next() {
            Some((key, docid)) => {
                if let Some(end) = &self.end_range {
                    if (self.reverse && &key < end) ||
                        (!self.reverse && &key > end) {
                        self.done = true;
                        return None
                    } else {
                        docid
                    }
                } else {
                    docid
                }
            },
            None => {
                self.done = true;
                return None
            }
        };

        let docid = DocID(u64::from_be_bytes(docid_bytes.try_into().expect("Invalid docid field")));
        let document = self.collection.borrow().get_document(docid, &self.page_cache);
        assert!(document.is_some(), "internal error: index out of sync, document doesn't exist");

        Some((docid, document.unwrap()))
    }
}

#[derive(Debug, Clone)]
enum PathElement {
    ArrayIndex(usize),
    FieldName(String)
}

impl std::fmt::Display for PathElement {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            PathElement::ArrayIndex(index) => write!(f, "[{}]", index)?,
            PathElement::FieldName(name) => write!(f, ".{}", name)?
        }

        Ok(())
    }
}

// A path uniquely identifies some element within a document.
#[derive(Debug, Clone)]
pub struct FieldPath(Vec<PathElement>);

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        for elem in &self.0 {
            elem.fmt(f)?;
        }

        Ok(())
    }
}

impl FieldPath {
    pub fn new(path_str: &str) -> Result<FieldPath, String> {
        let index_re = Regex::new(r"^([a-zA-Z0-9_\-]+)\[([0-9]+)\]$").unwrap();
        let field_re = Regex::new(r"^[a-zA-Z0-9_\-]+$").unwrap();
        let mut elements: Vec<PathElement> = Vec::new();
        for elem in path_str.split('.') {
            if elem.is_empty() {
                return Err("Empty path element".to_string());
            }

            if let Some(cap) = index_re.captures(elem) {
                elements.push(PathElement::FieldName(cap[1].to_string()));
                elements.push(PathElement::ArrayIndex(cap[2].parse().unwrap()));
            } else if field_re.is_match(elem) {
                elements.push(PathElement::FieldName(elem.to_string()));
            } else {
                return Err(format!("Invalid path element: {}", elem));
            }
        }

        Ok(FieldPath(elements))
    }
}

pub fn lookup_field(path: &FieldPath, record: &Value) -> Result<Value, String> {
    let mut current_val = record;
    let root = PathElement::FieldName("".to_string()); // TODO: this incurs allocation costs, slow.
    let mut parent = &root;
    for elem in &path.0 {
        match elem {
            PathElement::ArrayIndex(index) =>  {
                match current_val {
                    Value::Array(arr) => {
                        if *index >= arr.len() {
                            return Err(format!("Array index {} out of bounds for {}", *index, parent));
                        }

                        current_val = &arr[*index];
                    },
                    _ => { return Err(format!("Indexed non-array {}", parent)); }
                }
            }

            PathElement::FieldName(name) => {
                match current_val {
                    Value::Object(obj) => {
                        if let Some(val) = obj.get(name) {
                            current_val = val;
                        } else {
                            return Err(format!("Unknown field {}", name));
                        }
                    },
                    _ => { return Err(format!("Attempt to access field {} on non-object", name)); }
                }
            }
        }

        parent = elem;
    }

    Ok(current_val.clone())
}

#[cfg(test)]
mod tests {
    use crate::page_cache::*;
    use crate::mocks::{MockPersistentStore};
    use crate::btree::*;
    use crate::superblock::*;
    use std::rc::Rc;
    use std::cell::RefCell;
    use serde_json::{Value, json, Number};
    use super::*;

    fn create_document(index: usize) -> Value {
        json!({
            "index": index,
            "value": "abcdefgjiklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        })
    }

    #[test]
    fn test_key_encodings() {
        assert!(encode_key(&json!("abc"), DocID(0)).unwrap() <
            encode_key(&json!("abcd"), DocID(0)).unwrap());
        assert!(&encode_key(&json!("abce"), DocID(0)).unwrap() >
            &encode_key(&json!("abcd"), DocID(0)).unwrap());

        // Ensure docID doesn't break sort order
        assert!(&encode_key(&json!("abc"), DocID(0xffffffff_ffffffff)).unwrap() <
            &encode_key(&json!("abcd"), DocID(0)).unwrap());

        // DocID as tiebreaker with dups
        assert!(&encode_key(&json!("abc"), DocID(1)).unwrap() >
            &encode_key(&json!("abc"), DocID(0)).unwrap());

        // Floating point
        assert!(&encode_key(&json!(123.5), DocID(0)).unwrap() >
            &encode_key(&json!(123.4), DocID(0)).unwrap());
        assert!(&encode_key(&json!(-1024.5), DocID(0)).unwrap() <
            &encode_key(&json!(123.5), DocID(0)).unwrap());
        assert!(&encode_key(&json!(-1024.5), DocID(0)).unwrap() <
            &encode_key(&json!(-1023.5), DocID(0)).unwrap());

        // Integer
        assert!(&encode_key(&json!(100), DocID(0)).unwrap()
            > &encode_key(&json!(99), DocID(0)).unwrap());
        assert!(&encode_key(&json!(-100), DocID(0)).unwrap()
            < &encode_key(&json!(99), DocID(0)).unwrap());
        assert!(&encode_key(&json!(-100), DocID(0)).unwrap()
            < &encode_key(&json!(-99), DocID(0)).unwrap());

        // Boolean
        assert!(&encode_key(&json!(true), DocID(0)).unwrap() >
            &encode_key(&json!(false), DocID(0)).unwrap());

        // Mixed types
        assert!(&encode_key(&json!(true), DocID(0)).unwrap() <
            &encode_key(&json!(-223.4), DocID(0)).unwrap());
        assert!(&encode_key(&json!(22.4), DocID(0)).unwrap() >
            &encode_key(&json!(100), DocID(0)).unwrap());
        assert!(&encode_key(&json!(100), DocID(0)).unwrap() >
            &encode_key(&json!(true), DocID(0)).unwrap());
    }

    #[test]
    fn test_encode_key_invalid() {
        assert_eq!(encode_key(&json!({"foo": "bar"}), DocID(0)), Err("Unindexable field type".to_string()));
        assert_eq!(encode_key(&json!([1,2,3,4,5]), DocID(0)), Err("Unindexable field type".to_string()));
    }

    #[test]
    fn test_insert() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let mut docids: Vec<DocID> = Vec::new();
        for i in 0..100 {
            let _transaction = page_cache.begin_transaction();
            docids.push(collection.insert_document(&create_document(i),
                &page_cache, &mut allocator));
        }

        let mut i = 0;
        let iter = SequentialScan::new(&collection, &page_cache);
        for (docid, value) in iter {
            assert_eq!(docids[i], docid);
            assert_eq!(create_document(i), value);
            i += 1;
        }
    }

    fn create_collection() -> (PageCache, PageAllocator, Collection) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();
        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);
        let document_btree_root = allocator.alloc();
        {
            let mut page = page_cache.lock_page_mut(document_btree_root);
            init_btree_node(&mut page);
        }

        let collection = Collection {
            next_docid: 1,
            document_btree_root,
            indices: Vec::new()
        };

        (page_cache, allocator, collection)
    }

    fn populate_test_collection() -> (PageCache, PageAllocator, Collection, Vec<(DocID, Value)>) {
        let (mut page_cache, mut allocator, mut collection) = create_collection();

        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("name").unwrap(), &mut page_cache, &mut allocator);
        collection.create_index(&FieldPath::new("age").unwrap(), &mut page_cache, &mut allocator);
        collection.create_index(&FieldPath::new("avg").unwrap(), &mut page_cache, &mut allocator);

        let entries: [&str; _] = [
            r#"{"name": "James Smith", "age": 9, "avg": 0.160}"#,
            r#"{"name": "Edward Jones", "age": 32, "avg": 0.220}"#,
            r#"{"name": "Michael James", "age": 47, "avg": 0.116}"#,
            r#"{"name": "Adam Mitchell", "age": 103, "avg": 0.010}"#,
            r#"{"name": "Emily Davis", "age": 22, "avg": 0.305}"#,
            r#"{"name": "Madison Garcia", "age": 19, "avg": 0.250}"#,
            r#"{"name": "David Wilson", "age": 56, "avg": 0.180}"#,
        ];

        let mut documents: Vec<(DocID, Value)> = Vec::new();
        for entry in entries {
            let doc: Value = serde_json::from_str(entry).unwrap();
            documents.push((collection.insert_document(&doc, &mut page_cache, &mut allocator),
                doc.clone()));
        }

        (page_cache, allocator, collection, documents)
    }

    #[test]
    fn test_get_document() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();

        let transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones", "age": 32}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        drop(transaction);

        assert_eq!(collection.get_document(docid1, &page_cache).unwrap(), doc1);
        assert_eq!(collection.get_document(docid2, &page_cache).unwrap(), doc2);
    }

    // Nothing is returned by cursor
    #[test]
    fn test_get_document_not_present1() {
        let (page_cache, _allocator, collection) = create_collection();
        assert!(collection.get_document(DocID(999), &page_cache).is_none());
    }

    // Cursor returns a value but docid doesn't match (usually because it's been
    // deleted).
    #[test]
    fn test_get_document_not_present2() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();

        let transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones", "age": 32}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        collection.delete(docid1, &page_cache, &mut allocator);
        drop(transaction);

        assert!(collection.get_document(docid1, &page_cache).is_none());
    }

    #[test]
    fn test_sequential_scan() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let iter = SequentialScan::new(&collection, &page_cache);

        documents.sort_by_key(|(docid, _doc)| *docid);
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    #[test]
    fn test_index_scan_bad_key() {
        let (page_cache, _allocator, collection, _documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        // An array can't be a key...
        let result = IndexScan::new(collection_ref.clone(), 0,
            Some(json!([1,2,3,4])), None, false, &page_cache);
        assert!(result.is_err());
        assert_eq!(result.err().unwrap(), "Unindexable field type".to_string());
    }

    #[test]
    fn test_index_scan_int() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let iter = IndexScan::new(collection_ref, 1, None, None, false,
            &page_cache).expect("Failed to scan index");

        documents.sort_by_key(|(_docid, doc)| doc["age"].as_i64().unwrap());
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    // Specify a start index
    #[test]
    fn test_index_scan_int_start() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let min_age: i64 = 30;
        let iter = IndexScan::new(collection_ref, 1,
            Some(Value::Number(Number::from_i128(min_age as i128).unwrap())), None, false,
            &page_cache).expect("Failed to scan index");

        documents.retain(|(_docid, doc)| doc["age"].as_i64().unwrap() >= min_age);
        documents.sort_by_key(|(_docid, doc)| doc["age"].as_i64().unwrap());
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    #[test]
    fn test_index_scan_int_end() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let max_age: i64 = 50;
        let iter = IndexScan::new(collection_ref, 1,
            None, Some(Value::Number(Number::from_i128(max_age as i128).unwrap())), false,
            &page_cache).expect("Failed to scan index");

        documents.retain(|(_docid, doc)| doc["age"].as_i64().unwrap() <= max_age);
        documents.sort_by_key(|(_docid, doc)| doc["age"].as_i64().unwrap());
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    #[test]
    fn test_index_scan_string() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let iter = IndexScan::new(collection_ref, 0, None, None, false,
            &page_cache).expect("Failed to scan index");

        documents.sort_by_key(|(_docid, doc)| doc["name"].as_str().unwrap().to_string());
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    #[test]
    fn test_index_scan_float() {
        let (page_cache, _allocator, collection, mut documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let iter = IndexScan::new(collection_ref, 2, None, None, false,
            &page_cache).expect("Failed to scan index");

        documents.sort_by(|(_docid_a, doc_a), (_docid_b, doc_b)| {
            doc_a["avg"].as_f64().unwrap().partial_cmp(&doc_b["avg"].as_f64().unwrap()).unwrap()
        });
        let got_documents: Vec<_> = iter.collect();
        assert_eq!(documents, got_documents);
    }

    #[test]
    fn test_index_missing_key() {
        // If we set up an index and that field is not present in a document,
        // we'll silently skip adding it to the index.
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age").unwrap(), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones"}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_array_key() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age").unwrap(), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": {"foo": 1}}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_object_key() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age").unwrap(), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": [1,2,3]}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_path_display() {
        let path = FieldPath::new("phones[1].number").unwrap();
        assert_eq!(path.to_string(), ".phones[1].number");
    }

    const JSON_EXAMPLE: &str = r#"
        {
            "name": "Bob Dobalina",
            "age": 45,
            "phones": [
                {
                    "number": "867-5309",
                    "type": "home"
                },
                {
                    "number": "+15551212",
                    "type": "mobile"
                }
            ]
        }
    "#;

    #[test]
    fn test_lookup_field1() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();

        let path = FieldPath::new("phones[1].number").unwrap();
        let fieldval = lookup_field(&path, &doc).unwrap();
        assert_eq!(fieldval.as_str().unwrap(), "+15551212".to_string());
    }

    #[test]
    fn test_lookup_field2() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age").unwrap();
        let fieldval = lookup_field(&path, &doc).unwrap();
        assert_eq!(fieldval.as_number().unwrap().as_u64().unwrap(), 45u64);
    }

    #[test]
    fn test_bad_lookup_not_array() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age[2]").unwrap();
        assert_eq!(lookup_field(&path, &doc),
            Err("Indexed non-array .age".to_string()));
    }

    #[test]
    fn test_bad_lookup_array_index_oob() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("phones[2]").unwrap();
        assert_eq!(lookup_field(&path, &doc),
            Err("Array index 2 out of bounds for .phones".to_string()));
    }

    #[test]
    fn test_bad_lookup_unknown_field() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("ssn").unwrap();
        assert_eq!(lookup_field(&path, &doc),
            Err("Unknown field ssn".to_string()));
    }

    #[test]
    fn test_bad_lookup_not_object() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age.month").unwrap();
        assert_eq!(lookup_field(&path, &doc),
            Err("Attempt to access field month on non-object".to_string()));
    }

    #[test]
    fn field_path_invalid_characters() {
        let path = FieldPath::new("age.$month");
        assert!(path.is_err());
        assert_eq!(path.err().unwrap(), "Invalid path element: $month".to_string());
    }

    #[test]
    fn field_path_invalid_index() {
        let path = FieldPath::new("age[%%]");
        assert!(path.is_err());
        assert_eq!(path.err().unwrap(), "Invalid path element: age[%%]".to_string());
    }

    #[test]
    fn field_path_empty_element() {
        let path = FieldPath::new("age..month");
        assert!(path.is_err());
        assert_eq!(path.err().unwrap(), "Empty path element".to_string());
    }

    #[test]
    fn test_delete() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("foo").unwrap(), &mut page_cache, &mut allocator);
        collection.create_index(&FieldPath::new("bar").unwrap(), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"foo": "AAA", "bar": 1.2, "baz": true}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"foo": "BBBB", "bar": 2.3, "baz": false}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"foo": "CCCCC", "bar": 3.4, "baz": true}"#).unwrap();
        let docid3 = collection.insert_document(&doc3, &mut page_cache, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"foo": "DDDDDD", "bar": 4.5, "baz": false}"#).unwrap();
        let docid4 = collection.insert_document(&doc4, &mut page_cache, &mut allocator);

        collection.delete(docid2, &mut page_cache, &mut allocator);

        // Ensure this is removed from all indices
        {
            let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache); // foo
            let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key1, encode_key(&Value::String("AAA".to_string()), docid1).unwrap());
            assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);

            let Some((key3, val3)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key3, encode_key(&Value::String("CCCCC".to_string()), docid3).unwrap());
            assert_eq!(u64::from_be_bytes(val3.try_into().unwrap()), docid3.0);

            let Some((key4, val4)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key4, encode_key(&Value::String("DDDDDD".to_string()), docid4).unwrap());
            assert_eq!(u64::from_be_bytes(val4.try_into().unwrap()), docid4.0);

            assert_eq!(iter.next(), None);
        }

        // Second index
        {
            let mut iter = btree_iterate(collection.indices[1].btree_root, false, &mut page_cache); // bar
            let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key1, encode_key(&Value::Number(Number::from_f64(1.2).unwrap()), docid1).unwrap());
            assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);

            let Some((key3, val3)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key3, encode_key(&Value::Number(Number::from_f64(3.4).unwrap()), docid3).unwrap());
            assert_eq!(u64::from_be_bytes(val3.try_into().unwrap()), docid3.0);

            let Some((key4, val4)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key4, encode_key(&Value::Number(Number::from_f64(4.5).unwrap()), docid4).unwrap());
            assert_eq!(u64::from_be_bytes(val4.try_into().unwrap()), docid4.0);

            assert_eq!(iter.next(), None);
        }

        // Scan main document btree
        {
            let mut iter = btree_iterate(collection.document_btree_root, false, &mut page_cache); // bar
            let Some((key1, _)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key1, docid1.0.to_be_bytes());

            let Some((key3, _)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key3, docid3.0.to_be_bytes());

            let Some((key4, _)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key4, docid4.0.to_be_bytes());

            assert_eq!(iter.next(), None);
        }
    }

    // The field never existed
    #[test]
    fn test_delete_nonexistent1() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.delete(DocID(999), &mut page_cache, &mut allocator);
        let mut cursor = btree_iterate(collection.document_btree_root, false, &mut page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Delete the same record twice. Delete takes a different code path in this case.
    #[test]
    fn test_delete_nonexistent2() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"foo": "AAA", "bar": 1.2}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"foo": "BBBB", "bar": 2.3}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        collection.delete(docid1, &mut page_cache, &mut allocator);
        collection.delete(docid1, &mut page_cache, &mut allocator);

        // Ensure second record is still present
        let mut cursor = btree_iterate(collection.document_btree_root, false, &mut page_cache);
        let Some((key2, _)) = cursor.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, docid2.0.to_be_bytes());
        assert_eq!(cursor.next(), None);
    }

    // We have an index, but this specific document does not have the corresponding field.
    #[test]
    fn test_delete_missing_index_field() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age").unwrap(), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith"}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones"}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        collection.delete(docid1, &mut page_cache, &mut allocator);

        // Ensure second record is still present
        let mut cursor = btree_iterate(collection.document_btree_root, false, &mut page_cache);
        let Some((key2, _)) = cursor.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, docid2.0.to_be_bytes());
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_overflow() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        // Create a large document
        let large_value = "x".repeat(0x4000);
        let doc = json!({"foo": large_value});
        let docid = collection.insert_document(&doc, &mut page_cache, &mut allocator);

        // Ensure we can read it back correctly
        let mut iter = SequentialScan::new(&collection, &page_cache);
        let Some((got_docid, got_doc)) = iter.next() else { panic!("couldn't get record"); };

        assert_eq!(docid, got_docid);
        assert_eq!(got_doc["foo"].as_str().unwrap(), large_value);

        // Delete the document, ensure pages are freed
        collection.delete(docid, &mut page_cache, &mut allocator);

        let mut iter = SequentialScan::new(&collection, &page_cache);
        assert!(iter.next().is_none());

    }
}
