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
use crate::page_cache::{PageCache, FilePageId, PAGE_SIZE};
use crate::page_allocator::{PageAllocator};
use bytemuck::{Pod, Zeroable};
use crate::record_array;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct NodeHeader {
    flags: u16,
    _unused: [u8; 6],
    next_sib: u64,
    prev_sib: u64,
    right_child: u64, // Node with keys larger than this one
}

const MAX_RECORD_SIZE: usize = (PAGE_SIZE - 32) / 4 - 16; // I added a little padding for safey

// Each entry is:
// key_length: u16
// key: variable length
// value: variable length
// (value length is inferred based on record length)

const FLAG_LEAF: u16 = 1;

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
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            if self.reverse {
                if record_array::get_num_entries(&page) > 0 && self.current_index != usize::MAX {
                    break page;
                }

                self.current_node_fpid = FilePageId(header.prev_sib);
                if self.current_node_fpid != INVALID_FPID {
                    let page = self.page_cache.lock_page(self.current_node_fpid);
                    self.current_index = record_array::get_num_entries(&page) - 1;
                }
            } else if self.current_index >= record_array::get_num_entries(&page) {
                self.current_node_fpid = FilePageId(header.next_sib);
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
                current_index: if reverse { record_array::get_num_entries(&page) - 1 } else { 0 },
                reverse,
                page_cache: page_cache.clone()
            }
        }

        if reverse {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = FilePageId(header.right_child);
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
        let index = find_key(&page, key, if reverse { Bias::Last } else { Bias::First });
        if is_leaf(&page) {
            if (reverse && index == 0) || (!reverse && index == record_array::get_num_entries(&page)) {
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

        if index == record_array::get_num_entries(&page) {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = FilePageId(header.right_child);
        } else {
            current_node_fpid = FilePageId(u64::from_le_bytes(get_entry_value(&page, index)
                .try_into().expect("value was not 8 bytes")));
        }
    }
}

pub fn btree_insert(root_node_fpid: FilePageId,
    key: &[u8],
    value: &[u8],
    page_cache: &PageCache,
    page_allocator: &mut PageAllocator)
{
    let mut current_node_fpid = root_node_fpid;
    let mut path: Vec<FilePageId> = Vec::new();

    assert!(key.len() + value.len() < MAX_RECORD_SIZE);

    loop {
        path.push(current_node_fpid);
        let page = page_cache.lock_page(current_node_fpid);
        if is_leaf(&page) {
            break;
        }

        let index = find_key(&page, key, Bias::Last);
        if index == record_array::get_num_entries(&page) {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = FilePageId(header.right_child);
        } else {
            current_node_fpid = FilePageId(u64::from_le_bytes(get_entry_value(&page, index).try_into()
                .expect("value was not 8 bytes")));
        }

        assert!(current_node_fpid != INVALID_FPID, "Interior node has non-leaf children");
    }

    // We're now at a leaf. Insert and walk back up the tree splitting nodes
    // as needed.
    let mut insert_value = value.to_vec();
    let mut insert_key = key.to_vec();
    for node_fpid in path.iter().rev() {
        let mut page = page_cache.lock_page_mut(*node_fpid);
        if record_array::get_free_space(&page) >= get_entry_size(&insert_key, &insert_value) {
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
                let header1: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page1[0..record_array::HEADER_SIZE]);
                let header2: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page2[0..record_array::HEADER_SIZE]);
                header1.next_sib = new_page_fpid2.0;
                header2.prev_sib = new_page_fpid1.0;
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
            let page_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut page[0..record_array::HEADER_SIZE]);
            page_header.right_child = new_page_fpid2.0;
            break;
        } else {
            // Split leaf or interior page.
            let new_page_fpid = page_allocator.alloc();
            let mut temp: [u8; PAGE_SIZE] = [0; PAGE_SIZE];
            let mut new_page = page_cache.lock_page_mut(new_page_fpid);
            let new_parent_key = split_node(&page, &mut new_page, &mut temp);

            // We will allocate a new page to be *before* this page. Temp is a holding
            // area for what will be copied back to this page.

            if is_leaf(&page) {
                let old_page_header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
                let new_page_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page[0..record_array::HEADER_SIZE]);
                let temp_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut temp[0..record_array::HEADER_SIZE]);

                temp_header.prev_sib = new_page_fpid.0;
                temp_header.next_sib = old_page_header.next_sib;
                new_page_header.prev_sib = old_page_header.prev_sib;
                new_page_header.next_sib = node_fpid.0;

                // Need to fix forward link
                let mut prev_sib_page = page_cache.lock_page_mut(FilePageId(old_page_header.prev_sib));
                let prev_sib_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut prev_sib_page[0..record_array::HEADER_SIZE]);
                prev_sib_header.next_sib = new_page_fpid.0;
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

