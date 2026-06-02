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

// This is a B+ tree implementation. Values are only stored in the leaf nodes.

use crate::util::*;
use crate::page_cache::{PageCache, FilePageId, PageData, PAGE_SIZE};
use crate::page_allocator::{PageAllocator};
use crate::vararray::*;

const HEADER_NEXT_SIB_OFFS: usize = 8;
const HEADER_PREV_SIB_OFFS: usize = 16;
const HEADER_RIGHT_CHILD_OFFS: usize = 24;

pub const MAX_RECORD_SIZE: usize = (PAGE_SIZE - 32) / 4 - 16; // I added a little padding for safey

// Each entry is:
// key_length: u16
// key: variable length
// value: variable length
// (value length is inferred based on record length)

const FLAG_LEAF: u8 = 1;

const INVALID_FPID: FilePageId = FilePageId(0);

pub struct BTreeCursor {
    current_node_fpid: FilePageId,
    current_index: usize,
    reverse: bool,
    page_cache: PageCache
}

impl Iterator for BTreeCursor {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        // Check if we need to go to the next page or skip empty pages.
        let page = loop {
            if self.current_node_fpid == INVALID_FPID {
                return None
            }

            let page = self.page_cache.lock_page(self.current_node_fpid);

            if self.reverse {
                if get_num_vararray_entries(&page) > 0 && self.current_index != usize::MAX {
                    break page;
                }

                self.current_node_fpid = get_prev_sib(&page);
                if self.current_node_fpid != INVALID_FPID {
                    let page = self.page_cache.lock_page(self.current_node_fpid);
                    self.current_index = get_num_vararray_entries(&page) - 1;
                }
            } else if self.current_index >= get_num_vararray_entries(&page) {
                self.current_node_fpid = get_next_sib(&page);
                self.current_index = 0;
            } else {
                break page;
            }
        };

        let entry = (get_entry_key(&page, self.current_index).to_vec(),
            get_entry_value(&page, self.current_index).to_vec());
        if self.reverse {
            if self.current_index == 0 {
                self.current_index = usize::MAX; // Indicate we need to fetch the next page
            } else {
                self.current_index -= 1;
            }
        } else {
            self.current_index += 1;
        }

        Some(entry)
    }
}

pub fn btree_create(page_cache: &PageCache,
    page_allocator: &mut PageAllocator) -> FilePageId {
    let btree_root = page_allocator.alloc();
    let mut page = page_cache.lock_page_mut(btree_root);
    init_btree_node(&mut page);

    btree_root
}

pub fn btree_iterate(root_node_fpid: FilePageId, reverse: bool, page_cache: &PageCache) -> BTreeCursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page(current_node_fpid);
        if is_leaf(&page) {
            return BTreeCursor {
                current_node_fpid,
                current_index: if reverse { get_num_vararray_entries(&page) - 1 } else { 0 },
                reverse,
                page_cache: page_cache.clone()
            }
        }

        if reverse {
            current_node_fpid = get_right_child(&page);
        } else {
            current_node_fpid = FilePageId(u64::from_le_bytes(get_entry_value(&page, 0)
                .try_into().expect("value was not 8 bytes")));
        }
    }
}

pub fn btree_find(root_node_fpid: FilePageId, key: &[u8], reverse: bool, page_cache: &PageCache) -> BTreeCursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page(current_node_fpid);
        let index = find_key(&page, key);
        if is_leaf(&page) {
            if (reverse && index == 0) || (!reverse && index == get_num_vararray_entries(&page)) {
                // Nothing to fetch, return dummy cursor
                return BTreeCursor {
                    current_node_fpid: INVALID_FPID,
                    current_index: 0,
                    reverse,
                    page_cache: page_cache.clone()
                }
            }

            return BTreeCursor {
                current_node_fpid,
                current_index: if reverse { index - 1 } else { index },
                reverse,
                page_cache: page_cache.clone()
            }
        }

        if index == get_num_vararray_entries(&page) {
            current_node_fpid = get_right_child(&page);
        } else {
            current_node_fpid = FilePageId(u64::from_le_bytes(get_entry_value(&page, index)
                .try_into().expect("value was not 8 bytes")));
        }
    }
}

