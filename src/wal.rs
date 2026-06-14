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

use crate::util::*;
use crate::page_cache::*;
use std::cell::RefCell;
use std::rc::Rc;

//
// The write ahead log ensures database consistency in the event of a crash
// by implementing a two-phase commit protcol
// <https://en.wikipedia.org/wiki/Two-phase_commit_protocol>.
//
// The on-disk data structure starts with two copies of the log header.
// The system alternates writes between these two pages to ensure there
// is always a valid copy in the event of a write failure. An incrementing
// sequence number is written in the header so the system can determine the
// newest one at startup.
//
// The log contains a circular buffer of page data to be written. The
// log header contains head and tail indices, as well as an array of metadata
// for each page. The head pointer refers to the next block to be used (this
// is one block beyond the last written block), and the tail pointer refers
// to the oldest valid block. If the head and tail are equal the log is empty.
// The pointers wrap around, so the tail pointer may be a lower block number
// than the head (because of this scheme, the maximum capacity of the buffer
// is one less than the number of blocks).
//
// The header is always written after the corresponding data blocks in the log
// are written. In order to ensure integrity, this never updates both the
// head and tail pointers in the same write.
//
// The headers are checksummed to detect failures like torn or failed writes.
//
// struct WriteAheadLog {
//      header_a: LogHeader,
//      header_b: LogHeader,
//      pages: [PageData; COUNT]
// }
//
// struct LogHeader {
//     checksum: u32,
//     header_sequence: u32,
//     head_page_num: u16,
//     tail_page_num: u16,
//     page_headers: [PageHeader; COUNT]
// }
//
// struct PageHeader {
//      transaction_id: u32,
//      file_page_id: u64
// }
//

const PAGE_HEADER_SIZE: usize = 12;
const HEADER_BLOCKS: usize = 2; // Assumes one block per header

const LH_CHECKSUM_OFFS: usize = 0;
const LH_SEQ_OFFS: usize = 4;
const LH_HEAD_OFFS: usize = 8;
const LH_TAIL_OFFS: usize = 10;
const LH_PAGE_HDRS_OFFS: usize = 12;

const PH_FPID_OFFS: usize = 4;

pub struct WriteAheadLog {
    backing_store: Rc<RefCell<dyn PersistentStore>>,
    next_transaction_id: u32,
    next_header_seq: u32, // Even sequences go to block 0, odd to 1
    header_data: PageData,  // Contents of next header to be written
    log_start: PageNum,
    num_log_blocks: usize,
    head: usize,
    tail: usize
}

impl WriteAheadLog {
    pub fn new(start: PageNum, size: usize, backing_store: &Rc<RefCell<dyn PersistentStore>>) -> Self {
        Self {
            backing_store: backing_store.clone(),
            next_transaction_id: 1,
            next_header_seq: 0,
            header_data: [0u8; PAGE_SIZE],
            log_start: start,
            num_log_blocks: size - HEADER_BLOCKS,
            head: 0,
            tail: 0
        }
    }

    pub fn replay(&mut self) {
        // Find the first valid log header
        let mut header_a: PageData = [0; PAGE_SIZE];
        let mut header_b: PageData = [0; PAGE_SIZE];
        let mut store = self.backing_store.borrow_mut();
        store.read(self.log_start, &mut header_a);
        store.read(PageNum::from_u64(self.log_start.as_u64() + 1), &mut header_b);

        // Validate checksums, find newest
        let a_valid = checksum(&header_a[4..]) == get_u32(&header_a, LH_CHECKSUM_OFFS);
        let b_valid = checksum(&header_b[4..]) == get_u32(&header_b, LH_CHECKSUM_OFFS);
        if a_valid && b_valid {
            if wrapping_gtr(get_u32(&header_a, LH_SEQ_OFFS),
                get_u32(&header_b, LH_SEQ_OFFS)) {
                self.header_data.copy_from_slice(&header_a);
                self.next_header_seq = get_u32(&header_a, LH_SEQ_OFFS)
                    .wrapping_add(1);
            } else {
                self.header_data.copy_from_slice(&header_b);
                self.next_header_seq = get_u32(&header_b, LH_SEQ_OFFS)
                    .wrapping_add(1);
            }
        } else if a_valid {
            self.header_data.copy_from_slice(&header_a);
            self.next_header_seq = get_u32(&header_a, LH_SEQ_OFFS)
                .wrapping_add(1);
        } else if b_valid {
            self.header_data.copy_from_slice(&header_b);
            self.next_header_seq = get_u32(&header_b, LH_SEQ_OFFS)
                .wrapping_add(1);
        } else {
            // No valid header, nothing to replay
            self.next_header_seq = 0;
            return;
        }

        self.head = get_u16(&self.header_data, LH_HEAD_OFFS) as usize;
        self.tail = get_u16(&self.header_data, LH_TAIL_OFFS) as usize;
        if self.head != self.tail {
            // Need to replay
            let count = if self.head > self.tail {
                self.head - self.tail
            } else {
                self.num_log_blocks - self.tail + self.head
            };

            let mut cur = self.tail;
            for _ in 0..count {
                let mut block: PageData = [0; PAGE_SIZE];
                // Copy from log to file area
                let read_pnum = PageNum::from_u64(self.log_start.as_u64()
                    + HEADER_BLOCKS as u64 + cur as u64);
                store.read(read_pnum, &mut block);
                let write_pnum = PageNum::from_u64(get_u64(&self.header_data,
                    LH_PAGE_HDRS_OFFS + cur * PAGE_HEADER_SIZE + PH_FPID_OFFS));
                store.write(write_pnum, &block);

                if cur == self.num_log_blocks - 1 {
                    cur = 0;
                } else {
                    cur += 1;
                }
            }

            store.sync();
            drop(store);

            self.blocks_written();
        }
    }

