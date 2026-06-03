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

pub fn get_u16(slice: &[u8], offs: usize) -> u16 {
    u16::from_le_bytes(slice[offs..offs + 2].try_into().unwrap())
}

pub fn get_u32(slice: &[u8], offs: usize) -> u32 {
    u32::from_le_bytes(slice[offs..offs + 4].try_into().unwrap())
}

pub fn get_u64(slice: &[u8], offs: usize) -> u64 {
    u64::from_le_bytes(slice[offs..offs + 8].try_into().unwrap())
}

pub fn set_u16(slice: &mut [u8], offs: usize, val: u16) {
    slice[offs..offs + 2].copy_from_slice(&val.to_le_bytes());
}

pub fn set_u32(slice: &mut [u8], offs: usize, val: u32) {
    slice[offs..offs + 4].copy_from_slice(&val.to_le_bytes());
}

pub fn set_u64(slice: &mut [u8], offs: usize, val: u64) {
    slice[offs..offs + 8].copy_from_slice(&val.to_le_bytes());
}

// This is a queue of array indices, which allow us to create LRUs
// without requiring weird pointer owning semantics and intrusive
// data structures.
pub struct IndexQueue {
    next: Vec<Option<usize>>,
    prev: Vec<Option<usize>>,
    head: Option<usize>,
    tail: Option<usize>
}

impl IndexQueue {
    pub fn new(num_elements: usize) -> Self {
        Self {
            next: vec![None; num_elements],
            prev: vec![None; num_elements],
            head: None,
            tail: None,
        }
    }

    pub fn empty(&self) -> bool {
        self.head.is_none()
    }

    pub fn remove(&mut self, index: usize) {
        match self.next[index] {
            Some(next_id) => {
                assert!(self.tail != Some(index));
                self.prev[next_id] = self.prev[index];
            }
            None => {
                assert!(self.tail.unwrap() == index);
                self.tail = self.prev[index];
            }
        }

        match self.prev[index] {
            Some(prev_id) => {
                assert!(self.head != Some(index));
                self.next[prev_id] = self.next[index];
            }
            None => {
                assert!(self.head.unwrap() == index);
                self.head = self.next[index];
            }
        }
    }

    pub fn push_head(&mut self, index: usize) {
        match self.head {
            Some(head) => {
                self.prev[head] = Some(index);
                self.next[index] = Some(head);
                self.head = Some(index);
                self.prev[index] = None;
            }

            None => {
                self.head = Some(index);
                self.tail = Some(index);
                self.next[index] = None;
                self.prev[index] = None;
            }
        }
    }

    pub fn pop_tail(&mut self) -> Option<usize> {
        match self.tail {
            Some(index) => {
                self.remove(index);
                Some(index)
            }
            None => None
        }
    }
}

pub struct IndexQueueIterator<'a> {
    queue: &'a IndexQueue,
    index: Option<usize>
}

impl<'a> Iterator for IndexQueueIterator<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let retval = self.index;
        if self.index.is_some() {
            self.index = self.queue.next[self.index.unwrap()];
        }

        retval
    }
}