fn find_with_path(root_node_fpid: FilePageId,
    key: &[u8],
    page_cache: &PageCache) -> (Vec<(FilePageId, usize)>, bool) {
    let mut current_node_fpid = root_node_fpid;
    let mut path: Vec<(FilePageId, usize)> = Vec::new();

    let found = loop {
        let page = page_cache.lock_page(current_node_fpid);
        let index = find_key(&page, key);
        path.push((current_node_fpid, index));
        if is_leaf(&page) {
            break index < get_num_vararray_entries(&page)
                && get_entry_key(&page, index) == key;
        }

        if index == get_num_vararray_entries(&page) {
            current_node_fpid = get_right_child(&page);
        } else {
            current_node_fpid = FilePageId(u64::from_le_bytes(get_entry_value(&page, index).try_into()
                .expect("value was not 8 bytes")));
        }

        assert!(current_node_fpid != INVALID_FPID,
            "Interior node has non-leaf children");
    };

    (path, found)
}

pub fn btree_insert(root_node_fpid: FilePageId,
    key: &[u8],
    value: &[u8],
    page_cache: &PageCache,
    page_allocator: &mut PageAllocator)
{
    assert!(key.len() + value.len() < MAX_RECORD_SIZE);

    let (path, found) = find_with_path(root_node_fpid, key, page_cache);
    assert!(!found);

    // We're now at a leaf. Insert and walk back up the tree splitting nodes
    // as needed.
    let mut insert_value = value.to_vec();
    let mut insert_key = key.to_vec();
    for (node_fpid, _) in path.iter().rev() {
        let mut page = page_cache.lock_page_mut(*node_fpid);
        if get_vararray_free_space(&page) >= get_entry_size(&insert_key, &insert_value) {
            insert_entry(&mut page, insert_key.as_slice(), insert_value.as_slice());
            break;
        }

        // Need to split...
        if *node_fpid == root_node_fpid {
            // Root node splits are special
            let new_page_fpid1 = page_allocator.alloc();
            let new_page_fpid2 = page_allocator.alloc();

            let mut new_page1 = page_cache.lock_page_mut(new_page_fpid1);
            let mut new_page2 = page_cache.lock_page_mut(new_page_fpid2);
            let split_key = split_node(&page, &mut new_page1, &mut new_page2);

            if is_leaf(&page) {
                set_next_sib(&mut new_page1, new_page_fpid2);
                set_prev_sib(&mut new_page2, new_page_fpid1);
            } else {
                set_not_leaf(&mut new_page1);
                set_not_leaf(&mut new_page2);
            }

            // Now do the actual insertion
            if insert_key >= split_key {
                insert_entry(&mut new_page2, insert_key.as_slice(), insert_value.as_slice());
            } else {
                insert_entry(&mut new_page1, insert_key.as_slice(), insert_value.as_slice());
            }

            // The root will have a single entry. It's no longer a leaf.
            init_btree_node(&mut page);
            set_not_leaf(&mut page);
            append_entry(&mut page, &split_key, &new_page_fpid1.0.to_le_bytes());
            set_right_child(&mut page, new_page_fpid2);
            break;
        } else {
            // Split leaf or interior page.
            let new_page_fpid = page_allocator.alloc();
            let mut temp: PageData = [0; PAGE_SIZE];
            let mut new_page = page_cache.lock_page_mut(new_page_fpid);
            let new_parent_key = split_node(&page, &mut new_page, &mut temp);

            // We will allocate a new page to be *before* this page. Temp is a holding
            // area for what will be copied back to this page.

            if is_leaf(&page) {
                set_prev_sib(&mut temp, new_page_fpid);
                set_next_sib(&mut temp, get_next_sib(&page));
                set_prev_sib(&mut new_page, get_prev_sib(&page));
                set_next_sib(&mut new_page, *node_fpid);

                // Need to fix forward link
                let mut prev_sib_page = page_cache.lock_page_mut(get_prev_sib(&page));
                set_next_sib(&mut prev_sib_page, new_page_fpid);
            } else {
                set_not_leaf(&mut new_page);
                set_not_leaf(&mut temp);
            }

            page.copy_from_slice(&temp);

            // Now do the actual insertion
            if insert_key >= new_parent_key {
                insert_entry(&mut page, insert_key.as_slice(), insert_value.as_slice());
            } else {
                insert_entry(&mut new_page, insert_key.as_slice(), insert_value.as_slice());
            }

            insert_key = new_parent_key;
            insert_value = new_page_fpid.0.to_le_bytes().to_vec();
        }
    }
}

