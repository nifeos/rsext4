//! Tests that pin the allocator's bookkeeping: the high halves of the free
//! counts, the inode table's unused tail, the checksum checks on the bitmaps,
//! the errors it passes on and the ones it absorbs, and the group searches.
//!
//! Each test builds the filesystem state directly. Every group has its block
//! bitmap at `10 + 4 * group`, its inode bitmap one block later and its inode
//! table one block after that, with 4 KiB blocks.

use super::*;

const DEVICE_BLOCKS: u64 = 64;

/// One more than the largest value the low half of a group counter holds.
const HIGH_HALF: u32 = 1 << 16;

fn filesystem(groups: u32, blocks_per_group: u32, inodes_per_group: u32) -> Ext4FileSystem {
    let sb = Ext4Superblock {
        s_inodes_count: groups * inodes_per_group,
        s_free_inodes_count: groups * inodes_per_group,
        s_blocks_count_lo: groups * blocks_per_group,
        s_free_blocks_count_lo: groups * blocks_per_group,
        s_blocks_per_group: blocks_per_group,
        s_inodes_per_group: inodes_per_group,
        s_log_block_size: 2,
        s_first_ino: 1,
        s_inode_size: Ext4Inode::LARGE_INODE_SIZE,
        ..Default::default()
    };
    let group_descs: Vec<Ext4GroupDesc> = (0..groups)
        .map(|group| Ext4GroupDesc {
            bg_block_bitmap_lo: 10 + 4 * group,
            bg_inode_bitmap_lo: 11 + 4 * group,
            bg_inode_table_lo: 12 + 4 * group,
            bg_free_blocks_count_lo: blocks_per_group as u16,
            bg_free_inodes_count_lo: inodes_per_group as u16,
            bg_itable_unused_lo: inodes_per_group as u16,
            bg_flags: Ext4GroupDesc::EXT4_BG_INODE_ZEROED,
            ..Default::default()
        })
        .collect();
    Ext4FileSystem {
        superblock: sb,
        superblock_dirty: false,
        dirty_group_descs: vec![false; groups as usize],
        group_descs,
        block_allocator: BlockAllocator::new(&sb),
        inode_allocator: InodeAllocator::new(&sb),
        bitmap_cache: BitmapCache::create_default(),
        inodetable_cache: InodeCache::default(sb.s_inode_size),
        datablock_cache: DataBlockCache::new(DATABLOCK_CACHE_MAX, BLOCK_SIZE),
        root_inode: InodeNumber::new(2).unwrap(),
        group_count: groups,
        mounted: true,
        mmp: Default::default(),
        journal_sb_block_start: None,
        system_zones: SystemZoneMap::default(),
        pending_shrinks: ::alloc::collections::BTreeSet::new(),
    }
}

fn device() -> Jbd2Dev<MemBlockDev> {
    Jbd2Dev::initial_jbd2dev(0, MemBlockDev::new(DEVICE_BLOCKS), false)
}

fn set_bitmap(dev: &mut Jbd2Dev<MemBlockDev>, block: u64, first_byte: u8) {
    dev.read_block(AbsoluteBN::new(block)).unwrap();
    dev.buffer_mut().fill(0);
    dev.buffer_mut()[0] = first_byte;
    dev.write_block(AbsoluteBN::new(block), true).unwrap();
}

fn with_metadata_checksums(fs: &mut Ext4FileSystem) {
    fs.superblock.s_feature_ro_compat |= Ext4Superblock::EXT4_FEATURE_RO_COMPAT_METADATA_CSUM;
}

/// Stores the checksums of the bitmaps as they are on the device.
fn seal_bitmaps(fs: &mut Ext4FileSystem, dev: &mut Jbd2Dev<MemBlockDev>, group: usize) {
    let sb = fs.superblock;
    let desc = fs.group_descs[group];
    dev.read_block(AbsoluteBN::new(desc.block_bitmap()))
        .unwrap();
    let blocks = dev.buffer().to_vec();
    dev.read_block(AbsoluteBN::new(desc.inode_bitmap()))
        .unwrap();
    let inodes = dev.buffer().to_vec();
    fs.group_descs[group].update_bitmap_checksums(&sb, Some(&blocks), Some(&inodes));
}

fn set_group_free_blocks(fs: &mut Ext4FileSystem, group: usize, count: u32) {
    fs.group_descs[group].bg_free_blocks_count_lo = (count & 0xFFFF) as u16;
    fs.group_descs[group].bg_free_blocks_count_hi = (count >> 16) as u16;
}

fn set_group_free_inodes(fs: &mut Ext4FileSystem, group: usize, count: u32) {
    fs.group_descs[group].bg_free_inodes_count_lo = (count & 0xFFFF) as u16;
    fs.group_descs[group].bg_free_inodes_count_hi = (count >> 16) as u16;
}

fn set_superblock_free_blocks(fs: &mut Ext4FileSystem, count: u64) {
    fs.superblock.s_free_blocks_count_lo = (count & 0xFFFF_FFFF) as u32;
    fs.superblock.s_free_blocks_count_hi = (count >> 32) as u32;
}

