//! Tests that pin the growth preflight: the target geometry it chooses and
//! each refusal it makes before any growth write.

use super::*;
use crate::error::ErrorContext;
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    mkfs_with_options,
};

const BLOCK: usize = 1024;
const PER_GROUP: u64 = 8192;

/// An in-memory device of 1 KiB sectors that may or may not offer flush.
struct MemoryDevice {
    bytes: Vec<u8>,
    flush: bool,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK;
        self.bytes
            .get_mut(start..start + buffer.len())
            .ok_or_else(Ext4Error::io)?
            .copy_from_slice(buffer);
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK;
        buffer.copy_from_slice(
            self.bytes
                .get(start..start + buffer.len())
                .ok_or_else(Ext4Error::io)?,
        );
        Ok(())
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(BLOCK as u32, (self.bytes.len() / BLOCK) as u64)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            flush: self.flush,
            ..DeviceCapabilities::default()
        }
    }

    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }
}

impl Clock for MemoryDevice {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        Ok(Ext4Timestamp::new(1_700_000_000, 0))
    }
}

/// Returns a journal device over `bytes` with the given journal use and flush support.
fn device(bytes: Vec<u8>, use_journal: bool, flush: bool) -> Jbd2Dev<MemoryDevice> {
    let mut journal = Jbd2Dev::initial_jbd2dev(0, MemoryDevice { bytes, flush }, use_journal);
    journal
        .set_filesystem_block_size(BLOCK)
        .expect("block size");
    journal
}