pub fn btree_delete(root_node_fpid: FilePageId,
    key: &[u8],
    page_cache: &PageCache,
    _allocator: &mut PageAllocator)
{
    let (path, found) = find_with_path(root_node_fpid, key, page_cache);
    if !found {
        println!("btree_delete: warning, key not found");
        return;
    }

    let (leaf_fpid, index) = path.iter().last().unwrap();
    let mut page = page_cache.lock_page_mut(*leaf_fpid);
    delete_vararray_entry(&mut page, *index);

    // TODO: at this point we could walk back up the path freeing empty pages.
}

fn print_btree(root_node_fpid: FilePageId, page_cache: &PageCache) {
    let mut fifo: Vec<FilePageId> = Vec::new();
    fifo.push(root_node_fpid);
    while !fifo.is_empty() {
        let fpid = fifo.remove(0);
        let page = page_cache.lock_page(fpid);
        println!("Node fpid {} is_leaf {} prev_sib {} next_sib {} right_child {}",
            fpid.0, is_leaf(&page), get_prev_sib(&page).0, get_next_sib(&page).0, get_right_child(&page).0);

        if is_leaf(&page) {
            for i in 0..get_num_vararray_entries(&page) {
                println!("{}. {} value {}", i,
                    to_hex(get_entry_key(&page, i), 16), to_hex(get_entry_value(&page, i), 16));
            }
        } else {
            for i in 0..get_num_vararray_entries(&page) {
                let child_fpid = u64::from_le_bytes(get_entry_value(&page, i).try_into()
                    .expect("value was not 8 bytes"));
                println!("{}. {} child page {}", i,
                    to_hex(get_entry_key(&page, i), 16), child_fpid);
                fifo.push(FilePageId(child_fpid));
            }

            if get_right_child(&page) != INVALID_FPID {
                fifo.push(get_right_child(&page));
            }
        }
    }
}

fn to_hex(bytes: &[u8], mut max_len: usize) -> String {
    let mut result: String = "".to_string();
    for x in bytes {
        if max_len == 0 {
            result += "...";
            break;
        }

        max_len -= 1;
        result += format!("{:02x}", x).as_str();
    }

    result
}

// Create an empty node
pub fn init_btree_node(page: &mut PageData) {
    init_vararray(page);
    page[0] = FLAG_LEAF;
}

fn is_leaf(page: &PageData) -> bool {
    (page[0] & FLAG_LEAF) != 0
}

fn set_not_leaf(page: &mut PageData) {
    page[0] &= !FLAG_LEAF;
}

fn get_next_sib(page: &PageData) -> FilePageId {
    FilePageId(get_u64(page, HEADER_NEXT_SIB_OFFS))
}

fn set_next_sib(page: &mut PageData, fpid: FilePageId) {
    set_u64(&mut page[..], HEADER_NEXT_SIB_OFFS, fpid.0);
}

fn get_prev_sib(page: &PageData) -> FilePageId {
    FilePageId(get_u64(page, HEADER_PREV_SIB_OFFS))
}

