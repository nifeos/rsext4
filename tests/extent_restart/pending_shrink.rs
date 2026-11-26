//! A restarted shrink that published its size but failed to zero its EOF
//! tail.
//!
//! The standalone data block cache keeps the failed block dirty, so these
//! tests run only without the multilevel cache. Every way of growing the file
//! again must still read zero past the published size, whatever happened to
//! its name, and a failed finish must not write the old tail back.

use rsext4::{ErrorContext, PreallocationOptions, RenameOptions, preallocate_inode, rename};

use super::*;

/// How a test grows a file whose shrink failed to zero its EOF tail.
#[derive(Clone, Copy, Debug)]
enum PendingShrinkGrowth {
    Write,
    ExtendingPreallocation,
}

/// What happens to the file's name before it grows again.
#[derive(Clone, Copy, Debug)]
enum PendingShrinkName {
    Kept,
    Unlinked,
    RenamedOver,
}

#[test]
fn extending_preallocation_after_failed_tail_zeroing_reads_zero_past_the_size() {
    assert_pending_shrink_growth_reads_zero(
        PendingShrinkGrowth::ExtendingPreallocation,
        PendingShrinkName::Kept,
    );
}

#[test]
fn write_to_an_open_unlinked_file_after_failed_tail_zeroing_reads_zero_past_the_size() {
    assert_pending_shrink_growth_reads_zero(
        PendingShrinkGrowth::Write,
        PendingShrinkName::Unlinked,
    );
}

#[test]
fn write_to_an_open_file_renamed_over_after_failed_tail_zeroing_reads_zero_past_the_size() {
    assert_pending_shrink_growth_reads_zero(
        PendingShrinkGrowth::Write,
        PendingShrinkName::RenamedOver,
    );
}

const PENDING_SHRINK_SIZE: usize = 100;

/// Shrinks the four-leaf extent file at `path` to `PENDING_SHRINK_SIZE`
/// bytes on the restart path and fails the write that zeroes its EOF tail
/// after the first commit, then installs a journal large enough for what
/// follows. Returns the fixture and the retained block's original bytes.
fn fail_restarted_tail_zeroing(path: &str) -> (LargeExtentFixture, Vec<u8>) {
    let mut fixture = build_large_extent_fixture(path);
    install_small_journal(&mut fixture);
    let retained_block = fixture.data_blocks[0];
    fixture
        .filesystem
        .datablock_cache
        .modify(&mut fixture.journal, retained_block, |data| {
            for (index, byte) in data.iter_mut().enumerate() {
                *byte = (index % 251) as u8 + 1;
            }
        })
        .expect("data block fill failed");
    fixture
        .filesystem
        .sync_filesystem(&mut fixture.journal)
        .expect("fixture sync failed");
    fixture.journal.flush().expect("fixture checkpoint failed");
    let mut original = vec![0u8; BLOCK_SIZE];
    read_inode_data_into(
        &mut fixture.journal,
        &mut fixture.filesystem,
        fixture.inode_number,
        0,
        &mut original,
    )
    .expect("retained block read failed");

    fixture.power_cut.arm_failed_block_write(retained_block);
    let error = truncate_inode(
        &mut fixture.journal,
        &mut fixture.filesystem,
        fixture.inode_number,
        PENDING_SHRINK_SIZE as u64,
    )
    .expect_err("a failed tail write must fail the truncate");
    assert_eq!(
        error.kind(),
        Ext4ErrorKind::Io,
        "unexpected error {error:?}"
    );
    assert_eq!(
        fixture.power_cut.failed_block_write.get(),
        None,
        "the tail zeroing must have failed"
    );
    let inode = fixture
        .filesystem
        .get_inode_by_num(&mut fixture.journal, fixture.inode_number)
        .expect("inode read failed");
    assert_eq!(
        inode.size(),
        PENDING_SHRINK_SIZE as u64,
        "the shrink published its size before zeroing the tail"
    );
    assert_eq!(
        fixture.filesystem.superblock.s_last_orphan,
        fixture.inode_number.raw()
    );
    fixture
        .filesystem
        .sync_filesystem(&mut fixture.journal)
        .expect("sync after the failed truncate failed");
    fixture
        .journal
        .flush()
        .expect("checkpoint before the journal swap failed");
    install_journal_with_maxlen(&mut fixture, LATER_WRITE_JOURNAL_BLOCKS * 2);
    (fixture, original)
}

