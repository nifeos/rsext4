//! Tests that pin the mkfs geometry limits, the descriptor writes and the backup descriptor table.

use super::super::*;
use crate::error::ErrorContext;
use crate::{Clock, DeviceCapabilities, DeviceGeometry, SectorId};

const KIB: usize = 1024;
const FOUR_KIB: u32 = 4096;

struct MemoryDevice {
    bytes: Vec<u8>,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * KIB;
        self.bytes
            .get_mut(start..start + buffer.len())
            .ok_or_else(Ext4Error::io)?
            .copy_from_slice(buffer);
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * KIB;
        buffer.copy_from_slice(
            self.bytes
                .get(start..start + buffer.len())
                .ok_or_else(Ext4Error::io)?,
        );
        Ok(())
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(KIB as u32, (self.bytes.len() / KIB) as u64)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            flush: true,
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

fn device(blocks: usize) -> Jbd2Dev<MemoryDevice> {
    let device = MemoryDevice {
        bytes: vec![0; blocks * KIB],
    };
    Jbd2Dev::initial_jbd2dev(0, device, false)
}

/// Makes a 1 KiB-block filesystem of `blocks` blocks.
fn made(blocks: usize) -> Jbd2Dev<MemoryDevice> {
    let mut device = device(blocks);
    mkfs_with_options(
        &mut device,
        MkfsOptions {
            block_size: KIB as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    device
}

/// Decodes descriptor `index` of the descriptor table that starts at `block`.
fn descriptor(device: &mut Jbd2Dev<MemoryDevice>, block: u64, index: u64) -> Ext4GroupDesc {
    let mut bytes = [0; GROUP_DESC_SIZE as usize];
    device
        .read_device_bytes(
            block * KIB as u64 + index * u64::from(GROUP_DESC_SIZE),
            &mut bytes,
        )
        .expect("read descriptor");
    Ext4GroupDesc::from_disk_bytes(&bytes)
}

#[test]
fn a_lone_group_must_hold_its_superblock_descriptor_table_and_bitmaps() {
    // Three blocks cannot hold the superblock, one descriptor block and two bitmaps, so the
    // last-group check refuses before the group-zero check is reached.
    let error = compute_fs_layout(DEFAULT_INODE_SIZE, 3, FOUR_KIB)
        .err()
        .expect("three blocks are too few");

    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "mkfs:last_group_metadata"
        })
    );
}

#[test]
fn every_group_holds_at_least_the_reserved_inodes() {
    // With 1 KiB inodes in 1 KiB blocks, a final group of twelve blocks keeps two for its bitmaps
    // and ten for an inode table of exactly the ten reserved inodes.
    let groups_before_the_last = 1 + 2 * 8 * KIB as u64;
    let layout = compute_fs_layout(1024, groups_before_the_last + 12, KIB as u32)
        .expect("ten inodes a group is enough");
    assert_eq!(layout.groups, 3);
    assert_eq!(layout.inodes_per_group, RESERVED_INODES);

    let error = compute_fs_layout(1024, groups_before_the_last + 11, KIB as u32)
        .err()
        .expect("nine inodes a group are too few");
    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "mkfs:inode_table"
        })
    );
}

#[test]
fn a_twentieth_of_the_blocks_is_reserved() {
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, 32768, FOUR_KIB).expect("one full group");

    assert_eq!(layout.reserved_blocks, 32768 / 20);
}

#[test]
fn group_zero_metadata_may_fill_the_whole_group() {
    // The superblock, one descriptor block, two bitmaps and a 512-block inode table make 516.
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, 516, FOUR_KIB).expect("an exact fit");
    assert_eq!(layout.group0_metadata_blocks, 516);

    let error = compute_fs_layout(DEFAULT_INODE_SIZE, 515, FOUR_KIB)
        .err()
        .expect("one block short");
    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "mkfs:group0_metadata"
        })
    );
}

#[test]
fn each_geometry_rule_refuses_on_its_own() {
    // A block size in range that is not a power of two.
    assert!(validate_mkfs_geometry(256, 3072).is_err());
    // An inode smaller than the original 128 bytes, though a power of two.
    assert!(validate_mkfs_geometry(64, 1024).is_err());
    // An inode size that is not a power of two.
    assert!(validate_mkfs_geometry(384, 1024).is_err());
    // An inode larger than the block.
    assert!(validate_mkfs_geometry(2048, 1024).is_err());

    // The edges of each rule are accepted.
    assert!(validate_mkfs_geometry(GOOD_OLD_INODE_SIZE, 1024).is_ok());
    assert!(validate_mkfs_geometry(1024, 1024).is_ok());
}

