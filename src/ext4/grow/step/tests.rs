//! Tests that pin the geometry choice, credit count and candidate contents of one growth step.

use super::*;
use crate::error::ErrorContext;
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    mkfs_with_options,
};

const BLOCK: usize = 1024;

struct MemoryDevice {
    bytes: Vec<u8>,
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

/// Makes a 1 KiB-block filesystem of `before` blocks, then extends the device to `after`.
fn grown_device(before: usize, after: usize) -> Jbd2Dev<MemoryDevice> {
    let device = MemoryDevice {
        bytes: vec![0; before * BLOCK],
    };
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs_with_options(
        &mut journal,
        MkfsOptions {
            block_size: BLOCK as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    let mut device = journal.into_inner();
    device.bytes.resize(after * BLOCK, 0);
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    journal
        .set_filesystem_block_size(BLOCK)
        .expect("block size");
    journal
}

const PER_GROUP: u64 = 8192;

fn operation<T>(result: Ext4Result<T>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    match error.context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

fn bit(bytes: &[u8], index: usize) -> bool {
    bytes[index / 8] & (1 << (index % 8)) != 0
}

fn image_of(step: &GrowthStep, block: u64) -> &Vec<u8> {
    step.images
        .get(&AbsoluteBN::new(block))
        .unwrap_or_else(|| panic!("no image for block {block}"))
}

fn superblock_at(bytes: &[u8], offset: usize) -> Ext4Superblock {
    Ext4Superblock::from_disk_bytes(&bytes[offset..offset + SUPERBLOCK_SIZE])
}

fn mounted(before: usize, after: usize) -> (Jbd2Dev<MemoryDevice>, Ext4FileSystem) {
    let mut journal = grown_device(before, after);
    let fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    (journal, fs)
}

// ---- next_geometry ----

#[test]
fn a_partial_tail_is_extended_alone_and_whole_groups_are_batched() {
    let (_journal, fs) = mounted(12000, 30000);
    // Group 1 is published but partial: the step finishes it and nothing more.
    let next = next_geometry(&fs.superblock, 30000, 1_000_000).unwrap();
    assert_eq!(next.blocks_count(), 1 + 2 * PER_GROUP);
    // A target inside the tail stops at the target.
    let next = next_geometry(&fs.superblock, 14000, 1_000_000).unwrap();
    assert_eq!(next.blocks_count(), 14000);
    // With whole groups only, the next step adds all the groups the target allows.
    let mut whole = fs.superblock;
    set_blocks(&mut whole, 1 + 2 * PER_GROUP);
    let next = next_geometry(&whole, 30000, 1_000_000).unwrap();
    assert_eq!(next.blocks_count(), 30000);
}

#[test]
fn the_tail_group_is_found_from_the_first_data_block() {
    let (_journal, fs) = mounted(12000, 30000);
    // Blocks 1..8192 are 8191 data blocks: group 0 is still a partial tail.
    let mut short = fs.superblock;
    set_blocks(&mut short, PER_GROUP);
    let next = next_geometry(&short, 3 * PER_GROUP, 1_000_000).unwrap();
    assert_eq!(next.blocks_count(), 1 + PER_GROUP);
}

#[test]
fn a_batch_is_bounded_by_the_bitmap_budget_of_the_block_size() {
    let (_journal, fs) = mounted(12000, 30000);
    // At 64 KiB blocks the 2 MiB budget allows sixteen groups, below the 256 cap.
    let mut large = fs.superblock;
    large.s_log_block_size = 6;
    large.s_first_data_block = 0;
    large.s_blocks_per_group = 8 * 65536;
    let per_group = u64::from(large.s_blocks_per_group);
    set_blocks(&mut large, per_group);
    let next = next_geometry(&large, 100 * per_group, 1_000_000).unwrap();
    assert_eq!(next.blocks_count(), 17 * per_group);
}

// ---- transaction_credits ----

fn credit_pair(
    old_blocks: u64,
    new_blocks: u64,
    reserved: u16,
) -> (Ext4Superblock, Ext4Superblock) {
    let (_journal, fs) = mounted(12000, 30000);
    let mut old = fs.superblock;
    old.s_reserved_gdt_blocks = reserved;
    set_blocks(&mut old, old_blocks);
    let mut new = old;
    set_blocks(&mut new, new_blocks);
    (old, new)
}

#[test]
fn a_full_last_group_needs_no_tail_bitmap_credit() {
    // Two whole groups: two superblock copies, each with one changed GDT block.
    let (old, new) = credit_pair(1 + 2 * PER_GROUP, 1 + 3 * PER_GROUP, 0);
    assert_eq!(transaction_credits(&old, &new).unwrap(), 4);
    // A partial last group adds its bitmap.
    let (old, new) = credit_pair(12000, 1 + 3 * PER_GROUP, 0);
    assert_eq!(transaction_credits(&old, &new).unwrap(), 5);
}

#[test]
fn the_resize_inode_is_charged_only_when_reserved_blocks_exist_and_change() {
    // Without reserved GDT blocks nothing is charged for inode 7, even when
    // the GDT grows past one block (16 descriptors per block at 1 KiB).
    let (old, new) = credit_pair(1 + 16 * PER_GROUP, 1 + 17 * PER_GROUP, 0);
    let plain = transaction_credits(&old, &new).unwrap();
    let (old_reserved, new_reserved) = credit_pair(1 + 16 * PER_GROUP, 1 + 17 * PER_GROUP, 4);
    let charged = transaction_credits(&old_reserved, &new_reserved).unwrap();
    // Inode 7, plus one for the changed GDT block count.
    assert!(charged >= plain + 2);
    // With reserved blocks but neither a new GDT block nor a new backup there is no charge.
    let (old, new) = credit_pair(1 + 2 * PER_GROUP, 1 + 3 * PER_GROUP, 4);
    assert_eq!(transaction_credits(&old, &new).unwrap(), 4);
}

// ---- prepare_growth_step: extending the published tail group ----

#[test]
fn extending_the_tail_frees_only_the_new_blocks_and_clears_its_uninitialized_flag() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let old = fs.superblock;
    let uninitialized = Ext4GroupDesc::EXT4_BG_BLOCK_UNINIT;
    let zeroed = Ext4GroupDesc::EXT4_BG_INODE_ZEROED;
    fs.group_descs[1].bg_flags = uninitialized | zeroed;
    let free_before = fs.group_descs[1].free_blocks_count();
    let step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    let candidate = &step.superblock;
    assert_eq!(candidate.blocks_count(), 14000);
    assert_eq!(step.descriptors.len(), 2);
    // The reserved-block count scales with the new size.
    let reserve = old.reserved_blocks_count() * 14000 / 12000;
    assert_eq!(reserve, 700);
    assert_eq!(candidate.reserved_blocks_count(), 700);
    assert_eq!(candidate.s_overhead_blocks, 0);
    assert_eq!(candidate.s_inodes_count, old.s_inodes_count);
    // The descriptor keeps its other flags and gains only the new blocks.
    let tail = &step.descriptors[1];
    assert_eq!(tail.bg_flags, zeroed);
    assert_eq!(tail.free_blocks_count(), free_before + 2000);
    // The bitmap frees the new blocks and keeps the padding allocated.
    let bitmap = image_of(&step, tail.block_bitmap());
    let valid = (14000 - (PER_GROUP + 1)) as usize;
    for index in (12000 - (PER_GROUP + 1)) as usize..valid {
        assert!(!bit(bitmap, index), "bit {index} is a new free block");
    }
    for index in valid..1024 * 8 {
        assert!(bit(bitmap, index), "bit {index} is padding");
    }
    // The totals follow the descriptors.
    let free: u64 = step
        .descriptors
        .iter()
        .map(|descriptor| u64::from(descriptor.free_blocks_count()))
        .sum();
    assert_eq!(candidate.free_blocks_count(), free);
    let free_inodes: u32 = step
        .descriptors
        .iter()
        .map(|descriptor| descriptor.free_inodes_count())
        .sum();
    assert_eq!(candidate.s_free_inodes_count, free_inodes);
}

#[test]
fn every_group_with_a_superblock_gets_its_own_copy_and_descriptor_table() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    // The primary copy sits at byte 1024 of the device.
    let primary = superblock_at(image_of(&step, 1), 0);
    assert_eq!(primary.s_block_group_nr, 0);
    assert_eq!(primary.blocks_count(), 14000);
    // The backup in group 1 sits at the group's first block, from offset 0.
    let backup = superblock_at(image_of(&step, PER_GROUP + 1), 0);
    assert_eq!(backup.s_block_group_nr, 1);
    assert_eq!(backup.blocks_count(), 14000);
    // The backup descriptor table is a copy of the primary one, right after it.
    assert_eq!(image_of(&step, PER_GROUP + 2), image_of(&step, 2));
    let slot = 64;
    let raw = &image_of(&step, 2)[slot..2 * slot];
    assert_eq!(
        Ext4GroupDesc::from_disk_bytes(raw).free_blocks_count(),
        step.descriptors[1].free_blocks_count()
    );
}

// ---- prepare_growth_step: new groups ----

#[test]
fn new_groups_get_local_metadata_and_a_full_descriptor() {
    let (mut journal, mut fs) = mounted(2 * PER_GROUP as usize + 1, 30000);
    let old = fs.superblock;
    let step = fs.prepare_growth_step(&mut journal, 30000).unwrap();
    assert_eq!(step.superblock.blocks_count(), 30000);
    assert_eq!(step.descriptors.len(), 4);
    assert_eq!(step.superblock.s_inodes_count, 4 * old.s_inodes_per_group);
    let table_blocks = (u64::from(old.s_inodes_per_group) * 256).div_ceil(1024);
    assert_eq!(table_blocks, 512);
    // Group 2 has no superblock copy; group 3 has one and a descriptor table.
    let cases = [
        (2usize, 16385u64, 0u64, 16385u64 + PER_GROUP),
        (3, 24577, 2, 30000),
    ];
    for (group, start, base, end) in cases {
        let descriptor = &step.descriptors[group];
        let metadata = base + 2 + table_blocks;
        assert_eq!(descriptor.block_bitmap(), start + base);
        assert_eq!(descriptor.inode_bitmap(), start + base + 1);
        assert_eq!(descriptor.inode_table(), start + base + 2);
        assert_eq!(
            u64::from(descriptor.free_blocks_count()),
            end - start - metadata
        );
        assert_eq!(descriptor.free_inodes_count(), old.s_inodes_per_group);
        assert_eq!(
            descriptor.bg_itable_unused_lo,
            old.s_inodes_per_group as u16
        );
        assert_eq!(descriptor.bg_flags, Ext4GroupDesc::EXT4_BG_INODE_ZEROED);
        assert!(
            step.new_inode_tables
                .contains(&(AbsoluteBN::new(start + base + 2), table_blocks))
        );
        // Block bitmap: metadata allocated, data free, padding allocated.
        let bitmap = image_of(&step, start + base);
        let valid = (end - start) as usize;
        for index in 0..metadata as usize {
            assert!(bit(bitmap, index), "bit {index} is metadata");
        }
        assert!(!bit(bitmap, metadata as usize));
        assert!(!bit(bitmap, valid - 1));
        for index in valid..1024 * 8 {
            assert!(bit(bitmap, index), "bit {index} is padding");
        }
        // Inode bitmap: every real inode free, padding allocated.
        let inodes = image_of(&step, start + base + 1);
        assert!(!bit(inodes, 0));
        assert!(!bit(inodes, old.s_inodes_per_group as usize - 1));
        assert!(bit(inodes, old.s_inodes_per_group as usize));
    }
    // The backup in group 3 is a whole copy placed after the superblock.
    let backup = superblock_at(image_of(&step, 24577), 0);
    assert_eq!(backup.s_block_group_nr, 3);
    assert_eq!(image_of(&step, 24578), image_of(&step, 2));
    // The descriptors reach the descriptor table block with the group's slot.
    let table = image_of(&step, 2);
    for group in [2usize, 3] {
        let raw = &table[group * 64..(group + 1) * 64];
        assert_eq!(
            Ext4GroupDesc::from_disk_bytes(raw).block_bitmap(),
            step.descriptors[group].block_bitmap()
        );
    }
}

#[test]
fn a_group_too_small_for_its_own_metadata_is_refused() {
    let (mut journal, mut fs) = mounted(2 * PER_GROUP as usize + 1, 30000);
    // Ten blocks cannot hold two bitmaps and an inode table.
    let target = 2 * PER_GROUP + 1 + 10;
    assert_eq!(
        operation(fs.prepare_growth_step(&mut journal, target)),
        "grow:small_group"
    );
}

#[test]
fn a_group_exactly_the_size_of_its_metadata_is_accepted() {
    let (mut journal, mut fs) = mounted(2 * PER_GROUP as usize + 1, 30000);
    let target = 2 * PER_GROUP + 1 + 2 + 512;
    let step = fs.prepare_growth_step(&mut journal, target).unwrap();
    assert_eq!(step.descriptors[2].free_blocks_count(), 0);
}

// ---- publish ----

#[test]
fn publishing_installs_the_candidate_in_the_mount() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    let expected = step.superblock.blocks_count();
    step.publish(&mut fs, &mut journal).unwrap();
    assert_eq!(fs.superblock.blocks_count(), expected);
    assert_eq!(fs.group_count, 2);
    assert_eq!(fs.group_descs.len(), 2);
}

#[test]
fn publishing_refuses_an_image_count_beyond_the_computed_credits() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    step.images.insert(AbsoluteBN::new(5000), vec![0; BLOCK]);
    assert_eq!(
        operation(step.publish(&mut fs, &mut journal)),
        "grow:step_credit_accounting"
    );
}