impl<'a> IntoIterator for &'a IndexQueue {
    type Item = usize;
    type IntoIter = IndexQueueIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        IndexQueueIterator {
            queue: self,
            index: self.head
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn sanity_check_lru(queue: &IndexQueue) {
        // You can't have a valid head pointer with valid tail and vice versa
        assert_eq!(queue.head.is_some(), queue.tail.is_some(), "Head/tail out of sync");
        if queue.head.is_none() {
            assert!(queue.empty());
            return;
        }

        assert!(!queue.empty());
        assert_eq!(queue.next.len(), queue.prev.len(), "Array size mismatch");

        // Forward traversal
        let mut node = queue.head;
        let mut forward_len: usize = 0;
        while let Some(_id) = node {
            forward_len += 1;
            assert!(forward_len <= queue.next.len(), "Cycle in next pointers");

            if queue.next[node.unwrap() as usize].is_none() {
                assert_eq!(node, queue.tail, "Forward traversal terminated early: not at tail");
                break;
            } else {
                node = queue.next[node.unwrap() as usize];
            }
        }

        // Backward traversal
        let mut node = queue.tail;
        let mut reverse_len: usize = 0;
        while let Some(_id) = node {
            reverse_len += 1;
            assert!(reverse_len <= queue.prev.len(), "Cycle in prev pointers");

            if queue.prev[node.unwrap() as usize].is_none() {
                assert_eq!(node, queue.head, "Backward traversal terminated early: not at head");
                break;
            } else {
                node = queue.prev[node.unwrap() as usize];
            }
        }
    }

    // Test that our validation tests catch bad lists
    #[test]
    #[should_panic = "Head/tail out of sync"]
    fn test_head_tail_assym1() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), None],
            prev: vec![None, Some(0)],
            head: Some(0),
            tail: None
        })
    }

    #[test]
    #[should_panic = "Head/tail out of sync"]
    fn test_head_tail_assym2() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), None],
            prev: vec![None, Some(0)],
            head: None,
            tail: Some(0)
        })
    }

    #[test]
    #[should_panic = "Array size mismatch"]
    fn test_array_size_mismatch() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), None],
            prev: vec![Some(0)],
            head: Some(1),
            tail: Some(0)
        })
    }

    #[test]
    #[should_panic = "Cycle in next pointers"]
    fn test_next_cycle() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), Some(1)],
            prev: vec![None, Some(0)],
            head: Some(0),
            tail: Some(1)
        })
    }

    #[test]
    #[should_panic = "Cycle in prev pointers"]
    fn test_prev_cycle() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), None],
            prev: vec![Some(0), Some(0)],
            head: Some(0),
            tail: Some(1)
        })
    }

    #[test]
    #[should_panic = "Forward traversal terminated early: not at tail"]
    fn test_array_tail_bad() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), None, None],
            prev: vec![None, Some(0), Some(1)],
            head: Some(0),
            tail: Some(2)
        })
    }

    #[test]
    #[should_panic = "Backward traversal terminated early: not at head"]
    fn test_array_head_bad() {
        sanity_check_lru(&IndexQueue {
            next: vec![Some(1), Some(2), None],
            prev: vec![None, None, Some(1)],
            head: Some(0),
            tail: Some(2)
        })
    }

    #[test]
    fn test_lru_remove_empty() {
        let mut queue = IndexQueue::new(10);
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), None);
    }

    #[test]
    fn test_lru_evict_one() {
        let mut queue = IndexQueue::new(10);
        queue.push_head(1);
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(1));
        assert_eq!(queue.pop_tail(), None);
    }

    #[test]
    fn test_lru_evict_many() {
        let mut queue = IndexQueue::new(10);
        queue.push_head(1);
        queue.push_head(2);
        queue.push_head(3);
        queue.push_head(4);
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(1));
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(2));
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(3));
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(4));
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), None);
        sanity_check_lru(&queue);
    }

    #[test]
    fn test_lru_insert_remove() {
        let mut queue = IndexQueue::new(10);
        queue.push_head(1);
        queue.push_head(2);
        queue.push_head(3);
        queue.remove(2);
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(1));
        assert_eq!(queue.pop_tail(), Some(3));
        sanity_check_lru(&queue);
    }

    // Regression test
    #[test]
    fn test_lru_remove_head() {
        let mut queue = IndexQueue::new(10);
        queue.push_head(1);
        sanity_check_lru(&queue);
        queue.push_head(2);
        sanity_check_lru(&queue);
        queue.remove(2);
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), Some(1));
        sanity_check_lru(&queue);
        assert_eq!(queue.pop_tail(), None);
        sanity_check_lru(&queue);
    }

    #[test]
    fn test_iterator() {
        let mut queue = IndexQueue::new(10);
        for i in 0..10 {
            queue.push_head(i);
        }

        let mut expect_index = 10;
        for element in &queue {
            expect_index -= 1;
            assert_eq!(expect_index, element);
        }
    }
}