#[test]
fn superblock_carries_free_blocks_past_32_bits() {
    let total_blocks = 1 << 33;
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, total_blocks, FOUR_KIB)
        .expect("an 8 Gi-block layout is representable");
    let superblock = build_superblock(total_blocks, &layout);
    let descriptor_free_blocks: u64 = (0..layout.groups)
        .map(|group_id| {
            u64::from(build_uninit_group_desc(&superblock, group_id, &layout).free_blocks_count())
        })
        .sum();

    assert!(descriptor_free_blocks > u64::from(u32::MAX));
    assert_eq!(superblock.free_blocks_count(), descriptor_free_blocks);
}

#[test]
fn the_backup_descriptor_table_must_fit_the_reserved_area() {
    // One 4 KiB descriptor block holds 64 descriptors of 64 bytes.
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, 32768, FOUR_KIB).expect("one full group");
    let superblock = build_superblock(32768, &layout);
    let mut device = device(1);
    let descriptors = |count| (0..count).map(|_| Ext4GroupDesc::default()).collect();

    write_gdt_redundant_backup(&mut device, &descriptors(64), &superblock, 1, &layout)
        .expect("64 descriptors fill the block exactly");
    assert_eq!(
        write_gdt_redundant_backup(&mut device, &descriptors(65), &superblock, 1, &layout),
        Err(Ext4Error::buffer_too_small(4096, 65 * 64))
    );
}

#[test]
fn the_backup_descriptor_table_follows_the_backup_superblock() {
    // Group 1 starts at block 8193, holds the backup superblock there and the backup descriptor
    // table in the next block.
    let mut device = made(1 + 2 * 8 * KIB);
    for group in 0..2 {
        let primary = descriptor(&mut device, 2, group);
        let backup = descriptor(&mut device, 8194, group);
        assert_eq!(backup.block_bitmap(), primary.block_bitmap());
        assert_eq!(backup.inode_bitmap(), primary.inode_bitmap());
        assert_eq!(backup.inode_table(), primary.inode_table());
    }
}

#[test]
fn a_descriptor_may_end_its_block() {
    // Sixteen descriptors fill a 1 KiB block, so the sixteenth ends exactly at the block's end.
    let mut device = made(1 + 2 * 8 * KIB);
    let mut desc = descriptor(&mut device, 2, 0);

    write_group_desc(&mut device, 15, &mut desc).expect("the last slot of the block");
    assert_eq!(
        descriptor(&mut device, 2, 15).inode_table(),
        desc.inode_table()
    );
}

#[test]
fn group_zero_descriptor_records_its_initialized_layout() {
    let total_blocks = 8 * KIB + 1;
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, total_blocks as u64, KIB as u32)
        .expect("one full group");
    let superblock = build_superblock(total_blocks as u64, &layout);
    let mut device = device(total_blocks);
    device.set_filesystem_block_size(KIB).expect("block size");
    write_superblock(&mut device, &superblock).expect("superblock");

    initialize_group_0(&mut device, &layout).expect("group zero");

    let desc = descriptor(&mut device, 2, 0);
    let free_inodes = layout.inodes_per_group - RESERVED_INODES;
    assert!(desc.is_inode_table_zeroed());
    assert_eq!(
        desc.free_blocks_count(),
        group_free_blocks(&layout, 0, layout.group0_metadata_blocks)
    );
    assert_eq!(desc.free_inodes_count(), free_inodes);
    assert_eq!(desc.itable_unused(), free_inodes);
    assert_eq!(desc.block_bitmap(), u64::from(layout.group0_block_bitmap));
    assert_eq!(desc.inode_bitmap(), u64::from(layout.group0_inode_bitmap));
    assert_eq!(desc.inode_table(), u64::from(layout.group0_inode_table));
}

fn bit(bitmap: &[u8], index: u32) -> bool {
    bitmap[(index / 8) as usize] & (1 << (index % 8)) != 0
}

