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

// TODO: collection metadata is not persisted.

use serde_json::Value;
use crate::btree::*;
use crate::page_cache::*;
use crate::page_allocator::*;
use regex::Regex;

// Unique identifier (within a collection) for a specific document.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
struct DocID(u64);

struct Index {
    field: FieldPath,
    btree_root: FilePageId
}

struct Collection {
    next_docid: u64,
    document_btree_root: FilePageId,
    indices: Vec<Index>
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
fn encode_key(key: &Value, docid: DocID) -> Option<Vec<u8>> {
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
                return None;
            }
        },
        Value::String(s) => {
            let mut v = vec![TAG_STRING];
            v.extend_from_slice(s.as_bytes());
            v
        }
        _ => { return None; }
    };

    // The delimiter ensures sort order is preserved even for variable length
    // keys.
    encoded.push(0);

    // Append document ID to serve as a tie breaker.
    encoded.extend_from_slice(&docid.0.to_be_bytes());

    Some(encoded)
}

impl Collection {
    pub fn insert_document(&mut self,
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

        // Update indices
        for index in &self.indices {
            if let Some(encoded) = lookup_field(&index.field, document)
                .ok()
                .and_then(|val| encode_key(&val, DocID(docid))) {
                btree_insert(index.btree_root,
                    &encoded,
                    &docid.to_le_bytes(),
                    page_cache,
                    page_allocator);
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
    }

    fn iterate(&mut self, page_cache: &PageCache) -> impl Iterator<Item = (DocID, Value)> {
        let doc_cursor = btree_iterate(self.document_btree_root, false, page_cache);
        doc_cursor.map(|(key, value)| {
            let docid = DocID(u64::from_be_bytes(key.try_into().unwrap()));
            let doc = serde_json::from_slice(&value).expect("Failed to parse JSON");

            (docid, doc)
        })
    }

    // TODO decide how to handle missing document. Should it be an error, or is it
    // fine to ignore it?
    fn delete(&mut self, docid: DocID, page_cache: &PageCache, page_allocator: &mut PageAllocator) {
        let docid_key = &docid.0.to_be_bytes();
        let mut cursor = btree_find(self.document_btree_root, docid_key, false, page_cache);
        let entry = cursor.next();
        if entry.is_none() {
            // Not present, case 1
            return;
        }

        let (got_docid, document_bytes) = entry.unwrap();
        if got_docid != docid_key {
            // not present, case 2
            return;
        }

        btree_delete(self.document_btree_root,
            docid_key,
            page_cache,
            page_allocator);

        let document = serde_json::from_slice(&document_bytes).expect("Failed to parse JSON");

        // Remove from indices
        for index in &self.indices {
            if let Some(encoded) = lookup_field(&index.field, &document)
                .ok()
                .and_then(|val| encode_key(&val, docid)) {
                btree_delete(index.btree_root,
                    &encoded,
                    page_cache,
                    page_allocator);
            }
        }
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

#[derive(Debug, Clone)]
struct FieldPath(Vec<PathElement>);

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        for elem in &self.0 {
            elem.fmt(f)?;
        }

        Ok(())
    }
}

impl FieldPath {
    fn new(path_str: &str) -> FieldPath {
        let index_re = Regex::new(r"^([a-zA-Z0-9_\-]+)\[([0-9]+)\]$").unwrap();
        let mut elements: Vec<PathElement> = Vec::new();
        for elem in path_str.split('.') {
            if let Some(cap) = index_re.captures(elem) {
                elements.push(PathElement::FieldName(cap[1].to_string()));
                elements.push(PathElement::ArrayIndex(cap[2].parse().unwrap()));
            } else {
                elements.push(PathElement::FieldName(elem.to_string()));
            }
        }

        FieldPath(elements)
    }
}

fn lookup_field(path: &FieldPath, record: &Value)
    -> Result<Value, String> {
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
            let mut node = page_cache.lock_page_mut(document_btree_root);
            init_btree_node(&mut node);
        }

        let collection = Collection {
            next_docid: 1,
            document_btree_root,
            indices: Vec::new()
        };

        (page_cache, allocator, collection)
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
        assert_eq!(encode_key(&json!({"foo": "bar"}), DocID(0)), None);
        assert_eq!(encode_key(&json!([1,2,3,4,5]), DocID(0)), None);
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
        for (key, value) in collection.iterate(&page_cache) {
            assert_eq!(docids[i], key);
            assert_eq!(create_document(i), value);
            i += 1;
        }
    }

