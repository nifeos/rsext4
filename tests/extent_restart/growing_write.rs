//! Which writes finish a pending shrink first.
//!
//! A write that ends inside the file or exactly at its size exposes nothing
//! past the published size, so it leaves an unfinished shrink alone. A write
//! that ends even one byte past the size must finish the shrink first. These
//! tests under-account the inode's blocks so that finishing always fails
//! with a known error, which shows whether a write tried.

use rsext4::ErrorContext;

use super::*;

const PENDING_SIZE: u64 = 100;

/// Shrinks the four-leaf extent file at `path` to `PENDING_SIZE` bytes on
/// the restart path with its blocks under-accounted, so the shrink publishes
/// its size and orphan record, then fails, and every later finish fails the
/// same way.
fn fail_shrink_after_publishing(path: &str) -> LargeExtentFixture {
    let mut fixture = build_large_extent_fixture(path);
    install_small_journal(&mut fixture);
    fixture
        .filesystem
        .modify_inode(&mut fixture.journal, fixture.inode_number, |inode| {
            inode.i_blocks_lo = 8;
        })
        .expect("inode block count edit failed");
    let error = truncate_inode(
        &mut fixture.journal,
        &mut fixture.filesystem,
        fixture.inode_number,
        PENDING_SIZE,
    )
    .expect_err("an under-accounted shrink must fail");
    assert_finish_failed(&error);
    let inode = fixture
        .filesystem
        .get_inode_by_num(&mut fixture.journal, fixture.inode_number)
        .expect("inode read failed");
    assert_eq!(inode.size(), PENDING_SIZE, "the shrink published its size");
    assert_eq!(
        fixture.filesystem.superblock.s_last_orphan,
        fixture.inode_number.raw(),
        "the shrink is still pending"
    );
    fixture
}

fn assert_finish_failed(error: &rsext4::Ext4Error) {
    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "extent:sub_inode_blocks"
        }),
        "unexpected error {error:?}"
    );
}

/// Writes `length` bytes ending at `end`.
fn write_ending_at(fixture: &mut LargeExtentFixture, end: u64, length: u64) -> Ext4Result<()> {
    let data = vec![0x5au8; length as usize];
    rsext4::write_inode_data(
        &mut fixture.journal,
        &mut fixture.filesystem,
        fixture.inode_number,
        end - length,
        &data,
    )
    .map(|_| ())
}

#[test]
fn write_inside_the_size_leaves_a_pending_shrink_alone() {
    let mut fixture = fail_shrink_after_publishing("/pending-inside");
    write_ending_at(&mut fixture, PENDING_SIZE - 1, 10)
        .expect("a write inside the size must not finish the shrink");
    assert_eq!(
        fixture.filesystem.superblock.s_last_orphan,
        fixture.inode_number.raw()
    );
}

#[test]
fn write_ending_at_the_size_leaves_a_pending_shrink_alone() {
    let mut fixture = fail_shrink_after_publishing("/pending-at-size");
    write_ending_at(&mut fixture, PENDING_SIZE, 10)
        .expect("a write ending at the size must not finish the shrink");
    let inode = fixture
        .filesystem
        .get_inode_by_num(&mut fixture.journal, fixture.inode_number)
        .expect("inode read failed");
    assert_eq!(inode.size(), PENDING_SIZE);
    assert_eq!(
        fixture.filesystem.superblock.s_last_orphan,
        fixture.inode_number.raw()
    );
}

#[test]
fn write_one_byte_past_the_size_finishes_a_pending_shrink_first() {
    let mut fixture = fail_shrink_after_publishing("/pending-past-size");
    let error = write_ending_at(&mut fixture, PENDING_SIZE + 1, 10)
        .expect_err("a growing write must finish the shrink first");
    assert_finish_failed(&error);
    let inode = fixture
        .filesystem
        .get_inode_by_num(&mut fixture.journal, fixture.inode_number)
        .expect("inode read failed");
    assert_eq!(
        inode.size(),
        PENDING_SIZE,
        "the failed write must not grow the file"
    );
}
