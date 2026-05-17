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

// This is a B+ tree implementation. Keys are only stored in the leaf nodes.

use std::cmp::Ordering;
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
    left_child: u64,
}

// Each node entry is:
// key: 16 bytes
// value: variable length

const FLAG_LEAF: u16 = 1;

const INVALID_FPID: u64 = 0;

struct BTreeCursor {
    current_page_fpid: u64,
    current_index: usize,
    reverse: bool,
    page_cache: PageCache
}

impl BTreeCursor {
    fn new(start_fpid: u64, start_index: usize, reverse: bool, page_cache: &PageCache) -> Self {
        BTreeCursor {
            current_page_fpid: start_fpid,
            current_index: start_index,
            reverse,
            page_cache: page_cache.clone()
        }
    }

    fn next(&mut self) -> Option<(Vec<u8>, u64)> {
        if self.current_page_fpid == INVALID_FPID {
            return None
        }

        let page = self.page_cache.lock_page(FilePageId(self.current_page_fpid));
        let entry = (get_entry_key(&page, self.current_index).to_vec(),
            get_entry_value(&page, self.current_index));
        let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
        if self.reverse {
            if self.current_index == 0 {
                self.current_page_fpid = header.prev_sib;
                if self.current_page_fpid != INVALID_FPID {
                    let page = self.page_cache.lock_page(FilePageId(self.current_page_fpid));
                    self.current_index = record_array::get_num_entries(&page) - 1;
                }
            } else {
                self.current_index -= 1;
            }
        } else {
            if self.current_index == record_array::get_num_entries(&page) - 1 {
                self.current_page_fpid = header.next_sib;
                self.current_index = 0;
            } else {
                self.current_index += 1;
            }
        }

        Some(entry)
    }
}

fn btree_iterate(root_node_fpid: u64, reverse: bool, page_cache: &PageCache) -> BTreeCursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page_mut(FilePageId(current_node_fpid));
        let index = if reverse { record_array::get_num_entries(&page) - 1 } else { 0 };
        if is_leaf(&page) {
            return BTreeCursor::new(current_node_fpid, index, reverse, page_cache);
        }

        if index == 0 {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = header.left_child;
        } else {
            current_node_fpid = get_entry_value(&page, index);
        }
    }
}

fn btree_find(root_node_fpid: u64, key: &[u8], reverse: bool, page_cache: &PageCache) -> BTreeCursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page_mut(FilePageId(current_node_fpid));
        let index = find_key(&page, key);
        if is_leaf(&page) {
            if (reverse && index == 0) || (index == record_array::get_num_entries(&page)) {
                // Nothing to fetch, return dummy cursor
                return BTreeCursor::new(INVALID_FPID, 0, false, page_cache);
            }

            return BTreeCursor::new(current_node_fpid, if reverse { index - 1 } else { index },
                reverse, page_cache);
        }

        if index == 0 {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = header.left_child;
        } else {
            current_node_fpid = get_entry_value(&page, index - 1);
        }
    }
}

