use super::{split::SplitInfo, *};

impl<'a> ExtentTree<'a> {
    /// Splits an unwritten extent so `start..start + len` is represented by an
    /// exact, still-unwritten leaf entry. This is the metadata preparation
    /// phase performed before data I/O.
    pub(crate) fn prepare_unwritten_write<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        start: u32,
        len: u32,
    ) -> Ext4Result<()> {
        self.bind_geometry(&fs.superblock);
        if len == 0 || len > Ext4Extent::EXT_UNINIT_MAX_LEN.into() {
            return Err(Ext4Error::invalid_input().with_operation("extent:unwritten_length"));
        }
        start
            .checked_add(len)
            .ok_or_else(Ext4Error::file_too_large)?;

        let mut root = self.load_root_from_inode()?;
        self.validate_node(&root, None, None, block_dev.total_blocks(), true)?;
        let split =
            self.split_extent_recursive(fs, block_dev, &mut root, start, len, true, true, None)?;
        match split {
            None => self.store_root_to_inode(&root),
            Some(right) => self.promote_split_root(fs, block_dev, root, right),
        }
    }

    /// Splits an initialized extent and marks the exact selected range
    /// unwritten without changing its physical allocation.
    pub(crate) fn prepare_initialized_zero<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        start: u32,
        len: u32,
    ) -> Ext4Result<()> {
        self.bind_geometry(&fs.superblock);
        if len == 0 || len > Ext4Extent::EXT_UNINIT_MAX_LEN.into() {
            return Err(Ext4Error::invalid_input().with_operation("extent:zero_length"));
        }
        start
            .checked_add(len)
            .ok_or_else(Ext4Error::file_too_large)?;

        let mut root = self.load_root_from_inode()?;
        self.validate_node(&root, None, None, block_dev.total_blocks(), true)?;
        let split =
            self.split_extent_recursive(fs, block_dev, &mut root, start, len, false, true, None)?;
        match split {
            None => self.store_root_to_inode(&root),
            Some(right) => self.promote_split_root(fs, block_dev, root, right),
        }
    }

    /// Marks an exact prepared unwritten extent initialized after its data I/O
    /// has completed successfully.
    pub(crate) fn finish_unwritten_write<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        start: u32,
        len: u32,
    ) -> Ext4Result<()> {
        let mut root = self.load_root_from_inode()?;
        self.validate_node(&root, None, None, block_dev.total_blocks(), true)?;
        self.initialize_exact_recursive(block_dev, &mut root, start, len, None)?;
        self.store_root_to_inode(&root)
    }

    #[allow(clippy::too_many_arguments)]
    fn split_extent_recursive<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        node: &mut ExtentNode,
        start: u32,
        len: u32,
        expected_unwritten: bool,
        middle_unwritten: bool,
        physical_node: Option<AbsoluteBN>,
    ) -> Ext4Result<Option<SplitInfo>> {
        match node {
            ExtentNode::Leaf { header, entries } => {
                let position = entries
                    .iter()
                    .position(|extent| {
                        extent.ee_block <= start
                            && start < extent.ee_block.saturating_add(extent.len())
                    })
                    .ok_or_else(|| {
                        Ext4Error::invalid_input().with_operation("extent:unwritten_missing")
                    })?;
                let original = entries[position];
                if original.is_unwritten() != expected_unwritten {
                    return Err(Ext4Error::invalid_input().with_operation("extent:rewrite_state"));
                }
                let original_end =
                    original
                        .ee_block
                        .checked_add(original.len())
                        .ok_or_else(|| {
                            Ext4Error::corrupted().with_operation("extent:logical_overflow")
                        })?;
                let write_end = start
                    .checked_add(len)
                    .ok_or_else(Ext4Error::file_too_large)?;
                if write_end > original_end {
                    return Err(
                        Ext4Error::invalid_input().with_operation("extent:unwritten_crossing")
                    );
                }

                let mut replacement = Vec::with_capacity(3);
                let left_len = start - original.ee_block;
                if left_len != 0 {
                    replacement.push(build_extent_with_state(
                        original.ee_block,
                        original.start_block(),
                        left_len,
                        original.is_unwritten(),
                        "extent:rewrite_left",
                    )?);
                }
                let middle_physical = original
                    .start_block()
                    .checked_add(u64::from(left_len))
                    .ok_or_else(Ext4Error::overflow)?;
                replacement.push(build_extent_with_state(
                    start,
                    middle_physical,
                    len,
                    middle_unwritten,
                    "extent:rewrite_middle",
                )?);
                let right_len = original_end - write_end;
                if right_len != 0 {
                    let right_physical = middle_physical
                        .checked_add(u64::from(len))
                        .ok_or_else(Ext4Error::overflow)?;
                    replacement.push(build_extent_with_state(
                        write_end,
                        right_physical,
                        right_len,
                        original.is_unwritten(),
                        "extent:rewrite_right",
                    )?);
                }

                entries.splice(position..=position, replacement);
                header.eh_entries = u16::try_from(entries.len())
                    .map_err(|_| Ext4Error::corrupted().with_operation("extent:entry_overflow"))?;
                if entries.len() <= usize::from(header.eh_max) {
                    if let Some(block) = physical_node {
                        let disk_node = ExtentNode::Leaf {
                            header: *header,
                            entries: entries.clone(),
                        };
                        self.write_node_to_block(block_dev, block, &disk_node)?;
                    }
                    return Ok(None);
                }
                self.split_leaf_after_rewrite(fs, block_dev, header, entries, physical_node)
                    .map(Some)
            }
            ExtentNode::Index { header, entries } => {
                let partition = entries.partition_point(|index| index.ei_block <= start);
                let position = partition.saturating_sub(1);
                let child_index = entries
                    .get(position)
                    .copied()
                    .ok_or_else(|| Ext4Error::corrupted().with_operation("extent:empty_index"))?;
                let child_block = AbsoluteBN::new(
                    (u64::from(child_index.ei_leaf_hi) << 32) | u64::from(child_index.ei_leaf_lo),
                );
                let mut child =
                    self.read_child_node(block_dev, &child_index, header.eh_depth - 1)?;
                let child_split = self.split_extent_recursive(
                    fs,
                    block_dev,
                    &mut child,
                    start,
                    len,
                    expected_unwritten,
                    middle_unwritten,
                    Some(child_block),
                )?;
                entries[position].ei_block = Self::get_node_start_block(&child);
                if let Some(split) = child_split {
                    let new_index = Ext4ExtentIdx {
                        ei_block: split.start_block,
                        ei_leaf_lo: split.phy_block.raw() as u32,
                        ei_leaf_hi: (split.phy_block.raw() >> 32) as u16,
                        ei_unused: 0,
                    };
                    let insert_at =
                        entries.partition_point(|index| index.ei_block < split.start_block);
                    entries.insert(insert_at, new_index);
                    header.eh_entries = u16::try_from(entries.len()).map_err(|_| {
                        Ext4Error::corrupted().with_operation("extent:index_overflow")
                    })?;
                }
                if entries.len() <= usize::from(header.eh_max) {
                    if let Some(block) = physical_node {
                        let disk_node = ExtentNode::Index {
                            header: *header,
                            entries: entries.clone(),
                        };
                        self.write_node_to_block(block_dev, block, &disk_node)?;
                    }
                    return Ok(None);
                }
                self.split_index_after_rewrite(fs, block_dev, header, entries, physical_node)
                    .map(Some)
            }
        }
    }

    fn split_leaf_after_rewrite<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        header: &mut Ext4ExtentHeader,
        entries: &mut Vec<Ext4Extent>,
        physical_node: Option<AbsoluteBN>,
    ) -> Ext4Result<SplitInfo> {
        let right_entries = entries.split_off(entries.len() / 2);
        header.eh_entries = entries.len() as u16;
        self.can_add_inode_sectors_for_block(fs)?;
        let right_block = fs.alloc_block(block_dev)?;
        self.add_inode_sectors_for_block(fs)?;
        let right_node = ExtentNode::Leaf {
            header: Ext4ExtentHeader {
                eh_magic: Ext4ExtentHeader::EXT4_EXT_MAGIC,
                eh_entries: right_entries.len() as u16,
                eh_max: self.calc_block_eh_max(),
                eh_depth: 0,
                eh_generation: 0,
            },
            entries: right_entries,
        };
        self.write_node_to_block(block_dev, right_block, &right_node)?;
        if let Some(left_block) = physical_node {
            let left_node = ExtentNode::Leaf {
                header: *header,
                entries: entries.clone(),
            };
            self.write_node_to_block(block_dev, left_block, &left_node)?;
        }
        Ok(SplitInfo {
            start_block: Self::get_node_start_block(&right_node),
            phy_block: right_block,
        })
    }

    fn split_index_after_rewrite<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        header: &mut Ext4ExtentHeader,
        entries: &mut Vec<Ext4ExtentIdx>,
        physical_node: Option<AbsoluteBN>,
    ) -> Ext4Result<SplitInfo> {
        let right_entries = entries.split_off(entries.len() / 2);
        header.eh_entries = entries.len() as u16;
        self.can_add_inode_sectors_for_block(fs)?;
        let right_block = fs.alloc_block(block_dev)?;
        self.add_inode_sectors_for_block(fs)?;
        let right_node = ExtentNode::Index {
            header: Ext4ExtentHeader {
                eh_magic: Ext4ExtentHeader::EXT4_EXT_MAGIC,
                eh_entries: right_entries.len() as u16,
                eh_max: self.calc_block_eh_max(),
                eh_depth: header.eh_depth,
                eh_generation: 0,
            },
            entries: right_entries,
        };
        self.write_node_to_block(block_dev, right_block, &right_node)?;
        if let Some(left_block) = physical_node {
            let left_node = ExtentNode::Index {
                header: *header,
                entries: entries.clone(),
            };
            self.write_node_to_block(block_dev, left_block, &left_node)?;
        }
        Ok(SplitInfo {
            start_block: Self::get_node_start_block(&right_node),
            phy_block: right_block,
        })
    }

    fn promote_split_root<B: BlockIo>(
        &mut self,
        fs: &mut Ext4FileSystem,
        block_dev: &mut Jbd2Dev<B>,
        left_root: ExtentNode,
        right: SplitInfo,
    ) -> Ext4Result<()> {
        self.can_add_inode_sectors_for_block(fs)?;
        let left_block = fs.alloc_block(block_dev)?;
        self.add_inode_sectors_for_block(fs)?;
        self.write_node_to_block(block_dev, left_block, &left_root)?;
        let depth = left_root
            .header()
            .eh_depth
            .checked_add(1)
            .filter(|depth| *depth <= Self::MAX_DEPTH)
            .ok_or_else(|| Ext4Error::corrupted().with_operation("extent:depth_overflow"))?;
        let root = ExtentNode::Index {
            header: Ext4ExtentHeader {
                eh_magic: Ext4ExtentHeader::EXT4_EXT_MAGIC,
                eh_entries: 2,
                eh_max: 4,
                eh_depth: depth,
                eh_generation: 0,
            },
            entries: vec![
                Ext4ExtentIdx {
                    ei_block: Self::get_node_start_block(&left_root),
                    ei_leaf_lo: left_block.raw() as u32,
                    ei_leaf_hi: (left_block.raw() >> 32) as u16,
                    ei_unused: 0,
                },
                Ext4ExtentIdx {
                    ei_block: right.start_block,
                    ei_leaf_lo: right.phy_block.raw() as u32,
                    ei_leaf_hi: (right.phy_block.raw() >> 32) as u16,
                    ei_unused: 0,
                },
            ],
        };
        self.store_root_to_inode(&root)
    }

    fn initialize_exact_recursive<B: BlockIo>(
        &self,
        block_dev: &mut Jbd2Dev<B>,
        node: &mut ExtentNode,
        start: u32,
        len: u32,
        physical_node: Option<AbsoluteBN>,
    ) -> Ext4Result<()> {
        match node {
            ExtentNode::Leaf { header, entries } => {
                let position = entries
                    .iter_mut()
                    .position(|extent| extent.ee_block == start && extent.len() == len)
                    .ok_or_else(|| {
                        Ext4Error::corrupted().with_operation("extent:prepared_range_missing")
                    })?;
                let extent = &mut entries[position];
                if !extent.is_unwritten() {
                    return Err(
                        Ext4Error::corrupted().with_operation("extent:prepared_range_initialized")
                    );
                }
                extent.ee_len = Ext4Extent::encode_len(len, false).ok_or_else(|| {
                    Ext4Error::corrupted().with_operation("extent:initialized_length")
                })?;
                Self::merge_leaf_extent_neighbors(entries, position)?;
                header.eh_entries = u16::try_from(entries.len())
                    .map_err(|_| Ext4Error::corrupted().with_operation("extent:entry_overflow"))?;
                if let Some(block) = physical_node {
                    let disk_node = ExtentNode::Leaf {
                        header: *header,
                        entries: entries.clone(),
                    };
                    self.write_node_to_block(block_dev, block, &disk_node)?;
                }
                Ok(())
            }
            ExtentNode::Index { header, entries } => {
                let partition = entries.partition_point(|index| index.ei_block <= start);
                let position = partition.saturating_sub(1);
                let index = entries
                    .get(position)
                    .ok_or_else(|| Ext4Error::corrupted().with_operation("extent:empty_index"))?;
                let child_block = AbsoluteBN::new(
                    (u64::from(index.ei_leaf_hi) << 32) | u64::from(index.ei_leaf_lo),
                );
                let mut child = self.read_child_node(block_dev, index, header.eh_depth - 1)?;
                self.initialize_exact_recursive(
                    block_dev,
                    &mut child,
                    start,
                    len,
                    Some(child_block),
                )
            }
        }
    }
}