/// Grows a file whose restarted shrink failed to zero its EOF tail, after
/// keeping, unlinking or renaming over its name, and requires every byte
/// past the published size to read as zero.
///
/// Upstream also covers a detached write; this crate has no detached write
/// interface, so the serialized write covers that path.
fn assert_pending_shrink_growth_reads_zero(growth: PendingShrinkGrowth, name: PendingShrinkName) {
    const LATER_OFFSET: usize = BLOCK_SIZE + 500;
    const LATER_BYTES: &[u8] = b"later";
    let path = "/pending-shrink";
    let (mut fixture, original) = fail_restarted_tail_zeroing(path);
    let number = fixture.inode_number;
    match name {
        PendingShrinkName::Kept => {}
        PendingShrinkName::Unlinked => {
            let outcome =
                unlink(&mut fixture.filesystem, &mut fixture.journal, path).expect("unlink failed");
            assert_eq!(outcome.inode, number);
            assert!(outcome.requires_reap());
        }
        PendingShrinkName::RenamedOver => {
            mkfile(
                &mut fixture.journal,
                &mut fixture.filesystem,
                "/pending-shrink-other",
                None,
                None,
            )
            .expect("second file creation failed");
            let outcome = rename(
                &mut fixture.journal,
                &mut fixture.filesystem,
                "/pending-shrink-other",
                path,
                RenameOptions::REPLACE,
            )
            .expect("rename over the file failed");
            let replaced = outcome.replaced.expect("the rename must replace the file");
            assert_eq!(replaced.inode, number);
            assert!(replaced.requires_reap());
        }
    }

    let mut expected = original[..PENDING_SHRINK_SIZE].to_vec();
    match growth {
        PendingShrinkGrowth::Write => {
            rsext4::write_inode_data(
                &mut fixture.journal,
                &mut fixture.filesystem,
                number,
                LATER_OFFSET as u64,
                LATER_BYTES,
            )
            .expect("a later write must finish the shrink and succeed");
            expected.resize(LATER_OFFSET, 0);
            expected.extend_from_slice(LATER_BYTES);
        }
        PendingShrinkGrowth::ExtendingPreallocation => {
            preallocate_inode(
                &mut fixture.journal,
                &mut fixture.filesystem,
                number,
                BLOCK_SIZE as u64,
                BLOCK_SIZE as u64,
                PreallocationOptions::EXTEND_SIZE,
            )
            .expect("an extending preallocation must finish the shrink and succeed");
            expected.resize(2 * BLOCK_SIZE, 0);
        }
    }
    let inode = fixture
        .filesystem
        .get_inode_by_num(&mut fixture.journal, number)
        .expect("inode read failed");
    assert_eq!(inode.size(), expected.len() as u64);
    let mut output = vec![0xa5u8; expected.len()];
    let copied = read_inode_data_into(
        &mut fixture.journal,
        &mut fixture.filesystem,
        number,
        0,
        &mut output,
    )
    .expect("read after the growth failed");
    assert_eq!(copied, expected.len());
    assert_eq!(
        output
            .iter()
            .zip(&expected)
            .position(|(now, want)| now != want),
        None,
        "after {growth:?} with the name {name:?}, the bytes past the published size must be zero"
    );
    match name {
        PendingShrinkName::Kept => {
            assert_eq!(fixture.filesystem.superblock.s_last_orphan, 0);
        }
        PendingShrinkName::Unlinked | PendingShrinkName::RenamedOver => {
            assert_eq!(
                fixture.filesystem.superblock.s_last_orphan,
                number.raw(),
                "an open unlinked file stays on the orphan chain until its reap"
            );
            reap_unlinked_inode(&mut fixture.filesystem, &mut fixture.journal, number)
                .expect("reap failed");
            assert_eq!(fixture.filesystem.superblock.s_last_orphan, 0);
        }
    }
}

#[test]
fn failed_finish_of_a_published_shrink_keeps_the_tail_zero_on_the_device() {
    let (mut fixture, _) = fail_restarted_tail_zeroing("/pending-shrink-finish");
    let retained_block = fixture.data_blocks[0];
    // Under-account the inode's blocks, so that finishing the shrink zeroes
    // the tail again and then fails inside its removal transaction.
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
        PENDING_SHRINK_SIZE as u64,
    )
    .expect_err("finishing an under-accounted shrink must fail");
    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "extent:sub_inode_blocks"
        }),
        "unexpected error {error:?}"
    );
    // The bytes past the published size are no longer part of the file, so
    // no read through the filesystem can show them. Only the device holds
    // the evidence: a failed finish must not write the old tail back.
    let _ = fixture.filesystem.sync_filesystem(&mut fixture.journal);
    let mut block = vec![0xa5u8; BLOCK_SIZE];
    fixture
        .journal
        .read_blocks(&mut block, retained_block, 1)
        .expect("retained block read failed");
    assert_eq!(
        block[PENDING_SHRINK_SIZE..]
            .iter()
            .position(|byte| *byte != 0),
        None,
        "the retained block must stay zero past the published size"
    );
}