fn btree_insert(root_node_fpid: u64,
    key: &[u8],
    value: u64,
    page_cache: &PageCache,
    page_allocator: &mut PageAllocator)
{
    let mut current_node_fpid = root_node_fpid;
    let mut path: Vec<u64> = Vec::new();

    loop {
        path.push(current_node_fpid);
        let page = page_cache.lock_page_mut(FilePageId(current_node_fpid));
        if is_leaf(&page) {
            break;
        }

        let index = find_key(&page, key);
        if index == 0 {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            current_node_fpid = header.left_child;
        } else {
            current_node_fpid = get_entry_value(&page, index - 1);
        }

        assert!(current_node_fpid != INVALID_FPID, "Interior node has non-leaf children");
    }

    // We're now at a leaf. Insert and walk back up the tree splitting nodes
    // as needed.
    let mut insert_value = value;
    let mut insert_key = key.to_vec();
    for node_fpid in path.iter().rev() {
        let mut page = page_cache.lock_page_mut(FilePageId(*node_fpid));
        if record_array::get_free_space(&page) >= get_entry_size(&insert_key) {
            insert_entry(&mut page, insert_key.as_slice(), insert_value);
            break;
        }

        // Need to split...
        if *node_fpid == root_node_fpid {
            // Root node splits are special
            let new_page_fpid1 = page_allocator.alloc();
            let new_page_fpid2 = page_allocator.alloc();

            let mut new_page1 = page_cache.lock_page_mut(FilePageId(new_page_fpid1));
            let mut new_page2 = page_cache.lock_page_mut(FilePageId(new_page_fpid2));
            let split_key = split_node(&page, &mut new_page1, &mut new_page2);

            // This really only matters when the root is a leaf
            let header1: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page1[0..record_array::HEADER_SIZE]);
            header1.next_sib = new_page_fpid2;
            let header2: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page2[0..record_array::HEADER_SIZE]);
            header2.prev_sib = new_page_fpid1;

            // Now do the actual insertion
            if insert_key > split_key {
                insert_entry(&mut new_page2, insert_key.as_slice(), insert_value);
            } else {
                insert_entry(&mut new_page1, insert_key.as_slice(), insert_value);
            }

            // The root will have a single entry. It't no longer a leaf.
            init_btree_node(&mut page);
            set_not_leaf(&mut page);
            append_entry(&mut page, &split_key, new_page_fpid2);
            let page_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut page[0..record_array::HEADER_SIZE]);
            page_header.left_child = new_page_fpid1;
            break;
        } else {
            // Split leaf or interior page.
            let new_page_fpid = page_allocator.alloc();
            let mut temp: [u8; PAGE_SIZE] = [0; PAGE_SIZE];
            let mut new_page = page_cache.lock_page_mut(FilePageId(new_page_fpid));
            let new_parent_key = split_node(&page, &mut temp, &mut new_page);
            if is_leaf(&page) {
                let old_page_header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
                let temp_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut temp[0..record_array::HEADER_SIZE]);
                let new_page_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut new_page[0..record_array::HEADER_SIZE]);
                temp_header.prev_sib = old_page_header.prev_sib;
                temp_header.next_sib = new_page_fpid;
                new_page_header.prev_sib = *node_fpid;
                new_page_header.next_sib = old_page_header.next_sib;

                // Need to fix back-link
                let mut next_sib_page = page_cache.lock_page_mut(FilePageId(old_page_header.next_sib));
                let next_sib_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut next_sib_page[0..record_array::HEADER_SIZE]);
                next_sib_header.prev_sib = new_page_fpid;
            }

            page.copy_from_slice(&temp);

            // Now do the actual insertion
            if insert_key > new_parent_key {
                insert_entry(&mut new_page, insert_key.as_slice(), insert_value);
            } else {
                insert_entry(&mut page, insert_key.as_slice(), insert_value);
            }

            insert_key = new_parent_key;
            insert_value = new_page_fpid;
        }
    }
}

fn btree_delete(root_node_fpid: u64,
    key: &[u8],
    value: u64,
    page_cache: &PageCache)
{
    // Since the btree doesn't enforce unique keys by default, we use a cursor
    // to find the specific entry to delete (for our use cases, we know the
    // key/value tuple will be unique, although btree code does not enforce
    // that).
    let mut cursor = btree_find(root_node_fpid, key, false, page_cache);
    loop {
        // Need to save these because cursor will post-update
        let page_fpid = cursor.current_page_fpid;
        let index = cursor.current_index;
        let next = cursor.next();
        if next.is_none() {
            break;
        }

        let (entry_key, entry_val) = next.unwrap();
        if key == entry_key && value == entry_val {
            let mut page = page_cache.lock_page_mut(FilePageId(page_fpid));
            record_array::delete_record(&mut page, index);
            break;
        }
    }
}