// If there are not unique keys, value should be specified here. Otherwise this will
// arbitrarily delete one entry. If there are unique keys, it is not required.
fn btree_delete(root_node_fpid: FilePageId,
    key: &[u8],
    value: Option<&[u8]>,
    page_cache: &PageCache)
{
    // Since the btree doesn't enforce unique keys by default, we use a cursor
    // to find the specific entry to delete (for our use cases, we know the
    // key/value tuple will be unique, although btree code does not enforce
    // that).
    let mut cursor = btree_find(root_node_fpid, key, false, page_cache);

    loop {
        // Need to save these because cursor will post-update
        let page_fpid = cursor.current_node_fpid;
        let index = cursor.current_index;
        let next = cursor.next();
        if next.is_none() {
            break;
        }

        let (entry_key, entry_val) = next.unwrap();
        if key == entry_key && (value.is_none() || value.unwrap() == entry_val) {
            let mut page = page_cache.lock_page_mut(page_fpid);

            // TODO: if the record array is now empty, we should delete it, and walk up
            // the parent chain, potentially cascading the delete. Some other places
            // in the code make assumptions there aren't empty nodes.
            record_array::delete_record(&mut page, index);
            break;
        }
    }
}

fn print_btree(root_node_fpid: FilePageId, page_cache: &PageCache) {
    let mut fifo: Vec<FilePageId> = Vec::new();
    fifo.push(root_node_fpid);
    while !fifo.is_empty() {
        let fpid = fifo.remove(0);
        let page = page_cache.lock_page(fpid);
        let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
        println!("Node fpid {} is_leaf {} prev_sib {} next_sib {} right_child {}",
            fpid.0, is_leaf(&page), header.prev_sib, header.next_sib, header.right_child);

        if is_leaf(&page) {
            for i in 0..record_array::get_num_entries(&page) {
                println!("{}. {} value {}", i,
                    to_hex(get_entry_key(&page, i), 16), to_hex(get_entry_value(&page, i), 16));
            }
        } else {
            for i in 0..record_array::get_num_entries(&page) {
                let child_fpid = u64::from_le_bytes(get_entry_value(&page, i).try_into()
                    .expect("value was not 8 bytes"));
                println!("{}. {} child page {}", i,
                    to_hex(get_entry_key(&page, i), 16), child_fpid);
                fifo.push(FilePageId(child_fpid));
            }

            if header.right_child != INVALID_FPID.0 {
                fifo.push(FilePageId(header.right_child));
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
pub fn init_btree_node(node: &mut [u8]) {
    record_array::init_array(node);
    let header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut node[0..record_array::HEADER_SIZE]);
    header.flags |= FLAG_LEAF;
}

fn is_leaf(node: &[u8]) -> bool {
    let header: &NodeHeader = bytemuck::from_bytes(&node[0..record_array::HEADER_SIZE]);
    (header.flags & FLAG_LEAF) != 0
}

fn set_not_leaf(node: &mut [u8]) {
    let header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut node[0..record_array::HEADER_SIZE]);
    header.flags &= !FLAG_LEAF;
}

fn get_entry_size(key: &[u8], value: &[u8]) -> usize {
    // 2 bytes for the index table entry (in record_array)
    // 2 bytes for the entry length (in record_array)
    // 2 bytes for the key length
    key.len() + value.len() + 6
}

fn get_entry_key(node: &[u8], rec_num: usize) -> &[u8] {
    let rec = record_array::get_record(node, rec_num);
    let key_len = get_u16(rec, 0) as usize;
    &rec[2..2 + key_len]
}

fn get_entry_value(node: &[u8], rec_num: usize) -> &[u8] {
    let rec = record_array::get_record(node, rec_num);
    let key_len = get_u16(rec, 0) as usize;
    &rec[2 + key_len..]
}

enum Bias {
    First,
    Last
}

//
// Return an index into the array:
// - If there is a single exact match, return the index of the matching entry.
// - If this matches multiple entries, the behavior is determined by the bias
//   parameter
//   * First: returns the lowest index match
//   * Last: returns the next index after the highest match
// - If there is not an exact match, return the index of the smallest
//   key that is larger than the search key (i.e. where this would be
//   inserted).
// - If the search key is lower than the lowest key, return 0
// - If it is higher than the highest key, return the number of entries
//   in the table.
//
fn find_key(node: &[u8], key: &[u8], bias: Bias) -> usize {
    let mut low = 0;
    let mut high = record_array::get_num_entries(node);
    while low < high {
        let mid = (low + high) / 2;
        let mid_key = get_entry_key(node, mid);
        match bias {
            Bias::First => {
                if key <= mid_key { high = mid } else { low = mid + 1 };
            }
            Bias::Last => {
                if key < mid_key { high = mid } else { low = mid + 1 };
            }
        }
    }

    low
}

// Insert a entry into a single node.
fn insert_entry(node: &mut [u8], key: &[u8], value: &[u8]) {
    let index = find_key(node, key, Bias::Last);
    let mut entry = Vec::with_capacity(key.len() + value.len() + 2);
    entry.push((key.len() & 0xff) as u8);
    entry.push((key.len() >> 8) as u8);
    entry.extend_from_slice(key);
    entry.extend_from_slice(value);
    record_array::insert_record(node, index, &entry);
}

// Helper function to add entry to next available slot. This assumes the entry is
// added in order. It also assumes there is adequate space in the node.
// Returns entry size
fn append_entry(node: &mut [u8], key: &[u8], value: &[u8]) -> usize {
    let mut entry: Vec<u8> = Vec::with_capacity(key.len() + value.len() + 2);
    // Length
    entry.push((key.len() & 0xff) as u8);
    entry.push((key.len() >> 8) as u8);
    entry.extend_from_slice(key);
    entry.extend_from_slice(value);
    record_array::insert_record(node, record_array::get_num_entries(node), &entry);

    get_entry_size(key, value)
}

// Split a single node into two new ones.
// Returns the separator key.
// NOTE: you must set the right_sibling in the returned out2 to the fpid of out1
// (we don't know it here)
fn split_node(orig: &[u8], out1: &mut [u8], out2: &mut [u8]) -> Vec<u8> {
    init_btree_node(out1);
    init_btree_node(out2);

    // Copy out entries from the orig into out1 until we have just over half.
    // then continue copying into out2.
    let orig_entries = record_array::get_num_entries(orig);

    let mut orig_index = 0;
    let mut bytes_copied = 0;

    // Copy into out1. Ensure we leave at least one entry to copy into out2.
    while bytes_copied < orig.len() / 2 && orig_index < orig_entries - 1 {
        bytes_copied += append_entry(out1, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    let separator = if is_leaf(orig) {
        // Remember the separator key, which is the highest key in the left node,
        // but don't remove it.
        get_entry_key(orig, orig_index - 1).to_vec()
    } else {
        // Remove the separator key, which will go into the parent. Save its
        // node pointer into the right child of the left node.
        let separator = get_entry_key(orig, orig_index).to_vec();

        let header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut out1[0..record_array::HEADER_SIZE]);
        header.right_child = u64::from_le_bytes(get_entry_value(orig, orig_index).try_into().expect("value was not 8 bytes"));
        orig_index += 1;

        separator
    };

    // Copy into out2
    while orig_index < orig_entries {
        append_entry(out2, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    let out_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut out2[0..record_array::HEADER_SIZE]);
    let orig_header: &NodeHeader = bytemuck::from_bytes(&orig[0..record_array::HEADER_SIZE]);
    out_header.right_child = orig_header.right_child;

    separator
}


#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};
    use crate::util::*;
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

    fn sanity_check_node(node: &[u8]) {
        // Ensure the keys are in order
        let mut last_key: &[u8] = &[0];
        for i in 0..record_array::get_num_entries(node) {
            let this_key = get_entry_key(node, i);
            assert_le!(last_key, this_key, "keys are out of order");
            last_key = this_key;
        }
    }

    #[test]
    fn test_get_key_val() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        append_entry(&mut node, "foobar".as_bytes(), "abcdefghijklmnopqrstuwxyz".as_bytes());
        append_entry(&mut node, "zzzz".as_bytes(), "3.1415926535897932384626433832".as_bytes());
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 2);

        assert_eq!(get_entry_key(&node, 0), "foobar".as_bytes());
        assert_eq!(get_entry_value(&node, 0), "abcdefghijklmnopqrstuwxyz".as_bytes());

        assert_eq!(get_entry_key(&node, 1), "zzzz".as_bytes());
        assert_eq!(get_entry_value(&node, 1), "3.1415926535897932384626433832".as_bytes());
    }

    #[test]
    fn test_find_key() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        append_entry(&mut node, "aaaa".as_bytes(), &[0u8]);
        append_entry(&mut node, "bbbb".as_bytes(), &[0u8]);
        append_entry(&mut node, "cccc".as_bytes(), &[0u8]);
        append_entry(&mut node, "dddd".as_bytes(), &[0u8]);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 4);

        assert_eq!(find_key(&node, "aaa".as_bytes(), Bias::First), 0); // Search key is before first key
        assert_eq!(find_key(&node, "aaaa".as_bytes(), Bias::First), 0); // Equal to first key
        assert_eq!(find_key(&node, "aaab".as_bytes(), Bias::First), 1); // Between first and second key
        assert_eq!(find_key(&node, "bbbb".as_bytes(), Bias::First), 1); // Equal to second key
        assert_eq!(find_key(&node, "bbbc".as_bytes(), Bias::First), 2); // Between second and third key
        assert_eq!(find_key(&node, "eeee".as_bytes(), Bias::First), 4); // Larger than largest key
    }

    #[test]
    fn test_find_key_empty() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        assert_eq!(find_key(&node, "foo".as_bytes(), Bias::First), 0);
    }

    #[test]
    fn test_find_key_duplicate() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        append_entry(&mut node, "aaaa".as_bytes(), &[0u8]);
        append_entry(&mut node, "bbbb".as_bytes(), &[1u8]);
        append_entry(&mut node, "bbbb".as_bytes(), &[2u8]);
        append_entry(&mut node, "bbbb".as_bytes(), &[3u8]);
        append_entry(&mut node, "bbbb".as_bytes(), &[4u8]);
        append_entry(&mut node, "cccc".as_bytes(), &[5u8]);
        append_entry(&mut node, "dddd".as_bytes(), &[6u8]);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 7);

        assert_eq!(find_key(&node, "bbbb".as_bytes(), Bias::First), 1);
        assert_eq!(find_key(&node, "bbbb".as_bytes(), Bias::Last), 5);
    }


    // Validates record_array::get_free_space and get_entry_size return
    // consistent values.
    #[test]
    fn test_entry_size() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        let init_free_space = record_array::get_free_space(&node);
        let key1 = "foo".as_bytes();
        let val1 = "00000000000000000000000000000".as_bytes();
        insert_entry(&mut node, key1, &val1);
        assert_lt!(record_array::get_free_space(&node), init_free_space);
        assert_eq!(record_array::get_free_space(&node), init_free_space -
            get_entry_size(key1, &val1));

        let key2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let val2 = "..ooOOO".as_bytes();
        let init_free_space = record_array::get_free_space(&node);
        insert_entry(&mut node, key2, &val2);
        assert_lt!(record_array::get_free_space(&node), init_free_space);
        assert_eq!(record_array::get_free_space(&node), init_free_space -
            get_entry_size(key2, &val2));
    }

    #[test]
    fn test_insert_entry() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        // Note these are out of order
        insert_entry(&mut node, "aardvark".as_bytes(), &[0u8]);
        insert_entry(&mut node, "zebra".as_bytes(), &[0u8]);
        insert_entry(&mut node, "apple".as_bytes(), &[0u8]);
        insert_entry(&mut node, "banana".as_bytes(), &[0u8]);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 4);

        assert_eq!(find_key(&node, "aardvark".as_bytes(), Bias::First), 0);
        assert_eq!(find_key(&node, "apple".as_bytes(), Bias::First), 1);
        assert_eq!(find_key(&node, "banana".as_bytes(), Bias::First), 2);
        assert_eq!(find_key(&node, "zebra".as_bytes(), Bias::First), 3);
    }

    #[test]
    #[should_panic = "Insufficient space to insert"]
    fn test_insert_entry_full() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        for _ in 0..4096 {
            insert_entry(&mut node, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), &[0u8]);
        }
    }

    #[test]
    fn test_split_interior_node() {
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        init_btree_node(&mut node1);
        node1[0] = 0; // Clear leaf flag
        const PAGE1_ENTRIES: usize = 25;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            insert_entry(&mut node1, &key, &(i as u64).to_le_bytes());
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = record_array::get_num_entries(&node2);
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        let header1: &NodeHeader = bytemuck::from_bytes(&node1[0..record_array::HEADER_SIZE]);
        let header2: &NodeHeader = bytemuck::from_bytes(&node2[0..record_array::HEADER_SIZE]);
        assert_eq!(header2.right_child, u64::from_le_bytes(get_entry_value(&node1, orig_sep_index)
            .try_into().expect("value was not 8 bytes")));
        let header3: &NodeHeader = bytemuck::from_bytes(&node3[0..record_array::HEADER_SIZE]);
        assert_eq!(header3.right_child, header1.right_child);

        // Ensure all entries are present and in order
        let node2_recs = record_array::get_num_entries(&node2);
        assert_eq!(record_array::get_num_entries(&node1) - 1,
            node2_recs + record_array::get_num_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..record_array::get_num_entries(&node1) {
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
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        init_btree_node(&mut node1);
        set_u16(&mut node1, 0, FLAG_LEAF);

        const PAGE1_ENTRIES: usize = 25;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            insert_entry(&mut node1, &key, &(i as u64).to_le_bytes());
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = record_array::get_num_entries(&node2) - 1;
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        // Ensure all entries are present and in order
        let node2_recs = record_array::get_num_entries(&node2);
        assert_eq!(record_array::get_num_entries(&node1),
            node2_recs + record_array::get_num_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..record_array::get_num_entries(&node1) {
            let orig_entry = get_entry_key(&node1, i);
            if i >= node2_recs {
                assert_eq!(orig_entry, get_entry_key(&node3, i - node2_recs));
            } else {
                assert_eq!(orig_entry, get_entry_key(&node2, i));
            }
        }
    }

    // This only has two entries. Ensure it doesn't put both entries in the
    // first node, leaving none in the second (regression test).
    #[test]
    fn test_split_large_leaf() {
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        init_btree_node(&mut node1);
        set_u16(&mut node1, 0, FLAG_LEAF);
        insert_entry(&mut node1, &[1u8; 2000], &[1u8, 8]);
        insert_entry(&mut node1, &[2u8; 2000], &[2u8, 8]);

        split_node(&node1, &mut node2, &mut node3);

        assert_eq!(record_array::get_num_entries(&node2), 1);
        assert_eq!(record_array::get_num_entries(&node3), 1);
        sanity_check_node(&node2);
        sanity_check_node(&node3);
    }

    #[test]
    fn test_leaf_flag() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        assert!(is_leaf(&node));
        node[0] = 0;
        assert!(!is_leaf(&node));
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
        index.to_be_bytes().to_vec()
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

    fn populate_test_btree(num_entries: usize) -> (PageCache, PageAllocator, FilePageId) {
        let (page_cache, mut allocator, root_page) = create_test_btree();
        let _transaction = page_cache.begin_transaction();
        for i in prand_order(num_entries) {
            btree_insert(root_page, &gen_key_for_index(i), &(i as u64).to_le_bytes(),
                &page_cache, &mut allocator);
        }

        (page_cache, allocator, root_page)
    }

    #[test]
    fn test_valid_btree_create() {
        const NUM_ENTRIES: usize = 127;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);
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
        const NUM_ENTRIES: usize = 139;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        let mut cursor = btree_iterate(root_page, true, &page_cache);
        for i in (0..NUM_ENTRIES).rev() {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }

        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_find() {
        const NUM_ENTRIES: usize = 149;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        const START_KEY_IDX: usize = 55;
        let mut cursor = btree_find(root_page, &gen_key_for_index(START_KEY_IDX), false, &page_cache);
        for i in START_KEY_IDX..START_KEY_IDX + 10 {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), &gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }
    }

    // Get the first node in the tree, which requires traversing the left child node.
    #[test]
    fn test_btree_find_begin() {
        const NUM_ENTRIES: usize = 151;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0u8], false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), &gen_key_for_index(0));
        assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), 0u64);
    }

    // Key is before first key and going in reverse. Nothing to fetch.
    #[test]
    fn test_btree_reverse_find_begin() {
        const NUM_ENTRIES: usize = 151;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0u8], true, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Key is after last key and going forward. Nothing to fetch.
    #[test]
    fn test_btree_find_past_end() {
        const NUM_ENTRIES: usize = 79;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0xff; 255], false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // If we have duplicate keys, ensure that a cursor find will hit all of them.
    #[test]
    fn test_btree_find_duplicate_key() {
        let (page_cache, mut allocator, root_page) = populate_test_btree(0);

        {
            let _transaction = page_cache.begin_transaction();
            btree_insert(root_page, "aardvark".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "apple".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "apple".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "apple".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "apple".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "apple".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "banana".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "crayon".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "domino".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "elephant".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "fish".as_bytes(), &[0u8], &page_cache, &mut allocator);
            btree_insert(root_page, "grass".as_bytes(), &[0u8], &page_cache, &mut allocator);
        }
        let mut cursor = btree_find(root_page, "apple".as_bytes(), false, &page_cache);
        for _ in 0..5 {
            let (key, _) = cursor.next().expect("cursor didn't return value");
            assert_eq!(key, "apple".as_bytes());
        }
    }

    #[test]
    fn test_btree_many_same_key() {
        let (page_cache, mut allocator, root_page) = populate_test_btree(0);
        let insert_key = "aaaaaaa".as_bytes();
        let num_entries = 1000;

        {
            let _transaction = page_cache.begin_transaction();
            for i in 0..num_entries {
                btree_insert(root_page, insert_key, &(i as u64).to_le_bytes(), &page_cache, &mut allocator);
            }
        }

        let mut cursor = btree_find(root_page, insert_key, false, &page_cache);
        for i in 0..num_entries {
            let (key, value) = cursor.next().expect("cursor didn't return value");
            assert_eq!(key, insert_key);
            assert_eq!(value, &(i as u64).to_le_bytes());
        }

        let mut cursor = btree_find(root_page, insert_key, true, &page_cache);
        for i in (0..num_entries).rev() {
            let (key, value) = cursor.next().expect("cursor didn't return value");
            assert_eq!(key, insert_key);
            assert_eq!(value, &(i as u64).to_le_bytes());
        }
    }

    #[test]
    fn test_btree_delete() {
        const NUM_ENTRIES: usize = 97;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        const INDEX_TO_DELETE: usize = 37;
        {
            let _transaction = page_cache.begin_transaction();
            btree_delete(root_page, gen_key_for_index(INDEX_TO_DELETE).as_slice(), Some(&INDEX_TO_DELETE.to_le_bytes()), &page_cache);
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
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
        const NUM_ENTRIES: usize = 103;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);

        {
            let _transaction = page_cache.begin_transaction();

            // Key is bogus
            btree_delete(root_page, &"yolo".as_bytes(), Some(&[0u8]), &page_cache);

            // Key is present, but value doesn't match
            btree_delete(root_page, gen_key_for_index(10).as_slice(), Some(&[0u8]), &page_cache);
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }
    }

    #[test]
    fn test_btree_delete_no_value() {
        const NUM_ENTRIES: usize = 103;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);
        const INDEX_TO_DELETE: usize = 10;

        {
            let _transaction = page_cache.begin_transaction();

            // No value specified
            btree_delete(root_page, gen_key_for_index(INDEX_TO_DELETE).as_slice(), None, &page_cache);
        }

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            if i == INDEX_TO_DELETE {
                continue;
            }

            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(u64::from_le_bytes(val.try_into()
                .expect("value was not 8 bytes")), i as u64);
        }
    }

    #[test]
    fn test_btree_delete_all() {
        const NUM_ENTRIES: usize = 67;
        let (page_cache, _alloc, root_page) = populate_test_btree(NUM_ENTRIES);
        {
            let _transaction = page_cache.begin_transaction();
            for i in 0..NUM_ENTRIES {
                btree_delete(root_page, gen_key_for_index(i).as_slice(), None, &page_cache);
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
        let (page_cache, _alloc, root_page) = populate_test_btree(50);
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
        let len = rng.random_range(1..256);
        (0..len).map(|_| rng.random()).collect()
    }

    #[test]
    fn test_btree_stress() {
        let seed: u64 = 0x12345;
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut oracle = Oracle{ entries: Vec::new() };
        let (page_cache, mut allocator, root_page) = create_test_btree();

        // TODO tests still fail when this number gets larger
        let TOTAL_REPS = 2000;
        let MIN_PSUB: f64 = 0.3;
        for rep in 0..TOTAL_REPS {
            let p_add: f64 = MIN_PSUB + (1.0 - MIN_PSUB) * (1.0 - (rep as f64 / TOTAL_REPS as f64));
            println!("rep {} entries {} p_add = {}", rep, oracle.entries.len(), p_add);
            if rng.random::<f64>() > p_add {
                // Delete entry
                if !oracle.entries.is_empty() {
                    println!("delete record");
                    let i = rng.random_range(0..oracle.entries.len());
                    let entry = &oracle.entries[i];
                    let _transaction = page_cache.begin_transaction();
                    btree_delete(root_page, &entry.0, Some(&entry.1), &page_cache);
                    oracle.entries.remove(i);
                }
            } else {
                // Insert entry
                let key = random_value(&mut rng);
                let value = random_value(&mut rng);
                println!("insert record");
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