fn set_prev_sib(page: &mut PageData, fpid: FilePageId) {
    set_u64(&mut page[..], HEADER_PREV_SIB_OFFS, fpid.0);
}

fn get_right_child(page: &PageData) -> FilePageId {
    FilePageId(get_u64(page, HEADER_RIGHT_CHILD_OFFS))
}

fn set_right_child(page: &mut PageData, fpid: FilePageId) {
    set_u64(&mut page[..], HEADER_RIGHT_CHILD_OFFS, fpid.0);
}

fn get_entry_size(key: &[u8], value: &[u8]) -> usize {
    // 2 bytes for the index table entry (in vararray)
    // 2 bytes for the entry length (in vararray)
    // 2 bytes for the key length
    key.len() + value.len() + 6
}

fn get_entry_key(page: &PageData, rec_num: usize) -> &[u8] {
    let rec = get_vararray_entry(page, rec_num);
    let key_len = get_u16(rec, 0) as usize;
    &rec[2..2 + key_len]
}

fn get_entry_value(page: &PageData, rec_num: usize) -> &[u8] {
    let rec = get_vararray_entry(page, rec_num);
    let key_len = get_u16(rec, 0) as usize;
    &rec[2 + key_len..]
}

//
// Return an index into the array:
// - If there is an exact match, return the index of the matching entry.
// - If there is not an exact match, return the index of the smallest
//   key that is larger than the search key (i.e. where this would be
//   inserted).
// - If the search key is lower than the lowest key, return 0
// - If it is higher than the highest key, return the number of entries
//   in the table.
//
fn find_key(page: &PageData, key: &[u8]) -> usize {
    let mut low = 0;
    let mut high = get_num_vararray_entries(page);
    while low < high {
        let mid = (low + high) / 2;
        let mid_key = get_entry_key(page, mid);
        if key <= mid_key {
            high = mid
        } else {
            low = mid + 1
        }
    }

    low
}

// Insert a entry into a single page.
fn insert_entry(page: &mut PageData, key: &[u8], value: &[u8]) {
    let index = find_key(page, key);
    assert!(index == get_num_vararray_entries(page) || get_entry_key(page, index) != key,
        "Duplicate key inserted");

    let mut entry = Vec::with_capacity(key.len() + value.len() + 2);
    entry.extend_from_slice(&(key.len() as u16).to_le_bytes());
    entry.extend_from_slice(key);
    entry.extend_from_slice(value);
    insert_vararray_entry(page, index, &entry);
}

// Helper function to add entry to next available slot. This assumes the entry is
// added in order. It also assumes there is adequate space in the page.
// Returns entry size
fn append_entry(page: &mut PageData, key: &[u8], value: &[u8]) -> usize {
    let mut entry: Vec<u8> = Vec::with_capacity(key.len() + value.len() + 2);
    entry.extend_from_slice(&(key.len() as u16).to_le_bytes());
    entry.extend_from_slice(key);
    entry.extend_from_slice(value);
    insert_vararray_entry(page, get_num_vararray_entries(page), &entry);

    get_entry_size(key, value)
}