fn print_btree(root_node_fpid: u64, page_cache: &PageCache) {
    let mut fifo: Vec<u64> = Vec::new();
    fifo.push(root_node_fpid);
    while fifo.len() != 0 {
        let next = fifo.remove(0);
        let page = page_cache.lock_page_mut(FilePageId(next));
        print_node(&page);
        if !is_leaf(&page) {
            let header: &NodeHeader = bytemuck::from_bytes(&page[0..record_array::HEADER_SIZE]);
            if header.left_child != INVALID_FPID {
                fifo.push(header.left_child);
            }

            for i in 0..record_array::get_num_entries(&page) {
                fifo.push(get_entry_value(&page, i));
            }
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut result: String = "".to_string();
    for x in bytes {
        result += format!("{:02x}", x).as_str();
    }

    result
}

fn print_node(node: &[u8]) {
    for i in 0..record_array::get_num_entries(node) {
        println!("{}. {} value {}", i,
            to_hex(get_entry_key(node, i)), get_entry_value(node, i));
    }
}

// Create an empty node
fn init_btree_node(node: &mut [u8]) {
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

fn get_entry_size(key: &[u8]) -> usize {
    // 8 bytes for value (64 bit integer)
    // 2 bytes for the entry length
    // 2 bytes for the index table entry
    key.len() + 8 + 2 + 2
}

fn get_entry_key(node: &[u8], rec_num: usize) -> &[u8] {
    let rec = record_array::get_record(node, rec_num);
    &rec[8..rec.len()]
}

fn get_entry_value(node: &[u8], rec_num: usize) -> u64 {
    let rec = record_array::get_record(node, rec_num);
    get_u64(rec, 0)
}

// A few ways to describe this:
// - Returns the index the key should be inserted in to keep the array
//   in order.
// - Returns the lowest key that is greater than or equal to the search key.
// If there are multiple copies of a key in the node, the index it chooses
// within that span of keys is undefined.
fn find_key(node: &[u8], key: &[u8]) -> usize {
    let mut low = 0;
    let mut high = record_array::get_num_entries(node);
    while low < high {
        let mid = (low + high) / 2;
        let mid_key = get_entry_key(node, mid);
        match key.cmp(mid_key) {
            Ordering::Less => high = mid,
            Ordering::Greater => low = mid + 1,
            Ordering::Equal => return mid,
        }
    }

    low
}

// Insert a entry into a single node.
fn insert_entry(node: &mut [u8], key: &[u8], value: u64) {
    let index = find_key(node, key);
    let mut entry = Vec::with_capacity(key.len() + 8);
    entry.extend_from_slice(&value.to_le_bytes());
    entry.extend_from_slice(key);
    record_array::insert_record(node, index, &entry);
}

// Helper function to add entry to next available slot. This assumes the entry is
// added in order. It assumes there is adequate space in the node.
// Returns entry size
fn append_entry(node: &mut [u8], key: &[u8], value: u64) -> usize {
    let mut entry = Vec::with_capacity(key.len() + 8);
    entry.extend_from_slice(&value.to_le_bytes());
    entry.extend_from_slice(key);
    record_array::insert_record(node, record_array::get_num_entries(node), &entry);

    get_entry_size(key)
}

// Returns the separator key.
// NOTE: you must set the right_child in the returned out1 to the fpid of out2
// (we don't know it here)
fn split_node(orig: &[u8], out1: &mut [u8], out2: &mut [u8]) -> Vec<u8> {
    init_btree_node(out1);
    init_btree_node(out2);

    // Copy out entries from the orig into out1 until we have just over half.
    // then continue copying into out2.
    let orig_entries = record_array::get_num_entries(orig);

    let mut orig_index = 0;
    let mut bytes_copied = 0;

    // Copy into out1
    while bytes_copied < orig.len() / 2 {
        bytes_copied += append_entry(out1, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    let separator = get_entry_key(orig, orig_index).to_vec();

    if !is_leaf(orig) {
        // Remove the separator key, which will go into the parent. Save its value
        // into the left child.
        let header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut out2[0..record_array::HEADER_SIZE]);
        header.left_child = get_entry_value(orig, orig_index);
        orig_index += 1;
    }

    // Copy into out2
    while orig_index < orig_entries {
        append_entry(out2, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    let out_header: &mut NodeHeader = bytemuck::from_bytes_mut(&mut out1[0..record_array::HEADER_SIZE]);
    let orig_header: &NodeHeader = bytemuck::from_bytes(&orig[0..record_array::HEADER_SIZE]);
    out_header.left_child = orig_header.left_child;

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

        append_entry(&mut node, "foobar".as_bytes(), 0x12345678abcdef);
        append_entry(&mut node, "zzzz".as_bytes(), 0xfedbca87654321);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 2);

        let key_bytes0 = get_entry_key(&node, 0);
        assert_eq!(key_bytes0, "foobar".as_bytes());
        assert_eq!(get_entry_value(&node, 0), 0x12345678abcdef);

        let key_bytes1 = get_entry_key(&node, 1);
        assert_eq!(key_bytes1, "zzzz".as_bytes());
        assert_eq!(get_entry_value(&node, 1), 0xfedbca87654321);
    }

    #[test]
    fn test_find() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        append_entry(&mut node, "abacus".as_bytes(), 0);
        append_entry(&mut node, "banana".as_bytes(), 0);
        append_entry(&mut node, "beta".as_bytes(), 0);
        append_entry(&mut node, "zebra".as_bytes(), 0);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 4);

        assert_eq!(find_key(&node, "aardvark".as_bytes()), 0); // Before first key
        assert_eq!(find_key(&node, "banana".as_bytes()), 1); // equal to second key
        assert_eq!(find_key(&node, "bananb".as_bytes()), 2); // slightly larger than second key
        assert_eq!(find_key(&node, "betas".as_bytes()), 3); // longer than third key
        assert_eq!(find_key(&node, "zzzzz".as_bytes()), 4); // higer than highest key
    }

    #[test]
    fn test_find_key_empty() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        assert_eq!(find_key(&node, "foo".as_bytes()), 0);
    }

    // Validates both record_array::get_free_space and get_entry_size return a coherent
    // value
    #[test]
    fn test_entry_size() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        let init_free_space = record_array::get_free_space(&node);
        let key1 = "foo".as_bytes();
        insert_entry(&mut node, key1, 0x1234);
        assert_lt!(record_array::get_free_space(&node), init_free_space);
        assert_eq!(record_array::get_free_space(&node), init_free_space -
            get_entry_size(key1));

        let key2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let init_free_space = record_array::get_free_space(&node);
        insert_entry(&mut node, key2, 0x1234);
        assert_lt!(record_array::get_free_space(&node), init_free_space);
        assert_eq!(record_array::get_free_space(&node), init_free_space -
            get_entry_size(key2));
    }

    #[test]
    fn test_insert_entry() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);

        // Note these are out of order
        insert_entry(&mut node, "aardvark".as_bytes(), 1000);
        insert_entry(&mut node, "zebra".as_bytes(), 4000);
        insert_entry(&mut node, "apple".as_bytes(), 2000);
        insert_entry(&mut node, "banana".as_bytes(), 3000);
        sanity_check_node(&node);
        assert_eq!(record_array::get_num_entries(&node), 4);

        assert_eq!(find_key(&node, "aardvark".as_bytes()), 0);
        assert_eq!(find_key(&node, "apple".as_bytes()), 1);
        assert_eq!(find_key(&node, "banana".as_bytes()), 2);
        assert_eq!(find_key(&node, "zebra".as_bytes()), 3);
    }

    #[test]
    #[should_panic = "Insufficient space to insert"]
    fn test_insert_entry_full() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        for _ in 0..4096 {
            insert_entry(&mut node, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), 0);
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
            insert_entry(&mut node1, &key, i as u64);
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = record_array::get_num_entries(&node2);
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        let header1: &NodeHeader = bytemuck::from_bytes(&node1[0..record_array::HEADER_SIZE]);
        let header2: &NodeHeader = bytemuck::from_bytes(&node2[0..record_array::HEADER_SIZE]);
        assert_eq!(header2.left_child, header1.left_child);
        let header3: &NodeHeader = bytemuck::from_bytes(&node3[0..record_array::HEADER_SIZE]);
        assert_eq!(header3.left_child, get_entry_value(&node1, orig_sep_index));

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
            insert_entry(&mut node1, &key, i as u64);
        }

        sanity_check_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = record_array::get_num_entries(&node2);
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

    #[test]
    fn test_leaf_flag() {
        let mut node: [u8; 4096] = [0; 4096];
        init_btree_node(&mut node);
        assert!(is_leaf(&node));
        node[0] = 0;
        assert!(!is_leaf(&node));
    }

    fn prand_order(n: usize) -> Vec<usize> {
        let mut result: Vec<usize> = (0..n).collect();
        let mut seed: u32 = 12345;
        for i in (1..n).rev() {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            let j = (seed & 0x7fffffff) as usize % n;
            result.swap(i, j);
        }

        result
    }

    fn gen_key_for_index(index: usize) -> Vec<u8> {
        vec![index as u8; (index % 64) + 64]
    }

    fn build_btree(num_entries: usize) -> (PageCache, PageAllocator, u64) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(50, Rc::clone(&mock_io));
        let mut allocator = PageAllocator::new(&mut page_cache);

        let root_page = allocator.alloc();
        {
            let mut node = page_cache.lock_page_mut(FilePageId(root_page));
            init_btree_node(&mut node);
        }

        for i in prand_order(num_entries) {
            btree_insert(root_page, &gen_key_for_index(i), i as u64,
                &page_cache, &mut allocator);
        }

        (page_cache, allocator, root_page)
    }

    #[test]
    fn test_valid_btree_create() {
        const NUM_ENTRIES: usize = 127;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);
        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }
    }

    #[test]
    fn test_btree_backward_scan() {
        const NUM_ENTRIES: usize = 139;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        let mut cursor = btree_iterate(root_page, true, &page_cache);
        for i in (0..NUM_ENTRIES).rev() {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }

        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_find() {
        const NUM_ENTRIES: usize = 149;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        const START_KEY_IDX: usize = 55;
        let mut cursor = btree_find(root_page, &gen_key_for_index(START_KEY_IDX), false, &page_cache);
        for i in START_KEY_IDX..START_KEY_IDX + 10 {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), &gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }
    }

    // Get the first node in the tree, which requires traversing the left child node.
    #[test]
    fn test_btree_find_begin() {
        const NUM_ENTRIES: usize = 151;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0u8], false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), &gen_key_for_index(0));
        assert_eq!(val, 0u64);
    }

    // Key is before first key and going in reverse. Nothing to fetch.
    #[test]
    fn test_btree_reverse_find_begin() {
        const NUM_ENTRIES: usize = 151;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0u8], true, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Key is after last key and going forward. Nothing to fetch.
    #[test]
    fn test_btree_find_past_end() {
        const NUM_ENTRIES: usize = 79;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        let mut cursor = btree_find(root_page, &[0xff; 255], false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_delete() {
        const NUM_ENTRIES: usize = 97;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        const INDEX_TO_DELETE: usize = 37;
        btree_delete(root_page, gen_key_for_index(INDEX_TO_DELETE).as_slice(), INDEX_TO_DELETE as u64, &page_cache);

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            if i == INDEX_TO_DELETE {
                continue;
            }

            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }

        assert!(cursor.next().is_none());
    }

    #[test]
    fn test_btree_delete_not_present() {
        const NUM_ENTRIES: usize = 103;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        // Key is bogus
        btree_delete(root_page, &"yolo".as_bytes(), 11, &page_cache);

        // Key is present, but value doesn't match
        btree_delete(root_page, gen_key_for_index(10).as_slice(), 11, &page_cache);

        let mut cursor = btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }
    }

    // Just ensures it doesn't crash...
    #[test]
    fn test_print_btree() {
        let (page_cache, _alloc, root_page) = build_btree(50);
        print_btree(root_page, &page_cache);
    }
}
