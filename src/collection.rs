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
// indexed by a unique 64-bit identifier (the DocId). There are ancilary BTrees
// for any indices fields within these documents to allow fast sorting and
// searching.
// If a document is too large to fit in the BTree node, this will allocate
// A linked list of external pages to store the data. This allows arbitrarily
// large records.

use crate::btree::*;
use crate::page_allocator::*;
use crate::page_cache::*;
use crate::util::*;
use regex::Regex;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;

const FLAG_OVERFLOW: u8 = 0x80;

// Unique identifier (within a collection) for a specific document.
#[derive(PartialEq, Eq, Ord, PartialOrd, Debug, Clone, Copy, Hash)]
pub struct DocId(pub u64);

struct Index {
    field: FieldPath,
    btree: BTree
}

pub struct Collection {
    name: String,
    next_docid: u64,
    document_tree: BTree,
    indices: Vec<Index>,
    page_cache: PageCache
}

impl Collection {
    pub fn open(metadata: &Value, page_cache: &PageCache) -> Self {
        let mut indices: Vec<Index> = Vec::new();
        for index in metadata["indices"].as_array().expect("indices is not an array") {
            indices.push(Index{
                field: FieldPath::new(index["path"].as_str()
                    .expect("path is not a string")).expect("invalid field path"),
                btree: BTree::open(PageNum::from_u64(index["root_pnum"].as_u64()
                    .expect("root_pnum is not an integer")), &page_cache)
            });
        }

        let document_tree = BTree::open(PageNum::from_u64(
            metadata["root_page_pnum"].as_u64()
            .expect("root_page_pnum is not an integer")),
            page_cache
        );

        let next_docid = if let Some(key) = document_tree.get_max_key() {
            u64::from_be_bytes(key.try_into().unwrap()) + 1
        } else {
            1u64
        };

        Collection {
            name: metadata["name"].to_string(),
            next_docid,
            document_tree,
            indices,
            page_cache: page_cache.clone()
        }
    }

    pub fn create(name: &str, page_cache: &PageCache, page_allocator: &mut PageAllocator) -> Self {
        Collection {
            name: name.to_string(),
            next_docid: 1,
            document_tree: BTree::create(page_cache, page_allocator),
            indices: Vec::new(),
            page_cache: page_cache.clone()
        }
    }

    // Creates a new collection data structure with the root of the document BTree
    // at a specified page. Used during database creation to create the _meta
    // table.
    pub fn create_at(name: &str, page_cache: &PageCache, root_page: PageNum) -> Self {
        Collection {
            name: name.to_string(),
            next_docid: 1,
            document_tree: BTree::create_at(page_cache, root_page),
            indices: Vec::new(),
            page_cache: page_cache.clone()
        }
    }

    // The returned JSON object describe the on-disk format of this collection.
    pub fn get_metadata(&self) -> Value {
        json!({
            "name": self.name,
            "root_page_pnum": self.document_tree.get_root_page_id().as_u64(),
            "indices": self.indices.iter().map(|index| {
                json!({
                    "path": index.field.to_string(),
                    "root_pnum": index.btree.get_root_page_id().as_u64()
                })
            }).collect::<Vec<Value>>()
        })
    }

    pub fn insert(&mut self,
        document: &Value,
        page_allocator: &mut PageAllocator) -> DocId {

        let docid = self.next_docid;
        self.next_docid += 1;
        self.insert_internal(DocId(docid), document, page_allocator);
        DocId(docid)
    }

    fn insert_internal(&mut self,
        docid: DocId,
        document: &Value,
        page_allocator: &mut PageAllocator) {

        assert!(document.is_object(), "Attempt to insert non-object");

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
            let mut page_num = Some(page_allocator.alloc());
            *pointer.u64_field_mut(9) = page_num.to_bytes();
            while offset < content.len() {
                let mut page = self.page_cache.lock_page_mut(page_num.expect("null page num"));
                let to_copy = std::cmp::min(content.len() - offset, PAGE_SIZE - 8);
                page[8..8 + to_copy].copy_from_slice(&content[offset..offset + to_copy]);
                page_num = if offset + to_copy < content.len() {
                    Some(page_allocator.alloc())
                } else {
                    None
                };

                *page.u64_field_mut(0) = page_num.to_bytes();
                offset += to_copy;
            }

            self.document_tree.insert(&docid.0.to_be_bytes(), &pointer, page_allocator);
        } else {
            self.document_tree.insert(&docid.0.to_be_bytes(), &content, page_allocator);
        }