    pub fn log_transaction(&mut self, pages: &Vec<(PageNum, PageData)>) {
        // Write all transaction blocks to log.
        for (page_num, block_data) in pages {
            // Update the block data structure
            let offset = LH_PAGE_HDRS_OFFS + self.head * PAGE_HEADER_SIZE;
            set_u32(&mut self.header_data, offset, self.next_transaction_id);
            set_u64(&mut self.header_data, offset + PH_FPID_OFFS, Some(*page_num).to_encoded());

            // Write the block data itself to the log.
            let write_pnum = PageNum::from_u64(self.log_start.as_u64()
                + HEADER_BLOCKS as u64 + self.head as u64);
            self.backing_store.borrow_mut().write(write_pnum, block_data);
            self.head = (self.head + 1) % self.num_log_blocks;
        }

        set_u16(&mut self.header_data, LH_HEAD_OFFS, self.head as u16);
        self.write_header();
        self.backing_store.borrow_mut().sync();
        self.next_transaction_id = self.next_transaction_id.wrapping_add(1);
    }

    // Write the current header to disk and swap
    fn write_header(&mut self) {
        let header_seq = self.next_header_seq;
        self.next_header_seq = self.next_header_seq.wrapping_add(1);
        set_u32(&mut self.header_data, LH_SEQ_OFFS, header_seq);
        let sum = checksum(&self.header_data[4..]);
        set_u32(&mut self.header_data, LH_CHECKSUM_OFFS, sum);
        self.backing_store.borrow_mut().write(PageNum::from_u64(self.log_start.as_u64() +
            (header_seq & 1) as u64), &self.header_data);
    }

    // Callback from cache when a page is flushed.
    // TODO: if this flushed more lazily, this could track outstanding blocks
    // that would improve performance.
    pub fn blocks_written(&mut self) {
        // Set tail = head
        let head = get_u16(&self.header_data, LH_HEAD_OFFS);
        set_u16(&mut self.header_data, LH_TAIL_OFFS, head);
        self.write_header();
    }
}

#[cfg(test)]
mod tests {
    use crate::mocks::MockPersistentStore;
    use rand::rngs::SmallRng;
    use rand::{SeedableRng, RngExt};
    use rand::seq::index;
    use super::*;