#[test]
fn publishing_accepts_fewer_images_than_the_computed_credits() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    // Dropping the backup descriptor table leaves fewer images than credits.
    step.images.remove(&AbsoluteBN::new(PER_GROUP + 2));
    step.publish(&mut fs, &mut journal).unwrap();
}

#[test]
fn publishing_refuses_credits_beyond_the_journal_limit_but_accepts_the_limit() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let limit = journal.transaction_credit_limit().unwrap().unwrap();
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    // Pretend a very large geometry so the credit accounting allows many images.
    let mut huge = step.superblock;
    set_blocks(&mut huge, 11_000 * PER_GROUP);
    assert!(transaction_credits(&fs.superblock, &huge).unwrap() > limit + 1);
    step.superblock = huge;
    step.images.clear();
    for block in 0..=limit as u64 {
        step.images
            .insert(AbsoluteBN::new(PER_GROUP + 700 + block), vec![0; BLOCK]);
    }
    assert_eq!(step.images.len(), limit + 1);
    assert_eq!(
        operation(step.publish(&mut fs, &mut journal)),
        "grow:step_transaction_capacity"
    );
}

#[test]
fn publishing_accepts_exactly_the_journal_limit() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let limit = journal.transaction_credit_limit().unwrap().unwrap();
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    let mut huge = step.superblock;
    set_blocks(&mut huge, 11_000 * PER_GROUP);
    step.superblock = huge;
    step.images.clear();
    for block in 0..limit as u64 {
        step.images
            .insert(AbsoluteBN::new(PER_GROUP + 700 + block), vec![0; BLOCK]);
    }
    assert_eq!(step.images.len(), limit);
    step.publish(&mut fs, &mut journal).unwrap();
}