        // Update indices
        for index in &mut self.indices {
            if let Ok(val) = lookup_field(&index.field, document) {
                if let Ok(encoded) = encode_key(&val, docid) {
                    index.btree.insert(
                        &encoded,
                        &docid.0.to_be_bytes(),
                        page_allocator);
                }
            }
        }
    }

    pub fn create_index(&mut self, path: &FieldPath,
        page_allocator: &mut PageAllocator) {

        // TODO fail if index already exists

        let index = Index {
            field: path.clone(),
            btree: BTree::create(&self.page_cache, page_allocator)
        };

        self.indices.push(index);

        // Scan existing documents and add to index
        let cursor = self.document_tree.iterate(false);
        for (docid_bytes, document_bytes) in cursor {
            let docid = DocId(u64::from_be_bytes(docid_bytes.try_into().unwrap()));
            let document = materialize_doc(&document_bytes, &self.page_cache);

            if let Ok(val) = lookup_field(path, &document) {
                if let Ok(encoded) = encode_key(&val, docid) {
                    self.indices.last().unwrap().btree.insert(
                        &encoded,
                        &docid.0.to_be_bytes(),
                        page_allocator);
                } else {
                    println!("Error encoding key");
                }
            }
        }
    }

    pub fn get(&self, docid: DocId) -> Option<Value> {
        let docid_key = &docid.0.to_be_bytes();
        let mut cursor = self.document_tree.find(docid_key, false);
        let entry = cursor.next();
        entry.as_ref()?; // Return if entry is None

        let (got_docid, document_bytes) = entry.unwrap();
        if got_docid != docid_key {
            return None;
        }

        Some(materialize_doc(&document_bytes, &self.page_cache))
    }

    pub fn update(&mut self,
        docid: DocId,
        document: &Value,
        page_allocator: &mut PageAllocator) {

        // TODO optimize this. We could retrieve the existing record and
        // determine which keys have changed to skip updating indices that
        // don't need it.
        self.delete(docid, page_allocator);
        self.insert_internal(docid, document, page_allocator);
    }

    // TODO decide how to handle missing document. Should it be an error, or is it
    // fine to ignore it?
    fn delete(&mut self, docid: DocId, page_allocator: &mut PageAllocator) {
        let docid_key = &docid.0.to_be_bytes();
        let mut cursor = self.document_tree.find(docid_key, false);
        let entry = cursor.next();
        if entry.is_none() {
            return;
        }

        let (got_docid, document_bytes) = entry.unwrap();
        if got_docid != docid_key {
            return;
        }

        let document = materialize_doc(&document_bytes, &self.page_cache);

        // Free overflow pages if present
        if (document_bytes[0] & FLAG_OVERFLOW) != 0 {
            let mut current_pnum = PageNum::from_bytes(document_bytes.u64_field(9));
            while let Some(pnum) = current_pnum {
                let page = self.page_cache.lock_page(pnum);
                let next_page = PageNum::from_bytes(page.u64_field(0));
                drop(page);
                page_allocator.free(pnum);
                current_pnum = next_page;
            }
        }

        self.document_tree.delete(docid_key, page_allocator);

        // Remove from indices
        for index in &mut self.indices {
            if let Ok(val) = lookup_field(&index.field, &document) {
                if let Ok(encoded) = encode_key(&val, docid) {
                    index.btree.delete(&encoded, page_allocator);
                }
            }
        }
    }

    pub fn find_index(&self, name: &str) -> Option<usize> {
        if let Ok(field) = FieldPath::new(name) {
            for (i, index) in self.indices.iter().enumerate() {
                if index.field == field {
                    return Some(i);
                }
            }
        }

        return None
    }
}

