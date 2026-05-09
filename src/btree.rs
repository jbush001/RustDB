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

use crate::page_cache;

// Btree page format
// [0..1] type_flags   u16
// [2..3] record_start  u16
// [4..5] num_slots    u16
// [6..13] prev_sib    u64
// [14..21] next_sib   u64
// slot array: slot_offs[i] u16
// free space
// records.
//   each record:
//   [0..1] key length
//   [2..key_len] key
//   [key_len..key_len + 8] value
//
// The slot array always points to records in order. The records themselves
// do not have to be in order, but they are always contiguous.
//

const RECORD_START_FIELD_OFFS: usize = 2;
const NUM_SLOTS_FIELD_OFFS: usize = 4;
const SLOT_ARRAY_OFFS: usize = 22;

fn get_u16(page: &[u8], offs: usize) -> u16 {
    u16::from_le_bytes(page[offs..offs + 2].try_into().unwrap())
}

fn get_u32(page: &[u8], offs: usize) -> u32 {
    u32::from_le_bytes(page[offs..offs + 4].try_into().unwrap())
}

fn get_u64(page: &[u8], offs: usize) -> u64 {
    u64::from_le_bytes(page[offs..offs + 8].try_into().unwrap())
}

fn set_u16(page: &mut [u8], offs: usize, val: u16) {
    page[offs..offs + 2].copy_from_slice(&val.to_le_bytes());
}

fn set_u32(page: &mut [u8], offs: usize, val: u32) {
    page[offs..offs + 4].copy_from_slice(&val.to_le_bytes());
}

fn set_u64(page: &mut [u8], offs: usize, val: u64) {
    page[offs..offs + 8].copy_from_slice(&val.to_le_bytes());
}

fn get_record_offs(page: &[u8], rec_num: usize) -> usize {
    get_u16(page, SLOT_ARRAY_OFFS + rec_num * 2) as usize
}

fn get_num_records(page: &[u8]) -> usize {
    get_u16(page, NUM_SLOTS_FIELD_OFFS) as usize
}

// Does not check that rec_num is < num_records
fn get_record_key(page: &[u8], rec_num: usize) -> &[u8] {
    let record_offs = get_record_offs(page, rec_num);
    let key_len = get_u16(&page, record_offs) as usize;
    let data_start = record_offs + 2;

    &page[data_start..data_start + key_len]
}

fn get_record_value(page: &[u8], rec_num: usize) -> u64 {
    let record_offs = get_record_offs(page, rec_num);
    let key_len = get_u16(&page, record_offs) as usize;

    get_u64(&page, record_offs + 2 + key_len)
}

// Return the index of the first key *greater* than the search key.
fn find_key(page: &[u8], key: &[u8]) -> usize {
    let mut low = 0;
    let mut high = get_num_records(page) as usize;
    while low < high {
        let mid = (low + high) / 2;
        let mid_key = get_record_key(page, mid);
        if key < mid_key {
            high = mid;
        } else if key > mid_key {
            low = mid + 1;
        } else {
            return mid;
        }
    }

    low
}

fn get_page_free_space(page: &[u8]) -> usize {
    let index_end = SLOT_ARRAY_OFFS + get_num_records(page) as usize * 2;
    let record_start = get_u16(page, RECORD_START_FIELD_OFFS) as usize;

    record_start - index_end
}

// Insert a record into a single node.
fn insert_record(page: &mut [u8], key: &[u8], value: u64) {
    assert!(get_page_free_space(page) >= key.len() + 12);

    let num_recs = get_num_records(page);
    let new_slot = find_key(page, key);

    // Move all index slots to make room
    let slot_start = SLOT_ARRAY_OFFS + new_slot * 2;
    let slot_end = SLOT_ARRAY_OFFS + num_recs * 2;
    page.copy_within(slot_start..slot_end, slot_start + 2);

    // Fill in the slot index
    let record_size = key.len() + 10; // 2 bytes for length, 8 for the value
    let new_record_offs = get_u16(page, RECORD_START_FIELD_OFFS) as usize - record_size;
    set_u16(page, NUM_SLOTS_FIELD_OFFS, num_recs as u16 + 1); // Increment number of used slots.
    set_u16(page, SLOT_ARRAY_OFFS + new_slot * 2, new_record_offs as u16); // Set record offs
    set_u16(page, RECORD_START_FIELD_OFFS, new_record_offs as u16); // update pointer to data area

    // Fill in the record
    set_u16(page, new_record_offs, key.len() as u16);
    page[new_record_offs + 2..new_record_offs + 2 + key.len()].copy_from_slice(key);
    set_u64(page, new_record_offs + 2 + key.len(), value);
}

fn init_page(page: &mut [u8]) {
    page.fill(0);
    set_u16(page, RECORD_START_FIELD_OFFS, page.len() as u16);
}