    #[test]
    fn test_log_transaction_replay() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));

        let mut log1 = WriteAheadLog::new(PageNum::from_u64(2), 10, &mock_io);
        log1.log_transaction(&vec![
            (PageNum::from_u64(13), [1; PAGE_SIZE]),
            (PageNum::from_u64(15), [2; PAGE_SIZE])
        ]);

        // Reopen, replay transaction
        let mut log2 = WriteAheadLog::new(PageNum::from_u64(2), 10, &mock_io);
        log2.replay();

        let mut store = mock_io.borrow_mut();
        let mut block: PageData = [0; PAGE_SIZE];
        store.read(PageNum::from_u64(13), &mut block);
        assert_eq!(block, [1; PAGE_SIZE]);
        store.read(PageNum::from_u64(15), &mut block);
        assert_eq!(block, [2; PAGE_SIZE]);
    }

    #[test]
    fn test_log_transaction_replay_wrap() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));

        let mut log1 = WriteAheadLog::new(PageNum::from_u64(2), 7, &mock_io);
        log1.log_transaction(&vec![
            (PageNum::from_u64(13), [1; PAGE_SIZE]),
            (PageNum::from_u64(14), [2; PAGE_SIZE]),
            (PageNum::from_u64(15), [3; PAGE_SIZE]),
        ]);

        mock_io.borrow_mut().write(PageNum::from_u64(13), &[1; PAGE_SIZE]);
        mock_io.borrow_mut().write(PageNum::from_u64(14), &[2; PAGE_SIZE]);
        mock_io.borrow_mut().write(PageNum::from_u64(15), &[3; PAGE_SIZE]);
        log1.blocks_written();

        log1.log_transaction(&vec![
            (PageNum::from_u64(16), [4; PAGE_SIZE]),
            (PageNum::from_u64(17), [5; PAGE_SIZE]),
            (PageNum::from_u64(18), [6; PAGE_SIZE])
        ]);

        // Reopen, replay transaction
        let mut log2 = WriteAheadLog::new(PageNum::from_u64(2), 7, &mock_io);
        log2.replay();

        let mut block: PageData = [0; PAGE_SIZE];
        for i in 0..5 {
            mock_io.borrow_mut().read(PageNum::from_u64(13 + i as u64), &mut block);
            assert_eq!(block, [i + 1; PAGE_SIZE]);
        }
    }

    // Transaction completes, no need to replay
    #[test]
    fn test_log_transaction_no_replay() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));

        let mut log1 = WriteAheadLog::new(PageNum::from_u64(2), 10, &mock_io);
        log1.log_transaction(&vec![
            (PageNum::from_u64(13), [1; PAGE_SIZE]),
            (PageNum::from_u64(15), [2; PAGE_SIZE])
        ]);

        log1.blocks_written();

        // Reopen
        let mut log2 = WriteAheadLog::new(PageNum::from_u64(2), 10, &mock_io);
        log2.replay();

        // Okay, we never actually wrote these blocks, so the replay will
        // do nothing and the blocks will be zero.
        let mut store = mock_io.borrow_mut();
        let mut block: PageData = [0; PAGE_SIZE];
        store.read(PageNum::from_u64(13), &mut block);
        assert_eq!(block, [0; PAGE_SIZE]);
        store.read(PageNum::from_u64(15), &mut block);
        assert_eq!(block, [0; PAGE_SIZE]);
    }

    #[test]
    fn test_transaction_stress() {
        let mock_io: Rc<RefCell<dyn PersistentStore>> =
            Rc::new(RefCell::new(MockPersistentStore::default()));
        const LOG_START: usize = 2;
        const LOG_SIZE: usize = 10;
        const FS_SIZE: usize = 40;
        const START_OF_DATA: usize = LOG_START + HEADER_BLOCKS + LOG_SIZE;
        let mut oracle: Vec<PageData> = vec![[0; PAGE_SIZE]; FS_SIZE as usize];
        let mut rng = SmallRng::seed_from_u64(0x12345);
        for _iteration in 0..1000 {
            // The write limit here will cause writes to fail at some random point.
            // We then will restart to ensure the disk is in a consistent state.
            mock_io.borrow_mut().as_any_mut().downcast_mut::<MockPersistentStore>().unwrap()
                .set_write_limit(rng.random_range(1..50));

            let mut log = WriteAheadLog::new(PageNum::from_u64(LOG_START as u64), LOG_SIZE as usize, &mock_io);
            log.replay();
            if mock_io.borrow().as_any().downcast_ref::<MockPersistentStore>().unwrap().hit_write_limit() {
                // Failed to write, did not replay log, it is not yet consistent with oracle, so
                // retry.
                continue;
            }

            // Validate disk against the oracle
            for (block_index, expected) in oracle.iter().enumerate().skip(START_OF_DATA) {
                let mut temp: PageData = [0; PAGE_SIZE];
                mock_io.borrow_mut().read(PageNum::from_u64(block_index as u64), &mut temp);
                assert_eq!(temp, *expected);
            }

            // Create some transactions.
            let transaction_count = rng.random_range(1..5);
            for _ti in 0..transaction_count {
                let page_count = rng.random_range(1..5);
                let block_offsets: Vec<usize> = index::sample(&mut rng, FS_SIZE - START_OF_DATA, page_count).into_vec();
                let pages: Vec<(PageNum, PageData)> = block_offsets.into_iter().map(|offset| {
                    let mut data: PageData = [0; PAGE_SIZE];
                    rng.fill(&mut data);
                    let page_num = PageNum::from_u64((START_OF_DATA + offset) as u64);
                    (page_num, data)
                }).collect();
                log.log_transaction(&pages);

                if mock_io.borrow().as_any().downcast_ref::<MockPersistentStore>().unwrap().hit_write_limit() {
                    // Failed to write to the log, skip updating the oracle; this data will be lost.
                    continue;
                }

                for (page_num, data) in &pages {
                    oracle[page_num.as_u64() as usize] = *data;
                }

                for (page_num, data) in pages {
                    mock_io.borrow_mut().write(page_num, &data);
                }

                log.blocks_written();
            }
        }
    }
}
