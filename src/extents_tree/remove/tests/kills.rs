//! Tests that pin how extent removal walks holes, leaf boundaries, deeper trees and
//! block numbers above 32 bits.

use super::*;
use alloc::collections::BTreeMap;

/// One block number above 32 bits, so the high half of every address built from it is
/// nonzero.
const HIGH: u64 = 1 << 32;

fn root_of(inode: &mut Ext4Inode) -> ExtentNode {
    ExtentTree::new(inode, BLOCK_SIZE)
        .load_root_from_inode()
        .unwrap()
}

fn index_entries(node: &ExtentNode) -> Vec<Ext4ExtentIdx> {
    match node {
        ExtentNode::Index { entries, .. } => entries.clone(),
        ExtentNode::Leaf { .. } => panic!("expected an index node"),
    }
}

fn child_address(index: &Ext4ExtentIdx) -> u64 {
    ((index.ei_leaf_hi as u64) << 32) | (index.ei_leaf_lo as u64)
}

fn read_node<B: BlockIo>(dev: &mut Jbd2Dev<B>, block: u64) -> ExtentNode {
    dev.read_block(AbsoluteBN::new(block)).unwrap();
    ExtentTree::parse_node_from_bytes(dev.buffer()).unwrap()
}

fn logical_blocks<B: BlockIo>(inode: &mut Ext4Inode, dev: &mut Jbd2Dev<B>) -> Vec<u32> {
    collect_extents_from_inode(inode, dev)
        .iter()
        .map(|extent| extent.ee_block)
        .collect()
}

fn remove<B: BlockIo>(
    fs: &mut Ext4FileSystem,
    dev: &mut Jbd2Dev<B>,
    inode: &mut Ext4Inode,
    start: u32,
    len: u16,
) -> Ext4Result<()> {
    ExtentTree::new(inode, BLOCK_SIZE).remove_extent(fs, Ext4Extent::new(start, 0, len), dev)
}

/// A root index with exactly two leaves of one-block extents at logical blocks `0..n`.
/// Returns the extents and the key of the second leaf.
fn two_leaf_tree(
    dev: &mut Jbd2Dev<MemBlockDev>,
    fs: &mut Ext4FileSystem,
    inode: &mut Ext4Inode,
) -> (Vec<Ext4Extent>, u32) {
    let mut extents = Vec::new();
    for lbn in 0..2000 {
        let phys = alloc_data_block(fs, dev);
        let _gap = alloc_data_block(fs, dev);
        let extent = Ext4Extent::new(lbn, phys.raw(), 1);
        insert_data_extent(fs, dev, inode, extent);
        extents.push(extent);
        if let ExtentNode::Index { header, entries } = root_of(inode)
            && entries.len() == 2
            && header.eh_depth == 1
        {
            // Fill the second leaf a little so a deletion can run past its first block.
            for lbn in lbn + 1..lbn + 4 {
                let phys = alloc_data_block(fs, dev);
                let _gap = alloc_data_block(fs, dev);
                let extent = Ext4Extent::new(lbn, phys.raw(), 1);
                insert_data_extent(fs, dev, inode, extent);
                extents.push(extent);
            }
            let entries = index_entries(&root_of(inode));
            assert_eq!(entries.len(), 2);
            return (extents, entries[1].ei_block);
        }
    }
    panic!("the tree never grew a second leaf");
}

// ---- holes before the first key and at a leaf boundary ----

#[test]
fn a_deletion_starting_before_the_first_index_key_skips_the_hole() {
    let (mut dev, mut fs) = setup_fs(16 * 1024);
    let mut inode = new_extent_inode();
    insert_n_extents_with_phys_gaps(&mut fs, &mut dev, &mut inode, 6);
    remove(&mut fs, &mut dev, &mut inode, 0, 1).unwrap();
    let entries = index_entries(&root_of(&mut inode));
    assert_eq!(entries[0].ei_block, 1);

    // Logical block 0 is now a hole before the only key, so the walk must start at the
    // first child and skip to block 1.
    remove(&mut fs, &mut dev, &mut inode, 0, 1).unwrap();
    assert_eq!(logical_blocks(&mut inode, &mut dev), vec![2, 3, 4, 5]);
}