    #[test]
    fn test_index_int() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();

        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 113}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones", "age": 9}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"name": "Michael James", "age": 32}"#).unwrap();
        let docid3 = collection.insert_document(&doc3, &mut page_cache, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"name": "Adam Mitchell", "age": 27}"#).unwrap();
        let docid4 = collection.insert_document(&doc4, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(9).unwrap()), docid2).unwrap());
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid2.0);

        let Some((key2, val2)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, encode_key(&Value::Number(Number::from_i128(27).unwrap()), docid4).unwrap());
        assert_eq!(u64::from_le_bytes(val2.try_into().unwrap()), docid4.0);

        let Some((key3, val3)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key3, encode_key(&Value::Number(Number::from_i128(32).unwrap()), docid3).unwrap());
        assert_eq!(u64::from_le_bytes(val3.try_into().unwrap()), docid3.0);

        let Some((key4, val4)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key4, encode_key(&Value::Number(Number::from_i128(113).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_le_bytes(val4.try_into().unwrap()), docid1.0);

        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_string() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("item"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"item": "Eggs", "cost": 6.79}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"item": "Whole Wheat Bread", "cost": 5.30}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"item": "Peanut Butter", "cost": 8.05}"#).unwrap();
        let docid3 = collection.insert_document(&doc3, &mut page_cache, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"item": "Dom Perignon", "cost": 250.99}"#).unwrap();
        let docid4 = collection.insert_document(&doc4, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((_key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid4.0);
        let Some((_key2, val2)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val2.try_into().unwrap()), docid1.0);
        let Some((_key3, val3)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val3.try_into().unwrap()), docid3.0);
        let Some((_key4, val4)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val4.try_into().unwrap()), docid2.0);

        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_float() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("profit"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"profit": 8.79}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"profit": -20.7}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"profit": -44.3}"#).unwrap();
        let docid3 = collection.insert_document(&doc3, &mut page_cache, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"profit": 5.33}"#).unwrap();
        let docid4 = collection.insert_document(&doc4, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((_key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid3.0);
        let Some((_key2, val2)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val2.try_into().unwrap()), docid2.0);
        let Some((_key3, val3)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val3.try_into().unwrap()), docid4.0);
        let Some((_key4, val4)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(u64::from_le_bytes(val4.try_into().unwrap()), docid1.0);

        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_bool() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("instock"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"item": "Eggs", "instock": true}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"item": "Whole Wheat Bread", "instock": false}"#).unwrap();
        let docid2 = collection.insert_document(&doc2, &mut page_cache, &mut allocator);
        let doc3 = serde_json::from_str(r#"{"item": "Peanut Butter", "instock": true}"#).unwrap();
        let docid3 = collection.insert_document(&doc3, &mut page_cache, &mut allocator);
        let doc4 = serde_json::from_str(r#"{"item": "Dom Perignon", "instock": false}"#).unwrap();
        let docid4 = collection.insert_document(&doc4, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key2, _val2)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key2, encode_key(&Value::Bool(false), docid2).unwrap());

        let Some((key4, _val4)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key4, encode_key(&Value::Bool(false), docid4).unwrap());

        let Some((key1, _val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Bool(true), docid1).unwrap());

        let Some((key3, _val3)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key3, encode_key(&Value::Bool(true), docid3).unwrap());

        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_missing_key() {
        // If we set up an index and that field is not present in a document,
        // we'll silently skip adding it to the index.
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": "Edward Jones"}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_array_key() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();
        collection.create_index(&FieldPath::new("age"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": {"foo": 1}}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_index_ignore_object_key() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("age"), &mut page_cache, &mut allocator);

        let doc1 = serde_json::from_str(r#"{"name": "James Smith", "age": 39}"#).unwrap();
        let docid1 = collection.insert_document(&doc1, &mut page_cache, &mut allocator);
        let doc2 = serde_json::from_str(r#"{"name": [1,2,3]}"#).unwrap();
        collection.insert_document(&doc2, &mut page_cache, &mut allocator);

        let mut iter = btree_iterate(collection.indices[0].btree_root, false, &mut page_cache);
        let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
        assert_eq!(key1, encode_key(&Value::Number(Number::from_i128(39).unwrap()), docid1).unwrap());
        assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid1.0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_path_display() {
        let path = FieldPath::new("phones[1].number");
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

        let path = FieldPath::new("phones[1].number");
        let fieldval = lookup_field(&path, &doc).unwrap();
        assert_eq!(fieldval.as_str().unwrap(), "+15551212".to_string());
    }

    #[test]
    fn test_lookup_field2() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age");
        let fieldval = lookup_field(&path, &doc).unwrap();
        assert_eq!(fieldval.as_number().unwrap().as_u64().unwrap(), 45u64);
    }

    #[test]
    fn test_bad_lookup_not_array() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age[2]");
        assert_eq!(lookup_field(&path, &doc),
            Err("Indexed non-array .age".to_string()));
    }

    #[test]
    fn test_bad_lookup_array_index_oob() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("phones[2]");
        assert_eq!(lookup_field(&path, &doc),
            Err("Array index 2 out of bounds for .phones".to_string()));
    }

    #[test]
    fn test_bad_lookup_unknown_field() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("ssn");
        assert_eq!(lookup_field(&path, &doc),
            Err("Unknown field ssn".to_string()));
    }

    #[test]
    fn test_bad_lookup_not_object() {
        let doc: Value = serde_json::from_str(JSON_EXAMPLE).unwrap();
        let path = FieldPath::new("age.month");
        assert_eq!(lookup_field(&path, &doc),
            Err("Attempt to access field month on non-object".to_string()));
    }

    #[test]
    #[ignore = "Need to implement checking"]
    fn field_path_invalid_characters() {
        let _path = FieldPath::new("age.$month");
    }

    #[test]
    #[ignore = "Need to implement checking"]
    fn field_path_invalid_index() {
        let _path = FieldPath::new("age[%%]");
    }

    #[test]
    fn test_delete() {
        let (mut page_cache, mut allocator, mut collection) = create_collection();
        let _transaction = page_cache.begin_transaction();

        collection.create_index(&FieldPath::new("foo"), &mut page_cache, &mut allocator);
        collection.create_index(&FieldPath::new("bar"), &mut page_cache, &mut allocator);

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
            assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid1.0);

            let Some((key3, val3)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key3, encode_key(&Value::String("CCCCC".to_string()), docid3).unwrap());
            assert_eq!(u64::from_le_bytes(val3.try_into().unwrap()), docid3.0);

            let Some((key4, val4)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key4, encode_key(&Value::String("DDDDDD".to_string()), docid4).unwrap());
            assert_eq!(u64::from_le_bytes(val4.try_into().unwrap()), docid4.0);

            assert_eq!(iter.next(), None);
        }

        // Second index
        {
            let mut iter = btree_iterate(collection.indices[1].btree_root, false, &mut page_cache); // bar
            let Some((key1, val1)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key1, encode_key(&Value::Number(Number::from_f64(1.2).unwrap()), docid1).unwrap());
            assert_eq!(u64::from_le_bytes(val1.try_into().unwrap()), docid1.0);

            let Some((key3, val3)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key3, encode_key(&Value::Number(Number::from_f64(3.4).unwrap()), docid3).unwrap());
            assert_eq!(u64::from_le_bytes(val3.try_into().unwrap()), docid3.0);

            let Some((key4, val4)) = iter.next() else { panic!("iterator did not return value") };
            assert_eq!(key4, encode_key(&Value::Number(Number::from_f64(4.5).unwrap()), docid4).unwrap());
            assert_eq!(u64::from_le_bytes(val4.try_into().unwrap()), docid4.0);

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
}
