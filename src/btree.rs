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

// The BTree is an efficient key-value store that supports O(log n) lookup
// and ordered iteration. It is the base storage mechanism for all structured
// data. This implementation is variant called a B+ tree, distinguished by the
// fact that values are only stored in the leaf nodes. Keys must be unique in
// the BTree.
//
// The BTree is structured so that the root node file ID never changes even if
// it is split. The upper layers depend on this.
//
// Each entry is:
// key_length: u16
// key: variable length
// value: variable length
// (value length is inferred based on record length)

use crate::page_allocator::PageAllocator;
use crate::page_cache::*;
use crate::vararray::*;
use crate::util::*;

const HEADER_NEXT_SIB_OFFS: usize = 8;
const HEADER_PREV_SIB_OFFS: usize = 16;
const HEADER_RIGHT_CHILD_OFFS: usize = 24;

pub const MAX_RECORD_SIZE: usize = (PAGE_SIZE - 32) / 4 - 16; // I added a little padding for safety

const FLAG_LEAF: u8 = 1;

pub struct BTreeCursor {
    current_node_pnum: Option<PageNum>,
    current_index: usize,
    reverse: bool,
    page_cache: PageCache
}

impl Iterator for BTreeCursor {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        self.current_node_pnum?;

        let page = self.page_cache.lock_page(self.current_node_pnum.unwrap());
        let result = (
            get_entry_key(&page, self.current_index).to_vec(),
            get_entry_value(&page, self.current_index).to_vec()
        );

        if self.reverse {
            if self.current_index == 0 {
                self.current_node_pnum = get_prev_sib(&page);
                if let Some(cur) = self.current_node_pnum {
                    let page = self.page_cache.lock_page(cur);
                    self.current_index = get_num_vararray_entries(&page) - 1;
                }
            } else {
                self.current_index -= 1;
            }
        } else {
            self.current_index += 1;
            if self.current_index >= get_num_vararray_entries(&page) {
                self.current_node_pnum = get_next_sib(&page);
                self.current_index = 0;
            }
        }

        Some(result)
    }
}

pub struct BTree {
    root: PageNum
}

impl BTree {
    // Allocate a new page and initializes an on-disk btree
    pub fn create(page_cache: &PageCache, page_allocator: &mut PageAllocator) -> Self {
        let root = page_allocator.alloc();
        Self::create_at(page_cache, root)
    }

    pub fn create_at(page_cache: &PageCache, root: PageNum) -> Self {
        let mut page = page_cache.lock_page_mut(root);
        init_btree_node(&mut page);

        BTree { root }
    }

    // Create a btree wrapper for an existing btree
    pub fn open(root: PageNum) -> Self {
        BTree { root }
    }

    pub fn get_root_page_id(&self) -> PageNum {
        self.root
    }

    pub fn iterate(&self, reverse: bool, page_cache: &PageCache) -> BTreeCursor {
        let mut current_node_pnum = self.root;
        loop {
            let page = page_cache.lock_page(current_node_pnum);
            if is_leaf(&page) {
                let num_entries = get_num_vararray_entries(&page);
                if num_entries == 0 {
                    // Empty tree
                    assert!(current_node_pnum == self.root, "Empty page in tree");

                    // Return a stub cursor that will immediately return None
                    return BTreeCursor {
                        current_node_pnum: None,
                        current_index: 0,
                        reverse: false,
                        page_cache: page_cache.clone()
                    };
                }

                return BTreeCursor {
                    current_node_pnum: Some(current_node_pnum),
                    current_index: if reverse && num_entries > 0 { num_entries - 1 } else { 0 },
                    reverse,
                    page_cache: page_cache.clone()
                }
            }

            if reverse {
                current_node_pnum = get_right_child(&page).expect("Right child is null");
            } else if get_num_vararray_entries(&page) > 0 {
                current_node_pnum = PageNum::from_bytes(get_entry_value(&page, 0))
                    .expect("Invalid entry");
            } else {
                // After a deletion, it's possible to have interior nodes
                // that only have a right child.
                current_node_pnum = get_right_child(&page).expect("Right child is null");
            }
        }
    }

    pub fn find(&self, key: &[u8], reverse: bool, page_cache: &PageCache) -> BTreeCursor {
        let mut current_node_pnum = self.root;
        loop {
            let page = page_cache.lock_page(current_node_pnum);
            let index = find_key(&page, key);
            if is_leaf(&page) {
                if (reverse && index == 0) || (!reverse && index == get_num_vararray_entries(&page)) {
                    // Nothing to fetch, return stub cursor
                    return BTreeCursor {
                        current_node_pnum: None,
                        current_index: 0,
                        reverse,
                        page_cache: page_cache.clone()
                    }
                }

                return BTreeCursor {
                    current_node_pnum: Some(current_node_pnum),
                    current_index: if reverse { index - 1 } else { index },
                    reverse,
                    page_cache: page_cache.clone()
                }
            }

            if index == get_num_vararray_entries(&page) {
                current_node_pnum = get_right_child(&page).expect("Right child is null");
            } else {
                current_node_pnum = PageNum::from_bytes(get_entry_value(&page, index))
                    .expect("invalid entry");
            }
        }
    }

