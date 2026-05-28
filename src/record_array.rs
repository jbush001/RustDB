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
// sized records on a fixed sized page. It's used for a number of different
// database structures.

use crate::util::*;

// HEADER [32 bytes, fixed]
// entry_start: u16
// num_entries: u16
// index array: [u16]
//  |
//  V
//  unused
//  ^
//  |
// entry array
// where each record starts with a 16-bit length, then the record contents.
//

const ENTRY_START_FIELD_OFFS: usize = 32;
const NUM_ENTRIES_FIELD_OFFS: usize = 34;
const INDEX_OFFS: usize = 36;

pub fn init_array(page: &mut [u8]) {
    page.fill(0);
    set_u16(page, ENTRY_START_FIELD_OFFS, page.len() as u16);
}

pub fn get_num_entries(page: &[u8]) -> usize {
    get_u16(page, NUM_ENTRIES_FIELD_OFFS) as usize
}

pub fn get_free_space(page: &[u8]) -> usize {
    let index_end = INDEX_OFFS + get_num_entries(page) * 2;
    let entry_start = get_u16(page, ENTRY_START_FIELD_OFFS) as usize;

    entry_start - index_end
}

pub fn insert_record(page: &mut [u8], index: usize, value: &[u8]) {
    assert!(get_free_space(page) >= value.len(), "Insufficient space to insert");

    let num_recs = get_num_entries(page);
    assert!(index <= num_recs, "Insert index out of range");

    if index < num_recs {
        // Move all index slots to make room
        let slot_start = INDEX_OFFS + index * 2;
        let slot_end = INDEX_OFFS + num_recs * 2;
        page.copy_within(slot_start..slot_end, slot_start + 2);
    }

    // Fill in the slot index and update header
    let new_entry_len = value.len() + 2; // Add two bytes for length (note, length field is self-inclusive)
    let new_entry_offs = get_u16(page, ENTRY_START_FIELD_OFFS) as usize - new_entry_len;
    set_u16(page, NUM_ENTRIES_FIELD_OFFS, num_recs as u16 + 1); // Increment number of used slots.
    set_u16(page, INDEX_OFFS + index * 2, new_entry_offs as u16); // Set entry offs
    set_u16(page, ENTRY_START_FIELD_OFFS, new_entry_offs as u16); // update pointer to data area

    // Fill in the entry
    set_u16(page, new_entry_offs, new_entry_len as u16);
    page[new_entry_offs + 2..new_entry_offs + 2 + value.len()].copy_from_slice(value);
}

pub fn delete_record(page: &mut [u8], index: usize) {
    let total_recs = get_num_entries(page);
    assert!(index < total_recs, "Invalid deletion index");

    let deleted_entry_offs = get_u16(page, INDEX_OFFS + index * 2) as usize;
    let deleted_entry_len = get_u16(page, deleted_entry_offs) as usize;

    // Remove this index entry and slide the other ones up to take the place
    let index_offs = INDEX_OFFS + index * 2;
    page.copy_within(index_offs + 2..INDEX_OFFS + total_recs * 2, index_offs);
    set_u16(page, NUM_ENTRIES_FIELD_OFFS, total_recs as u16 - 1);

    // Now move all the entrie data down so there are no gaps
    let old_entries_start = get_u16(page, ENTRY_START_FIELD_OFFS) as usize;
    page.copy_within(old_entries_start..deleted_entry_offs,
        old_entries_start + deleted_entry_len);

    // Walk through the remaining index, adjust ofss of anything that was before
    // the deleted entry.
    for i in 0..total_recs - 1 {
        let old_offs = get_u16(page, INDEX_OFFS + i * 2) as usize;
        if old_offs < deleted_entry_offs {
            set_u16(page, INDEX_OFFS + i * 2, (old_offs + deleted_entry_len) as u16);
        }
    }

    // Adjust the new start of entries
    set_u16(page, ENTRY_START_FIELD_OFFS, (old_entries_start + deleted_entry_len) as u16);
}

pub fn get_record(page: &[u8], index: usize) -> &[u8] {
    let num_recs = get_num_entries(page);
    assert!(index < num_recs, "Record index out of range");

    let offset = get_u16(page, INDEX_OFFS + index * 2) as usize;
    let length = get_u16(page, offset) as usize;
    &page[offset + 2..offset + length]
}

#[cfg(test)]
mod tests {
    use more_asserts::{assert_le, assert_lt};
    use rand::rngs::{SmallRng};
    use rand::{SeedableRng, RngExt};
    use super::*;