#[test]
fn an_inode_table_inside_the_old_geometry_is_refused() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let old_blocks = fs.superblock.blocks_count();
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    step.new_inode_tables
        .push((AbsoluteBN::new(old_blocks - 1), 1));
    assert_eq!(
        operation(step.publish(&mut fs, &mut journal)),
        "grow:published_inode_table"
    );
}

#[test]
fn an_inode_table_starting_at_the_old_end_is_published() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let old_blocks = fs.superblock.blocks_count();
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    step.new_inode_tables.push((AbsoluteBN::new(old_blocks), 1));
    step.publish(&mut fs, &mut journal).unwrap();
}

#[test]
fn a_tail_bitmap_keeps_its_metadata_bits_and_allocates_its_padding() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    // Start from an all-zero on-disk bitmap that the step must rebuild.
    let tail = &mut fs.group_descs[1];
    tail.bg_flags |= Ext4GroupDesc::EXT4_BG_BLOCK_UNINIT;
    let block = AbsoluteBN::new(tail.block_bitmap());
    journal
        .write_unpublished_blocks(&[0; BLOCK], block, 1)
        .unwrap();
    let step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    let bitmap = image_of(&step, block.raw());
    // Backup superblock, descriptor table, two bitmaps and the inode table.
    for index in 0..2 + 2 + 512 {
        assert!(bit(bitmap, index), "bit {index} is metadata");
    }
    let valid = (14000 - (PER_GROUP + 1)) as usize;
    assert!(!bit(bitmap, valid - 1));
    for index in valid..1024 * 8 {
        assert!(bit(bitmap, index), "bit {index} is padding");
    }
}