// Helper function to add record to next available slot. This assumes the record is
// added in order. It assumes there is adequate space in the page.
// Returns record size
fn append_record(page: &mut [u8], key: &[u8], value: u64) -> usize {
    // 8 bytes for the value, 2 for the record length, 2 for the index entry
    assert!(get_page_free_space(page) >= key.len() + 12);

    let record_length = 2 + key.len() + 8;
    let record_offs = get_u16(page, RECORD_START_FIELD_OFFS) as usize - record_length;
    let next_slot = get_num_records(page);

    // Write the record itself
    set_u16(page, record_offs, key.len() as u16);
    page[record_offs + 2..record_offs + 2 + key.len()]
        .copy_from_slice(key);
    set_u64(page, record_offs + 2 + key.len(), value);

    // Update slots
    set_u16(page, SLOT_ARRAY_OFFS + next_slot * 2, record_offs as u16);
    set_u16(page, NUM_SLOTS_FIELD_OFFS, next_slot as u16 + 1);

    // Update start of records
    set_u16(page, RECORD_START_FIELD_OFFS, record_offs as u16);

    record_length
}

fn split_node(orig: &[u8], out1: &mut [u8], out2: &mut [u8]) {
    init_page(out1);
    init_page(out2);

    // Copy out entries from the orig into out1 until we have just over half.
    // then continue copying into out2.
    let orig_records = get_num_records(orig);

    let mut orig_index = 0;
    let mut bytes_copied = 0;

    // Copy into out1
    while bytes_copied < orig.len() / 2 {
        bytes_copied += append_record(out1, get_record_key(orig, orig_index),
            get_record_value(orig, orig_index));
        orig_index += 1;
    }

    // Copy into out2
    while orig_index < orig_records {
        append_record(out2, get_record_key(orig, orig_index),
            get_record_value(orig, orig_index));
        orig_index += 1;
    }
}