// Split a single page into two new ones.
// Returns the separator key.
// NOTE: you must set the right_sibling in the returned out2 to the fpid of out1
// (we don't know it here)
fn split_node(orig: &PageData, out1: &mut PageData, out2: &mut PageData) -> Vec<u8> {
    init_btree_node(out1);
    init_btree_node(out2);

    // Copy out entries from the orig into out1 until we have just over half.
    // then continue copying into out2.
    let orig_entries = get_num_vararray_entries(orig);

    let mut orig_index = 0;
    let mut bytes_copied = 0;

    // Copy into out1. Ensure we leave at least one entry to copy into out2.
    while bytes_copied < orig.len() / 2 && orig_index < orig_entries - 1 {
        bytes_copied += append_entry(out1, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    let separator = if is_leaf(orig) {
        // Remember the separator key, which is the highest key in the left page,
        // but don't remove it.
        get_entry_key(orig, orig_index - 1).to_vec()
    } else {
        // Remove the separator key, which will go into the parent. Save its
        // node pointer into the right child of the left node.
        let separator = get_entry_key(orig, orig_index).to_vec();


        set_right_child(out1, FilePageId(u64::from_le_bytes(get_entry_value(orig, orig_index)
            .try_into().expect("value was not 8 bytes"))));
        orig_index += 1;

        separator
    };

    // Copy into out2
    while orig_index < orig_entries {
        append_entry(out2, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    set_right_child(out2, get_right_child(orig));

    separator
}


#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};
    use crate::page_allocator::*;
    use std::rc::Rc;
    use std::cell::RefCell;
    use crate::page_cache::*;
    use crate::mocks::{MockPersistentStore};
    use crate::superblock::*;
    use rand::rngs::{SmallRng};
    use rand::{SeedableRng, RngExt};
    use std::cmp::{Ord};
    use super::*;

    fn sanity_check_node(page: &PageData) {
        // Ensure the keys are in order
        let mut last_key: &[u8] = &[0];
        for i in 0..get_num_vararray_entries(page) {
            let this_key = get_entry_key(page, i);
            assert_le!(last_key, this_key, "keys are out of order");
            last_key = this_key;
        }
    }

    #[test]
    fn test_get_key_val() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        append_entry(&mut page, "foobar".as_bytes(), "abcdefghijklmnopqrstuwxyz".as_bytes());
        append_entry(&mut page, "zzzz".as_bytes(), "3.1415926535897932384626433832".as_bytes());
        sanity_check_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 2);

        assert_eq!(get_entry_key(&page, 0), "foobar".as_bytes());
        assert_eq!(get_entry_value(&page, 0), "abcdefghijklmnopqrstuwxyz".as_bytes());

        assert_eq!(get_entry_key(&page, 1), "zzzz".as_bytes());
        assert_eq!(get_entry_value(&page, 1), "3.1415926535897932384626433832".as_bytes());
    }

    #[test]
    fn test_find_key() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        append_entry(&mut page, "aaaa".as_bytes(), &[0u8]);
        append_entry(&mut page, "bbbb".as_bytes(), &[0u8]);
        append_entry(&mut page, "cccc".as_bytes(), &[0u8]);
        append_entry(&mut page, "dddd".as_bytes(), &[0u8]);
        sanity_check_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 4);

        assert_eq!(find_key(&page, "aaa".as_bytes()), 0); // Search key is before first key
        assert_eq!(find_key(&page, "aaaa".as_bytes()), 0); // Equal to first key
        assert_eq!(find_key(&page, "aaab".as_bytes()), 1); // Between first and second key
        assert_eq!(find_key(&page, "bbbb".as_bytes()), 1); // Equal to second key
        assert_eq!(find_key(&page, "bbbc".as_bytes()), 2); // Between second and third key
        assert_eq!(find_key(&page, "eeee".as_bytes()), 4); // Larger than largest key
    }

    #[test]
    fn test_find_key_empty() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        assert_eq!(find_key(&page, "foo".as_bytes()), 0);
    }

    // Validates get_vararray_free_space and get_entry_size return
    // consistent values.
    #[test]
    fn test_entry_size() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);
        let init_free_space = get_vararray_free_space(&page);
        let key1 = "foo".as_bytes();
        let val1 = "00000000000000000000000000000".as_bytes();
        insert_entry(&mut page, key1, &val1);
        assert_lt!(get_vararray_free_space(&page), init_free_space);
        assert_eq!(get_vararray_free_space(&page), init_free_space -
            get_entry_size(key1, &val1));

        let key2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let val2 = "..ooOOO".as_bytes();
        let init_free_space = get_vararray_free_space(&page);
        insert_entry(&mut page, key2, &val2);
        assert_lt!(get_vararray_free_space(&page), init_free_space);
        assert_eq!(get_vararray_free_space(&page), init_free_space -
            get_entry_size(key2, &val2));
    }

    #[test]
    fn test_insert_entry() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        // Note these are out of order
        insert_entry(&mut page, "aardvark".as_bytes(), &[0u8]);
        insert_entry(&mut page, "zebra".as_bytes(), &[0u8]);
        insert_entry(&mut page, "apple".as_bytes(), &[0u8]);
        insert_entry(&mut page, "banana".as_bytes(), &[0u8]);
        sanity_check_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 4);

        assert_eq!(find_key(&page, "aardvark".as_bytes()), 0);
        assert_eq!(find_key(&page, "apple".as_bytes()), 1);
        assert_eq!(find_key(&page, "banana".as_bytes()), 2);
        assert_eq!(find_key(&page, "zebra".as_bytes()), 3);
    }

    #[test]
    #[should_panic = "Insufficient space to insert"]
    fn test_insert_entry_full() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);
        for i in 0..PAGE_SIZE {
            insert_entry(&mut page, &(i as i64).to_le_bytes(), &[0u8]);
        }
    }

    #[test]
    #[should_panic = "Duplicate key inserted"]
    fn test_insert_duplicate() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        insert_entry(&mut page, "aardvark".as_bytes(), &[0u8]);
        insert_entry(&mut page, "aardvark".as_bytes(), &[0u8]);
    }

    #[test]
    fn test_split_interior_node() {
        let mut node1: PageData = [0; PAGE_SIZE];
        let mut node2: PageData = [0; PAGE_SIZE];
        let mut node3: PageData = [0; PAGE_SIZE];

        init_btree_node(&mut node1);
        node1[0] = 0; // Clear leaf flag
        // TODO this number is fudged, should derive from page size.
        const PAGE1_ENTRIES: usize = 50;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            insert_entry(&mut node1, &key, &(i as u64).to_le_bytes());
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = get_num_vararray_entries(&node2);
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        assert_eq!(get_right_child(&node2), FilePageId(u64::from_le_bytes(
            get_entry_value(&node1, orig_sep_index)
            .try_into().expect("value was not 8 bytes"))));
        assert_eq!(get_right_child(&node3), get_right_child(&node1));

        // Ensure all entries are present and in order
        let node2_recs = get_num_vararray_entries(&node2);
        assert_eq!(get_num_vararray_entries(&node1) - 1,
            node2_recs + get_num_vararray_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..get_num_vararray_entries(&node1) {
            if i == node2_recs {
                continue; // ignore splitter
            }

            let orig_entry = get_entry_key(&node1, i);
            if i > node2_recs {
                assert_eq!(orig_entry, get_entry_key(&node3, i - node2_recs - 1));
            } else {
                assert_eq!(orig_entry, get_entry_key(&node2, i));
            }
        }
    }

    #[test]
    fn test_split_leaf_node() {
        let mut node1: PageData = [0; PAGE_SIZE];
        let mut node2: PageData = [0; PAGE_SIZE];
        let mut node3: PageData = [0; PAGE_SIZE];

        init_btree_node(&mut node1);

        // TODO this number is fudged, should derive from page size.
        const PAGE1_ENTRIES: usize = 50;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            insert_entry(&mut node1, &key, &(i as u64).to_le_bytes());
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = get_num_vararray_entries(&node2) - 1;
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        // Ensure all entries are present and in order
        let node2_recs = get_num_vararray_entries(&node2);
        assert_eq!(get_num_vararray_entries(&node1),
            node2_recs + get_num_vararray_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..get_num_vararray_entries(&node1) {
            let orig_entry = get_entry_key(&node1, i);
            if i >= node2_recs {
                assert_eq!(orig_entry, get_entry_key(&node3, i - node2_recs));
            } else {
                assert_eq!(orig_entry, get_entry_key(&node2, i));
            }
        }
    }

    // This only has two entries. Ensure it doesn't put both entries in the
    // first page, leaving none in the second (regression test).
    #[test]
    fn test_split_large_leaf() {
        let mut node1: PageData = [0; PAGE_SIZE];
        let mut node2: PageData = [0; PAGE_SIZE];
        let mut node3: PageData = [0; PAGE_SIZE];

        init_btree_node(&mut node1);
        insert_entry(&mut node1, &[1u8; 2000], &[1u8, 8]);
        insert_entry(&mut node1, &[2u8; 2000], &[2u8, 8]);

        split_node(&node1, &mut node2, &mut node3);

        assert_eq!(get_num_vararray_entries(&node2), 1);
        assert_eq!(get_num_vararray_entries(&node3), 1);
        sanity_check_node(&node2);
        sanity_check_node(&node3);
    }

    #[test]
    fn test_leaf_flag() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);
        assert!(is_leaf(&page));
        page[0] = 0;
        assert!(!is_leaf(&page));
    }

    // Helper function to create a shuffled list of indices. Each index
    // is only present once.
    fn prand_order(n: usize) -> Vec<usize> {
        let seed = 0xc0fc47a65d406179;
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut result: Vec<usize> = (0..n).collect();
        for i in (1..n).rev() {
            let j = rng.random_range(0..n);
            result.swap(i, j);
        }

        result
    }

    fn gen_key_for_index(index: usize) -> Vec<u8> {
        let mut key = index.to_be_bytes().to_vec();
        key.extend_from_slice(&[0u8].repeat(32));
        key
    }

    fn create_test_btree() -> (PageCache, PageAllocator, FilePageId) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(50, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();

        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);
        let root_page = btree_create(&page_cache, &mut allocator);

        (page_cache, allocator, root_page)
    }

    const NUM_TEST_ENTRIES: usize = 256;

    fn populate_test_btree() -> (PageCache, PageAllocator, FilePageId) {
        let (page_cache, mut allocator, root_page) = create_test_btree();
        let _transaction = page_cache.begin_transaction();
        for i in prand_order(NUM_TEST_ENTRIES) {
            btree_insert(root_page, &gen_key_for_index(i), &(i as u64).to_le_bytes(),
                &page_cache, &mut allocator);
        }

        (page_cache, allocator, root_page)
    }

    #[test]
    fn test_valid_btree_create() {
        let (page_cache, _alloc, root_page) = populate_test_btree();
        let mut i = 0;
        for (key, val) in btree_iterate(root_page, false, &page_cache) {
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
            i += 1;
        }
    }

    #[test]
    fn test_btree_backward_scan() {
        let (page_cache, _alloc, root_page) = populate_test_btree();

        let mut cursor = btree_iterate(root_page, true, &page_cache);
        for i in (0..NUM_TEST_ENTRIES).rev() {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }

        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_find() {
        let (page_cache, _alloc, root_page) = populate_test_btree();

        const START_KEY_IDX: usize = 55;
        let mut cursor = btree_find(root_page, &gen_key_for_index(START_KEY_IDX), false, &page_cache);
        for i in START_KEY_IDX..START_KEY_IDX + 10 {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), &gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }
    }

    // Get the first page in the tree, which requires traversing the left child page.
    #[test]
    fn test_btree_find_begin() {
        let (page_cache, _alloc, root_page) = populate_test_btree();

        let mut cursor = btree_find(root_page, &[0u8], false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), &gen_key_for_index(0));
        assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), 0u64);
    }

    // Key is before first key and going in reverse. Nothing to fetch.
    #[test]
    fn test_btree_reverse_find_begin() {
        let (page_cache, _alloc, root_page) = populate_test_btree();

        let mut cursor = btree_find(root_page, &[0u8], true, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Key is after last key and going forward. Nothing to fetch.
    #[test]
    fn test_btree_find_past_end() {
        let (page_cache, _alloc, root_page) = populate_test_btree();

        let mut cursor = btree_find(root_page, &[0xff; 255], false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_delete() {
        let (page_cache, mut allocator, root_page) = populate_test_btree();

        const INDEX_TO_DELETE: usize = 37;
        {
            let _transaction = page_cache.begin_transaction();
            btree_delete(root_page, gen_key_for_index(INDEX_TO_DELETE).as_slice(),
                &page_cache, &mut allocator);
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_TEST_ENTRIES {
            if i == INDEX_TO_DELETE {
                continue;
            }

            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }

        assert!(cursor.next().is_none());
    }

    #[test]
    fn test_btree_delete_not_present() {
        let (page_cache, mut allocator, root_page) = populate_test_btree();

        {
            let _transaction = page_cache.begin_transaction();

            // Key is bogus
            btree_delete(root_page, &"yolo".as_bytes(), &page_cache, &mut allocator);
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_TEST_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }
    }

    #[test]
    fn test_btree_delete_all() {
        let (page_cache, mut allocator, root_page) = populate_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            for i in 0..NUM_TEST_ENTRIES {
                btree_delete(root_page, gen_key_for_index(i).as_slice(),
                     &page_cache, &mut allocator);
            }
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn iterate_empty() {
        let (page_cache, mut _allocator, root_page) = create_test_btree();
        let mut cursor = btree_iterate(root_page, false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Just ensures it doesn't crash...
    #[test]
    fn test_print_btree() {
        let (page_cache, _alloc, root_page) = populate_test_btree();
        print_btree(root_page, &page_cache);
    }

    #[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
    struct KeyValue(Vec<u8>, Vec<u8>);

    // The Oracle is a parallel data structure that tracks the expected
    // btree state based on random operations below.
    struct Oracle {
        entries: Vec<KeyValue>
    }

    impl Oracle {
        // TODO: this doesn't guarantee uniqueness.
        fn add(&mut self, key: &[u8], value: &[u8]) {
            let kv = KeyValue(key.to_vec(), value.to_vec());
            let pos = match self.entries.binary_search(&kv) {
                Ok(pos) | Err(pos) => pos
            };

            self.entries.insert(pos, kv);
        }

        fn validate(&self, cursor: BTreeCursor) {
            let mut db_entries: Vec<KeyValue> = cursor.map(|x | KeyValue(x.0, x.1)).collect();
            db_entries.sort();

            assert_eq!(db_entries.len(), self.entries.len());
            assert_eq!(db_entries, self.entries);
        }
    }

    fn random_value(rng: &mut impl RngExt) -> Vec<u8> {
        let len = rng.random_range(8..MAX_RECORD_SIZE / 2);
        (0..len).map(|_| rng.random()).collect()
    }

    #[test]
    fn test_btree_stress() {
        let seed: u64 = 0x12345;
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut oracle = Oracle{ entries: Vec::new() };
        let (page_cache, mut allocator, root_page) = create_test_btree();

        let total_reps = 2000;
        let min_psub: f64 = 0.3;
        for rep in 0..total_reps {
            let p_add: f64 = min_psub + (1.0 - min_psub) * (1.0 - (rep as f64 / total_reps as f64));
            if rng.random::<f64>() > p_add {
                // Delete entry
                if !oracle.entries.is_empty() {
                    let i = rng.random_range(0..oracle.entries.len());
                    let entry = &oracle.entries[i];
                    let _transaction = page_cache.begin_transaction();
                    btree_delete(root_page, &entry.0, &page_cache, &mut allocator);
                    oracle.entries.remove(i);
                }
            } else {
                // Insert entry
                let key = random_value(&mut rng);
                let value = random_value(&mut rng);

                // TODO ensure the key is unique by looking in the oracle.
                oracle.add(&key, &value);
                let _transaction = page_cache.begin_transaction();
                btree_insert(root_page, &key, &value, &page_cache, &mut allocator);
            }

            if oracle.entries.len() > 0 {
                oracle.validate(btree_iterate(root_page, false, &page_cache));
            }
        }

        oracle.validate(btree_iterate(root_page, false, &page_cache));
    }
}