#[test]
fn a_hole_at_a_leaf_boundary_moves_the_walk_to_the_next_leaf() {
    let (mut dev, mut fs) = setup_fs(32 * 1024);
    let mut inode = new_extent_inode();
    let (extents, key) = two_leaf_tree(&mut dev, &mut fs, &mut inode);
    let last = extents.len() as u32;

    // Leave logical block key - 1 as a hole at the end of the first leaf.
    remove(&mut fs, &mut dev, &mut inode, key - 1, 1).unwrap();

    // Two allocated blocks counted from the hole are key and key + 1.
    remove(&mut fs, &mut dev, &mut inode, key - 1, 2).unwrap();
    let expected: Vec<u32> = (0..key - 1).chain(key + 2..last).collect();
    assert_eq!(logical_blocks(&mut inode, &mut dev), expected);
    for extent in &extents[key as usize..key as usize + 2] {
        assert!(!bitmap_block_is_allocated(
            &mut fs,
            &mut dev,
            AbsoluteBN::new(extent.start_block())
        ));
    }
    assert!(bitmap_block_is_allocated(
        &mut fs,
        &mut dev,
        AbsoluteBN::new(extents[0].start_block())
    ));
}

#[test]
fn a_deletion_past_the_last_extent_changes_nothing() {
    let (mut dev, mut fs) = setup_fs(32 * 1024);
    let mut inode = new_extent_inode();
    let (extents, key) = two_leaf_tree(&mut dev, &mut fs, &mut inode);
    let before = logical_blocks(&mut inode, &mut dev);

    // Only the blocks from key + 1 to the end exist, and the request asks for more.
    let available = extents.len() as u32 - key - 1;
    let result = remove(&mut fs, &mut dev, &mut inode, key + 1, available as u16 + 1);
    assert!(result.is_err());
    assert_eq!(logical_blocks(&mut inode, &mut dev), before);
    for extent in &extents {
        assert!(bitmap_block_is_allocated(
            &mut fs,
            &mut dev,
            AbsoluteBN::new(extent.start_block())
        ));
    }
}

#[test]
fn a_count_that_runs_out_of_blocks_after_a_leaf_boundary_is_refused() {
    let (mut dev, mut fs) = setup_fs(32 * 1024);
    let mut inode = new_extent_inode();
    let (extents, key) = two_leaf_tree(&mut dev, &mut fs, &mut inode);
    remove(&mut fs, &mut dev, &mut inode, key - 1, 1).unwrap();

    // One more block than the second leaf holds, counted from the hole before it.
    let available = extents.len() as u32 - key;
    let result = remove(&mut fs, &mut dev, &mut inode, key - 1, available as u16 + 1);
    assert!(result.is_err());
}

#[test]
fn a_count_that_outlasts_every_leaf_is_refused() {
    let (mut dev, mut fs) = setup_fs(32 * 1024);
    let mut inode = new_extent_inode();
    let (extents, key) = two_leaf_tree(&mut dev, &mut fs, &mut inode);
    remove(&mut fs, &mut dev, &mut inode, key - 1, 1).unwrap();

    // One more block than the tree holds, so both leaves empty before the count does.
    let allocated = extents.len() as u16 - 1;
    let result = remove(&mut fs, &mut dev, &mut inode, 0, allocated + 1);
    assert!(result.is_err());
}

// ---- phase 3: an emptied index root ----

#[test]
fn emptying_an_index_root_leaves_an_empty_inline_leaf() {
    let (mut dev, mut fs) = setup_fs(16 * 1024);
    let mut inode = new_extent_inode();
    insert_n_extents_with_phys_gaps(&mut fs, &mut dev, &mut inode, 6);
    assert!(matches!(root_of(&mut inode), ExtentNode::Index { .. }));

    remove(&mut fs, &mut dev, &mut inode, 0, 6).unwrap();
    match root_of(&mut inode) {
        ExtentNode::Leaf { header, entries } => {
            assert!(entries.is_empty());
            assert_eq!(header.eh_depth, 0);
            assert_eq!(header.eh_entries, 0);
            // Sixty bytes of inline root less the twelve-byte header hold four extents.
            assert_eq!(header.eh_max, 4);
        }
        ExtentNode::Index { .. } => panic!("expected an inline leaf"),
    }
}

// ---- a tree two levels deep ----

#[test]
fn an_external_index_node_is_written_back_with_its_new_key() {
    let (mut dev, mut fs) = setup_fs(32 * 1024);
    let mut inode = new_extent_inode();
    let mut lbn = 0;
    loop {
        let phys = alloc_data_block(&mut fs, &mut dev);
        let _gap = alloc_data_block(&mut fs, &mut dev);
        insert_data_extent(
            &mut fs,
            &mut dev,
            &mut inode,
            Ext4Extent::new(lbn, phys.raw(), 1),
        );
        lbn += 1;
        if root_of(&mut inode).header().eh_depth == 2 {
            break;
        }
        assert!(lbn < 4000, "the tree never grew a second level");
    }

    let root = index_entries(&root_of(&mut inode));
    let external = child_address(&root[0]);
    let leaves = index_entries(&read_node(&mut dev, external));
    assert!(leaves.len() >= 2);
    let key = leaves[1].ei_block;

    // Removing the first extent of the second leaf changes that leaf's key in the
    // external index node, which must be written back for the next walk to accept it.
    remove(&mut fs, &mut dev, &mut inode, key, 1).unwrap();
    let leaves = index_entries(&read_node(&mut dev, external));
    assert_eq!(leaves[1].ei_block, key + 1);
    remove(&mut fs, &mut dev, &mut inode, key + 1, 1).unwrap();
    let leaves = index_entries(&read_node(&mut dev, external));
    assert_eq!(leaves[1].ei_block, key + 2);
}