/// Makes and mounts a 1 KiB-block filesystem of 12000 blocks: two groups, the
/// second partial, with 64-bit descriptors, sixteen to a block, no reserved
/// descriptor blocks and inode tables of 512 blocks.
fn mounted() -> (Jbd2Dev<MemoryDevice>, Ext4FileSystem) {
    let mut journal = device(vec![0; 12000 * BLOCK], true, true);
    mkfs_with_options(
        &mut journal,
        MkfsOptions {
            block_size: BLOCK as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    let mut journal = device(journal.into_inner().bytes, true, true);
    let fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    assert_eq!(fs.group_count, 2);
    assert_eq!(fs.superblock.s_reserved_gdt_blocks, 0);
    assert_eq!(fs.superblock.descs_per_block(), 16);
    assert_eq!(inode_table_blocks(&fs.superblock).unwrap(), 512);
    (journal, fs)
}

/// Checks growth to `target` blocks after `change` edits the mounted superblock.
fn check_with(
    change: impl FnOnce(&mut Ext4Superblock),
    target: u64,
) -> Ext4Result<(u64, u32, u32)> {
    let (journal, mut fs) = mounted();
    change(&mut fs.superblock);
    GrowthLayout::check(&fs, &journal, target * BLOCK as u64)
        .map(|layout| (layout.blocks, layout.groups, layout.gdt_blocks))
}

fn check(target: u64) -> Ext4Result<(u64, u32, u32)> {
    check_with(|_| {}, target)
}

/// Returns the growth target, in blocks, for `target` blocks after `change`.
fn target_with(change: impl FnOnce(&mut Ext4Superblock), target: u64) -> u64 {
    let (_journal, mut fs) = mounted();
    change(&mut fs.superblock);
    fs.superblock
        .growth_target_blocks(target * BLOCK as u64)
        .unwrap()
}

fn operation<T>(result: Ext4Result<T>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    match error.context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

fn first_block(group: u64) -> u64 {
    1 + group * PER_GROUP
}

// ---- growth_target_blocks ----

#[test]
fn a_target_at_or_below_the_current_size_keeps_the_current_size() {
    for target in [5000, 12000] {
        assert_eq!(target_with(|_| {}, target), 12000);
    }
}

#[test]
fn a_target_counts_whole_blocks_of_its_bytes() {
    let (_journal, fs) = mounted();
    assert_eq!(
        fs.superblock
            .growth_target_blocks(14000 * BLOCK as u64 + BLOCK as u64 - 1)
            .unwrap(),
        14000
    );
}

#[test]
fn a_target_inside_the_published_tail_group_is_kept_whatever_its_metadata() {
    // Group 1 is published, so its metadata is already in place: even a
    // reserve that a new group of that size could not hold does not drop it.
    assert_eq!(
        target_with(|sb| sb.s_reserved_gdt_blocks = 6000, 14000),
        14000
    );
}

#[test]
fn a_new_final_group_is_kept_only_when_its_bitmaps_and_inode_table_fit() {
    // Group 2 keeps no superblock copy: two bitmaps and 512 table blocks.
    assert_eq!(
        target_with(|_| {}, first_block(2) + 514),
        first_block(2) + 514
    );
    assert_eq!(target_with(|_| {}, first_block(2) + 513), first_block(2));
}

#[test]
fn a_new_final_group_with_a_superblock_copy_also_needs_room_for_it_and_its_table() {
    // Group 3 also keeps a superblock and one descriptor block.
    assert_eq!(
        target_with(|_| {}, first_block(3) + 516),
        first_block(3) + 516
    );
    assert_eq!(target_with(|_| {}, first_block(3) + 515), first_block(3));
}

// ---- GrowthLayout::check ----

#[test]
fn a_target_that_does_not_grow_returns_the_current_geometry() {
    for target in [5000, 12000] {
        assert_eq!(check(target).unwrap(), (12000, 2, 1));
    }
}

#[test]
fn a_growing_target_returns_the_target_geometry() {
    assert_eq!(check(30000).unwrap(), (30000, 4, 1));
}

#[test]
fn a_filesystem_without_64_bit_descriptors_grows_below_the_32_bit_limit() {
    let without_64_bit = |sb: &mut Ext4Superblock| {
        sb.s_feature_incompat &= !Ext4Superblock::EXT4_FEATURE_INCOMPAT_64BIT
    };
    assert_eq!(check_with(without_64_bit, 30000).unwrap(), (30000, 4, 1));
    // The largest 32-bit count passes this limit and meets the descriptor one.
    assert_eq!(
        operation(check_with(without_64_bit, u64::from(u32::MAX))),
        "grow:reserved_gdt_capacity"
    );
    assert_eq!(
        operation(check_with(without_64_bit, u64::from(u32::MAX) + 1)),
        "grow:32bit_capacity"
    );
}

#[test]
fn each_unsupported_feature_or_state_is_refused_on_its_own() {
    let changes: [fn(&mut Ext4Superblock); 5] = [
        |sb| sb.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_HAS_JOURNAL,
        |sb| sb.s_state |= Ext4Superblock::EXT4_ERROR_FS,
        |sb| sb.s_feature_compat |= Ext4Superblock::EXT4_FEATURE_COMPAT_SPARSE_SUPER2,
        |sb| sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_META_BG,
        |sb| sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_MMP,
    ];
    for change in changes {
        assert_eq!(
            operation(check_with(change, 30000)),
            "grow:features_or_durability"
        );
    }
}

#[test]
fn each_supported_compatible_feature_is_accepted() {
    let supported = Ext4Superblock::EXT4_FEATURE_COMPAT_HAS_JOURNAL
        | Ext4Superblock::EXT4_FEATURE_COMPAT_EXT_ATTR
        | Ext4Superblock::EXT4_FEATURE_COMPAT_RESIZE_INODE
        | Ext4Superblock::EXT4_FEATURE_COMPAT_DIR_INDEX
        | Ext4Superblock::EXT4_FEATURE_COMPAT_ORPHAN_FILE;
    assert_eq!(
        check_with(|sb| sb.s_feature_compat = supported, 30000).unwrap(),
        (30000, 4, 1)
    );
}

#[test]
fn a_device_without_the_journal_or_without_flush_is_refused() {
    let (journal, fs) = mounted();
    let bytes = journal.into_inner().bytes;
    for (use_journal, flush) in [(false, true), (true, false)] {
        let device = device(bytes.clone(), use_journal, flush);
        assert_eq!(
            operation(GrowthLayout::check(&fs, &device, 30000 * BLOCK as u64)),
            "grow:features_or_durability"
        );
    }
}

#[test]
fn new_groups_must_fit_the_descriptor_blocks_already_reserved() {
    // Sixteen groups fill the one descriptor block; a seventeenth needs another.
    assert_eq!(check(first_block(16)).unwrap(), (first_block(16), 16, 1));
    assert_eq!(
        operation(check(first_block(16) + 600)),
        "grow:reserved_gdt_capacity"
    );
}

#[test]
fn an_inode_count_that_does_not_match_the_groups_is_refused() {
    assert_eq!(
        operation(check_with(|sb| sb.s_inodes_count += 1, 30000)),
        "grow:inode_geometry"
    );
}

#[test]
fn an_intermediate_group_with_a_superblock_copy_must_hold_its_metadata() {
    // Group 3 keeps a superblock and the descriptor table with its reserve:
    // 1 + 1 + reserve blocks, then two bitmaps and 512 table blocks. A reserve
    // of 7676 fills its 8192 blocks exactly, and the preflight goes on to the
    // journal, which cannot hold that reserve's backup in one transaction.
    let target = first_block(4) + 1000;
    assert_eq!(
        operation(check_with(|sb| sb.s_reserved_gdt_blocks = 7676, target)),
        "grow:transaction_capacity"
    );
    assert_eq!(
        operation(check_with(|sb| sb.s_reserved_gdt_blocks = 7677, target)),
        "grow:intermediate_group_metadata"
    );
}