    fn find_with_path(&self,
        key: &[u8],
        page_cache: &PageCache) -> (Vec<(PageNum, usize)>, bool) {
        let mut current_node_pnum = self.root;
        let mut path: Vec<(PageNum, usize)> = Vec::new();

        let found = loop {
            let page = page_cache.lock_page(current_node_pnum);
            let index = find_key(&page, key);
            path.push((current_node_pnum, index));
            if is_leaf(&page) {
                break index < get_num_vararray_entries(&page)
                    && get_entry_key(&page, index) == key;
            }

            if index == get_num_vararray_entries(&page) {
                current_node_pnum = get_right_child(&page).expect("Right child is null");
            } else {
                current_node_pnum = PageNum::from_bytes(get_entry_value(&page, index))
                    .expect("value was not 8 bytes");
            }
        };

        (path, found)
    }

    pub fn insert(&self,
        key: &[u8],
        value: &[u8],
        page_cache: &PageCache,
        page_allocator: &mut PageAllocator)
    {
        assert!(key.len() + value.len() < MAX_RECORD_SIZE);

        let (path, found) = self.find_with_path(key, page_cache);
        assert!(!found);

        // We're now at a leaf. Insert and walk back up the tree splitting nodes
        // as needed.
        let mut insert_value = value.to_vec();
        let mut insert_key = key.to_vec();
        for (node_pnum, _) in path.iter().rev() {
            let mut page = page_cache.lock_page_mut(*node_pnum);
            if get_vararray_free_space(&page) >= get_entry_size(&insert_key, &insert_value) {
                insert_entry(&mut page, insert_key.as_slice(), insert_value.as_slice());
                break;
            }

            // Need to split...
            if *node_pnum == self.root {
                // Split the root node. This is the only place where the tree grows.
                // The same page number will continue to be the root, since it is
                // referenced in other places.
                let new_page_pnum1 = page_allocator.alloc();
                let new_page_pnum2 = page_allocator.alloc();

                let mut new_page1 = page_cache.lock_page_mut(new_page_pnum1);
                let mut new_page2 = page_cache.lock_page_mut(new_page_pnum2);
                let split_key = split_node(&page, &mut new_page1, &mut new_page2);

                // The root node can be a leaf if the number of entries is small. If so,
                // need to fix the linked list of nodes.
                if is_leaf(&page) {
                    set_next_sib(&mut new_page1, Some(new_page_pnum2));
                    set_prev_sib(&mut new_page2, Some(new_page_pnum1));
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

                // Reinitialize the root. We've created two new pages and copied half
                // of the roots entries into each of them. Now one of them will be the
                // "right_child" member, the other gets an entry with a key.
                init_btree_node(&mut page);
                set_not_leaf(&mut page); // If the root was a leaf, it is not now.
                append_entry(&mut page, &split_key, &new_page_pnum1.to_bytes());
                set_right_child(&mut page, Some(new_page_pnum2));
                break;
            } else {
                // Split leaf or interior page.
                let new_page_pnum = page_allocator.alloc();
                let mut temp: PageData = [0; PAGE_SIZE];
                let mut new_page = page_cache.lock_page_mut(new_page_pnum);
                let new_parent_key = split_node(&page, &mut new_page, &mut temp);

                // We will allocate a new page to be *before* this page. Temp is a holding
                // area for what will be copied back to this page.

                if is_leaf(&page) {
                    set_prev_sib(&mut temp, Some(new_page_pnum));
                    set_next_sib(&mut temp, get_next_sib(&page));
                    set_prev_sib(&mut new_page, get_prev_sib(&page));
                    set_next_sib(&mut new_page, Some(*node_pnum));

                    // Need to fix forward link
                    if let Some(prev) = get_prev_sib(&page) {
                        let mut prev_sib_page = page_cache.lock_page_mut(prev);
                        set_next_sib(&mut prev_sib_page, Some(new_page_pnum));
                    }
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
                insert_value = new_page_pnum.to_bytes().to_vec();
            }
        }
    }

    // Here be dragons. delete is one of the most complex functions, with tons of edge
    // cases.
    pub fn delete(&self, key: &[u8], page_cache: &PageCache, allocator: &mut PageAllocator) {
        let (path, found) = self.find_with_path(key, page_cache);
        if !found {
            println!("btree_delete: warning, key not found");
            return;
        }

        for (page_num, index) in path.iter().rev() {
            let mut page = page_cache.lock_page_mut(*page_num);
            let num_entries = get_num_vararray_entries(&page);
            if !is_leaf(&page) && *index == num_entries {
                // Need to remove right child. Remove it and set the next highest
                // entry in the node to be the new right child (if possible)
                if num_entries > 0 {
                    let nu_right = PageNum::from_bytes(get_entry_value(
                        &page, num_entries - 1)).expect("invalid entry");
                    set_right_child(&mut page, Some(nu_right));
                    delete_vararray_entry(&mut page, num_entries - 1);
                } else {
                    set_right_child(&mut page, None);
                }

                // Otherwise this node is truly empty. We will continue up
                // the stack deleting.

                if *page_num == self.root {
                    // If the root is now empty, turn it back into a leaf.
                    set_leaf(&mut page);
                    break;
                }
            } else {
                delete_vararray_entry(&mut page, *index);
            }

            if *page_num == self.root {
                // Never free root.
                return;
            }

            // If an interior node only has one entry, we could just propagate
            // its key up to its parent. It's a bit trickier when it is the
            // right child, since we don't know the key here. As such, we don't
            // do that for simplicity.
            if get_num_vararray_entries(&page) != 0
                || (!is_leaf(&page) && get_right_child(&page).is_some()) {
                break; // Is not empty, we are done for now.
            }

            // This page ie empty. Delete the page itself, then the next loop
            // iteration will remove its entry from its parent.
            if is_leaf(&page) {
                // Remove from the linked list
                if let Some(prev) = get_prev_sib(&page) {
                    let mut prev_page = page_cache.lock_page_mut(prev);
                    set_next_sib(&mut prev_page, get_next_sib(&page));
                }

                if let Some(next) = get_next_sib(&page) {
                    let mut next_page = page_cache.lock_page_mut(next);
                    set_prev_sib(&mut next_page, get_prev_sib(&page));
                }
            }

            allocator.free(*page_num);
        }
    }

    fn print(&self, page_cache: &PageCache) {
        let mut fifo: Vec<PageNum> = Vec::new();
        fifo.push(self.root);
        while !fifo.is_empty() {
            let page_num = fifo.remove(0);
            let page = page_cache.lock_page(page_num);
            println!("Node page_num {:?} is_leaf {} prev_sib {:?} next_sib {:?} right_child {:?}",
                page_num, is_leaf(&page), get_prev_sib(&page), get_next_sib(&page), get_right_child(&page));

            if is_leaf(&page) {
                for i in 0..get_num_vararray_entries(&page) {
                    println!("{}. {} value {}", i,
                        to_hex_string(get_entry_key(&page, i), 16),
                        to_hex_string(get_entry_value(&page, i), 16));
                }
            } else {
                for i in 0..get_num_vararray_entries(&page) {
                    let child_pnum = PageNum::from_bytes(get_entry_value(&page, i))
                        .expect("Invalid entry");
                    println!("{}. {} child page {:?}", i,
                        to_hex_string(get_entry_key(&page, i), 16), child_pnum);
                    fifo.push(child_pnum);
                }

                if let Some(child) = get_right_child(&page) {
                    fifo.push(child);
                }
            }
        }
    }
}

// Create an empty node
pub fn init_btree_node(page: &mut PageData) {
    init_vararray(page);
    page[0] = FLAG_LEAF;
    set_next_sib(page, None);
    set_prev_sib(page, None);
    set_right_child(page, None);
}

fn is_leaf(page: &PageData) -> bool {
    (page[0] & FLAG_LEAF) != 0
}

fn set_leaf(page: &mut PageData) {
    page[0] |= FLAG_LEAF;
}

fn set_not_leaf(page: &mut PageData) {
    page[0] &= !FLAG_LEAF;
}

fn get_next_sib(page: &PageData) -> Option<PageNum> {
    PageNum::from_bytes(page.u64_field(HEADER_NEXT_SIB_OFFS))
}

fn set_next_sib(page: &mut PageData, page_num: Option<PageNum>) {
    *page.u64_field_mut(HEADER_NEXT_SIB_OFFS) = page_num.to_bytes();
}

fn get_prev_sib(page: &PageData) -> Option<PageNum> {
    PageNum::from_bytes(page.u64_field(HEADER_PREV_SIB_OFFS))
}

fn set_prev_sib(page: &mut PageData, page_num: Option<PageNum>) {
    *page.u64_field_mut(HEADER_PREV_SIB_OFFS) = page_num.to_bytes();
}

fn get_right_child(page: &PageData) -> Option<PageNum> {
    PageNum::from_bytes(page.u64_field(HEADER_RIGHT_CHILD_OFFS))
}

fn set_right_child(page: &mut PageData, page_num: Option<PageNum>) {
    *page.u64_field_mut(HEADER_RIGHT_CHILD_OFFS) = page_num.to_bytes();
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
    assert!(key_len + 2 <= rec.len(),
        "Invalid key length, exceeds record length");
    &rec[2..2 + key_len]
}

fn get_entry_value(page: &PageData, rec_num: usize) -> &[u8] {
    let rec = get_vararray_entry(page, rec_num);
    let key_len = get_u16(rec, 0) as usize;
    assert!(key_len + 2 <= rec.len(),
        "Invalid key length, exceeds record length");
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
    assert!(!key.is_empty(), "Find with empty key");

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
    assert!(key.len() + value.len() < MAX_RECORD_SIZE);

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
// NOTE: The caller must set the right_sibling in the returned out2 to the
// page_num of out1 (we don't know it here)
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
        set_right_child(out1, PageNum::from_bytes(get_entry_value(orig, orig_index)));
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
    use crate::mocks::{MockPersistentStore};
    use crate::page_allocator::*;
    use crate::page_cache::*;
    use crate::superblock::*;
    use more_asserts::{assert_le, assert_lt, assert_gt};
    use rand::rngs::SmallRng;
    use rand::{SeedableRng, RngExt};
    use rand::seq::SliceRandom;
    use std::cell::RefCell;
    use std::rc::Rc;
    use super::*;

    // The length of this key is important to ensure tests like
    // delete_all create enough layers.
    fn gen_key_for_index(index: usize) -> Vec<u8> {
        let mut key = index.to_be_bytes().to_vec();
        key.extend_from_slice(&[0u8].repeat(256));
        key
    }

    fn create_test_btree() -> (PageCache, PageAllocator, BTree) {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        let mut page_cache = PageCache::new(50, Rc::clone(&mock_io));
        let _transaction = page_cache.begin_transaction();

        {
            let mut page = page_cache.lock_page_mut(SUPERBLOCK_FPID);
            init_superblock(&mut page);
        }

        let mut allocator = PageAllocator::new(&mut page_cache);
        let tree = BTree::create(&page_cache, &mut allocator);

        (page_cache, allocator, tree)
    }

    fn populate_test_btree(count: usize) -> (PageCache, PageAllocator, BTree) {
        let (page_cache, mut allocator, tree) = create_test_btree();

        let mut test_sequence: Vec<usize> = (0..count).collect();
        test_sequence.shuffle(&mut SmallRng::seed_from_u64(0xc0fc47a65d406179));
        for i in test_sequence {
            let _transaction = page_cache.begin_transaction();
            tree.insert(&gen_key_for_index(i), &(i as u64).to_le_bytes(), &page_cache, &mut allocator);
        }

        (page_cache, allocator, tree)
    }

    fn validate_node(page: &PageData) {
        // Ensure the keys are in order
        let mut last_key: &[u8] = &[0];
        for i in 0..get_num_vararray_entries(page) {
            let this_key = get_entry_key(page, i);
            assert_le!(last_key, this_key, "keys are out of order");
            last_key = this_key;
        }
    }

    // TODO: could verify leaf nodes are all properly linked. This is implicitly done when
    // tests iterate the whole tree already.
    // TODO: this doesn't check for loops. The test will hang if such exists, so it's not a
    // silent failure anyway, this would just give more diagnostics.
    fn validate_btree(root: &BTree, page_cache: &PageCache) {
        fn walk_tree(page_num: PageNum, page_cache: &PageCache, low_key: Option<&[u8]>,
            high_key: Option<&[u8]>, is_root: bool,
            link_info: &mut Vec<(PageNum, Option<PageNum>, Option<PageNum>)>) {
            let page = page_cache.lock_page(page_num);
            let num_entries = get_num_vararray_entries(&page);
            if is_root && num_entries == 0 {
                return;
            }

            assert_gt!(num_entries, 0, "Empty page {:?} in tree", page_num);
            validate_node(&page);
            if let Some(low) = low_key {
                assert_gt!(get_entry_key(&page, 0), low, "Low key on page {:?} overlaps previous sibling",
                    page_num);
            }

            if let Some(high) = high_key {
                assert_le!(get_entry_key(&page, num_entries - 1), high, "High key on page {:?} past parent key",
                    page_num);
            }

            if is_leaf(&page) {
                link_info.push((page_num, get_prev_sib(&page), get_next_sib(&page)));
            } else {
                assert!(get_right_child(&page).is_some(), "Right child is null");

                for i in 0..num_entries {
                    let child_page_num = PageNum::from_bytes(get_entry_value(&page, i))
                        .expect("Invalid entry");
                    let low_key = if i > 0 { Some(get_entry_key(&page, i - 1)) } else { None };
                    let high_key = Some(get_entry_key(&page, i));

                    walk_tree(child_page_num, page_cache, low_key, high_key, false, link_info);
                }

                walk_tree(get_right_child(&page).expect("Invalid right child"), page_cache,
                    Some(get_entry_key(&page, num_entries - 1)), None, false, link_info);
            }
        }

        let mut link_info: Vec<(PageNum, Option<PageNum>, Option<PageNum>)> = Vec::new();
        walk_tree(root.root, page_cache, None, None, true, &mut link_info);

        if !link_info.is_empty() {
            assert_eq!(link_info[0].1, None, "Bad prev pointer"); // Prev of first element is null
            assert_eq!(link_info[link_info.len() - 1].2, None, "Bad next pointer"); // Next of last element is null.
            for node in link_info.windows(2) {
                assert_eq!(node[0].2, Some(node[1].0), "Bad next pointer"); // first.next = second
                assert_eq!(node[1].1, Some(node[0].0), "Bad prev pointer"); // second.prev = first
            }
        }
    }

    #[test]
    fn test_validate_empty() {
        let (page_cache, _allocator, tree) = create_test_btree();
        validate_btree(&tree, &page_cache);
    }

    #[test]
    #[should_panic = "High key on page PageNum(13) past parent key"]
    fn test_validate_bad_order1() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            // Lock a page and alter the leaf key so it is larger than its parent.
            let mut root_page = page_cache.lock_page_mut(tree.root);
            let child_page1_num = allocator.alloc();
            let mut child_page1 = page_cache.lock_page_mut(child_page1_num);
            let child_page2_num = allocator.alloc();
            let mut child_page2 = page_cache.lock_page_mut(child_page2_num);

            init_btree_node(&mut child_page1);
            init_btree_node(&mut child_page2);
            set_not_leaf(&mut root_page);
            set_next_sib(&mut child_page1, Some(child_page2_num));
            set_prev_sib(&mut child_page2, Some(child_page1_num));

            append_entry(&mut root_page, b"banana", &child_page1_num.to_bytes());
            set_right_child(&mut root_page, Some(child_page2_num));

            append_entry(&mut child_page1, b"bbnana", b"foo"); // Err, past parent key
            append_entry(&mut child_page2, b"cabana", b"foo");
        }

        validate_btree(&tree, &page_cache);
    }

