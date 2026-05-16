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
use crate::page_cache::*;
use crate::page_allocator::*;


// Node format
// Bytes  |  Type  | Description
// -------|--------|----------------------
// 0..1   |  u16   | Flags
// 2..3   |  u16   | Start of entry storage
// 4..5   |  u16   | Num entries
// 6..13  |  u64   | Next sibling leaf, for in-order traversal
// 14..21 |  u64   | Prev sibling leaf, same as above
// 22..29 |  u64   | Right child. Value for nodes greater than max key (only in leaves)
// 30..   |  u16   | Index (see below)
//        |        | Free space
// N..    |        | Entry storage
//
// Each entry is:
//  key_length: u16
//  key: [u8; key_length]
//  value: u64
//
// The index contains offsets to each entry in the node. The index entries
// are always sorted in lexigraphical order, but the entries themselves
// do not have to be in sorted. The entries are, however always contiguous.
// As entries are added, they grow downward.
// The 'entries_start' field contains the address of the lowest entry.
// Each entry is a key/value pair, where the key is a variable length
// field and the value is a 64-bit integer.
// the 'right_child' field for internal nodes represents the child that
// is greater than all the keys. Since each entry in the page splits
// the keys, you end up with one extra pointer. Rather then using a
// dummy node, it is just stored here.
//

const ENTRY_START_FIELD_OFFS: usize = 2;
const NUM_ENTRIES_FIELD_OFFS: usize = 4;
const NEXT_SIB_FIELD_OFFS: usize = 6;
const PREV_SIB_FIELD_OFFS: usize = 14;
const LEFT_CHILD_FIELD_OFFS: usize = 22;
const INDEX_OFFS: usize = 30;

const FLAG_LEAF: u16 = 1;

const INVALID_FPID: u64 = 0;

struct Cursor {
    current_page_fpid: u64,
    current_index: usize,
    reverse: bool,
    page_cache: PageCache
}

impl Cursor {
    fn new(start_fpid: u64, start_index: usize, reverse: bool, page_cache: &PageCache) -> Self {
        Cursor {
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
        if self.reverse {
            if self.current_index == 0 {
                self.current_page_fpid = get_u64(&page, PREV_SIB_FIELD_OFFS);
                if self.current_page_fpid != INVALID_FPID {
                    let page = self.page_cache.lock_page(FilePageId(self.current_page_fpid));
                    self.current_index = get_num_entries(&page) - 1;
                }
            } else {
                self.current_index -= 1;
            }
        } else {
            if self.current_index == get_num_entries(&page) - 1 {
                self.current_page_fpid = get_u64(&page, NEXT_SIB_FIELD_OFFS);
                self.current_index = 0;
            } else {
                self.current_index += 1;
            }
        }

        Some(entry)
    }
}

fn btree_iterate(root_node_fpid: u64, reverse: bool, page_cache: &PageCache) -> Cursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page_mut(FilePageId(current_node_fpid));
        let index = if reverse { get_num_entries(&page) - 1 } else { 0 };
        if is_leaf(&page) {
            return Cursor::new(current_node_fpid, index, reverse, page_cache);
        }

        if index == 0 {
            current_node_fpid = get_u64(&page, LEFT_CHILD_FIELD_OFFS);
        } else {
            current_node_fpid = get_entry_value(&page, index);
        }
    }
}

