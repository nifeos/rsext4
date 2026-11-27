//! Unit tests of the block run lookup used by full-block writes.

use super::*;

const BLOCK_BYTES: u64 = 4096;

#[test]
fn full_block_run_takes_the_run_that_starts_at_the_block() {
    let runs = [
        ExtentRun {
            logical_start: 10,
            physical_start: AbsoluteBN::new(100),
            len: 4,
        },
        ExtentRun {
            logical_start: 14,
            physical_start: AbsoluteBN::new(200),
            len: 6,
        },
    ];

    assert_eq!(
        existing_full_block_run(&runs, 14, 14 * BLOCK_BYTES, 20 * BLOCK_BYTES, BLOCK_BYTES),
        Some((AbsoluteBN::new(200), 6))
    );
}

#[test]
fn full_block_run_is_bounded_by_the_blocks_left_to_write() {
    let runs = [ExtentRun {
        logical_start: 0,
        physical_start: AbsoluteBN::new(100),
        len: 16,
    }];

    assert_eq!(
        existing_full_block_run(&runs, 2, 2 * BLOCK_BYTES, 8 * BLOCK_BYTES, BLOCK_BYTES),
        Some((AbsoluteBN::new(102), 6))
    );
    assert_eq!(
        existing_full_block_run(&runs, 3, 2 * BLOCK_BYTES, 8 * BLOCK_BYTES, BLOCK_BYTES),
        Some((AbsoluteBN::new(103), 5))
    );
}
