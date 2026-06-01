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

// This is a set of utility functions for maintaining an array of variable
// sized entries packed into a fixed size page. It's the underlying storage
// format for BTree nodes.

use crate::util::*;
use crate::page_cache::{PageData, PAGE_SIZE};

// header [u8; 32] (used by btree, opaque to this module)
// data array start offset: u16
// num_entries: u16
// offsets table: [(offset: u16, length: u16); num_entries]
//  |
//  V
//  unused
//  ^
//  |
// data array
//
// The data array is always packed and at the end of the page, the offset
// table grows towards it (although the data array doesn't necessarily
// have to be in the same order as the offsets table).
//

const DATA_START_FIELD_OFFS: usize = 32;
const NUM_ENTRIES_FIELD_OFFS: usize = 34;
const OFFSETS_LOC: usize = 36;
const OFFSETS_ENTRY_SIZE: usize = 4;

pub fn init_vararray(page: &mut PageData) {
    page.fill(0);
    set_u16(&mut page[..], DATA_START_FIELD_OFFS, PAGE_SIZE as u16);
}

pub fn get_num_vararray_entries(page: &PageData) -> usize {
    get_u16(page, NUM_ENTRIES_FIELD_OFFS) as usize
}

pub fn get_vararray_free_space(page: &PageData) -> usize {
    let offsets_end = OFFSETS_LOC + get_num_vararray_entries(page) * OFFSETS_ENTRY_SIZE;
    let data_start = get_u16(page, DATA_START_FIELD_OFFS) as usize;

    data_start - offsets_end
}

pub fn insert_vararray_entry(page: &mut PageData, index: usize, value: &[u8]) {
    assert!(get_vararray_free_space(page) >= value.len() + OFFSETS_ENTRY_SIZE,
        "Insufficient space to insert");

    let num_entries = get_num_vararray_entries(page);
    assert!(index <= num_entries, "Insert index out of range");

    if index < num_entries {
        // Move all offsets to make room
        let offsets_start = OFFSETS_LOC + index * OFFSETS_ENTRY_SIZE;
        let offsets_end = OFFSETS_LOC + num_entries * OFFSETS_ENTRY_SIZE;
        page.copy_within(offsets_start..offsets_end, offsets_start
            + OFFSETS_ENTRY_SIZE);
    }

    // Fill in the offsets table and update header
    let new_entry_offs = get_u16(page, DATA_START_FIELD_OFFS) as usize - value.len();
    set_u16(&mut page[..], NUM_ENTRIES_FIELD_OFFS, num_entries as u16 + 1); // Increment number of entries.
    set_u16(&mut page[..], OFFSETS_LOC + index * OFFSETS_ENTRY_SIZE, new_entry_offs as u16); // data offset
    set_u16(&mut page[..], OFFSETS_LOC + index * OFFSETS_ENTRY_SIZE + 2, value.len() as u16); // length
    set_u16(&mut page[..], DATA_START_FIELD_OFFS, new_entry_offs as u16); // update start of data area

    // Fill in the entry
    page[new_entry_offs..new_entry_offs + value.len()].copy_from_slice(value);
}

pub fn delete_vararray_entry(page: &mut PageData, index: usize) {
    let total_entries = get_num_vararray_entries(page);
    assert!(index < total_entries, "Invalid deletion index");

    let deleted_entry_offs = get_u16(page, OFFSETS_LOC + index
        * OFFSETS_ENTRY_SIZE) as usize;
    let deleted_entry_len = get_u16(page, OFFSETS_LOC + index
        * OFFSETS_ENTRY_SIZE + 2) as usize;

    // Remove this index entry and slide the other ones up to take the place
    let index_offs = OFFSETS_LOC + index * OFFSETS_ENTRY_SIZE;
    page.copy_within(index_offs + OFFSETS_ENTRY_SIZE..OFFSETS_LOC
        + total_entries * OFFSETS_ENTRY_SIZE, index_offs);
    set_u16(&mut page[..], NUM_ENTRIES_FIELD_OFFS, total_entries as u16 - 1);

    // Now move all the entry data down so there are no gaps
    let old_data_start = get_u16(page, DATA_START_FIELD_OFFS) as usize;
    page.copy_within(old_data_start..deleted_entry_offs,
        old_data_start + deleted_entry_len);

    // Walk through the remaining index, adjust offsets of anything that was before
    // the deleted entry.
    for i in 0..total_entries - 1 {
        let old_offs = get_u16(page, OFFSETS_LOC + i * OFFSETS_ENTRY_SIZE) as usize;
        if old_offs < deleted_entry_offs {
            set_u16(&mut page[..], OFFSETS_LOC + i * OFFSETS_ENTRY_SIZE,
                (old_offs + deleted_entry_len) as u16);
        }
    }

    // Adjust the new start of data
    set_u16(&mut page[..], DATA_START_FIELD_OFFS, (old_data_start
        + deleted_entry_len) as u16);
}