#[test]
fn allocating_a_block_keeps_the_high_halves_of_both_free_counts() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    set_bitmap(&mut dev, 10, 0b0000_0111);
    set_group_free_blocks(&mut fs, 0, HIGH_HALF + 13);
    set_superblock_free_blocks(&mut fs, (1 << 32) + 13);

    assert_eq!(fs.alloc_block(&mut dev).unwrap(), AbsoluteBN::new(3));
    assert_eq!(fs.group_descs[0].free_blocks_count(), HIGH_HALF + 12);
    assert_eq!(fs.superblock.free_blocks_count(), (1 << 32) + 12);
}

#[test]
fn allocating_blocks_checks_the_bitmap_checksum_of_an_initialized_group() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    with_metadata_checksums(&mut fs);
    set_bitmap(&mut dev, 10, 0b0000_0001);

    let error = fs.alloc_block(&mut dev).unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
    assert_eq!(fs.group_descs[0].free_blocks_count(), 16);

    seal_bitmaps(&mut fs, &mut dev, 0);
    assert_eq!(fs.alloc_block(&mut dev).unwrap(), AbsoluteBN::new(1));
}

#[test]
fn allocating_an_inode_checks_the_bitmap_checksum_of_an_initialized_group() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    with_metadata_checksums(&mut fs);
    set_bitmap(&mut dev, 11, 0b0000_0001);

    let error = fs.alloc_inode(&mut dev).unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
    assert_eq!(fs.group_descs[0].free_inodes_count(), 8);

    seal_bitmaps(&mut fs, &mut dev, 0);
    assert_eq!(
        fs.alloc_inode(&mut dev).unwrap(),
        InodeNumber::new(2).unwrap()
    );
}

#[test]
fn an_inode_number_past_the_range_is_reported_rather_than_skipped() {
    // In the third group the first inode number is 2 * 2^31 + 1, which does
    // not fit in 32 bits. The first two groups are full.
    let mut dev = device();
    let mut fs = filesystem(3, 16, 8);
    fs.superblock.s_inodes_per_group = 1 << 31;
    fs.inode_allocator = InodeAllocator::new(&fs.superblock);
    set_group_free_inodes(&mut fs, 0, 0);
    set_group_free_inodes(&mut fs, 1, 0);

    let error = fs.alloc_inodes(&mut dev, 1).unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::Overflow);
}

#[test]
fn allocating_an_inode_updates_the_free_count_the_unused_tail_and_the_flags() {
    let mut dev = device();
    let inodes_per_group = HIGH_HALF + 100;
    let mut fs = filesystem(1, 16, inodes_per_group);
    set_group_free_inodes(&mut fs, 0, inodes_per_group);
    fs.group_descs[0].bg_itable_unused_lo = (inodes_per_group & 0xFFFF) as u16;
    fs.group_descs[0].bg_itable_unused_hi = (inodes_per_group >> 16) as u16;

    assert_eq!(
        fs.alloc_inodes(&mut dev, 1).unwrap(),
        [InodeNumber::new(1).unwrap()]
    );
    let desc = fs.group_descs[0];
    assert_eq!(desc.free_inodes_count(), inodes_per_group - 1);
    assert_eq!(desc.itable_unused(), inodes_per_group - 1);
    assert_eq!(desc.bg_flags, Ext4GroupDesc::EXT4_BG_INODE_ZEROED);
}

#[test]
fn an_inode_below_the_unused_tail_leaves_the_tail_alone() {
    // Inodes 1 and 2 are in use and the tail starts at index 4, so the free
    // inode at index 2 is below it.
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    set_bitmap(&mut dev, 11, 0b0000_0011);
    set_group_free_inodes(&mut fs, 0, 6);
    fs.group_descs[0].bg_itable_unused_lo = 4;

    assert_eq!(
        fs.alloc_inodes(&mut dev, 1).unwrap(),
        [InodeNumber::new(3).unwrap()]
    );
    assert_eq!(fs.group_descs[0].itable_unused(), 4);

    // Index 3 is the last one before the tail: it is used now, and the tail
    // stays where it was.
    assert_eq!(
        fs.alloc_inodes(&mut dev, 1).unwrap(),
        [InodeNumber::new(4).unwrap()]
    );
    assert_eq!(fs.group_descs[0].itable_unused(), 4);

    // Index 4 starts the tail, which then begins one later.
    assert_eq!(
        fs.alloc_inodes(&mut dev, 1).unwrap(),
        [InodeNumber::new(5).unwrap()]
    );
    assert_eq!(fs.group_descs[0].itable_unused(), 3);
}

#[test]
fn freeing_a_block_keeps_the_high_halves_of_both_free_counts() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    set_bitmap(&mut dev, 10, 0b0000_1111);
    set_group_free_blocks(&mut fs, 0, HIGH_HALF + 12);
    set_superblock_free_blocks(&mut fs, (1 << 32) + 12);

    fs.free_block(&mut dev, AbsoluteBN::new(2)).unwrap();
    assert_eq!(fs.group_descs[0].free_blocks_count(), HIGH_HALF + 13);
    assert_eq!(fs.superblock.free_blocks_count(), (1 << 32) + 13);
}