fn materialize_doc(document_bytes: &[u8], page_cache: &PageCache) -> Value {
    if (document_bytes[0] & FLAG_OVERFLOW) != 0 {
        // This is using overflow pages
        let mut length = get_u64(document_bytes, 1) as usize;
        let mut current_pnum = PageNum::from_bytes(document_bytes.u64_field(9));

        let mut content = Vec::with_capacity(length);
        while length > 0 {
            if current_pnum.is_none() {
                panic!("Error: record truncated");
            }

            let page = page_cache.lock_page(current_pnum.unwrap());
            current_pnum = PageNum::from_bytes(page.u64_field(0));
            let to_copy = std::cmp::min(length, PAGE_SIZE - 8);
            content.extend_from_slice(&page[8..8 + to_copy]);
            length -= to_copy;
        }

        serde_json::from_slice(&content).expect("Failed to parse JSON")
    } else {
        // Stored inline
        serde_json::from_slice(&document_bytes[1..]).expect("Failed to parse JSON")
    }
}

//
// Converts a database field value into a byte array suitable for use as a
// B-tree key. B-tree keys must conform to two rules:
//   1. They must be unique.
//   2. Lexicographic byte order must match expected logical value order.
//
// However, collection indices don't obey these rules, allowing duplicate keys
// and supporting encodings like floating point which are not lexicographically
// ordered. To bridge this gap, we create an alternate encoding to use as keys
// in the indices. This appends the document ID to enforce uniqueness and
// converts non-lexicographic types to alternate forms: floats as
// sign-magnitude binary, integers as unsigned offset binary. Strings are already
// in lexicographic order, but since they are variable-length, the document ID
// must be separated with a zero byte to preserve the sort order.
//
// Note: this encoding is one-way and not intended to be decoded. We do the
// same one-way transform on keys when we are searching and the unaltered
// version is stored in the record body)
//
pub fn encode_key(key: &Value, docid: DocId) -> Result<Vec<u8>, String> {
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
            if s.len() > MAX_RECORD_SIZE - 16 {
                return Err("Key is too large".to_string());
            }

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
    pub fn new(collection: &Collection, page_cache: &PageCache) -> Self {
        Self {
            iterator: collection.document_tree.iterate(false),
            page_cache: page_cache.clone()
        }
    }
}