fn read_block(device: &mut Jbd2Dev<MemoryDevice>, block: u64) -> Vec<u8> {
    let mut bytes = vec![0; KIB];
    device
        .read_device_bytes(block * KIB as u64, &mut bytes)
        .expect("read block");
    bytes
}

/// A 1 KiB-block layout whose second and last group holds 1000 blocks.
fn partial_second_group() -> (u64, FsLayoutInfo) {
    let total_blocks = 1 + 8 * KIB as u64 + 1000;
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, total_blocks, KIB as u32)
        .expect("a partial second group");
    (total_blocks, layout)
}

#[test]
fn group_block_counts_end_at_the_last_block() {
    let (_, layout) = partial_second_group();

    assert_eq!(group_blocks_count(&layout, 0), 8 * KIB as u32);
    assert_eq!(group_blocks_count(&layout, 1), 1000);
    assert_eq!(group_blocks_count(&layout, 2), 0);
}

#[test]
fn a_marked_range_reaches_the_end_of_the_bitmap() {
    let mut bitmap = [0; 4];
    mark_bitmap_range_allocated(&mut bitmap, 0, 40);

    assert_eq!(bitmap, [0xFF; 4]);
}

#[test]
fn the_superblock_names_the_first_inode_after_the_reserved_ones() {
    let (total_blocks, layout) = partial_second_group();

    assert_eq!(
        build_superblock(total_blocks, &layout).s_first_ino,
        RESERVED_INODES + 1
    );
}

#[test]
fn only_group_zero_starts_with_the_reserved_inodes_used() {
    let (total_blocks, layout) = partial_second_group();
    let superblock = build_superblock(total_blocks, &layout);
    let group_zero = build_uninit_group_desc(&superblock, 0, &layout);
    let group_one = build_uninit_group_desc(&superblock, 1, &layout);

    assert_eq!(
        group_zero.free_inodes_count(),
        layout.inodes_per_group - RESERVED_INODES
    );
    assert_eq!(group_one.free_inodes_count(), layout.inodes_per_group);
}

#[test]
fn group_zero_inode_bitmap_marks_the_reserved_inodes_and_the_padding() {
    let total_blocks = 8 * KIB + 1;
    let layout = compute_fs_layout(DEFAULT_INODE_SIZE, total_blocks as u64, KIB as u32)
        .expect("one full group");
    let superblock = build_superblock(total_blocks as u64, &layout);
    let mut device = device(total_blocks);
    device.set_filesystem_block_size(KIB).expect("block size");
    write_superblock(&mut device, &superblock).expect("superblock");

    initialize_group_0(&mut device, &layout).expect("group zero");

    let bitmap = read_block(&mut device, u64::from(layout.group0_inode_bitmap));
    assert!(layout.inodes_per_group < 8 * KIB as u32);
    for index in 0..8 * KIB as u32 {
        let used = index < RESERVED_INODES || index >= layout.inodes_per_group;
        assert_eq!(bit(&bitmap, index), used, "inode bit {index}");
    }
}

#[test]
fn later_groups_mark_their_metadata_and_padding_only() {
    let (total_blocks, layout) = partial_second_group();
    let superblock = build_superblock(total_blocks, &layout);
    let mut device = device(total_blocks as usize);
    device.set_filesystem_block_size(KIB).expect("block size");
    write_superblock(&mut device, &superblock).expect("superblock");

    initialize_other_groups_bitmaps(&mut device, &layout, &superblock).expect("group one");

    let group = calc_group_layout(
        1,
        &superblock,
        layout.blocks_per_group,
        layout.inode_table_blocks,
        layout.group0_block_bitmap,
        layout.group0_inode_bitmap,
        layout.group0_inode_table,
        layout.gdt_blocks,
    );
    let blocks = read_block(&mut device, group.group_block_bitmap_start_block);
    let inodes = read_block(&mut device, group.group_inode_bitmap_start_block);
    assert!(group.metadata_blocks_in_group < 1000);
    for index in 0..8 * KIB as u32 {
        let used = index < group.metadata_blocks_in_group || index >= 1000;
        assert_eq!(bit(&blocks, index), used, "block bit {index}");
        assert_eq!(
            bit(&inodes, index),
            index >= layout.inodes_per_group,
            "inode bit {index}"
        );
    }
}