    #[test]
    #[should_panic = "Low key on page PageNum(14) overlaps previous sibling"]
    fn test_validate_bad_order2() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            // Lock a page and alter the leaf key so it is larger than its parent.
            let mut root_page = page_cache.lock_page_mut(tree.root);
            let child_page1_num = allocator.alloc();
            let mut child_page1 = page_cache.lock_page_mut(child_page1_num);
            let child_page2_num = allocator.alloc();
            let mut child_page2 = page_cache.lock_page_mut(child_page2_num);

            init_btree_node(&mut child_page1);
            init_btree_node(&mut child_page2);
            set_not_leaf(&mut root_page);
            set_next_sib(&mut child_page1, Some(child_page2_num));
            set_prev_sib(&mut child_page2, Some(child_page1_num));

            append_entry(&mut root_page, b"banana", &child_page1_num.to_bytes());
            set_right_child(&mut root_page, Some(child_page2_num));

            append_entry(&mut child_page1, b"abacus", b"foo");
            append_entry(&mut child_page2, b"aardvark", b"foo"); // Err, before last parent key
        }

        validate_btree(&tree, &page_cache);
    }

    #[test]
    #[should_panic = "Empty page PageNum(13) in tree"]
    fn test_validate_empty_node() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            // Lock a page and alter the leaf key so it is larger than its parent.
            let mut root_page = page_cache.lock_page_mut(tree.root);
            let child_page1_num = allocator.alloc();
            let mut child_page1 = page_cache.lock_page_mut(child_page1_num);
            let child_page2_num = allocator.alloc();
            let mut child_page2 = page_cache.lock_page_mut(child_page2_num);

            init_btree_node(&mut child_page1);
            init_btree_node(&mut child_page2);
            set_not_leaf(&mut root_page);
            set_next_sib(&mut child_page1, Some(child_page2_num));
            set_prev_sib(&mut child_page2, Some(child_page1_num));

            append_entry(&mut root_page, b"banana", &child_page1_num.to_bytes());
            set_right_child(&mut root_page, Some(child_page2_num));
        }

        validate_btree(&tree, &page_cache);
    }

    #[test]
    #[should_panic = "Bad next pointer"]
    fn test_validate_bad_next() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            // Lock a page and alter the leaf key so it is larger than its parent.
            let mut root_page = page_cache.lock_page_mut(tree.root);
            let child_page1_num = allocator.alloc();
            let mut child_page1 = page_cache.lock_page_mut(child_page1_num);
            let child_page2_num = allocator.alloc();
            let mut child_page2 = page_cache.lock_page_mut(child_page2_num);

            init_btree_node(&mut child_page1);
            init_btree_node(&mut child_page2);
            set_not_leaf(&mut root_page);
            set_prev_sib(&mut child_page2, Some(child_page1_num));

            append_entry(&mut root_page, b"banana", &child_page1_num.to_bytes());
            set_right_child(&mut root_page, Some(child_page2_num));

            append_entry(&mut child_page1, b"abacus", b"foo");
            append_entry(&mut child_page2, b"cencus", b"foo");
        }

        validate_btree(&tree, &page_cache);
    }

    #[test]
    #[should_panic = "Bad prev pointer"]
    fn test_validate_bad_prev() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        {
            let _transaction = page_cache.begin_transaction();
            // Lock a page and alter the leaf key so it is larger than its parent.
            let mut root_page = page_cache.lock_page_mut(tree.root);
            let child_page1_num = allocator.alloc();
            let mut child_page1 = page_cache.lock_page_mut(child_page1_num);
            let child_page2_num = allocator.alloc();
            let mut child_page2 = page_cache.lock_page_mut(child_page2_num);

            init_btree_node(&mut child_page1);
            init_btree_node(&mut child_page2);
            set_not_leaf(&mut root_page);
            set_next_sib(&mut child_page1, Some(child_page2_num));

            append_entry(&mut root_page, b"banana", &child_page1_num.to_bytes());
            set_right_child(&mut root_page, Some(child_page2_num));

            append_entry(&mut child_page1, b"abacus", b"foo");
            append_entry(&mut child_page2, b"cencus", b"foo");
        }

        validate_btree(&tree, &page_cache);
    }

    #[test]
    fn test_get_key_val() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        append_entry(&mut page, b"foobar", b"abcdefghijklmnopqrstuwxyz");
        append_entry(&mut page, b"zzzz", b"3.1415926535897932384626433832");
        validate_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 2);

        assert_eq!(get_entry_key(&page, 0), b"foobar");
        assert_eq!(get_entry_value(&page, 0), b"abcdefghijklmnopqrstuwxyz");

        assert_eq!(get_entry_key(&page, 1), b"zzzz");
        assert_eq!(get_entry_value(&page, 1), b"3.1415926535897932384626433832");
    }

    #[test]
    fn test_find_key() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        append_entry(&mut page, b"aaaa", &[0u8]);
        append_entry(&mut page, b"bbbb", &[0u8]);
        append_entry(&mut page, b"cccc", &[0u8]);
        append_entry(&mut page, b"dddd", &[0u8]);
        validate_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 4);

        assert_eq!(find_key(&page, b"aaa"), 0); // Search key is before first key
        assert_eq!(find_key(&page, b"aaaa"), 0); // Equal to first key
        assert_eq!(find_key(&page, b"aaab"), 1); // Between first and second key
        assert_eq!(find_key(&page, b"bbbb"), 1); // Equal to second key
        assert_eq!(find_key(&page, b"bbbc"), 2); // Between second and third key
        assert_eq!(find_key(&page, b"eeee"), 4); // Larger than largest key
    }

    #[test]
    fn test_find_key_empty() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        assert_eq!(find_key(&page, b"foo"), 0);
    }

    // Validates get_vararray_free_space and get_entry_size return
    // consistent values.
    #[test]
    fn test_entry_size() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);
        let init_free_space = get_vararray_free_space(&page);
        let key1 = b"foo";
        let val1 = b"00000000000000000000000000000";
        insert_entry(&mut page, key1, val1);
        assert_lt!(get_vararray_free_space(&page), init_free_space);
        assert_eq!(get_vararray_free_space(&page), init_free_space -
            get_entry_size(key1, val1));

        let key2 = b"abcdefghijklmnopqrstuvwxyz";
        let val2 = b"..ooOOO";
        let init_free_space = get_vararray_free_space(&page);
        insert_entry(&mut page, key2, val2);
        assert_lt!(get_vararray_free_space(&page), init_free_space);
        assert_eq!(get_vararray_free_space(&page), init_free_space -
            get_entry_size(key2, val2));
    }

    #[test]
    fn test_insert_entry() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);

        // Note these are out of order
        insert_entry(&mut page, b"aardvark", &[0u8]);
        insert_entry(&mut page, b"zebra", &[0u8]);
        insert_entry(&mut page, b"apple", &[0u8]);
        insert_entry(&mut page, b"banana", &[0u8]);
        validate_node(&page);
        assert_eq!(get_num_vararray_entries(&page), 4);

        assert_eq!(find_key(&page, b"aardvark"), 0);
        assert_eq!(find_key(&page, b"apple"), 1);
        assert_eq!(find_key(&page, b"banana"), 2);
        assert_eq!(find_key(&page, b"zebra"), 3);
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

        insert_entry(&mut page, b"aardvark", &[0u8]);
        insert_entry(&mut page, b"aardvark", &[0u8]);
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

        validate_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        validate_node(&node2);
        validate_node(&node3);

        let orig_sep_index = get_num_vararray_entries(&node2);
        assert_eq!(&separator_key, &get_entry_key(&node1, orig_sep_index));

        assert_eq!(get_right_child(&node2), PageNum::from_bytes(
            get_entry_value(&node1, orig_sep_index)));
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

        validate_node(&node1);

        let separator_key = split_node(&node1, &mut node2, &mut node3);
        validate_node(&node2);
        validate_node(&node3);

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
        validate_node(&node2);
        validate_node(&node3);
    }

    #[test]
    fn test_leaf_flag() {
        let mut page: PageData = [0; PAGE_SIZE];
        init_btree_node(&mut page);
        assert!(is_leaf(&page));
        page[0] = 0;
        assert!(!is_leaf(&page));
    }

    #[test]
    fn test_valid_btree_create() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);
        let mut i = 0;
        for (key, val) in tree.iterate(false, &page_cache) {
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(PageNum::from_bytes(&val).unwrap(), PageNum::from_u64(i as u64));
            i += 1;
        }
    }

    #[test]
    fn test_btree_backward_scan() {
        const NUM_TEST_ENTRIES: usize = 256;
        let (page_cache, _alloc, tree) = populate_test_btree(NUM_TEST_ENTRIES);

        let mut cursor = tree.iterate(true, &page_cache);
        for i in (0..NUM_TEST_ENTRIES).rev() {
            let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(PageNum::from_bytes(&val).unwrap(), PageNum::from_u64(i as u64));
        }

        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_find() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);

        const START_KEY_IDX: usize = 55;
        let mut cursor = tree.find(&gen_key_for_index(START_KEY_IDX), false, &page_cache);
        for i in START_KEY_IDX..START_KEY_IDX + 10 {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), &gen_key_for_index(i));
            assert_eq!(PageNum::from_bytes(&val).unwrap(),  PageNum::from_u64(i as u64));
        }
    }

    // Get the first page in the tree, which requires traversing the left child page.
    #[test]
    fn test_btree_find_begin() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);

        let mut cursor = tree.find(&[0u8], false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("cursor failed"); };
        assert_eq!(key.as_slice(), &gen_key_for_index(0));
            assert_eq!(PageNum::from_bytes(&val).unwrap(),  PageNum::from_u64(0));
    }

    // Key is before first key and going in reverse. Nothing to fetch.
    #[test]
    fn test_btree_reverse_find_begin() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);

        let mut cursor = tree.find(&[0u8], true, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Key is after last key and going forward. Nothing to fetch.
    #[test]
    fn test_btree_find_past_end() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);

        let mut cursor = tree.find(&[0xff; 255], false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_btree_delete() {
        const NUM_TEST_ENTRIES: usize = 256;
        let (page_cache, mut allocator, tree) = populate_test_btree(NUM_TEST_ENTRIES);

        const INDEX_TO_DELETE: usize = 37;
        {
            let _transaction = page_cache.begin_transaction();
            tree.delete(gen_key_for_index(INDEX_TO_DELETE).as_slice(),
                &page_cache, &mut allocator);
        }

        let mut cursor = tree.iterate(false, &page_cache);
        for i in 0..NUM_TEST_ENTRIES {
            if i == INDEX_TO_DELETE {
                continue;
            }

            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(PageNum::from_bytes(&val).unwrap(),  PageNum::from_u64(i as u64));
        }

        assert!(cursor.next().is_none());
        validate_btree(&tree, &page_cache);
    }

    #[test]
    fn test_btree_delete_not_present() {
        const NUM_TEST_ENTRIES: usize = 256;
        let (page_cache, mut allocator, tree) = populate_test_btree(NUM_TEST_ENTRIES);

        {
            let _transaction = page_cache.begin_transaction();

            // Key is bogus
            tree.delete(b"yolo", &page_cache, &mut allocator);
        }

        let mut cursor = tree.iterate(false, &page_cache);
        for i in 0..NUM_TEST_ENTRIES {
            let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
            assert_eq!(key.as_slice(), gen_key_for_index(i));
            assert_eq!(PageNum::from_bytes(&val).unwrap(),  PageNum::from_u64(i as u64));
        }
    }

    #[test]
    fn test_btree_delete_all() {
        // I've empirically verified this is large enough that there is a layer of internal
        // nodes. That's improtant to catch edge cases.
        const TEST_ENTRIES: usize = 2048;

        let (page_cache, mut allocator, tree) = populate_test_btree(TEST_ENTRIES);
        {
            let mut delete_seq: Vec<usize> = (0..TEST_ENTRIES).collect();
            delete_seq.shuffle(&mut SmallRng::seed_from_u64(0xc0fc47a65d406179));

            for i in delete_seq {
                let _transaction = page_cache.begin_transaction();
                tree.delete(&gen_key_for_index(i), &page_cache, &mut allocator);
            }
        }

        validate_btree(&tree, &page_cache);

        let mut cursor = tree.iterate(false, &page_cache);
        assert_eq!(cursor.next(), None);

        // Ensure we've reclaimed storage
        assert!(allocator.total_allocs - allocator.total_frees < 2);
    }

    #[test]
    fn test_iterate_empty() {
        let (page_cache, mut _allocator, tree) = create_test_btree();
        let mut cursor = tree.iterate(false, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    #[test]
    fn test_reverse_iterate_empty() {
        let (page_cache, mut _allocator, tree) = create_test_btree();
        let mut cursor = tree.iterate(true, &page_cache);
        assert_eq!(cursor.next(), None);
    }

    // Just ensures it doesn't crash...
    #[test]
    fn test_print_btree() {
        let (page_cache, _alloc, tree) = populate_test_btree(256);
        tree.print(&page_cache);
    }

    // The Oracle is a parallel data structure that tracks the expected
    // btree state based on random operations below.
    struct Oracle {
        entries: Vec<(Vec<u8>, Vec<u8>)>
    }

    impl Oracle {
        // TODO: this doesn't guarantee uniqueness.
        fn add(&mut self, key: &[u8], value: &[u8]) {
            let kv = (key.to_vec(), value.to_vec());
            let pos = match self.entries.binary_search(&kv) {
                Ok(pos) | Err(pos) => pos
            };

            self.entries.insert(pos, kv);
        }

        fn validate(&self, cursor: BTreeCursor) {
            let mut db_entries: Vec<(Vec<u8>, Vec<u8>)> = cursor.map(|x | (x.0, x.1)).collect();
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
        let mut rng = SmallRng::seed_from_u64(0x12345);
        let mut oracle = Oracle{ entries: Vec::new() };
        let (page_cache, mut allocator, tree) = create_test_btree();

        let total_reps = 3000;
        let min_psub: f64 = 0.3;
        for rep in 0..total_reps {
            let p_add: f64 = min_psub + (1.0 - min_psub) * (1.0 - (rep as f64 / total_reps as f64));
            if rng.random::<f64>() > p_add {
                // Delete entry
                if !oracle.entries.is_empty() {
                    let i = rng.random_range(0..oracle.entries.len());
                    let entry = &oracle.entries[i];
                    let _transaction = page_cache.begin_transaction();
                    tree.delete(&entry.0, &page_cache, &mut allocator);
                    oracle.entries.remove(i);
                }
            } else {
                // Insert entry
                let key = random_value(&mut rng);
                let value = random_value(&mut rng);

                // TODO ensure the key is unique by looking in the oracle.
                oracle.add(&key, &value);
                let _transaction = page_cache.begin_transaction();
                tree.insert(&key, &value, &page_cache, &mut allocator);
            }

            if oracle.entries.len() > 0 {
                oracle.validate(tree.iterate(false, &page_cache));
            }

            // This is a bit expensive, so do it periodically
            if rep % 100 == 0 {
                validate_btree(&tree, &page_cache);
            }
        }

        oracle.validate(tree.iterate(false, &page_cache));
        validate_btree(&tree, &page_cache);
    }

    #[test]
    fn test_open() {
        let (page_cache, mut allocator, tree) = create_test_btree();
        let root_pnum = tree.root;

        // Insert an entry and then drop the tree and page cache, simulating
        // closing and reopening the database.
        {
            let _transaction = page_cache.begin_transaction();
            tree.insert(b"abcd", b"efg", &page_cache, &mut allocator);
        }

        drop(tree);

        let tree = BTree::open(root_pnum);
        let mut cursor = tree.iterate(false, &page_cache);
        let Some((key, val)) = cursor.next() else { panic!("failed to fetch entry"); };
        assert_eq!(key.as_slice(), b"abcd");
        assert_eq!(val.as_slice(), b"efg");
    }
}