fn build_extent_with_state(
    logical: u32,
    physical: u64,
    len: u32,
    unwritten: bool,
    operation: &'static str,
) -> Ext4Result<Ext4Extent> {
    let extent = if unwritten {
        Ext4Extent::new_unwritten(logical, physical, len)
    } else {
        u16::try_from(len)
            .ok()
            .map(|len| Ext4Extent::new(logical, physical, len))
    };
    extent.ok_or_else(|| Ext4Error::corrupted().with_operation(operation))
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::Cell;

    use super::*;
    use crate::{
        config::BLOCK_SIZE,
        ext4::{Ext4FileSystem, mkfs},
    };

    struct MemBlockDev {
        data: Vec<u8>,
        now: Cell<i64>,
    }

    impl MemBlockDev {
        fn range(&self, block_id: crate::io::SectorId, count: u32) -> Ext4Result<(usize, usize)> {
            let start = block_id.as_usize()? * BLOCK_SIZE;
            let end = start + BLOCK_SIZE * count as usize;
            if end > self.data.len() {
                return Err(Ext4Error::block_out_of_range(
                    block_id.to_u32()?,
                    (self.data.len() / BLOCK_SIZE) as u64,
                ));
            }
            Ok((start, end))
        }
    }

    impl BlockIo for MemBlockDev {
        fn write(
            &mut self,
            buffer: &[u8],
            block_id: crate::io::SectorId,
            count: u32,
        ) -> Ext4Result<()> {
            let (start, end) = self.range(block_id, count)?;
            self.data[start..end].copy_from_slice(&buffer[..end - start]);
            Ok(())
        }

        fn read(
            &mut self,
            buffer: &mut [u8],
            block_id: crate::io::SectorId,
            count: u32,
        ) -> Ext4Result<()> {
            let (start, end) = self.range(block_id, count)?;
            buffer[..end - start].copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn geometry(&self) -> crate::io::DeviceGeometry {
            crate::io::DeviceGeometry::new(BLOCK_SIZE as u32, (self.data.len() / BLOCK_SIZE) as u64)
        }

        fn capabilities(&self) -> crate::io::DeviceCapabilities {
            crate::io::DeviceCapabilities {
                flush: true,
                ..crate::io::DeviceCapabilities::default()
            }
        }

        fn flush(&mut self) -> Ext4Result<()> {
            Ok(())
        }
    }

    impl crate::runtime::Clock for MemBlockDev {
        fn now(&self) -> Ext4Result<Ext4Timestamp> {
            let sec = self.now.get();
            self.now.set(sec + 1);
            Ok(Ext4Timestamp::new(sec, 0))
        }
    }

    fn setup_fs() -> (Jbd2Dev<MemBlockDev>, Ext4FileSystem) {
        let dev = MemBlockDev {
            data: vec![0u8; 16384 * BLOCK_SIZE],
            now: Cell::new(1_700_000_000),
        };
        let mut jbd = Jbd2Dev::initial_jbd2dev(0, dev, false);
        mkfs(&mut jbd).unwrap();
        let fs = Ext4FileSystem::mount(&mut jbd).unwrap();
        (jbd, fs)
    }

    fn new_extent_inode() -> Ext4Inode {
        let mut inode = Ext4Inode::default();
        inode.i_flags |= Ext4Inode::EXT4_EXTENTS_FL;
        inode.write_extend_header();
        inode
    }

    /// Logical distance between the first blocks of neighboring leaves.
    const LEAF_SPAN: u32 = 3000;
    /// Physical block of logical block zero; the data itself is never read.
    const DATA_BASE: u64 = 4000;

    fn data_extent(logical: u32, unwritten: bool) -> Ext4Extent {
        build_extent_with_state(
            logical,
            DATA_BASE + u64::from(logical),
            4,
            unwritten,
            "test",
        )
        .unwrap()
    }

    /// Logical start, length, physical start and unwritten state of an extent.
    fn shape(extent: &Ext4Extent) -> (u32, u32, u64, bool) {
        (
            extent.ee_block,
            extent.len(),
            extent.start_block(),
            extent.is_unwritten(),
        )
    }

    fn header(entries: usize, max: u16, depth: u16) -> Ext4ExtentHeader {
        Ext4ExtentHeader {
            eh_magic: Ext4ExtentHeader::EXT4_EXT_MAGIC,
            eh_entries: entries as u16,
            eh_max: max,
            eh_depth: depth,
            eh_generation: 0,
        }
    }

    /// Stores four leaves of extents spaced eight blocks apart below an index
    /// root that is already at its four-entry capacity. The last leaf is full,
    /// so splitting one of its extents must grow the tree to depth two.
    fn store_full_two_level_tree(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemBlockDev>,
        tree: &mut ExtentTree<'_>,
        unwritten: bool,
    ) {
        tree.bind_geometry(&fs.superblock);
        let leaf_max = tree.calc_block_eh_max();
        let mut index = Vec::new();
        for leaf in 0..4u32 {
            let count = if leaf == 3 { u32::from(leaf_max) } else { 2 };
            let entries: Vec<Ext4Extent> = (0..count)
                .map(|slot| data_extent(leaf * LEAF_SPAN + slot * 8, unwritten))
                .collect();
            let block = fs.alloc_block(dev).unwrap();
            let node = ExtentNode::Leaf {
                header: header(entries.len(), leaf_max, 0),
                entries,
            };
            tree.write_node_to_block(dev, block, &node).unwrap();
            index.push(Ext4ExtentIdx {
                ei_block: leaf * LEAF_SPAN,
                ei_leaf_lo: block.raw() as u32,
                ei_leaf_hi: (block.raw() >> 32) as u16,
                ei_unused: 0,
            });
        }
        let root = ExtentNode::Index {
            header: header(index.len(), 4, 1),
            entries: index,
        };
        tree.store_root_to_inode(&root).unwrap();
    }

    /// First logical block of an extent in the middle of the full leaf.
    const TARGET: u32 = 3 * LEAF_SPAN + 100 * 8;

    fn store_leaf_root(tree: &mut ExtentTree<'_>, entries: Vec<Ext4Extent>) {
        let root = ExtentNode::Leaf {
            header: header(entries.len(), 4, 0),
            entries,
        };
        tree.store_root_to_inode(&root).unwrap();
    }

    #[test]
    fn prepare_rejects_zero_lengths_before_reading_the_tree() {
        let (mut dev, mut fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);

        let unwritten = tree
            .prepare_unwritten_write(&mut fs, &mut dev, 0, 0)
            .unwrap_err();
        let zero = tree
            .prepare_initialized_zero(&mut fs, &mut dev, 0, 0)
            .unwrap_err();

        assert_eq!(
            unwritten,
            Ext4Error::invalid_input().with_operation("extent:unwritten_length")
        );
        assert_eq!(
            zero,
            Ext4Error::invalid_input().with_operation("extent:zero_length")
        );
    }

    #[test]
    fn prepare_rejects_unmapped_mismatched_and_crossing_ranges() {
        let (mut dev, mut fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);
        store_leaf_root(&mut tree, vec![data_extent(0, true), data_extent(8, false)]);
        let before = *tree.inode;

        let unmapped = tree.prepare_unwritten_write(&mut fs, &mut dev, 4, 1);
        let initialized = tree.prepare_unwritten_write(&mut fs, &mut dev, 8, 1);
        let already_unwritten = tree.prepare_initialized_zero(&mut fs, &mut dev, 1, 1);
        let crossing = tree.prepare_unwritten_write(&mut fs, &mut dev, 2, 4);

        let invalid = |operation| Ext4Error::invalid_input().with_operation(operation);
        assert_eq!(unmapped, Err(invalid("extent:unwritten_missing")));
        assert_eq!(initialized, Err(invalid("extent:rewrite_state")));
        assert_eq!(already_unwritten, Err(invalid("extent:rewrite_state")));
        assert_eq!(crossing, Err(invalid("extent:unwritten_crossing")));
        assert_eq!(tree.inode.i_block, before.i_block);
    }

    #[test]
    fn finish_requires_an_exact_prepared_unwritten_range() {
        let (mut dev, _fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);
        store_leaf_root(&mut tree, vec![data_extent(0, true), data_extent(8, false)]);

        let partial = tree.finish_unwritten_write(&mut dev, 0, 2).unwrap_err();
        let initialized = tree.finish_unwritten_write(&mut dev, 8, 4).unwrap_err();

        let corrupted = |operation| Ext4Error::corrupted().with_operation(operation);
        assert_eq!(partial, corrupted("extent:prepared_range_missing"));
        assert_eq!(initialized, corrupted("extent:prepared_range_initialized"));
        let extent = tree.find_extent(&mut dev, 0).unwrap().unwrap();
        assert_eq!(shape(&extent), (0, 4, DATA_BASE, true));
    }

    fn root_depth(tree: &ExtentTree<'_>) -> u16 {
        tree.load_root_from_inode().unwrap().header().eh_depth
    }

    fn extent_at(
        tree: &mut ExtentTree<'_>,
        dev: &mut Jbd2Dev<MemBlockDev>,
        logical: u32,
    ) -> (u32, u32, u64, bool) {
        shape(&tree.find_extent(dev, logical).unwrap().unwrap())
    }

    #[test]
    fn unwritten_write_in_a_full_leaf_under_a_full_root_grows_the_tree_and_finishes() {
        let (mut dev, mut fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);
        store_full_two_level_tree(&mut fs, &mut dev, &mut tree, true);
        let free_before = fs.superblock.free_blocks_count();
        let physical = DATA_BASE + u64::from(TARGET);

        tree.prepare_unwritten_write(&mut fs, &mut dev, TARGET + 1, 2)
            .unwrap();

        // The leaf split, the root index split, and a new root level each
        // allocated one block.
        assert_eq!(root_depth(&tree), 2);
        assert_eq!(fs.superblock.free_blocks_count(), free_before - 3);
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET),
            (TARGET, 1, physical, true)
        );
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 1),
            (TARGET + 1, 2, physical + 1, true)
        );
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 3),
            (TARGET + 3, 1, physical + 3, true)
        );
        assert_eq!(extent_at(&mut tree, &mut dev, 0), (0, 4, DATA_BASE, true));

        tree.finish_unwritten_write(&mut dev, TARGET + 1, 2)
            .unwrap();

        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 1),
            (TARGET + 1, 2, physical + 1, false)
        );
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET),
            (TARGET, 1, physical, true)
        );
        assert_eq!(
            tree.finish_unwritten_write(&mut dev, TARGET + 1, 2),
            Err(Ext4Error::corrupted().with_operation("extent:prepared_range_initialized"))
        );
    }

    #[test]
    fn zeroing_an_initialized_range_in_a_full_leaf_grows_the_tree() {
        let (mut dev, mut fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);
        store_full_two_level_tree(&mut fs, &mut dev, &mut tree, false);
        let physical = DATA_BASE + u64::from(TARGET);

        tree.prepare_initialized_zero(&mut fs, &mut dev, TARGET, 2)
            .unwrap();

        assert_eq!(root_depth(&tree), 2);
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET),
            (TARGET, 2, physical, true)
        );
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 2),
            (TARGET + 2, 2, physical + 2, false)
        );
        let last = 3 * LEAF_SPAN + (u32::from(tree.calc_block_eh_max()) - 1) * 8;
        assert_eq!(
            extent_at(&mut tree, &mut dev, last),
            (last, 4, DATA_BASE + u64::from(last), false)
        );
    }

    #[test]
    fn finishing_a_prepared_range_merges_it_back_into_initialized_neighbors() {
        let (mut dev, mut fs) = setup_fs();
        let mut inode = new_extent_inode();
        let mut tree = ExtentTree::new(&mut inode, BLOCK_SIZE);
        store_full_two_level_tree(&mut fs, &mut dev, &mut tree, false);
        let physical = DATA_BASE + u64::from(TARGET);
        tree.prepare_initialized_zero(&mut fs, &mut dev, TARGET + 1, 2)
            .unwrap();
        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 1),
            (TARGET + 1, 2, physical + 1, true)
        );

        tree.finish_unwritten_write(&mut dev, TARGET + 1, 2)
            .unwrap();

        assert_eq!(
            extent_at(&mut tree, &mut dev, TARGET + 2),
            (TARGET, 4, physical, false)
        );
    }
}