impl Iterator for SequentialScan {
    type Item = (DocId, Value);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some((docid_bytes, value_bytes)) = self.iterator.next() {
            let docid = DocId(u64::from_be_bytes(docid_bytes.try_into().unwrap()));
            let doc = materialize_doc(&value_bytes, &self.page_cache);
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
    pub fn new(collection_ref: Rc<RefCell<Collection>>, field_index: usize,
        start_range: Option<Value>, end_range: Option<Value>, reverse: bool,
        page_cache: &PageCache) -> Result<Self, String> {
        let collection = collection_ref.borrow_mut();

        let iterator = if let Some(start_range) = start_range {
            let start_key = encode_key(&start_range, DocId(if reverse {u64::MAX} else {0}))?;
            collection.indices[field_index].btree.find(&start_key, reverse)
        } else {
            collection.indices[field_index].btree.iterate(false)
        };

        let end_key = if let Some(end_range) = end_range {
            Some(encode_key(&end_range, DocId(if reverse {0} else {u64::MAX}))?)
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
    type Item = (DocId, Value);

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

        let docid = DocId(u64::from_be_bytes(docid_bytes.try_into().expect("Invalid docid field")));
        let document = self.collection.borrow().get(docid);
        assert!(document.is_some(), "internal error: index out of sync, document doesn't exist");

        Some((docid, document.unwrap()))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum PathElement {
    ArrayIndex(usize),
    FieldName(String)
}

impl std::fmt::Display for PathElement {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        match self {
            PathElement::ArrayIndex(index) => { write!(f, "[{}]", index)?; }
            PathElement::FieldName(name) => { write!(f, "{}", name)?; }
        }

        Ok(())
    }
}

// A path uniquely identifies some element within a document.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FieldPath(Vec<PathElement>);

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        for (i, elem) in self.0.iter().enumerate() {
            if i > 0 && matches!(elem, PathElement::FieldName(_)) {
                write!(f, ".")?;
            }

            write!(f, "{}", elem)?;
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
    use crate::mocks::MockPersistentStore;
    use crate::page_cache::*;
    use crate::superblock::*;
    use rand::rngs::SmallRng;
    use rand::SeedableRng;
    use rand::seq::SliceRandom;
    use serde_json::{Value, json, Number};
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::*;

    fn create_document(index: usize) -> Value {
        json!({
            "index": index,
            "value": "abcdefgjiklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        })
    }

    fn create_collection() -> (PageCache, PageAllocator, Collection) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(25, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();
        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);
        let collection = Collection::create("stuff", &page_cache, &mut allocator);

        (page_cache, allocator, collection)
    }

    fn populate_test_collection() -> (PageCache, PageAllocator, Collection, Vec<(DocId, Value)>) {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("name").unwrap(), &mut allocator);
        collection.create_index(&FieldPath::new("age").unwrap(), &mut allocator);
        collection.create_index(&FieldPath::new("avg").unwrap(), &mut allocator);

        let entries: [&str; _] = [
            r#"{"name": "James Smith", "age": 9, "avg": 0.160}"#,
            r#"{"name": "Edward Jones", "age": 32, "avg": 0.220}"#,
            r#"{"name": "Michael James", "age": 47, "avg": 0.116}"#,
            r#"{"name": "Adam Mitchell", "age": 103, "avg": 0.010}"#,
            r#"{"name": "Emily Davis", "age": 22, "avg": 0.305}"#,
            r#"{"name": "Madison Garcia", "age": 19, "avg": 0.250}"#,
            r#"{"name": "David Wilson", "age": 56, "avg": 0.180}"#,
        ];

        let mut documents: Vec<(DocId, Value)> = Vec::new();
        for entry in entries {
            let doc: Value = serde_json::from_str(entry).unwrap();
            documents.push((collection.insert(&doc, &mut allocator),
                doc.clone()));
        }

        (page_cache, allocator, collection, documents)
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
    fn test_key_encodings() {
        assert!(encode_key(&json!("abc"), DocId(0)).unwrap() <
            encode_key(&json!("abcd"), DocId(0)).unwrap());
        assert!(&encode_key(&json!("abce"), DocId(0)).unwrap() >
            &encode_key(&json!("abcd"), DocId(0)).unwrap());

        // Ensure docID doesn't break sort order
        assert!(&encode_key(&json!("abc"), DocId(0xffffffff_ffffffff)).unwrap() <
            &encode_key(&json!("abcd"), DocId(0)).unwrap());

        // DocId as tiebreaker with dups
        assert!(&encode_key(&json!("abc"), DocId(1)).unwrap() >
            &encode_key(&json!("abc"), DocId(0)).unwrap());

        // Floating point
        assert!(&encode_key(&json!(123.5), DocId(0)).unwrap() >
            &encode_key(&json!(123.4), DocId(0)).unwrap());
        assert!(&encode_key(&json!(-1024.5), DocId(0)).unwrap() <
            &encode_key(&json!(123.5), DocId(0)).unwrap());
        assert!(&encode_key(&json!(-1024.5), DocId(0)).unwrap() <
            &encode_key(&json!(-1023.5), DocId(0)).unwrap());

        // Integer
        assert!(&encode_key(&json!(100), DocId(0)).unwrap()
            > &encode_key(&json!(99), DocId(0)).unwrap());
        assert!(&encode_key(&json!(-100), DocId(0)).unwrap()
            < &encode_key(&json!(99), DocId(0)).unwrap());
        assert!(&encode_key(&json!(-100), DocId(0)).unwrap()
            < &encode_key(&json!(-99), DocId(0)).unwrap());

        // Boolean
        assert!(&encode_key(&json!(true), DocId(0)).unwrap() >
            &encode_key(&json!(false), DocId(0)).unwrap());

        // Mixed types
        assert!(&encode_key(&json!(true), DocId(0)).unwrap() <
            &encode_key(&json!(-223.4), DocId(0)).unwrap());
        assert!(&encode_key(&json!(22.4), DocId(0)).unwrap() >
            &encode_key(&json!(100), DocId(0)).unwrap());
        assert!(&encode_key(&json!(100), DocId(0)).unwrap() >
            &encode_key(&json!(true), DocId(0)).unwrap());
    }

    #[test]
    fn test_encode_key_bad_type() {
        assert_eq!(encode_key(&json!({"foo": "bar"}), DocId(0)), Err("Unindexable field type".to_string()));
        assert_eq!(encode_key(&json!([1,2,3,4,5]), DocId(0)), Err("Unindexable field type".to_string()));
    }

    #[test]
    fn test_encode_key_too_large() {
        assert_eq!(encode_key(&json!("x".repeat(0x4000)), DocId(0)), Err("Key is too large".to_string()));
    }

    #[test]
    fn test_create_reopen() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let mut docids = {
            let _transaction = page_cache.begin_transaction();
            collection.create_index(&FieldPath::new("index").unwrap(), &mut allocator);
            vec![
                collection.insert(&create_document(1), &mut allocator),
                collection.insert(&create_document(2), &mut allocator)
            ]
        };

        let metadata = collection.get_metadata();
        drop(collection);

        let mut collection = Collection::open(&metadata, &page_cache);

        assert_eq!(collection.get(docids[0]).unwrap(), create_document(1));
        assert_eq!(collection.get(docids[1]).unwrap(), create_document(2));

        // Ensure index is correct
        assert_eq!(collection.indices.len(), 1);
        assert_eq!(collection.indices[0].field.to_string(), "index");

        // Insert another record to ensure the docid is unique
        {
            let _transaction = page_cache.begin_transaction();
            let new_docid = collection.insert(&create_document(3), &mut allocator);
            assert!(!docids.contains(&new_docid));
            docids.push(new_docid);
        }

        // Validate everything is intact
        let index_iter = collection.indices[0].btree.iterate(false);
        for (i, (key, value)) in index_iter.enumerate() {
            assert_eq!(key, encode_key(&json!(i + 1), docids[i]).unwrap());
            assert_eq!(value, docids[i].0.to_be_bytes());
        }
    }

    #[test]
    fn test_insert() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let mut docids: Vec<DocId> = Vec::new();
        for i in 0..100 {
            let _transaction = page_cache.begin_transaction();
            docids.push(collection.insert(&create_document(i),
                &mut allocator));
        }

        let mut i = 0;
        let iter = SequentialScan::new(&collection, &page_cache);
        for (docid, value) in iter {
            assert_eq!(docids[i], docid);
            assert_eq!(create_document(i), value);
            i += 1;
        }
    }

    #[test]
    #[should_panic = "Attempt to insert non-object"]
    fn test_insert_non_object() {
        let (_page_cache, mut allocator, mut collection) = create_collection();
        collection.insert(&json!([1,2,3,4]), &mut allocator);
    }

    #[test]
    fn test_get_document() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones", "age": 32}"#).unwrap();
        let docid2 = collection.insert(&doc2, &mut allocator);
        drop(transaction);

        assert_eq!(collection.get(docid1).unwrap(), doc1);
        assert_eq!(collection.get(docid2).unwrap(), doc2);
    }

    // Nothing is returned by cursor
    #[test]
    fn test_get_document_not_present1() {
        let (_page_cache, _allocator, collection) = create_collection();
        assert!(collection.get(DocId(999)).is_none());
    }

    // Cursor returns a value but docid doesn't match (usually because it's been
    // deleted).
    #[test]
    fn test_get_document_not_present2() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones", "age": 32}"#).unwrap();
        collection.insert(&doc2, &mut allocator);
        collection.delete(docid1, &mut allocator);
        drop(transaction);

        assert!(collection.get(docid1).is_none());
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
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age").unwrap(), &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones"}"#).unwrap();
        collection.insert(&doc2, &mut allocator);