fn btree_find(root_node_fpid: u64, key: &[u8], reverse: bool, page_cache: &PageCache) -> Cursor {
    let mut current_node_fpid = root_node_fpid;
    loop {
        let page = page_cache.lock_page_mut(FilePageId(current_node_fpid));
        let index = find_key(&page, key);
        if is_leaf(&page) {
            return Cursor::new(current_node_fpid, if reverse { index - 1 } else { index },
                reverse, page_cache);
        }

        if index == 0 {
            current_node_fpid = get_u64(&page, LEFT_CHILD_FIELD_OFFS);
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
            current_node_fpid = get_u64(&page, LEFT_CHILD_FIELD_OFFS);
        } else {
            current_node_fpid = get_entry_value(&page, index - 1);
        }

        // This would indicate the tree doesn't has interior nodes that don't
        // have leaves
        assert!(current_node_fpid != INVALID_FPID);
    }

    // We're now at a leaf. Insert and walk back up the tree splitting nodes
    // as needed.
    let mut insert_value = value;
    let mut insert_key = key.to_vec();
    for node_fpid in path.iter().rev() {
        let mut page = page_cache.lock_page_mut(FilePageId(*node_fpid));
        if get_node_free_space(&page) >= get_entry_size(&insert_key) {
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
            set_u64(&mut new_page1, NEXT_SIB_FIELD_OFFS, new_page_fpid2);
            set_u64(&mut new_page2, PREV_SIB_FIELD_OFFS, new_page_fpid1);

            // Now do the actual insertion
            if insert_key > split_key {
                insert_entry(&mut new_page2, insert_key.as_slice(), insert_value);
            } else {
                insert_entry(&mut new_page1, insert_key.as_slice(), insert_value);
            }

            // The root will have a single entry. It't no longer a leaf.
            init_node(&mut page);
            set_not_leaf(&mut page);
            append_entry(&mut page, &split_key, new_page_fpid2);
            set_u64(&mut page, LEFT_CHILD_FIELD_OFFS, new_page_fpid1);
            break;
        } else {
            // Split leaf or interior page.
            let new_page_fpid = page_allocator.alloc();
            let mut temp: [u8; PAGE_SIZE] = [0; PAGE_SIZE];
            let mut new_page = page_cache.lock_page_mut(FilePageId(new_page_fpid));
            let new_parent_key = split_node(&page, &mut temp, &mut new_page);
            if is_leaf(&page) {
                set_u64(&mut temp, PREV_SIB_FIELD_OFFS, get_u64(&page, PREV_SIB_FIELD_OFFS));
                set_u64(&mut temp, NEXT_SIB_FIELD_OFFS, new_page_fpid);
                set_u64(&mut new_page, PREV_SIB_FIELD_OFFS, *node_fpid);
                set_u64(&mut new_page, NEXT_SIB_FIELD_OFFS, get_u64(&page, NEXT_SIB_FIELD_OFFS));

                // Need to fix back-link
                let mut next_sib_page = page_cache.lock_page_mut(FilePageId(get_u64(&page, NEXT_SIB_FIELD_OFFS)));
                set_u64(&mut next_sib_page, PREV_SIB_FIELD_OFFS, new_page_fpid);
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
            delete_entry(&mut page, index);
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
            let left_child = get_u64(&page, LEFT_CHILD_FIELD_OFFS);
            if left_child != INVALID_FPID {
                fifo.push(left_child);
            }

            for i in 0..get_num_entries(&page) {
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
    for i in 0..get_num_entries(node) {
        println!("{}. {} value {}", i,
            to_hex(get_entry_key(node, i)), get_entry_value(node, i));
    }
}

// Create an empty node
fn init_node(node: &mut [u8]) {
    node.fill(0);
    set_u16(node, ENTRY_START_FIELD_OFFS, node.len() as u16);
    set_u16(node, 0, FLAG_LEAF);
}

fn is_leaf(node: &[u8]) -> bool {
    (get_u16(node, 0) & FLAG_LEAF) != 0
}

fn set_not_leaf(node: &mut [u8]) {
    set_u16(node, 0, get_u16(node, 0) & !FLAG_LEAF);
}

fn get_num_entries(node: &[u8]) -> usize {
    get_u16(node, NUM_ENTRIES_FIELD_OFFS) as usize
}

fn get_node_free_space(node: &[u8]) -> usize {
    let index_end = INDEX_OFFS + get_num_entries(node) * 2;
    let entry_start = get_u16(node, ENTRY_START_FIELD_OFFS) as usize;

    entry_start - index_end
}

fn get_entry_size(key: &[u8]) -> usize {
    // 8 bytes for value (64 bit integer)
    // 2 bytes for the entry length
    // 2 bytes for the index table entry
    key.len() + 8 + 2 + 2
}

fn get_entry_offs(node: &[u8], rec_num: usize) -> usize {
    get_u16(node, INDEX_OFFS + rec_num * 2) as usize
}

fn get_entry_key(node: &[u8], rec_num: usize) -> &[u8] {
    assert!(rec_num < get_num_entries(node));

    let entry_offs = get_entry_offs(node, rec_num);
    let key_len = get_u16(node, entry_offs) as usize;
    let data_start = entry_offs + 2;

    &node[data_start..data_start + key_len]
}

fn get_entry_value(node: &[u8], rec_num: usize) -> u64 {
    let num_entries = get_num_entries(node);
    assert!(rec_num <= num_entries);

    let entry_offs = get_entry_offs(node, rec_num);
    let key_len = get_u16(node, entry_offs) as usize;

    get_u64(node, entry_offs + 2 + key_len)
}

// A few ways to describe this:
// - Returns the index the key should be inserted in to keep the array
//   in order.
// - Returns the lowest key that is greater than or equal to the search key.
// If there are multiple copies of a key in the node, the index it chooses
// within that span of keys is undefined.
fn find_key(node: &[u8], key: &[u8]) -> usize {
    let mut low = 0;
    let mut high = get_num_entries(node);
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
    assert!(get_node_free_space(node) >= key.len() + 12);

    let num_recs = get_num_entries(node);
    let new_slot = find_key(node, key);

    // Move all index slots to make room
    let slot_start = INDEX_OFFS + new_slot * 2;
    let slot_end = INDEX_OFFS + num_recs * 2;
    node.copy_within(slot_start..slot_end, slot_start + 2);

    // Fill in the slot index
    let entry_size = key.len() + 10; // 2 bytes for length, 8 for the value
    let new_entry_offs = get_u16(node, ENTRY_START_FIELD_OFFS) as usize - entry_size;
    set_u16(node, NUM_ENTRIES_FIELD_OFFS, num_recs as u16 + 1); // Increment number of used slots.
    set_u16(node, INDEX_OFFS + new_slot * 2, new_entry_offs as u16); // Set entry offs
    set_u16(node, ENTRY_START_FIELD_OFFS, new_entry_offs as u16); // update pointer to data area

    // Fill in the entry
    set_u16(node, new_entry_offs, key.len() as u16);
    node[new_entry_offs + 2..new_entry_offs + 2 + key.len()].copy_from_slice(key);
    set_u64(node, new_entry_offs + 2 + key.len(), value);
}

// Helper function to add entry to next available slot. This assumes the entry is
// added in order. It assumes there is adequate space in the node.
// Returns entry size
fn append_entry(node: &mut [u8], key: &[u8], value: u64) -> usize {
    // 8 bytes for the value, 2 for the entry length, 2 for the index entry
    assert!(get_node_free_space(node) >= key.len() + 12);

    let entry_length = 2 + key.len() + 8;
    let entry_offs = get_u16(node, ENTRY_START_FIELD_OFFS) as usize - entry_length;
    let next_slot = get_num_entries(node);

    // Write the entry itself
    set_u16(node, entry_offs, key.len() as u16);
    node[entry_offs + 2..entry_offs + 2 + key.len()]
        .copy_from_slice(key);
    set_u64(node, entry_offs + 2 + key.len(), value);

    // Update index
    set_u16(node, INDEX_OFFS + next_slot * 2, entry_offs as u16);
    set_u16(node, NUM_ENTRIES_FIELD_OFFS, next_slot as u16 + 1);

    // Update start of entries
    set_u16(node, ENTRY_START_FIELD_OFFS, entry_offs as u16);

    entry_length
}

// Returns the separator key.
// NOTE: you must set the right_child in the returned out1 to the fpid of out2
// (we don't know it here)
fn split_node(orig: &[u8], out1: &mut [u8], out2: &mut [u8]) -> Vec<u8> {
    init_node(out1);
    init_node(out2);

    // Copy out entries from the orig into out1 until we have just over half.
    // then continue copying into out2.
    let orig_entries = get_num_entries(orig);

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
        assert!(get_entry_value(orig, orig_index) != 0);
        set_u64(out2, LEFT_CHILD_FIELD_OFFS, get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    // Copy into out2
    while orig_index < orig_entries {
        append_entry(out2, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }

    assert!(get_entry_value(orig, LEFT_CHILD_FIELD_OFFS) != 0);
    set_u64(out1, LEFT_CHILD_FIELD_OFFS, get_u64(orig, LEFT_CHILD_FIELD_OFFS));

    separator
}

fn delete_entry(node: &mut [u8], index: usize) {
    let total_recs = get_num_entries(node);
    assert!(index < total_recs);

    let deleted_entry_offs = get_u16(node, INDEX_OFFS + index * 2) as usize;
    let deleted_entry_len = get_u16(node, deleted_entry_offs) as usize + 10;

    // Remove this index entry and slide the other ones up to take the place
    let index_offs = INDEX_OFFS + index * 2;
    node.copy_within(index_offs + 2..INDEX_OFFS + total_recs * 2, index_offs);
    set_u16(node, NUM_ENTRIES_FIELD_OFFS, total_recs as u16 - 1);

    // Now move all the entries down so there are no gaps
    let old_entries_start = get_u16(node, ENTRY_START_FIELD_OFFS) as usize;
    node.copy_within(old_entries_start..deleted_entry_offs,
        old_entries_start + deleted_entry_len);

    // Walk through the remaining index, adjust offss of anything that was before
    // the deleted entry.
    for i in 0..total_recs - 1 {
        let old_offs = get_u16(node, INDEX_OFFS + i * 2) as usize;
        if old_offs < deleted_entry_offs {
            set_u16(node, INDEX_OFFS + i * 2, (old_offs + deleted_entry_len) as u16);
        }
    }

    // Adjust the new start of entries
    set_u16(node, ENTRY_START_FIELD_OFFS, (old_entries_start + deleted_entry_len) as u16);
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};
    use crate::util::*;
    use crate::page_allocator::*;
    use std::rc::Rc;
    use std::cell::RefCell;
    use crate::page_cache::*;
    use std::any::Any;
    use std::collections::HashSet;

    #[derive(Default)]
    struct MockIO {
        loaded_pages: HashSet<u64>
    }

    impl MockIO {
        fn default() -> Self {
            Self {
                loaded_pages: HashSet::new()
            }
        }
    }

    impl PersistentStore for MockIO {
        fn read(&mut self, offset: u64, slice: &mut [u8]) {
            if self.loaded_pages.contains(&offset) {
                // This indicates the page cache evicted a page and is trying to
                // reload it. Since we're not testing page cache here, we should
                // just make it large enough that it doesn't need to evict.
                panic!("reloaded pages: make PageCache larger");
            }

            self.loaded_pages.insert(offset);
            slice.fill(0);
        }

        fn write(&mut self, _offset: u64, _slice: &[u8]) {
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    fn sanity_check_node(node: &[u8]) {
        let mut sorted_rec_offs: Vec<usize> = Vec::new();

        // Walk through the entries, put offss into a list.
        let num_entries = get_u16(node, 4) as usize;
        if num_entries == 0 {
            // Ensure first offs in header is correct
            let header_first_offs = get_u16(node, 2) as usize;
            assert_eq!(node.len(), header_first_offs);
            return
        }

        for i in 0..num_entries {
            let rec_offs = get_u16(node, super::INDEX_OFFS + i * 2) as usize;
            assert_lt!(rec_offs, node.len());
            sorted_rec_offs.push(rec_offs);
        }

        // The entries don't have to be in order in the node, but put them
        // in order for our test.
        sorted_rec_offs.sort();

        // Ensure first offs in header is correct
        let header_first_offs = get_u16(node, 2) as usize;
        assert_eq!(sorted_rec_offs[0], header_first_offs);

        // Now ensure the entry are packed end-to-end, the lengths are in
        // the node.
        let mut last_entry_end = header_first_offs;
        for rec_offs in sorted_rec_offs {
            assert_eq!(rec_offs, last_entry_end); // ensure non-overlapping
            last_entry_end = rec_offs + get_u16(node, rec_offs) as usize + 10;
            assert_le!(last_entry_end, node.len()); // Ensure it doesn't spill off node
        }

        // Ensure the keys are in order
        let mut last_key: &[u8] = &[0];
        for i in 0..num_entries {
            let entry_offs = get_u16(node, super::INDEX_OFFS + i * 2) as usize;
            let key_len = get_u16(&node, entry_offs) as usize;
            let key_start = entry_offs + 2;

            let this_key = &node[key_start..key_start + key_len];
            assert_le!(last_key, this_key);
            last_key = this_key;
        }
    }

    #[test]
    #[should_panic]
    fn test_persistent_store_reread() {
        let mut mock_io = MockIO::default();
        let mut temp: [u8; 4096] = [0; 4096];
        mock_io.read(0, &mut temp);
        mock_io.read(0, &mut temp);
    }

    // Ensure sanity check catches out of order entries
    #[test]
    #[should_panic]
    fn test_sanity_check_out_of_order() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        // ! out of order
        super::append_entry(&mut node, "zzzzzzz".as_bytes(), 0);
        super::append_entry(&mut node, "aaaaaaa".as_bytes(), 0);
        sanity_check_node(&node);
    }

    // Ensure sanity check catches incorrect start of entry area
    #[test]
    #[should_panic]
    fn test_sanity_check_bad_offs() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        super::append_entry(&mut node, "a".as_bytes(), 0);
        super::append_entry(&mut node, "z".as_bytes(), 0);

        node[3] = 14; // Start of entry area = 3584

        sanity_check_node(&node);
    }

    // Ensure sanity check catches overlapping entries
    #[test]
    #[should_panic]
    fn test_sanity_check_overlapping_entry() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        super::append_entry(&mut node, "a".as_bytes(), 0);
        super::append_entry(&mut node, "z".as_bytes(), 0);

        let rec1_offs = get_u16(&node, super::INDEX_OFFS + 2) as usize; // second entry
        node[rec1_offs] += 1;

        sanity_check_node(&node);
    }

    #[test]
    fn test_get_key_val() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        super::append_entry(&mut node, "foobar".as_bytes(), 0x12345678abcdef);
        super::append_entry(&mut node, "zzzz".as_bytes(), 0xfedbca87654321);
        sanity_check_node(&node);
        assert_eq!(super::get_num_entries(&node), 2);

        let key_bytes0 = super::get_entry_key(&node, 0);
        assert_eq!(key_bytes0, "foobar".as_bytes());
        assert_eq!(super::get_entry_value(&node, 0), 0x12345678abcdef);

        let key_bytes1 = super::get_entry_key(&node, 1);
        assert_eq!(key_bytes1, "zzzz".as_bytes());
        assert_eq!(super::get_entry_value(&node, 1), 0xfedbca87654321);
    }

    #[test]
    fn test_find() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        super::append_entry(&mut node, "abacus".as_bytes(), 0);
        super::append_entry(&mut node, "banana".as_bytes(), 0);
        super::append_entry(&mut node, "beta".as_bytes(), 0);
        super::append_entry(&mut node, "zebra".as_bytes(), 0);
        sanity_check_node(&node);
        assert_eq!(super::get_num_entries(&node), 4);

        assert_eq!(super::find_key(&node, "aardvark".as_bytes()), 0); // Before first key
        assert_eq!(super::find_key(&node, "banana".as_bytes()), 1); // equal to second key
        assert_eq!(super::find_key(&node, "bananb".as_bytes()), 2); // slightly larger than second key
        assert_eq!(super::find_key(&node, "betas".as_bytes()), 3); // longer than third key
        assert_eq!(super::find_key(&node, "zzzzz".as_bytes()), 4); // higer than highest key
    }

    #[test]
    #[should_panic]
    fn test_append_full() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        for _ in 0..4096 {
            super::append_entry(&mut node, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), 0);
        }
    }

    #[test]
    fn test_find_key_empty() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        assert_eq!(super::find_key(&node, "foo".as_bytes()), 0);
    }

    // Validates both get_node_free_space and get_entry_size return a coherent
    // value
    #[test]
    fn test_entry_size() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        let init_free_space = super::get_node_free_space(&node);
        let key1 = "foo".as_bytes();
        super::insert_entry(&mut node, key1, 0x1234);
        assert_lt!(super::get_node_free_space(&node), init_free_space);
        assert_eq!(super::get_node_free_space(&node), init_free_space -
            super::get_entry_size(key1));

        let key2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let init_free_space = super::get_node_free_space(&node);
        super::insert_entry(&mut node, key2, 0x1234);
        assert_lt!(super::get_node_free_space(&node), init_free_space);
        assert_eq!(super::get_node_free_space(&node), init_free_space -
            super::get_entry_size(key2));
    }

    #[test]
    fn test_insert_entry() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        // Note these are out of order
        super::insert_entry(&mut node, "aardvark".as_bytes(), 1000);
        super::insert_entry(&mut node, "zebra".as_bytes(), 4000);
        super::insert_entry(&mut node, "apple".as_bytes(), 2000);
        super::insert_entry(&mut node, "banana".as_bytes(), 3000);
        sanity_check_node(&node);
        assert_eq!(super::get_num_entries(&node), 4);

        assert_eq!(super::find_key(&node, "aardvark".as_bytes()), 0);
        assert_eq!(super::find_key(&node, "apple".as_bytes()), 1);
        assert_eq!(super::find_key(&node, "banana".as_bytes()), 2);
        assert_eq!(super::find_key(&node, "zebra".as_bytes()), 3);
    }

    #[test]
    #[should_panic]
    fn test_insert_entry_full() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        for _ in 0..4096 {
            super::insert_entry(&mut node, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), 0);
        }
    }

    #[test]
    fn test_split_interior_node() {
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        super::init_node(&mut node1);
        node1[0] = 0; // Clear leaf flag
        const PAGE1_ENTRIES: usize = 25;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            super::insert_entry(&mut node1, &key, i as u64);
        }

        sanity_check_node(&node1);

        let separator_key = super::split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = super::get_num_entries(&node2);
        assert_eq!(&separator_key, &super::get_entry_key(&node1, orig_sep_index));
        assert_eq!(get_u64(&node3, super::LEFT_CHILD_FIELD_OFFS),
            super::get_entry_value(&node1, orig_sep_index));
        assert_eq!(get_u64(&node2, super::LEFT_CHILD_FIELD_OFFS),
            get_u64(&node1, super::LEFT_CHILD_FIELD_OFFS));

        // Ensure all entries are present and in order
        let node2_recs = super::get_num_entries(&node2);
        assert_eq!(super::get_num_entries(&node1) - 1,
            node2_recs + super::get_num_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..super::get_num_entries(&node1) {
            if i == node2_recs {
                continue; // ignore splitter
            }

            let orig_entry = super::get_entry_key(&node1, i);
            if i > node2_recs {
                assert_eq!(orig_entry, super::get_entry_key(&node3, i - node2_recs - 1));
            } else {
                assert_eq!(orig_entry, super::get_entry_key(&node2, i));
            }
        }
    }

    #[test]
    fn test_split_leaf_node() {
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        super::init_node(&mut node1);
        set_u16(&mut node1, 0, super::FLAG_LEAF);

        const PAGE1_ENTRIES: usize = 25;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            super::insert_entry(&mut node1, &key, i as u64);
        }

        sanity_check_node(&node1);

        let separator_key = super::split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

        let orig_sep_index = super::get_num_entries(&node2);
        assert_eq!(&separator_key, &super::get_entry_key(&node1, orig_sep_index));

        // Ensure all entries are present and in order
        let node2_recs = super::get_num_entries(&node2);
        assert_eq!(super::get_num_entries(&node1),
            node2_recs + super::get_num_entries(&node3));
        assert_lt!(node2_recs, PAGE1_ENTRIES * 2 / 3);
        for i in 0..super::get_num_entries(&node1) {
            let orig_entry = super::get_entry_key(&node1, i);
            if i >= node2_recs {
                assert_eq!(orig_entry, super::get_entry_key(&node3, i - node2_recs));
            } else {
                assert_eq!(orig_entry, super::get_entry_key(&node2, i));
            }
        }
    }

    #[test]
    fn test_delete_entry() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);

        // Note these are out of order
        super::insert_entry(&mut node, "aardvark".as_bytes(), 1000);
        super::insert_entry(&mut node, "apple".as_bytes(), 2000);
        super::insert_entry(&mut node, "banana".as_bytes(), 3000);
        super::insert_entry(&mut node, "zebra".as_bytes(), 4000);
        sanity_check_node(&node);
        assert_eq!(super::get_num_entries(&node), 4);

        // Remove from middle (apple)
        super::delete_entry(&mut node, 1);
        assert_eq!(super::get_num_entries(&node), 3);
        sanity_check_node(&node);

        assert_eq!(super::get_entry_key(&node, 0), "aardvark".as_bytes());
        assert_eq!(super::get_entry_value(&node, 0), 1000);
        assert_eq!(super::get_entry_key(&node, 1), "banana".as_bytes());
        assert_eq!(super::get_entry_value(&node, 1), 3000);
        assert_eq!(super::get_entry_key(&node, 2), "zebra".as_bytes());
        assert_eq!(super::get_entry_value(&node, 2), 4000);

        // Remove first entry (aardvark)
        super::delete_entry(&mut node, 0);
        assert_eq!(super::get_num_entries(&node), 2);
        sanity_check_node(&node);
        assert_eq!(super::get_entry_key(&node, 0), "banana".as_bytes());
        assert_eq!(super::get_entry_value(&node, 0), 3000);
        assert_eq!(super::get_entry_key(&node, 1), "zebra".as_bytes());
        assert_eq!(super::get_entry_value(&node, 1), 4000);

        // Remove last entry (zebra)
        super::delete_entry(&mut node, 1);
        assert_eq!(super::get_num_entries(&node), 1);
        sanity_check_node(&node);
        assert_eq!(super::get_entry_key(&node, 0), "banana".as_bytes());
        assert_eq!(super::get_entry_value(&node, 0), 3000);

        // Remove only remaining entry
        super::delete_entry(&mut node, 0);
        sanity_check_node(&node);
        assert_eq!(super::get_num_entries(&node), 0);
    }

    #[test]
    #[should_panic]
    fn test_delete_empty() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        super::delete_entry(&mut node, 0);
    }

    #[test]
    fn test_leaf_flag() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        assert!(super::is_leaf(&node));
        node[0] = 0;
        assert!(!super::is_leaf(&node));
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
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let mut page_cache = PageCache::new(50, Rc::clone(&mock_io));
        let mut allocator = PageAllocator::new(&mut page_cache);

        let root_page = allocator.alloc();
        {
            let mut node = page_cache.lock_page_mut(FilePageId(root_page));
            super::init_node(&mut node);
        }

        for i in prand_order(num_entries) {
            super::btree_insert(root_page, &gen_key_for_index(i), i as u64,
                &page_cache, &mut allocator);
        }

        (page_cache, allocator, root_page)
    }

    #[test]
    fn test_valid_btree_create() {
        const NUM_ENTRIES: usize = 120;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);
        let mut cursor = super::btree_iterate(root_page, false, &page_cache);
        for i in 0..NUM_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }
    }

    #[test]
    fn test_btree_backward_scan() {
        const NUM_ENTRIES: usize = 120;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        let mut cursor = super::btree_iterate(root_page, true, &page_cache);
        for i in (0..NUM_ENTRIES).rev() {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }

        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_find() {
        const NUM_ENTRIES: usize = 120;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        const START_KEY_IDX: usize = 55;
        let mut cursor = super::btree_find(root_page, &gen_key_for_index(START_KEY_IDX), false, &page_cache);
        for i in START_KEY_IDX..START_KEY_IDX + 10 {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), &gen_key_for_index(i));
            assert_eq!(val, i as u64);
        }
    }

    // Get the first node in the tree, which requires traversing the left child node.
    #[test]
    fn test_btree_find_begin() {
        const NUM_ENTRIES: usize = 120;
        let (page_cache, _alloc, root_page) = build_btree(NUM_ENTRIES);

        const START_KEY_IDX: usize = 55;
        let mut cursor = super::btree_find(root_page, &[0u8], false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), &gen_key_for_index(0));
        assert_eq!(val, 0u64);
    }

    #[test]
    fn test_btree_delete() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let mut allocator = PageAllocator::new(&mut page_cache);

        let root_page = allocator.alloc();

        {
            let mut node = page_cache.lock_page_mut(FilePageId(root_page));
            super::init_node(&mut node);
        }

        super::btree_insert(root_page, "aardvark".as_bytes(), 1000, &page_cache, &mut allocator);
        super::btree_insert(root_page, "aardvark".as_bytes(), 1001, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2000, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2001, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2002, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2003, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2004, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2005, &page_cache, &mut allocator);
        super::btree_insert(root_page, "banana".as_bytes(), 3000, &page_cache, &mut allocator);
        super::btree_insert(root_page, "banana".as_bytes(), 3001, &page_cache, &mut allocator);
        super::btree_insert(root_page, "zebra".as_bytes(), 4000, &page_cache, &mut allocator);

        super::btree_delete(root_page, "apple".as_bytes(), 2001, &page_cache);

        let mut cursor = super::btree_iterate(root_page, true, &page_cache);
        for _ in 0..10 {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert!(key.as_slice() != "apple".as_bytes() || val != 2001);
        }

        assert!(cursor.next().is_none());
    }

    #[test]
    fn test_btree_delete_not_present() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> = Rc::new(RefCell::new(MockIO::default()));
        let mut page_cache = PageCache::new(10, Rc::clone(&mock_io));
        let mut allocator = PageAllocator::new(&mut page_cache);

        let root_page = allocator.alloc();

        {
            let mut node = page_cache.lock_page_mut(FilePageId(root_page));
            super::init_node(&mut node);
        }

        super::btree_insert(root_page, "aardvark".as_bytes(), 1000, &page_cache, &mut allocator);
        super::btree_insert(root_page, "apple".as_bytes(), 2000, &page_cache, &mut allocator);
        super::btree_insert(root_page, "banana".as_bytes(), 3000, &page_cache, &mut allocator);

        super::btree_delete(root_page, "apple".as_bytes(), 2001, &page_cache);

        let mut cursor = super::btree_iterate(root_page, false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), "aardvark".as_bytes());
        assert_eq!(val, 1000);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), "apple".as_bytes());
        assert_eq!(val, 2000);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), "banana".as_bytes());
        assert_eq!(val, 3000);
        assert!(cursor.next().is_none());
    }

    // Just ensures it doesn't crash...
    #[test]
    fn test_print_btree() {
        let (page_cache, _alloc, root_page) = build_btree(50);
        let mut cursor = super::print_btree(root_page, &page_cache);
    }
}