#[test]
fn freeing_a_free_block_changes_nothing() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    set_bitmap(&mut dev, 10, 0b0000_0001);
    set_group_free_blocks(&mut fs, 0, 15);
    set_superblock_free_blocks(&mut fs, 15);

    fs.free_block(&mut dev, AbsoluteBN::new(5)).unwrap();
    assert_eq!(fs.group_descs[0].free_blocks_count(), 15);
    assert_eq!(fs.superblock.free_blocks_count(), 15);
}

#[test]
fn freeing_a_block_past_the_bitmap_is_an_error() {
    // A group of 2^16 blocks needs an 8 KiB bitmap, so a 4 KiB bitmap block
    // cannot hold the bit of block 40000.
    let mut dev = device();
    let mut fs = filesystem(1, HIGH_HALF, 8);

    let error = fs
        .free_block(&mut dev, AbsoluteBN::new(40_000))
        .unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
}

#[test]
fn freeing_a_block_checks_the_bitmap_checksum() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    with_metadata_checksums(&mut fs);
    set_bitmap(&mut dev, 10, 0b0000_0011);
    set_group_free_blocks(&mut fs, 0, 14);

    let error = fs.free_block(&mut dev, AbsoluteBN::new(1)).unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
    assert_eq!(fs.group_descs[0].free_blocks_count(), 14);

    seal_bitmaps(&mut fs, &mut dev, 0);
    fs.free_block(&mut dev, AbsoluteBN::new(1)).unwrap();
    assert_eq!(fs.group_descs[0].free_blocks_count(), 15);
}

#[test]
fn freeing_an_inode_keeps_the_high_half_of_the_group_count() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, HIGH_HALF + 100);
    set_bitmap(&mut dev, 11, 0b0000_0011);
    set_group_free_inodes(&mut fs, 0, HIGH_HALF + 98);
    fs.superblock.s_free_inodes_count = HIGH_HALF + 98;

    fs.free_inode(&mut dev, InodeNumber::new(2).unwrap())
        .unwrap();
    assert_eq!(fs.group_descs[0].free_inodes_count(), HIGH_HALF + 99);
    assert_eq!(fs.superblock.s_free_inodes_count, HIGH_HALF + 99);
}

#[test]
fn freeing_a_free_inode_changes_nothing() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    set_bitmap(&mut dev, 11, 0b0000_0001);
    set_group_free_inodes(&mut fs, 0, 7);
    fs.superblock.s_free_inodes_count = 7;

    fs.free_inode(&mut dev, InodeNumber::new(5).unwrap())
        .unwrap();
    assert_eq!(fs.group_descs[0].free_inodes_count(), 7);
    assert_eq!(fs.superblock.s_free_inodes_count, 7);
}

#[test]
fn freeing_an_inode_past_the_bitmap_is_an_error() {
    // A group of 2^16 inodes needs an 8 KiB bitmap, so a 4 KiB bitmap block
    // cannot hold the bit of inode 40000.
    let mut dev = device();
    let mut fs = filesystem(1, 16, HIGH_HALF);

    let error = fs
        .free_inode(&mut dev, InodeNumber::new(40_000).unwrap())
        .unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
}

#[test]
fn freeing_an_inode_checks_the_bitmap_checksum() {
    let mut dev = device();
    let mut fs = filesystem(1, 16, 8);
    with_metadata_checksums(&mut fs);
    set_bitmap(&mut dev, 11, 0b0000_0011);
    set_group_free_inodes(&mut fs, 0, 6);

    let error = fs
        .free_inode(&mut dev, InodeNumber::new(2).unwrap())
        .unwrap_err();
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
    assert_eq!(fs.group_descs[0].free_inodes_count(), 6);

    seal_bitmaps(&mut fs, &mut dev, 0);
    fs.free_inode(&mut dev, InodeNumber::new(2).unwrap())
        .unwrap();
    assert_eq!(fs.group_descs[0].free_inodes_count(), 7);
}

#[test]
fn the_group_searches_return_the_first_group_with_something_free() {
    let mut fs = filesystem(3, 16, 8);
    set_group_free_blocks(&mut fs, 0, 0);
    set_group_free_inodes(&mut fs, 0, 0);
    set_group_free_inodes(&mut fs, 1, 0);
    assert_eq!(fs.find_group_with_free_blocks(), Some(BGIndex::new(1)));
    assert_eq!(fs.find_group_with_free_inodes(), Some(BGIndex::new(2)));

    for group in 0..3 {
        set_group_free_blocks(&mut fs, group, 0);
        set_group_free_inodes(&mut fs, group, 0);
    }
    assert_eq!(fs.find_group_with_free_blocks(), None);
    assert_eq!(fs.find_group_with_free_inodes(), None);
}