#[test]
fn a_checksummed_tail_bitmap_gets_its_padding_allocated() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    // An empty bitmap with a matching checksum, so the step takes the verified path.
    let old = fs.superblock;
    let empty = vec![0; BLOCK];
    let block = AbsoluteBN::new(fs.group_descs[1].block_bitmap());
    fs.group_descs[1].update_bitmap_checksums(&old, Some(&empty), None);
    journal.write_unpublished_blocks(&empty, block, 1).unwrap();
    let step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    let bitmap = image_of(&step, block.raw());
    let valid = (14000 - (PER_GROUP + 1)) as usize;
    for index in 0..valid {
        assert!(!bit(bitmap, index), "bit {index} is data");
    }
    for index in valid..1024 * 8 {
        assert!(bit(bitmap, index), "bit {index} is padding");
    }
}

#[test]
fn the_tail_is_found_when_the_old_end_is_on_a_group_boundary() {
    // 16384 blocks leave 16383 data blocks: the second group is a partial tail.
    let (mut journal, mut fs) = mounted(2 * PER_GROUP as usize, 30000);
    let free_before = fs.group_descs[1].free_blocks_count();
    let step = fs.prepare_growth_step(&mut journal, 30000).unwrap();
    assert_eq!(step.superblock.blocks_count(), 1 + 2 * PER_GROUP);
    assert_eq!(step.descriptors.len(), 2);
    assert_eq!(step.descriptors[1].free_blocks_count(), free_before + 1);
}