// ---- block numbers above 32 bits ----

/// A device that stores only the blocks written to it, so it can report more than 2^32
/// blocks.
struct SparseDev {
    blocks: BTreeMap<u64, Vec<u8>>,
    total_blocks: u64,
    now: Cell<i64>,
}

impl SparseDev {
    fn new(total_blocks: u64) -> Self {
        Self {
            blocks: BTreeMap::new(),
            total_blocks,
            now: Cell::new(1_700_000_000),
        }
    }

    fn range(&self, block_id: crate::io::SectorId, count: u32) -> Ext4Result<(u64, u64)> {
        let start = block_id.raw();
        let end = start + u64::from(count);
        if end > self.total_blocks {
            return Err(Ext4Error::corrupted());
        }
        Ok((start, end))
    }
}

impl BlockIo for SparseDev {
    fn write(
        &mut self,
        buffer: &[u8],
        block_id: crate::io::SectorId,
        count: u32,
    ) -> Ext4Result<()> {
        let (start, end) = self.range(block_id, count)?;
        for (i, block) in (start..end).enumerate() {
            let bytes = &buffer[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            self.blocks.insert(block, bytes.to_vec());
        }
        Ok(())
    }

    fn read(
        &mut self,
        buffer: &mut [u8],
        block_id: crate::io::SectorId,
        count: u32,
    ) -> Ext4Result<()> {
        let (start, end) = self.range(block_id, count)?;
        for (i, block) in (start..end).enumerate() {
            let out = &mut buffer[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            match self.blocks.get(&block) {
                Some(bytes) => out.copy_from_slice(bytes),
                None => out.fill(0),
            }
        }
        Ok(())
    }

    fn geometry(&self) -> crate::io::DeviceGeometry {
        crate::io::DeviceGeometry::new(BLOCK_SIZE as u32, self.total_blocks)
    }

    fn capabilities(&self) -> crate::io::DeviceCapabilities {
        crate::io::DeviceCapabilities {
            read_only: false,
            flush: true,
            ..crate::io::DeviceCapabilities::default()
        }
    }

    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }
}

impl crate::runtime::Clock for SparseDev {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        let sec = self.now.get();
        self.now.set(sec + 1);
        Ok(Ext4Timestamp::new(sec, 0))
    }
}

/// A small file system with `n` one-block extents under a two-leaf index root, whose
/// device and superblock then claim blocks above 32 bits. Returns the second leaf. The groups still end at the
/// small size, so a block above 32 bits can be referenced but never freed.
fn high_tree(n: u32) -> (Jbd2Dev<SparseDev>, Ext4FileSystem, Ext4Inode, u64) {
    let mut dev = Jbd2Dev::initial_jbd2dev(0, SparseDev::new(16 * 1024), false);
    mkfs(&mut dev).unwrap();
    let mut fs = Ext4FileSystem::mount(&mut dev).unwrap();
    let mut inode = new_extent_inode();
    insert_n_extents_with_phys_gaps(&mut fs, &mut dev, &mut inode, n);
    let root = index_entries(&root_of(&mut inode));
    assert_eq!(root.len(), 2);
    let leaf = child_address(&root[1]);

    dev.flush().unwrap();
    let mut device = dev.into_inner();
    device.total_blocks = 4 * HIGH;
    let dev = Jbd2Dev::initial_jbd2dev(0, device, false);
    fs.superblock.s_blocks_count_hi = 2;
    (dev, fs, inode, leaf)
}

/// Copies the root's second leaf to `HIGH + leaf` and points the root at the copy.
fn move_leaf_high(
    dev: &mut Jbd2Dev<SparseDev>,
    fs: &Ext4FileSystem,
    inode: &mut Ext4Inode,
    leaf: u64,
) -> u64 {
    let high = HIGH + leaf;
    dev.read_block(AbsoluteBN::new(leaf)).unwrap();
    let bytes = dev.buffer().to_vec();
    dev.write_blocks(&bytes, AbsoluteBN::new(high), 1, true)
        .unwrap();

    let mut tree = ExtentTree::new(inode, BLOCK_SIZE);
    tree.bind_geometry(&fs.superblock);
    let mut root = tree.load_root_from_inode().unwrap();
    if let ExtentNode::Index { entries, .. } = &mut root {
        entries[1].ei_leaf_hi = (high >> 32) as u16;
        entries[1].ei_leaf_lo = high as u32;
    }
    tree.store_root_to_inode(&root).unwrap();
    high
}

