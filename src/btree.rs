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

use std::cmp::Ordering;
use crate::util::*;

// Btree node format
// [0..1] type_flags   u16
// [2..3] entries_start  u16
// [4..5] num_entries    u16
// [6..13] prev_sib    u64
// [14..21] next_sib   u64
// index: slot_offs[i] u16
// free space
// entries.
//   each entry:
//   [0..1] key length
//   [2..key_len] key
//   [key_len..key_len + 8] value
//
// The index contains offsets to each entry in the node. The index entries
// are always sorted in lexigraphical order, but the entries themselves
// do not have to be in sorted. The entries are, however always contiguous.
// The 'entries_start' field contains the address of the lowest entry.
// Each entry is a key/value pair, where the key is a variable length
// field and the value is a 64-bit integer.
//

const ENTRY_START_FIELD_OFFS: usize = 2;
const NUM_ENTRIES_FIELD_OFFS: usize = 4;
const INDEX_OFFS: usize = 22;

// Create an empty node
fn init_node(node: &mut [u8]) {
    node.fill(0);
    set_u16(node, ENTRY_START_FIELD_OFFS, node.len() as u16);
}

fn get_num_entries(node: &[u8]) -> usize {
    get_u16(node, NUM_ENTRIES_FIELD_OFFS) as usize
}

fn get_node_free_space(node: &[u8]) -> usize {
    let index_end = INDEX_OFFS + get_num_entries(node) * 2;
    let entry_start = get_u16(node, ENTRY_START_FIELD_OFFS) as usize;

    entry_start - index_end
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
    assert!(rec_num < get_num_entries(node));

    let entry_offs = get_entry_offs(node, rec_num);
    let key_len = get_u16(node, entry_offs) as usize;

    get_u64(node, entry_offs + 2 + key_len)
}

// Return the index of the first key *greater* than the search key.
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

fn split_node(orig: &[u8], out1: &mut [u8], out2: &mut [u8]) {
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

    // Copy into out2
    while orig_index < orig_entries {
        append_entry(out2, get_entry_key(orig, orig_index),
            get_entry_value(orig, orig_index));
        orig_index += 1;
    }
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
            let rec_offs = get_u16(node, 22 + i * 2) as usize;
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
            let entry_offs = get_u16(node, 22 + i * 2) as usize;
            let key_len = get_u16(&node, entry_offs) as usize;
            let key_start = entry_offs + 2;

            let this_key = &node[key_start..key_start + key_len];
            assert_le!(last_key, this_key);
            last_key = this_key;
        }
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

        let rec1_offs = get_u16(&node, 24) as usize; // second entry
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

    #[test]
    fn test_get_free_space() {
        let mut node: [u8; 4096] = [0; 4096];
        super::init_node(&mut node);
        assert_eq!(super::get_node_free_space(&node), 4096 - 22);
        super::insert_entry(&mut node, "aardvark".as_bytes(), 1000);
        assert_eq!(super::get_node_free_space(&node), 4096 - 42);
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
    fn test_split_node() {
        let mut node1: [u8; 4096] = [0; 4096];
        let mut node2: [u8; 4096] = [0; 4096];
        let mut node3: [u8; 4096] = [0; 4096];

        super::init_node(&mut node1);
        const PAGE1_ENTRIES: usize = 25;
        for i in 0..PAGE1_ENTRIES {
            let key = vec![b'A' + i as u8; 128];
            super::insert_entry(&mut node1, &key, i as u64);
        }

        sanity_check_node(&node1);

        super::split_node(&node1, &mut node2, &mut node3);
        sanity_check_node(&node2);
        sanity_check_node(&node3);

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
}