// ---- prepare_growth_step: a second descriptor block and reserved blocks ----

/// Moves the bitmaps of every group that keeps a superblock copy out of the
/// area that `reserved` descriptor-table blocks would occupy, in the mount's view.
fn reserve_descriptor_blocks(fs: &mut Ext4FileSystem, reserved: u16) {
    fs.superblock.s_reserved_gdt_blocks = reserved;
    for group in 0..fs.group_count {
        if group_has_superblock(&fs.superblock, group) {
            let start = group_first_block(&fs.superblock, group).unwrap();
            let descriptor = &mut fs.group_descs[group as usize];
            descriptor.bg_block_bitmap_lo = (start + 4000) as u32;
            descriptor.bg_inode_bitmap_lo = (start + 4001) as u32;
        }
    }
}

#[test]
fn a_step_past_sixteen_groups_adds_a_descriptor_block_and_backups() {
    let (mut journal, mut fs) = mounted(1 + 16 * PER_GROUP as usize, 1 + 26 * PER_GROUP as usize);
    reserve_descriptor_blocks(&mut fs, 2);
    // Stale bytes where the new descriptor block goes must not survive.
    journal
        .write_unpublished_blocks(&[0xFF; BLOCK], AbsoluteBN::new(3), 1)
        .unwrap();
    let target = 1 + 26 * PER_GROUP;
    let step = fs.prepare_growth_step(&mut journal, target).unwrap();
    let candidate = &step.superblock;
    assert_eq!(candidate.blocks_count(), target);
    assert_eq!(step.descriptors.len(), 26);
    // One reserved block became a live descriptor block.
    assert_eq!(candidate.s_reserved_gdt_blocks, 1);
    // The new descriptors land in the second descriptor block, in slot order.
    let second = image_of(&step, 3);
    for (group, descriptor) in step.descriptors.iter().enumerate().skip(16) {
        let slot = (group % 16) * 64;
        let raw = &second[slot..slot + 64];
        assert_eq!(
            Ext4GroupDesc::from_disk_bytes(raw).block_bitmap(),
            descriptor.block_bitmap(),
            "group {group}"
        );
    }
    // Unused slots of the new descriptor block are zero, not stale reserved bytes.
    assert!(second[10 * 64..].iter().all(|byte| *byte == 0));
    // A group without a superblock starts with its bitmaps; group 25 keeps
    // the superblock, both descriptor blocks and the remaining reserve first.
    let plain = 1 + 16 * PER_GROUP;
    assert_eq!(step.descriptors[16].block_bitmap(), plain);
    let backup = 1 + 25 * PER_GROUP;
    assert_eq!(step.descriptors[25].block_bitmap(), backup + 1 + 2 + 1);
    assert_eq!(step.descriptors[25].inode_bitmap(), backup + 1 + 2 + 2);
    // The remaining reserved block of the new backup group is staged zeroed.
    assert_eq!(image_of(&step, backup + 3), &vec![0; BLOCK]);
    // The new backup holds the superblock and both descriptor blocks.
    assert_eq!(
        superblock_at(image_of(&step, backup), 0).s_block_group_nr,
        25
    );
    journal.read_block(AbsoluteBN::new(2)).unwrap();
    assert_eq!(image_of(&step, backup + 1), &journal.buffer().to_vec());
    assert_eq!(image_of(&step, backup + 2), second);
    // Existing backups gain only the changed descriptor block.
    for group in [1u64, 3, 5, 7, 9] {
        let base = 1 + group * PER_GROUP;
        assert!(!step.images.contains_key(&AbsoluteBN::new(base + 1)));
        assert_eq!(image_of(&step, base + 2), second);
        assert_eq!(
            superblock_at(image_of(&step, base), 0).s_block_group_nr,
            group as u16
        );
    }
}