fn delete_record(page: &mut [u8], index: usize) {
    let total_recs = get_num_records(page);
    assert!(index < total_recs);

    let deleted_record_offs = get_u16(page, SLOT_ARRAY_OFFS + index * 2) as usize;
    let deleted_record_len = get_u16(page, deleted_record_offs) as usize + 10;

    // Remove this index entry and slide the other ones up to take the place
    let index_offs = SLOT_ARRAY_OFFS + index * 2;
    page.copy_within(index_offs + 2..SLOT_ARRAY_OFFS + total_recs * 2, index_offs);
    set_u16(page, NUM_SLOTS_FIELD_OFFS, total_recs as u16 - 1);

    // Now move all the records down so there are no gaps
    let old_records_start = get_u16(page, RECORD_START_FIELD_OFFS) as usize;
    page.copy_within(old_records_start..deleted_record_offs,
        old_records_start + deleted_record_len);

    // Walk through the remaining index, adjust offss of anything that was before
    // the deleted record.
    for i in 0..total_recs - 1 {
        let old_offs = get_u16(page, SLOT_ARRAY_OFFS + i * 2) as usize;
        if old_offs < deleted_record_offs {
            set_u16(page, SLOT_ARRAY_OFFS + i * 2, (old_offs + deleted_record_len) as u16);
        }
    }

    // Adjust the new start of records
    set_u16(page, RECORD_START_FIELD_OFFS, (old_records_start + deleted_record_len) as u16);
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};

    fn sanity_check_node(page: &[u8]) {
        let mut sorted_rec_offs: Vec<usize> = Vec::new();

        // Walk through the records, put offss into a list.
        let num_records = super::get_u16(page, 4) as usize;
        if num_records == 0 {
            // Ensure first offs in header is correct
            let header_first_offs = super::get_u16(page, 2) as usize;
            assert_eq!(page.len(), header_first_offs);
            return
        }

        for i in 0..num_records {
            let rec_offs = super::get_u16(page, 22 + i * 2) as usize;
            assert_lt!(rec_offs, page.len());
            sorted_rec_offs.push(rec_offs);
        }

        // The records don't have to be in order in the page, but put them
        // in order for our test.
        sorted_rec_offs.sort();

        // Ensure first offs in header is correct
        let header_first_offs = super::get_u16(page, 2) as usize;
        assert_eq!(sorted_rec_offs[0], header_first_offs);

        // Now ensure the record are packed end-to-end, the lengths are in
        // the page.
        let mut last_record_end = header_first_offs;
        for rec_offs in sorted_rec_offs {
            assert_eq!(rec_offs, last_record_end); // ensure non-overlapping
            last_record_end = rec_offs + super::get_u16(page, rec_offs) as usize + 10;
            assert_le!(last_record_end, page.len()); // Ensure it doesn't spill off page
        }

        // Ensure the keys are in order
        let mut last_key: &[u8] = &[0];
        for i in 0..num_records {
            let record_offs = super::get_u16(page, 22 + i * 2) as usize;
            let key_len = super::get_u16(&page, record_offs) as usize;
            let key_start = record_offs + 2;

            let this_key = &page[key_start..key_start + key_len];
            assert_le!(last_key, this_key);
            last_key = this_key;
        }
    }

    // Ensure sanity check catches problems
    #[test]
    #[should_panic]
    fn test_sanity_check_ooo() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);
        // ! out of order
        super::append_record(&mut page, "zzzzzzz".as_bytes(), 0);
        super::append_record(&mut page, "aaaaaaa".as_bytes(), 0);
        sanity_check_node(&page);
    }

    #[test]
    #[should_panic]
    fn test_sanity_check_bad_offs() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        super::append_record(&mut page, "a".as_bytes(), 0);
        super::append_record(&mut page, "z".as_bytes(), 0);

        page[3] = 14; // Start of record area = 3584

        sanity_check_node(&page);
    }

    #[test]
    #[should_panic]
    fn test_sanity_check_overlapping_record() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        super::append_record(&mut page, "a".as_bytes(), 0);
        super::append_record(&mut page, "z".as_bytes(), 0);

        let rec1_offs = super::get_u16(&page, 24) as usize; // second record
        page[rec1_offs] += 1;

        sanity_check_node(&page);
    }

    #[test]
    fn test_get_value() {
        let value_array: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(super::get_u16(value_array, 1), 0x0302);
        assert_eq!(super::get_u32(value_array, 2), 0x06050403);
        assert_eq!(super::get_u64(value_array, 1), 0x0908070605040302);
    }

    #[test]
    fn test_set_value() {
        let value_array: &mut [u8] = &mut [0; 16];
        super::set_u16(value_array, 1, 0x1234);
        assert_eq!(value_array[1], 0x34);
        assert_eq!(value_array[2], 0x12);

        super::set_u32(value_array, 1, 0x12345678);
        assert_eq!(value_array[1], 0x78);
        assert_eq!(value_array[2], 0x56);
        assert_eq!(value_array[3], 0x34);
        assert_eq!(value_array[4], 0x12);

        super::set_u64(value_array, 1, 0x12345678abcdef52);
        assert_eq!(value_array[1], 0x52);
        assert_eq!(value_array[2], 0xef);
        assert_eq!(value_array[3], 0xcd);
        assert_eq!(value_array[4], 0xab);
        assert_eq!(value_array[5], 0x78);
        assert_eq!(value_array[6], 0x56);
        assert_eq!(value_array[7], 0x34);
        assert_eq!(value_array[8], 0x12);
    }

    #[test]
    fn test_get_key_val() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        super::append_record(&mut page, "foobar".as_bytes(), 0x12345678abcdef);
        super::append_record(&mut page, "zzzz".as_bytes(), 0xfedbca87654321);
        sanity_check_node(&page);
        assert_eq!(super::get_num_records(&page), 2);

        let key_bytes0 = super::get_record_key(&page, 0);
        assert_eq!(key_bytes0, "foobar".as_bytes());
        assert_eq!(super::get_record_value(&page, 0), 0x12345678abcdef);

        let key_bytes1 = super::get_record_key(&page, 1);
        assert_eq!(key_bytes1, "zzzz".as_bytes());
        assert_eq!(super::get_record_value(&page, 1), 0xfedbca87654321);
    }

    #[test]
    fn test_find() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        super::append_record(&mut page, "abacus".as_bytes(), 0);
        super::append_record(&mut page, "banana".as_bytes(), 0);
        super::append_record(&mut page, "beta".as_bytes(), 0);
        super::append_record(&mut page, "zebra".as_bytes(), 0);
        sanity_check_node(&page);
        assert_eq!(super::get_num_records(&page), 4);

        assert_eq!(super::find_key(&page, "aardvark".as_bytes()), 0); // Before first key
        assert_eq!(super::find_key(&page, "banana".as_bytes()), 1); // equal to second key
        assert_eq!(super::find_key(&page, "bananb".as_bytes()), 2); // slightly larger than second key
        assert_eq!(super::find_key(&page, "betas".as_bytes()), 3); // longer than third key
        assert_eq!(super::find_key(&page, "zzzzz".as_bytes()), 4); // higer than highest key
    }

    #[test]
    #[should_panic]
    fn test_append_full() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);
        for _ in 0..4096 {
            super::append_record(&mut page, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), 0);
        }
    }

    #[test]
    fn test_find_key_empty() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        assert_eq!(super::find_key(&page, "foo".as_bytes()), 0);
    }

    #[test]
    fn test_get_free_space() {
        let mut page: [u8; 4096] = [0; 4096];
        super::set_u16(&mut page, super::RECORD_START_FIELD_OFFS, 2048);
        super::set_u16(&mut page, super::NUM_SLOTS_FIELD_OFFS, 10);

        assert_eq!(super::get_page_free_space(&page), 2006);
    }

    #[test]
    fn test_insert_record() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        // Note these are out of order
        super::insert_record(&mut page, "aardvark".as_bytes(), 1000);
        super::insert_record(&mut page, "zebra".as_bytes(), 4000);
        super::insert_record(&mut page, "apple".as_bytes(), 2000);
        super::insert_record(&mut page, "banana".as_bytes(), 3000);
        sanity_check_node(&page);
        assert_eq!(super::get_num_records(&page), 4);

        assert_eq!(super::find_key(&page, "aardvark".as_bytes()), 0);
        assert_eq!(super::find_key(&page, "apple".as_bytes()), 1);
        assert_eq!(super::find_key(&page, "banana".as_bytes()), 2);
        assert_eq!(super::find_key(&page, "zebra".as_bytes()), 3);
    }

    #[test]
    #[should_panic]
    fn test_insert_record_full() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);
        for _ in 0..4096 {
            super::insert_record(&mut page, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".as_bytes(), 0);
        }
    }

    #[test]
    fn test_split_node() {
        let mut page1: [u8; 4096] = [0; 4096];
        let mut page2: [u8; 4096] = [0; 4096];
        let mut page3: [u8; 4096] = [0; 4096];

        super::init_page(&mut page1);
        const PAGE1_RECORDS: usize = 25;
        for i in 0..PAGE1_RECORDS {
            let key = vec![b'A' + i as u8; 128];
            super::insert_record(&mut page1, &key, i as u64);
        }

        sanity_check_node(&page1);

        super::split_node(&page1, &mut page2, &mut page3);
        sanity_check_node(&page2);
        sanity_check_node(&page3);

        // Ensure all records are present and in order
        let page2_recs = super::get_num_records(&page2);
        assert_eq!(super::get_num_records(&page1),
            page2_recs + super::get_num_records(&page3));
        assert_lt!(page2_recs, PAGE1_RECORDS * 2 / 3);
        for i in 0..super::get_num_records(&page1) {
            let orig_record = super::get_record_key(&page1, i);
            if i >= page2_recs {
                assert_eq!(orig_record, super::get_record_key(&page3, i - page2_recs));
            } else {
                assert_eq!(orig_record, super::get_record_key(&page2, i));
            }
        }
    }

    #[test]
    fn test_delete_record() {
        let mut page: [u8; 4096] = [0; 4096];
        super::init_page(&mut page);

        // Note these are out of order
        super::insert_record(&mut page, "aardvark".as_bytes(), 1000);
        super::insert_record(&mut page, "apple".as_bytes(), 2000);
        super::insert_record(&mut page, "banana".as_bytes(), 3000);
        super::insert_record(&mut page, "zebra".as_bytes(), 4000);
        sanity_check_node(&page);
        assert_eq!(super::get_num_records(&page), 4);

        // Remove from middle (apple)
        super::delete_record(&mut page, 1);
        assert_eq!(super::get_num_records(&page), 3);
        sanity_check_node(&page);

        assert_eq!(super::get_record_key(&page, 0), "aardvark".as_bytes());
        assert_eq!(super::get_record_value(&page, 0), 1000);
        assert_eq!(super::get_record_key(&page, 1), "banana".as_bytes());
        assert_eq!(super::get_record_value(&page, 1), 3000);
        assert_eq!(super::get_record_key(&page, 2), "zebra".as_bytes());
        assert_eq!(super::get_record_value(&page, 2), 4000);

        // Remove first entry (aardvark)
        super::delete_record(&mut page, 0);
        assert_eq!(super::get_num_records(&page), 2);
        sanity_check_node(&page);
        assert_eq!(super::get_record_key(&page, 0), "banana".as_bytes());
        assert_eq!(super::get_record_value(&page, 0), 3000);
        assert_eq!(super::get_record_key(&page, 1), "zebra".as_bytes());
        assert_eq!(super::get_record_value(&page, 1), 4000);

        // Remove last entry (zebra)
        super::delete_record(&mut page, 1);
        assert_eq!(super::get_num_records(&page), 1);
        sanity_check_node(&page);
        assert_eq!(super::get_record_key(&page, 0), "banana".as_bytes());
        assert_eq!(super::get_record_value(&page, 0), 3000);

        // Remove only remaining entry
        super::delete_record(&mut page, 0);
        sanity_check_node(&page);
        assert_eq!(super::get_num_records(&page), 0);
    }
}