        let mut iter = collection.indices[0].btree.iterate(false);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_array_key() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age").unwrap(), &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": {"foo": 1}}"#).unwrap();
        collection.insert(&doc2, &mut allocator);

        let mut iter = collection.indices[0].btree.iterate(false);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_object_key() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age").unwrap(), &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": [1,2,3]}"#).unwrap();
        collection.insert(&doc2, &mut allocator);

        let mut iter = collection.indices[0].btree.iterate(false);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_be_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    // Add an index after populating the collection, ensure it reindexes
    #[test]
    fn test_create_index_after() {
        let (page_cache, mut allocator, collection) = create_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let _transaction = page_cache.begin_transaction();

        let mut rng = SmallRng::seed_from_u64(12345u64);
        let mut shuffled_keys: Vec<u64> = (0..10).collect();
        shuffled_keys.shuffle(&mut rng);

        let mut orig_docs: Vec<(DocId, Value)> = Vec::new();
        for key in shuffled_keys {
            let doc: Value = json!({"key": key});
            orig_docs.push((collection_ref.borrow_mut().insert(&doc, &mut allocator),
                doc.clone()));
        }

        // Insert a record with an unindexable key. Ensure indexing code properly ignores it.
        collection_ref.borrow_mut().insert(&json!({"key": [1,2,3,4,5]}), &mut allocator);

        // Create record where the key doesn't exist
        collection_ref.borrow_mut().insert(&json!({"foo": 0}), &mut allocator);

        collection_ref.borrow_mut().create_index(&FieldPath::new("key").unwrap(), &mut allocator);
        orig_docs.sort_by_key(|(_docid, doc)| doc["key"].as_u64().unwrap());

        let fetched_docs: Vec<_> = IndexScan::new(collection_ref, 0, None, None, false,
            &page_cache).expect("Failed to scan index").collect();
        assert_eq!(orig_docs, fetched_docs);
    }

    #[test]
    fn test_path_display() {
        let path = FieldPath::new("phones[1].number").unwrap();
        assert_eq!(path.to_string(), "phones[1].number");
    }

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
            Err("Indexed non-array age".to_string()));
    }

    #[test]
    fn test_bad_lookup_array_index_oob() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("phones[2]").unwrap();
        assert_eq!(lookup_field(&path, &doc),
            Err("Array index 2 out of bounds for phones".to_string()));
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
    fn test_update() {
        let (page_cache, mut page_allocator, collection, documents) = populate_test_collection();
        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));

        let _transaction = page_cache.begin_transaction();
        let mut new_doc = documents[1].1.clone();
        let new_name = Value::String(format!("Dr. {}", documents[1].1["name"].as_str().unwrap()));
        new_doc["name"] = new_name;
        collection_ref.borrow_mut().update(documents[1].0, &new_doc,
            &mut page_allocator);
        let mut new_documents: Vec<_> = documents.iter().map(|(docid, doc)| {
            if *docid == documents[1].0 {
                (*docid, new_doc.clone())
            } else {
                (*docid, doc.clone())
            }
        }).collect();

        let iter = IndexScan::new(collection_ref, 0, None, None, false,
            &page_cache).expect("Failed to scan index");
        let got_documents: Vec<_> = iter.collect();

        new_documents.sort_by_key(|(_docid, doc)| doc["name"].as_str().unwrap().to_string());
        assert_eq!(new_documents, got_documents);
    }


    #[test]
    fn test_delete() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("foo").unwrap(), &mut allocator);
        collection.create_index(&FieldPath::new("bar").unwrap(), &mut allocator);

        let doc1 = serde_json::from_str(r#"{"foo": "AAA", "bar": 1.2, "baz": true}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"foo": "BBBB", "bar": 2.3, "baz": false}"#).unwrap();
        let docid2 = collection.insert(&doc2, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"foo": "CCCCC", "bar": 3.4, "baz": true}"#).unwrap();
        let docid3 = collection.insert(&doc3, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"foo": "DDDDDD", "bar": 4.5, "baz": false}"#).unwrap();
        let docid4 = collection.insert(&doc4, &mut allocator);

        collection.delete(docid2, &mut allocator);

        // Ensure this is removed from all indices
        {
            let mut iter = collection.indices[0].btree.iterate(false); // foo
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
            let mut iter = collection.indices[1].btree.iterate(false); // bar
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
            let mut iter = collection.document_tree.iterate(false); // bar
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
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.delete(DocId(999), &mut allocator);
        let mut cursor = collection.document_tree.iterate(false);
        assert_eq!(cursor.next(), None);
    }

    // Delete the same record twice. Delete takes a different code path in this case.
    #[test]
    fn test_delete_nonexistent2() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        let doc1 = serde_json::from_str(r#"{"foo": "AAA", "bar": 1.2}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"foo": "BBBB", "bar": 2.3}"#).unwrap();
        let docid2 = collection.insert(&doc2, &mut allocator);

        collection.delete(docid1, &mut allocator);
        collection.delete(docid1, &mut allocator);

        // Ensure second record is still present
        let mut cursor = collection.document_tree.iterate(false);
        let Some((key2, _)) = cursor.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, docid2.0.to_be_bytes());
        assert_eq!(cursor.next(), None);
    }

    // We have an index, but this specific document does not have the corresponding field.
    #[test]
    fn test_delete_missing_index_field() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age").unwrap(), &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith"}"#).unwrap();
        let docid1 = collection.insert(&doc1, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones"}"#).unwrap();
        let docid2 = collection.insert(&doc2, &mut allocator);

        collection.delete(docid1, &mut allocator);

        // Ensure second record is still present
        let mut cursor = collection.document_tree.iterate(false);
        let Some((key2, _)) = cursor.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, docid2.0.to_be_bytes());
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_overflow_pages() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        // Create a large document (>128k)
        let large_value = "x".repeat(0x20000);
        let doc = json!({"foo": large_value});
        let docid = collection.insert(&doc, &mut allocator);

        // Ensure we can read it back correctly
        let mut iter = SequentialScan::new(&collection, &page_cache);
        let Some((got_docid, got_doc)) = iter.next() else { panic!("couldn't get record"); };

        assert_eq!(docid, got_docid);
        assert_eq!(got_doc["foo"].as_str().unwrap(), large_value);

        // Delete the document
        collection.delete(docid, &mut allocator);

        let mut iter = SequentialScan::new(&collection, &page_cache);
        assert!(iter.next().is_none());

        // Ensure we've reclaimed storage (the root page will remain)
        assert!(allocator.total_allocs - allocator.total_frees < 2);
    }

    #[test]
    #[should_panic = "Error: record truncated"]
    fn test_overflow_truncated() {
        let (page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        // XXX hack: allocate and free the page, which will tell us where the
        // pages will be placed.
        let page_num = allocator.alloc();
        allocator.free(page_num);

        // Create a large document
        let large_value = "x".repeat(0x4000);
        let doc = json!({"foo": large_value});
        let _docid = collection.insert(&doc, &mut allocator);

        // XXX hack hard coded page address
        {
            let mut page = page_cache.lock_page_mut(PageNum::from_u64(page_num.as_u64() + 1));
            *page.u64_field_mut(0) = None.to_bytes();
        }

        SequentialScan::new(&collection, &page_cache).next();
    }

    // Key lengths are limited in this implementation. Ensure if we try to add a very
    // large key it ignores it rather than doing something bad.
    #[test]
    fn test_index_large_field() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("image_data").unwrap(), &mut allocator);

        let doc1 = json!({"image_data": "abcd"});
        let docid1 = collection.insert(&doc1, &mut allocator);

        let doc2 = json!({"image_data": "x".repeat(0x4000)});
        let _docid2 = collection.insert(&doc2, &mut allocator);

        drop(transaction);

        let collection_ref: Rc<RefCell<Collection>> = Rc::new(RefCell::new(collection));
        let mut iter = IndexScan::new(collection_ref.clone(), 0, None, None, false, &page_cache)
            .expect("failed to open cursor");
        assert_eq!(iter.next(), Some((docid1, doc1)));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_find_index() {
        let (page_cache, mut allocator, mut collection) = create_collection();

        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("foo").unwrap(), &mut allocator);
        collection.create_index(&FieldPath::new("bar.baz").unwrap(), &mut allocator);
        collection.create_index(&FieldPath::new("boo").unwrap(), &mut allocator);

        assert_eq!(collection.find_index("foo"), Some(0));
        assert_eq!(collection.find_index("bar.baz"), Some(1));
        assert_eq!(collection.find_index("boo"), Some(2));
        assert_eq!(collection.find_index("frotz"), None);
    }
}