    fn sanity_check_record_array(page: &[u8]) {
        let mut sorted_rec_offs: Vec<usize> = Vec::new();

        // Walk through the entries, put offss into a list.
        let num_entries = get_num_entries(page);
        let header_first_offs = get_u16(page, ENTRY_START_FIELD_OFFS) as usize;
        if num_entries == 0 {
            // Ensure first offs in header is correct
            assert_eq!(page.len(), header_first_offs, "First offset field is incorrect");
            return;
        }

        for i in 0..num_entries {
            let rec_offs = get_u16(page, INDEX_OFFS + i * 2) as usize;
            assert_lt!(rec_offs, page.len(), "Record offset out of range");
            sorted_rec_offs.push(rec_offs);
        }

        // The entries don't have to be in order in the page, but put them
        // in order for our test.
        sorted_rec_offs.sort();

        // Ensure first offs in header is correct
        assert_eq!(sorted_rec_offs[0], header_first_offs, "First record offset is incorrect");

        // Now ensure the entry are packed end-to-end, the lengths are in
        // the page.
        let mut last_entry_end = header_first_offs;
        for rec_offs in sorted_rec_offs {
            assert_eq!(rec_offs, last_entry_end, "Entries are not packed"); // ensure non-overlapping
            last_entry_end = rec_offs + get_u16(page, rec_offs) as usize;
            assert_le!(last_entry_end, page.len(), "Entry length out of bounds"); // Ensure it doesn't spill off page
        }

        assert_eq!(last_entry_end, page.len(), "Gap at end of records");
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "First offset field is incorrect"]
    fn test_sanity_check_bad_offs() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        page[ENTRY_START_FIELD_OFFS] += 1; // Adjust start of data field

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "Record offset out of range"]
    fn test_sanity_check_offset_out_of_range() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        insert_record(&mut page, 0, "aaaaa".as_bytes());
        insert_record(&mut page, 1, "bbbbb".as_bytes());
        page[INDEX_OFFS + 3] = 17; // Set the high byte of the offset past the page

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "First record offset is incorrect"]
    fn test_sanity_incorrect_record_offset() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        insert_record(&mut page, 0, "a".as_bytes());
        insert_record(&mut page, 1, "z".as_bytes());

        page[ENTRY_START_FIELD_OFFS] += 1; // Adjust start of data field

        sanity_check_record_array(&page);
    }

    // Validate the sanity_check routine
    #[test]
    #[should_panic = "Entries are not packed"]
    fn test_sanity_check_overlapping_entry() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        insert_record(&mut page, 0, "a".as_bytes());
        insert_record(&mut page, 1, "z".as_bytes());

        let rec1_offs = get_u16(&page, INDEX_OFFS + 2) as usize; // second entry
        page[rec1_offs] += 1; // Increase the length by one byte

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_after() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_record(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_record(&mut page, 1, &record2);

        assert_eq!(record1, get_record(&page, 0));
        assert_eq!(record2, get_record(&page, 1));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_before() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_record(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_record(&mut page, 0, &record2);

        assert_eq!(record2, get_record(&page, 0));
        assert_eq!(record1, get_record(&page, 1));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_insert_mid() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        let record0 = "aaaa".as_bytes();
        insert_record(&mut page, 0, &record0);

        let record1 = "bbbbb".as_bytes();
        insert_record(&mut page, 0, &record1);

        let record2 = "ccc".as_bytes();
        insert_record(&mut page, 1, &record2);

        assert_eq!(record1, get_record(&page, 0));
        assert_eq!(record2, get_record(&page, 1));
        assert_eq!(record0, get_record(&page, 2));

        sanity_check_record_array(&page);
    }

    #[test]
    fn test_free_space() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        let record1 = "aaaa".as_bytes();
        let init_free_space = get_free_space(&page);
        insert_record(&mut page, 0, &record1);
        assert_eq!(get_free_space(&page), init_free_space - record1.len() - 4);

        let record2 = "abcdefghijklmnopqrstuvwxyz".as_bytes();
        let init_free_space = get_free_space(&page);
        insert_record(&mut page, 1, &record2);
        assert_eq!(get_free_space(&page), init_free_space - record2.len() - 4);

        sanity_check_record_array(&page);
    }

    #[test]
    #[should_panic = "Insert index out of range"]
    fn test_insert_invalid_index() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        let record1 = "aaaa".as_bytes();
        insert_record(&mut page, 0, &record1);

        let record2 = "bbbbb".as_bytes();
        insert_record(&mut page, 1, &record2);

        let record3 = "ccc".as_bytes();
        insert_record(&mut page, 3, &record3);

        sanity_check_record_array(&page);
    }

    #[test]
    #[should_panic = "Insufficient space to insert"]
    fn test_out_of_space() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);

        for _ in 0..1024 {
            insert_record(&mut page, 0, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".as_bytes());
        }
    }

    #[test]
    fn test_delete_first() {
        // note entries are out of order
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        insert_record(&mut page, 0, "apple".as_bytes());
        insert_record(&mut page, 0, "aardvark".as_bytes());
        insert_record(&mut page, 2, "banana".as_bytes());
        insert_record(&mut page, 3, "zebra".as_bytes());

        // Remove first entry (aardvark)
        delete_record(&mut page, 0);
        assert_eq!(get_num_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_record(&page, 0), "apple".as_bytes());
        assert_eq!(get_record(&page, 1), "banana".as_bytes());
        assert_eq!(get_record(&page, 2), "zebra".as_bytes());
    }

    #[test]
    fn test_delete_middle() {
        // note entries are out of order
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        insert_record(&mut page, 0, "apple".as_bytes());
        insert_record(&mut page, 0, "aardvark".as_bytes());
        insert_record(&mut page, 2, "banana".as_bytes());
        insert_record(&mut page, 3, "zebra".as_bytes());

        // Remove from middle (apple)
        delete_record(&mut page, 1);
        assert_eq!(get_num_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_record(&page, 0), "aardvark".as_bytes());
        assert_eq!(get_record(&page, 1), "banana".as_bytes());
        assert_eq!(get_record(&page, 2), "zebra".as_bytes());
    }

    #[test]
    fn test_delete_last() {
        // Remove last entry (zebra)
        // note entries are out of order
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        insert_record(&mut page, 0, "apple".as_bytes());
        insert_record(&mut page, 0, "aardvark".as_bytes());
        insert_record(&mut page, 2, "banana".as_bytes());
        insert_record(&mut page, 3, "zebra".as_bytes());

        // Remove last entry (zebra)
        delete_record(&mut page, 3);
        assert_eq!(get_num_entries(&page), 3);
        sanity_check_record_array(&page);

        assert_eq!(get_record(&page, 0), "aardvark".as_bytes());
        assert_eq!(get_record(&page, 1), "apple".as_bytes());
        assert_eq!(get_record(&page, 2), "banana".as_bytes());
    }

    #[test]
    fn test_remove_all() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        let capacity = get_free_space(&page);

        insert_record(&mut page, 0, "apple".as_bytes());
        insert_record(&mut page, 1, "aardvark".as_bytes());

        delete_record(&mut page, 0);
        delete_record(&mut page, 0);
        assert_eq!(get_num_entries(&page), 0);
        sanity_check_record_array(&page);

        assert_eq!(capacity, get_free_space(&page));
    }

    #[test]
    #[should_panic = "Invalid deletion index"]
    fn test_delete_bad_index() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        insert_record(&mut page, 0, "aardvark".as_bytes());
        delete_record(&mut page, 1);
    }

    #[test]
    #[should_panic = "Record index out of range"]
    fn test_get_record_out_of_range() {
        let mut page: [u8; 4096] = [0; 4096];
        init_array(&mut page);
        insert_record(&mut page, 0, "aardvark".as_bytes());
        get_record(&mut page, 1);
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
        let mut page: [u8; 4096] = [0; 4096];
        let mut space_available: usize = 4000; // Rounded down a bit

        init_array(&mut page);
        for rep in 0..500 {
            if rng.random::<f64>() > 0.5 || rep < 20 {
                // Insert entry
                let record = random_value(&mut rng);
                let space_needed = record.len() + 4;
                let slot = rng.random_range(0..oracle.len() + 1);
                if space_needed <= space_available {
                    insert_record(&mut page, slot, &record);
                    oracle.insert(slot, record.clone());
                    space_available -= space_needed;
                }
            } else if oracle.len() > 0 {
                // Delete entry
                let slot = rng.random_range(0..oracle.len());
                delete_record(&mut page, slot);
                space_available += oracle[slot].len() + 4;
                oracle.remove(slot);
            }

            // Validate
            sanity_check_record_array(&page);
            for i in 0..oracle.len() {
                assert_eq!(get_record(&page, i), &oracle[i]);
            }
        }
    }
}