#[test]
fn a_group_with_a_superblock_places_its_bitmaps_after_the_reserved_blocks() {
    let (mut journal, mut fs) = mounted(2 * PER_GROUP as usize + 1, 30000);
    reserve_descriptor_blocks(&mut fs, 2);
    let step = fs.prepare_growth_step(&mut journal, 30000).unwrap();
    // Group 3: superblock, one descriptor block, two reserved blocks, bitmaps.
    let start = 1 + 3 * PER_GROUP;
    assert_eq!(step.descriptors[3].block_bitmap(), start + 4);
    assert_eq!(step.descriptors[3].inode_bitmap(), start + 5);
    assert_eq!(step.descriptors[3].inode_table(), start + 6);
    assert_eq!(image_of(&step, start + 2), &vec![0; BLOCK]);
    assert_eq!(image_of(&step, start + 3), &vec![0; BLOCK]);
    // Group 2 has no superblock and no reserved blocks.
    assert_eq!(step.descriptors[2].block_bitmap(), 1 + 2 * PER_GROUP);
    assert!(
        !step
            .images
            .contains_key(&AbsoluteBN::new(start + 4 + 2 * PER_GROUP))
    );
}

// ---- next_geometry: fitting the journal budget ----

fn candidate_with_groups(old: &Ext4Superblock, groups: u64) -> Ext4Superblock {
    let mut next = *old;
    let end = group_first_block(old, (old.blocks_count() / PER_GROUP) as u32).unwrap()
        + groups * PER_GROUP;
    set_blocks(&mut next, end);
    let delta = next.checked_block_groups_count().unwrap().div_ceil(16)
        - old.checked_block_groups_count().unwrap().div_ceil(16);
    next.s_reserved_gdt_blocks -= delta as u16;
    next
}