pub fn get_vararray_entry(page: &PageData, index: usize) -> &[u8] {
    assert!(index < get_num_vararray_entries(page),
        "Record index out of range");

    let offset = get_u16(page, OFFSETS_LOC + index
        * OFFSETS_ENTRY_SIZE) as usize;
    let length = get_u16(page, OFFSETS_LOC + index
        * OFFSETS_ENTRY_SIZE + 2) as usize;
    &page[offset..offset + length]
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};
    use rand::rngs::{SmallRng};
    use rand::{SeedableRng, RngExt};
    use super::*;

    fn sanity_check_record_array(page: &PageData) {
        let mut sorted_rec_offs: Vec<(usize, usize)> = Vec::new();

        // Walk through the entries, put offss into a list.
        let num_entries = get_num_vararray_entries(page);
        let data_start_offs = get_u16(page, DATA_START_FIELD_OFFS) as usize;
        if num_entries == 0 {
            // Ensure first offs in header is correct
            assert_eq!(PAGE_SIZE, data_start_offs, "First offset field is incorrect");
            return;
        }

        for i in 0..num_entries {
            let entry_offs = get_u16(page, OFFSETS_LOC + i * OFFSETS_ENTRY_SIZE) as usize;
            let entry_len = get_u16(page, OFFSETS_LOC + i * OFFSETS_ENTRY_SIZE + 2) as usize;
            assert_lt!(entry_offs, PAGE_SIZE, "Record offset out of range");
            sorted_rec_offs.push((entry_offs, entry_len));
        }

        // The entrie offset aren't necessarily in in order in the page, but put them
        // in order for our test.
        sorted_rec_offs.sort();

        // Ensure first offs in header is correct
        assert_eq!(sorted_rec_offs[0].0, data_start_offs,
            "First record offset is incorrect");

        // Now ensure the entry are packed end-to-end, the lengths are in
        // the page.
        let mut last_entry_end = data_start_offs;
        for (entry_start, entry_len) in sorted_rec_offs {
            assert_eq!(entry_start, last_entry_end, "Entries are not packed"); // ensure non-overlapping
            last_entry_end = entry_start + entry_len;
            assert_le!(last_entry_end, PAGE_SIZE, "Entry length out of bounds"); // Ensure it doesn't spill off page
        }

        assert_eq!(last_entry_end, PAGE_SIZE, "Gap at end of records");
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "First offset field is incorrect"]
    fn test_sanity_check_bad_offs() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        page[DATA_START_FIELD_OFFS] += 1; // Adjust start of data field

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "Record offset out of range"]
    fn test_sanity_check_offset_out_of_range() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        insert_vararray_entry(&mut page, 0, "aaaaa".as_bytes());
        insert_vararray_entry(&mut page, 1, "bbbbb".as_bytes());
        set_u16(&mut page, OFFSETS_LOC + OFFSETS_ENTRY_SIZE, PAGE_SIZE as u16 + 1);

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "First record offset is incorrect"]
    fn test_sanity_incorrect_record_offset() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        insert_vararray_entry(&mut page, 0, "a".as_bytes());
        insert_vararray_entry(&mut page, 1, "z".as_bytes());

        page[DATA_START_FIELD_OFFS] += 1; // Adjust start of data field

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "Entries are not packed"]
    fn test_sanity_check_overlapping_entry() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        insert_vararray_entry(&mut page, 0, "a".as_bytes());
        insert_vararray_entry(&mut page, 1, "z".as_bytes());

        page[OFFSETS_LOC + OFFSETS_ENTRY_SIZE + 2] += 1; // Increase the length by one byte

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_after() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_vararray_entry(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_vararray_entry(&mut page, 1, &record2);

        assert_eq!(record1, get_vararray_entry(&page, 0));
        assert_eq!(record2, get_vararray_entry(&page, 1));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_before() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_vararray_entry(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_vararray_entry(&mut page, 0, &record2);

        assert_eq!(record2, get_vararray_entry(&page, 0));
        assert_eq!(record1, get_vararray_entry(&page, 1));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_mid() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        let record0 = "aaaa".as_bytes();
        insert_vararray_entry(&mut page, 0, &record0);

        let record1 = "bbbbb".as_bytes();
        insert_vararray_entry(&mut page, 0, &record1);

        let record2 = "ccc".as_bytes();
        insert_vararray_entry(&mut page, 1, &record2);

        assert_eq!(record1, get_vararray_entry(&page, 0));
        assert_eq!(record2, get_vararray_entry(&page, 1));
        assert_eq!(record0, get_vararray_entry(&page, 2));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_free_space() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        let record1 = "aaaa".as_bytes();
        let init_free_space = get_vararray_free_space(&page);
        insert_vararray_entry(&mut page, 0, &record1);
        assert_eq!(get_vararray_free_space(&page), init_free_space - record1.len() - OFFSETS_ENTRY_SIZE);

        let record2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let init_free_space = get_vararray_free_space(&page);
        insert_vararray_entry(&mut page, 1, &record2);
        assert_eq!(get_vararray_free_space(&page), init_free_space - record2.len() - OFFSETS_ENTRY_SIZE);

        sanity_check_record_array(&page);
    }

    #[test]
    #[should_panic = "Insert index out of range"]
    fn test_insert_invalid_index() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_vararray_entry(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_vararray_entry(&mut page, 1, &record2);

        let record3 = "ccc".as_bytes();
        insert_vararray_entry(&mut page, 3, &record3);

        sanity_check_record_array(&page);
    }

    #[test]
    #[should_panic = "Insufficient space to insert"]
    fn test_out_of_space() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);

        for _ in 0..1024 {
            insert_vararray_entry(&mut page, 0, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".as_bytes());
        }
    }

    #[test]
    fn test_delete_first() {
        // note entries are out of order
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        insert_vararray_entry(&mut page, 0, "apple".as_bytes());
        insert_vararray_entry(&mut page, 0, "aardvark".as_bytes());
        insert_vararray_entry(&mut page, 2, "banana".as_bytes());
        insert_vararray_entry(&mut page, 3, "zebra".as_bytes());

        // Remove first entry (aardvark)
        delete_vararray_entry(&mut page, 0);
        assert_eq!(get_num_vararray_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_vararray_entry(&page, 0), "apple".as_bytes());
        assert_eq!(get_vararray_entry(&page, 1), "banana".as_bytes());
        assert_eq!(get_vararray_entry(&page, 2), "zebra".as_bytes());
    }

    #[test]
    fn test_delete_middle() {
        // note entries are out of order
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        insert_vararray_entry(&mut page, 0, "apple".as_bytes());
        insert_vararray_entry(&mut page, 0, "aardvark".as_bytes());
        insert_vararray_entry(&mut page, 2, "banana".as_bytes());
        insert_vararray_entry(&mut page, 3, "zebra".as_bytes());

        // Remove from middle (apple)
        delete_vararray_entry(&mut page, 1);
        assert_eq!(get_num_vararray_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_vararray_entry(&page, 0), "aardvark".as_bytes());
        assert_eq!(get_vararray_entry(&page, 1), "banana".as_bytes());
        assert_eq!(get_vararray_entry(&page, 2), "zebra".as_bytes());
    }

    #[test]
    fn test_delete_last() {
        // Remove last entry (zebra)
        // note entries are out of order
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        insert_vararray_entry(&mut page, 0, "apple".as_bytes());
        insert_vararray_entry(&mut page, 0, "aardvark".as_bytes());
        insert_vararray_entry(&mut page, 2, "banana".as_bytes());
        insert_vararray_entry(&mut page, 3, "zebra".as_bytes());

        // Remove last entry (zebra)
        delete_vararray_entry(&mut page, 3);
        assert_eq!(get_num_vararray_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_vararray_entry(&page, 0), "aardvark".as_bytes());
        assert_eq!(get_vararray_entry(&page, 1), "apple".as_bytes());
        assert_eq!(get_vararray_entry(&page, 2), "banana".as_bytes());
    }

    #[test]
    fn test_remove_all() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        let capacity = get_vararray_free_space(&page);

        insert_vararray_entry(&mut page, 0, "apple".as_bytes());
        insert_vararray_entry(&mut page, 1, "aardvark".as_bytes());

        delete_vararray_entry(&mut page, 0);
        delete_vararray_entry(&mut page, 0);
        assert_eq!(get_num_vararray_entries(&page), 0);
        sanity_check_record_array(&page);

        assert_eq!(capacity, get_vararray_free_space(&page));
    }

    #[test]
    #[should_panic = "Invalid deletion index"]
    fn test_delete_bad_index() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        insert_vararray_entry(&mut page, 0, "aardvark".as_bytes());
        delete_vararray_entry(&mut page, 1);
    }

    #[test]
    #[should_panic = "Record index out of range"]
    fn test_get_record_out_of_range() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_vararray(&mut page);
        insert_vararray_entry(&mut page, 0, "aardvark".as_bytes());
        get_vararray_entry(&mut page, 1);
    }

    fn random_value(rng: &mut impl RngExt) -> Vec<u8> {
        let len = rng.random_range(1..256);
        (0..len).map(|_| rng.random()).collect()
    }

    #[test]
    fn test_record_array_stress() {
        let seed: u64 = 0x12345;
        let mut rng = SmallRng::seed_from_u64(seed);
        let mut oracle: Vec<Vec<u8>> = Vec::new();
        let mut page: PageData = [0; PAGE_SIZE];
        let mut space_available: usize = 4000; // Rounded down a bit

        init_vararray(&mut page);
        for rep in 0..500 {
            if rng.random::<f64>() > 0.5 || rep < 20 {
                // Insert entry
                let record = random_value(&mut rng);
                let space_needed = record.len() + 4;
                let slot = rng.random_range(0..oracle.len() + 1);
                if space_needed <= space_available {
                    insert_vararray_entry(&mut page, slot, &record);
                    oracle.insert(slot, record.clone());
                    space_available -= space_needed;
                }
            } else if oracle.len() > 0 {
                // Delete entry
                let slot = rng.random_range(0..oracle.len());
                delete_vararray_entry(&mut page, slot);
                space_available += oracle[slot].len() + 4;
                oracle.remove(slot);
            }

            // Validate
            sanity_check_record_array(&page);
            for i in 0..oracle.len() {
                assert_eq!(get_vararray_entry(&page, i), &oracle[i]);
            }
        }
    }
}