/// Replaces the last extent of the leaf at `leaf` with one of eight blocks starting at
/// `start`.
fn put_last_extent(
    dev: &mut Jbd2Dev<SparseDev>,
    fs: &Ext4FileSystem,
    inode: &mut Ext4Inode,
    leaf: u64,
    start: u64,
) -> u32 {
    let mut node = read_node(dev, leaf);
    let lbn = match &mut node {
        ExtentNode::Leaf { entries, .. } => {
            let last = entries.last_mut().unwrap();
            *last = Ext4Extent::new(last.ee_block, start, 8);
            last.ee_block
        }
        ExtentNode::Index { .. } => panic!("expected a leaf"),
    };
    let mut tree = ExtentTree::new(inode, BLOCK_SIZE);
    tree.bind_geometry(&fs.superblock);
    tree.write_node_to_block(dev, AbsoluteBN::new(leaf), &node)
        .unwrap();
    lbn
}

fn leaf_extents(dev: &mut Jbd2Dev<SparseDev>, leaf: u64) -> Vec<Ext4Extent> {
    match read_node(dev, leaf) {
        ExtentNode::Leaf { entries, .. } => entries,
        ExtentNode::Index { .. } => panic!("expected a leaf"),
    }
}

#[test]
fn a_leaf_above_32_bits_is_rewritten_in_place() {
    let (mut dev, mut fs, mut inode, leaf) = high_tree(8);
    let high = move_leaf_high(&mut dev, &fs, &mut inode, leaf);

    remove(&mut fs, &mut dev, &mut inode, 4, 1).unwrap();
    let blocks: Vec<u32> = leaf_extents(&mut dev, high)
        .iter()
        .map(|extent| extent.ee_block)
        .collect();
    assert_eq!(blocks, vec![2, 3, 5, 6, 7]);
    assert_eq!(leaf_extents(&mut dev, leaf).len(), 6);
}

#[test]
fn collapsing_a_root_frees_the_child_it_points_at() {
    let (mut dev, mut fs, mut inode, leaf) = high_tree(5);
    move_leaf_high(&mut dev, &fs, &mut inode, leaf);

    // Emptying the first leaf leaves the root one child of three extents, which fit
    // inline, so the root collapses and frees that child. The child lies above every
    // group, so the free is refused, and the low copy stays allocated.
    let result = remove(&mut fs, &mut dev, &mut inode, 0, 2);
    assert!(result.is_err());
    assert!(bitmap_block_is_allocated(
        &mut fs,
        &mut dev,
        AbsoluteBN::new(leaf)
    ));
}

#[test]
fn trimming_the_front_of_an_extent_above_32_bits_keeps_its_high_half() {
    let (mut dev, mut fs, mut inode, leaf) = high_tree(6);
    let start = HIGH + 100;
    let lbn = put_last_extent(&mut dev, &fs, &mut inode, leaf, start);

    // The leaf is written before its blocks are freed, and the free of a block above
    // every group is refused, so the rewritten leaf is what the deletion leaves.
    let result = remove(&mut fs, &mut dev, &mut inode, lbn, 2);
    assert!(result.is_err());
    let last = *leaf_extents(&mut dev, leaf).last().unwrap();
    assert_eq!(last.ee_block, lbn + 2);
    assert_eq!(last.len(), 6);
    assert_eq!(last.start_block(), start + 2);
}

#[test]
fn splitting_an_extent_above_32_bits_keeps_the_high_half_of_the_right_part() {
    let (mut dev, mut fs, mut inode, leaf) = high_tree(6);
    let start = HIGH + 100;
    let lbn = put_last_extent(&mut dev, &fs, &mut inode, leaf, start);

    let result = remove(&mut fs, &mut dev, &mut inode, lbn + 2, 2);
    assert!(result.is_err());
    let extents = leaf_extents(&mut dev, leaf);
    let right = extents[extents.len() - 1];
    let left = extents[extents.len() - 2];
    assert_eq!(
        (left.ee_block, left.len(), left.start_block()),
        (lbn, 2, start)
    );
    assert_eq!(
        (right.ee_block, right.len(), right.start_block()),
        (lbn + 4, 4, start + 4)
    );
}