#[test]
fn a_batch_is_halved_until_its_credits_fit() {
    let (_journal, fs) = mounted(12000, 30000);
    let mut old = fs.superblock;
    old.s_reserved_gdt_blocks = 100;
    set_blocks(&mut old, 1 + 2 * PER_GROUP);
    let target = 1 + 600 * PER_GROUP;
    let full = candidate_with_groups(&old, 256);
    let half = candidate_with_groups(&old, 128);
    let quarter = candidate_with_groups(&old, 64);
    let wide = transaction_credits(&old, &full).unwrap();
    let narrow = transaction_credits(&old, &half).unwrap();
    let narrower = transaction_credits(&old, &quarter).unwrap();
    assert!(narrower < narrow && narrow < wide);
    let next = next_geometry(&old, target, wide).unwrap();
    assert_eq!(next.blocks_count(), full.blocks_count());
    let next = next_geometry(&old, target, wide - 1).unwrap();
    assert_eq!(next.blocks_count(), half.blocks_count());
    let next = next_geometry(&old, target, narrow - 1).unwrap();
    assert_eq!(next.blocks_count(), quarter.blocks_count());
}

#[test]
fn a_single_group_that_cannot_fit_the_budget_is_unsupported() {
    let (_journal, fs) = mounted(12000, 30000);
    let mut old = fs.superblock;
    set_blocks(&mut old, 1 + 2 * PER_GROUP);
    assert_eq!(
        operation(next_geometry(&old, 30000, 0)),
        "grow:transaction_capacity"
    );
    let mut tail = fs.superblock;
    set_blocks(&mut tail, 12000);
    assert_eq!(
        operation(next_geometry(&tail, 30000, 0)),
        "grow:transaction_capacity"
    );
}

// ---- transaction_credits: first group and descriptor blocks ----

#[test]
fn credits_follow_the_partial_tail_and_the_changed_descriptor_blocks() {
    // The last group of 17 * 8192 blocks holds 8191 data blocks, so it is a tail.
    let (old, new) = credit_pair(17 * PER_GROUP, 1 + 18 * PER_GROUP, 0);
    assert_eq!(transaction_credits(&old, &new).unwrap(), 13);
}

#[test]
fn a_new_backup_with_reserved_blocks_is_charged_for_the_reserve() {
    let (old, new) = credit_pair(1 + 3 * PER_GROUP, 1 + 4 * PER_GROUP, 4);
    // Two copies with one changed descriptor block each, then inode 7 once
    // plus the four reserved blocks the new group-3 backup must point at.
    assert_eq!(transaction_credits(&old, &new).unwrap(), 4 + 1 + 4);
}

// ---- publish: what is written where ----

#[test]
fn publishing_writes_old_blocks_through_the_journal_and_new_blocks_directly() {
    let (mut journal, mut fs) = mounted(12000, 30000);
    let old_blocks = fs.superblock.blocks_count();
    let mut step = fs.prepare_growth_step(&mut journal, 14000).unwrap();
    step.images
        .insert(AbsoluteBN::new(old_blocks), vec![0xAB; BLOCK]);
    step.publish(&mut fs, &mut journal).unwrap();
    journal.read_block(AbsoluteBN::new(old_blocks)).unwrap();
    assert_eq!(journal.buffer(), &vec![0xAB; BLOCK][..]);
    journal.read_block(AbsoluteBN::new(1)).unwrap();
    assert_eq!(superblock_at(journal.buffer(), 0).blocks_count(), 14000);
}
