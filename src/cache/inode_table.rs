//! Inode table cache helpers.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::{
    blockdev::*,
    bmalloc::{AbsoluteBN, BGIndex, InodeNumber},
    config::*,
    disknode::*,
    error::*,
};

/// Cache key for one global inode number.
pub type InodeCacheKey = InodeNumber;

/// Cached inode payload.
#[derive(Debug, Clone)]
pub struct CachedInode {
    pub inode: Ext4Inode,
    raw_inode: Arc<Vec<u8>>,
    pub dirty: bool,
    pub block_num: AbsoluteBN,
    pub offset_in_block: usize,
    pub inode_num: InodeNumber,
    pub last_access: u64,
    pub generation: u64,
}

impl CachedInode {
    pub fn new(
        inode: Ext4Inode,
        raw_inode: Vec<u8>,
        inode_num: InodeNumber,
        block_num: AbsoluteBN,
        offset_in_block: usize,
    ) -> Self {
        Self {
            inode,
            raw_inode: Arc::new(raw_inode),
            dirty: false,
            block_num,
            offset_in_block,
            inode_num,
            last_access: 0,
            generation: 0,
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn handle(&self) -> InodeHandle {
        InodeHandle {
            inode_num: self.inode_num,
        }
    }

    pub(crate) fn raw_inode(&self) -> &[u8] {
        self.raw_inode.as_slice()
    }
}

/// Lightweight cached inode handle.
#[derive(Debug, Clone, Copy)]
pub struct InodeHandle {
    pub inode_num: InodeNumber,
}

/// Inode cache owned exclusively by one mounted filesystem.
#[derive(Clone)]
pub struct InodeCache {
    cache: BTreeMap<InodeCacheKey, CachedInode>,
    max_entries: usize,
    access_counter: u64,
    inode_size: usize,
}

impl InodeCache {
    pub fn new(max_entries: usize, inode_size: usize) -> Self {
        Self {
            cache: BTreeMap::new(),
            max_entries,
            access_counter: 0,
            inode_size,
        }
    }

    pub fn default(inode_size: u16) -> Self {
        Self::new(INODE_CACHE_MAX, inode_size as usize)
    }

    pub fn calc_inode_location(
        &self,
        inode_num: InodeNumber,
        inodes_per_group: u32,
        inode_table_start: AbsoluteBN,
        block_size: usize,
    ) -> Ext4Result<(AbsoluteBN, usize, BGIndex)> {
        let (group_idx, idx_in_group) = inode_num.to_group(inodes_per_group)?;
        let byte_offset = idx_in_group.as_usize()? * self.inode_size;
        let block_offset = byte_offset / block_size;
        let offset_in_block = byte_offset % block_size;
        Ok((
            inode_table_start.checked_add_usize(block_offset)?,
            offset_in_block,
            group_idx,
        ))
    }

    fn load_inode<B: BlockIo>(
        &self,
        block_dev: &mut Jbd2Dev<B>,
        block_num: AbsoluteBN,
        offset: usize,
    ) -> Ext4Result<(Ext4Inode, Vec<u8>)> {
        let mut buffer = alloc::vec![0u8; block_dev.block_size() as usize];
        block_dev.read_blocks(&mut buffer, block_num, 1)?;
        let end = offset
            .checked_add(self.inode_size)
            .ok_or(Ext4Error::corrupted())?;
        let bytes = buffer.get(offset..end).ok_or(Ext4Error::corrupted())?;
        let raw_inode = bytes.to_vec();
        Ok((Ext4Inode::decode_checked(&raw_inode)?, raw_inode))
    }

    pub fn get_or_load<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
        block_num: AbsoluteBN,
        offset: usize,
    ) -> Ext4Result<CachedInode> {
        self.ensure_loaded(block_dev, inode_num, block_num, offset)?;
        self.touch(inode_num);
        self.cache
            .get(&inode_num)
            .cloned()
            .ok_or(Ext4Error::corrupted())
    }

    fn ensure_loaded<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
        block_num: AbsoluteBN,
        offset: usize,
    ) -> Ext4Result<()> {
        if self.cache.contains_key(&inode_num) {
            return Ok(());
        }

        // A failed load leaves the previous cache contents untouched.
        let (inode, raw_inode) = self.load_inode(block_dev, block_num, offset)?;

        self.make_room(block_dev)?;

        self.cache.insert(
            inode_num,
            CachedInode::new(inode, raw_inode, inode_num, block_num, offset),
        );
        Ok(())
    }

    fn make_room<B: BlockIo>(&mut self, block_dev: &mut Jbd2Dev<B>) -> Ext4Result<()> {
        if self.cache.len() >= self.max_entries
            && let Some(victim_num) = self.lru_inode()
        {
            let victim = self
                .cache
                .get(&victim_num)
                .cloned()
                .ok_or(Ext4Error::corrupted())?;
            if victim.dirty {
                Self::write_inode_bytes_static(
                    block_dev,
                    victim.block_num,
                    victim.offset_in_block,
                    &victim.raw_inode,
                )?;
            }
            self.cache.remove(&victim_num);
        }

        Ok(())
    }

    /// Installs an all-zero record for a newly allocated inode without reading
    /// stale bytes from an inode table that Linux has not initialized yet.
    pub(crate) fn initialize_zeroed<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
        block_num: AbsoluteBN,
        offset: usize,
    ) -> Ext4Result<()> {
        if self
            .cache
            .get(&inode_num)
            .is_some_and(|cached| cached.dirty)
        {
            return Err(Ext4Error::corrupted().with_operation("inode_cache:initialize_dirty"));
        }
        let end = offset
            .checked_add(self.inode_size)
            .ok_or_else(Ext4Error::overflow)?;
        if end > block_dev.block_size() as usize {
            return Err(Ext4Error::corrupted().with_operation("inode_cache:initialize_range"));
        }
        self.cache.remove(&inode_num);
        self.make_room(block_dev)?;

        self.cache.insert(
            inode_num,
            CachedInode::new(
                Ext4Inode::default(),
                alloc::vec![0; self.inode_size],
                inode_num,
                block_num,
                offset,
            ),
        );
        Ok(())
    }

    fn lru_inode(&self) -> Option<InodeNumber> {
        self.cache
            .iter()
            .min_by_key(|(_, cached)| cached.last_access)
            .map(|(inode_num, _)| *inode_num)
    }

    fn touch(&mut self, inode_num: InodeNumber) {
        self.access_counter = self.access_counter.saturating_add(1);
        if let Some(cached) = self.cache.get_mut(&inode_num) {
            cached.last_access = self.access_counter;
            cached.generation = cached.generation.saturating_add(1);
        }
    }

    pub fn get(&self, inode_num: InodeNumber) -> Option<CachedInode> {
        self.cache.get(&inode_num).cloned()
    }

    pub fn get_mut(&mut self, inode_num: InodeNumber) -> Option<CachedInode> {
        self.touch(inode_num);
        self.cache.get(&inode_num).cloned()
    }

    pub fn mark_dirty(&mut self, inode_num: InodeNumber) {
        if let Some(cached) = self.cache.get_mut(&inode_num) {
            cached.mark_dirty();
            cached.generation = cached.generation.saturating_add(1);
        }
    }

    pub fn modify<B, F>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
        block_num: AbsoluteBN,
        offset: usize,
        f: F,
    ) -> Ext4Result<()>
    where
        B: BlockIo,
        F: FnOnce(&mut Ext4Inode, &mut [u8]) -> Ext4Result<()>,
    {
        self.ensure_loaded(block_dev, inode_num, block_num, offset)?;
        self.touch(inode_num);

        let cached = self
            .cache
            .get_mut(&inode_num)
            .ok_or(Ext4Error::corrupted())?;
        let previous_inode = cached.inode;
        let previous_raw_inode = cached.raw_inode.clone();
        if let Err(error) = f(
            &mut cached.inode,
            Arc::make_mut(&mut cached.raw_inode).as_mut_slice(),
        ) {
            cached.inode = previous_inode;
            cached.raw_inode = previous_raw_inode;
            return Err(error);
        }
        cached.mark_dirty();
        cached.generation = cached.generation.saturating_add(1);

        if !USE_MULTILEVEL_CACHE {
            let block_num = cached.block_num;
            let offset = cached.offset_in_block;
            let data = cached.raw_inode.clone();
            Self::write_inode_bytes_static(block_dev, block_num, offset, &data)?;
            let cached = self
                .cache
                .get_mut(&inode_num)
                .ok_or(Ext4Error::corrupted())?;
            cached.dirty = false;
            cached.generation = cached.generation.saturating_add(1);
        }
        Ok(())
    }

    pub fn modify_by_handle<B, F>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        handle: InodeHandle,
        block_num: AbsoluteBN,
        offset: usize,
        f: F,
    ) -> Ext4Result<()>
    where
        B: BlockIo,
        F: FnOnce(&mut Ext4Inode, &mut [u8]) -> Ext4Result<()>,
    {
        self.modify(block_dev, handle.inode_num, block_num, offset, f)
    }

    pub fn evict<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
    ) -> Ext4Result<()> {
        let Some(cached) = self.cache.get(&inode_num).cloned() else {
            return Ok(());
        };
        if cached.dirty {
            Self::write_inode_bytes_static(
                block_dev,
                cached.block_num,
                cached.offset_in_block,
                &cached.raw_inode,
            )?;
        }
        self.cache.remove(&inode_num);
        Ok(())
    }

    pub fn flush_all<B: BlockIo>(&mut self, block_dev: &mut Jbd2Dev<B>) -> Ext4Result<()> {
        let mut dirty = self
            .cache
            .iter()
            .filter(|(_, cached)| cached.dirty)
            .map(|(inode_num, cached)| {
                (
                    *inode_num,
                    cached.block_num,
                    cached.offset_in_block,
                    cached.raw_inode.clone(),
                )
            })
            .collect::<Vec<_>>();
        dirty.sort_by_key(|(_, block_num, offset, _)| (*block_num, *offset));
        Self::write_dirty_inode_blocks(block_dev, &dirty)?;

        for (inode_num, ..) in dirty {
            if let Some(cached) = self.cache.get_mut(&inode_num) {
                cached.dirty = false;
                cached.generation = cached.generation.saturating_add(1);
            }
        }
        Ok(())
    }

    pub fn flush<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        inode_num: InodeNumber,
    ) -> Ext4Result<()> {
        let Some(cached) = self.cache.get(&inode_num).cloned() else {
            return Ok(());
        };
        if cached.dirty {
            Self::write_inode_bytes_static(
                block_dev,
                cached.block_num,
                cached.offset_in_block,
                &cached.raw_inode,
            )?;
            let cached = self
                .cache
                .get_mut(&inode_num)
                .ok_or(Ext4Error::corrupted())?;
            cached.dirty = false;
            cached.generation = cached.generation.saturating_add(1);
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    pub fn stats(&self) -> InodeCacheStats {
        InodeCacheStats {
            total_entries: self.cache.len(),
            dirty_entries: self.cache.values().filter(|cached| cached.dirty).count(),
            max_entries: self.max_entries,
        }
    }

    fn write_inode_bytes_static<B: BlockIo>(
        block_dev: &mut Jbd2Dev<B>,
        block_num: AbsoluteBN,
        offset: usize,
        data: &[u8],
    ) -> Ext4Result<()> {
        let mut buffer = alloc::vec![0u8; block_dev.block_size() as usize];
        block_dev.read_blocks(&mut buffer, block_num, 1)?;
        let end = offset
            .checked_add(data.len())
            .ok_or(Ext4Error::corrupted())?;
        let dst = buffer.get_mut(offset..end).ok_or(Ext4Error::corrupted())?;
        dst.copy_from_slice(data);
        block_dev.write_blocks(&buffer, block_num, 1, true)
    }

    fn write_dirty_inode_blocks<B: BlockIo>(
        block_dev: &mut Jbd2Dev<B>,
        dirty: &[(InodeNumber, AbsoluteBN, usize, Arc<Vec<u8>>)],
    ) -> Ext4Result<()> {
        let mut index = 0;
        while index < dirty.len() {
            let block_num = dirty[index].1;
            let mut buffer = alloc::vec![0u8; block_dev.block_size() as usize];
            block_dev.read_blocks(&mut buffer, block_num, 1)?;

            while index < dirty.len() && dirty[index].1 == block_num {
                let (_, _, offset, data) = &dirty[index];
                let end = offset
                    .checked_add(data.len())
                    .ok_or(Ext4Error::corrupted())?;
                let dst = buffer.get_mut(*offset..end).ok_or(Ext4Error::corrupted())?;
                dst.copy_from_slice(data);
                index += 1;
            }
            block_dev.write_blocks(&buffer, block_num, 1, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InodeCacheStats {
    pub total_entries: usize,
    pub dirty_entries: usize,
    pub max_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disknode::Ext4Timestamp;

    const TEST_INODE_SIZE: usize = 256;

    struct TestBlockDevice {
        data: Vec<u8>,
        fail_writes: bool,
        read_calls: Vec<u64>,
        write_calls: Vec<u64>,
    }

    impl TestBlockDevice {
        fn new(blocks: usize) -> Self {
            Self {
                data: alloc::vec![0; blocks * BLOCK_SIZE],
                fail_writes: false,
                read_calls: Vec::new(),
                write_calls: Vec::new(),
            }
        }

        fn failing_writes(blocks: usize) -> Self {
            Self {
                fail_writes: true,
                ..Self::new(blocks)
            }
        }

        fn record(&self, block: usize, offset: usize) -> &[u8] {
            let start = block * BLOCK_SIZE + offset;
            &self.data[start..start + TEST_INODE_SIZE]
        }

        fn set_mode(&mut self, block: usize, offset: usize, mode: u16) {
            let start = block * BLOCK_SIZE + offset;
            self.data[start..start + 2].copy_from_slice(&mode.to_le_bytes());
        }
    }

    impl BlockIo for TestBlockDevice {
        fn read(
            &mut self,
            buffer: &mut [u8],
            block_id: crate::io::SectorId,
            _count: u32,
        ) -> Ext4Result<()> {
            self.read_calls.push(block_id.raw());
            let start = block_id.as_usize()? * BLOCK_SIZE;
            buffer.copy_from_slice(&self.data[start..start + buffer.len()]);
            Ok(())
        }

        fn write(
            &mut self,
            buffer: &[u8],
            block_id: crate::io::SectorId,
            _count: u32,
        ) -> Ext4Result<()> {
            if self.fail_writes {
                return Err(Ext4Error::io());
            }
            self.write_calls.push(block_id.raw());
            let start = block_id.as_usize()? * BLOCK_SIZE;
            self.data[start..start + buffer.len()].copy_from_slice(buffer);
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

    impl crate::runtime::Clock for TestBlockDevice {
        fn now(&self) -> Ext4Result<Ext4Timestamp> {
            Ok(Ext4Timestamp::new(0, 0))
        }
    }

    fn device_with(device: TestBlockDevice) -> Jbd2Dev<TestBlockDevice> {
        Jbd2Dev::initial_jbd2dev(0, device, false)
    }

    fn inode_number(raw: u32) -> InodeNumber {
        InodeNumber::new(raw).expect("valid inode number")
    }

    fn set_raw_mode(mode: u16) -> impl FnOnce(&mut Ext4Inode, &mut [u8]) -> Ext4Result<()> {
        move |inode, raw| {
            inode.i_mode = mode;
            raw[..2].copy_from_slice(&mode.to_le_bytes());
            Ok(())
        }
    }

    #[test]
    fn test_inode_location_calc() {
        let cache = InodeCache::default(DEFAULT_INODE_SIZE);
        let inodes_per_group = 128;
        let inode_table_start = AbsoluteBN::new(100);

        let (block, offset, group) = cache
            .calc_inode_location(
                InodeNumber::new(1).unwrap(),
                inodes_per_group,
                inode_table_start,
                BLOCK_SIZE,
            )
            .unwrap();
        assert_eq!(block, inode_table_start);
        assert_eq!(offset, 0);
        assert_eq!(group, BGIndex::new(0));

        let inodes_per_block = (BLOCK_SIZE / DEFAULT_INODE_SIZE as usize) as u32;
        let (block, offset, group) = cache
            .calc_inode_location(
                InodeNumber::new(inodes_per_block + 1).unwrap(),
                inodes_per_group,
                inode_table_start,
                BLOCK_SIZE,
            )
            .unwrap();
        assert_eq!(block, AbsoluteBN::new(101));
        assert_eq!(offset, 0);
        assert_eq!(group, BGIndex::new(0));
    }

    #[test]
    fn get_or_load_decodes_each_inode_record_once() {
        let mut device = TestBlockDevice::new(32);
        device.set_mode(10, TEST_INODE_SIZE, 0o100644);
        let mut jbd2_dev = device_with(device);
        let mut cache = InodeCache::new(4, TEST_INODE_SIZE);
        let number = inode_number(2);

        let first = cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(10), TEST_INODE_SIZE)
            .expect("load inode");
        assert_eq!(first.inode.i_mode, 0o100644);
        assert_eq!(first.raw_inode()[..2], 0o100644u16.to_le_bytes());
        assert_eq!(first.raw_inode().len(), TEST_INODE_SIZE);
        assert_eq!(first.handle().inode_num, number);
        assert_eq!(
            (first.block_num, first.offset_in_block),
            (AbsoluteBN::new(10), TEST_INODE_SIZE)
        );
        assert_eq!((first.last_access, first.generation), (1, 1));

        let again = cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(10), TEST_INODE_SIZE)
            .expect("reuse cached inode");
        assert_eq!((again.last_access, again.generation), (2, 2));
        assert_eq!(
            cache.get_mut(number).map(|cached| cached.last_access),
            Some(3)
        );
        assert!(cache.get_mut(inode_number(3)).is_none());
        assert_eq!(jbd2_dev.into_inner().read_calls, [10]);
    }

    #[test]
    fn failed_modify_restores_previous_inode_and_raw_bytes() {
        let mut device = TestBlockDevice::new(32);
        device.set_mode(10, 0, 0o040755);
        let mut jbd2_dev = device_with(device);
        let mut cache = InodeCache::new(4, TEST_INODE_SIZE);
        let number = inode_number(2);

        let error = cache
            .modify(
                &mut jbd2_dev,
                number,
                AbsoluteBN::new(10),
                0,
                |inode, raw| {
                    inode.i_mode = 0o100600;
                    raw[..2].fill(0xff);
                    Err(Ext4Error::invalid_input())
                },
            )
            .expect_err("the closure error must be returned");

        assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
        let cached = cache.get(number).expect("loaded inode stays cached");
        assert_eq!(cached.inode.i_mode, 0o040755);
        assert_eq!(cached.raw_inode()[..2], 0o040755u16.to_le_bytes());
        assert!(!cached.dirty);
        assert!(jbd2_dev.into_inner().write_calls.is_empty());
    }

    #[test]
    fn modify_by_handle_writes_record_in_place_on_flush() {
        let mut device = TestBlockDevice::new(32);
        device.data[10 * BLOCK_SIZE..11 * BLOCK_SIZE].fill(0xee);
        device.data[10 * BLOCK_SIZE + TEST_INODE_SIZE..10 * BLOCK_SIZE + 2 * TEST_INODE_SIZE]
            .fill(0);
        let mut jbd2_dev = device_with(device);
        let mut cache = InodeCache::new(4, TEST_INODE_SIZE);
        let number = inode_number(3);
        let handle = cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(10), TEST_INODE_SIZE)
            .expect("load inode")
            .handle();

        cache
            .modify_by_handle(
                &mut jbd2_dev,
                handle,
                AbsoluteBN::new(10),
                TEST_INODE_SIZE,
                set_raw_mode(0o120777),
            )
            .expect("modify by handle");
        assert_eq!(
            cache.stats().dirty_entries,
            usize::from(USE_MULTILEVEL_CACHE)
        );
        cache.flush(&mut jbd2_dev, number).expect("flush inode");
        cache
            .flush(&mut jbd2_dev, inode_number(4))
            .expect("flushing an absent inode is a no-op");

        assert_eq!(cache.stats().dirty_entries, 0);
        assert_eq!(cache.get(number).expect("cached").inode.i_mode, 0o120777);
        let device = jbd2_dev.into_inner();
        assert_eq!(device.write_calls, [10]);
        assert_eq!(
            device.record(10, TEST_INODE_SIZE)[..2],
            0o120777u16.to_le_bytes()
        );
        assert!(device.record(10, 0).iter().all(|byte| *byte == 0xee));
        assert!(
            device
                .record(10, 2 * TEST_INODE_SIZE)
                .iter()
                .all(|byte| *byte == 0xee)
        );
    }

    #[test]
    fn full_cache_writes_back_dirty_least_recently_used_inode() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(32));
        let mut cache = InodeCache::new(1, TEST_INODE_SIZE);
        let victim = inode_number(2);
        cache
            .modify(
                &mut jbd2_dev,
                victim,
                AbsoluteBN::new(10),
                0,
                set_raw_mode(0o100644),
            )
            .expect("modify victim");

        cache
            .get_or_load(&mut jbd2_dev, inode_number(3), AbsoluteBN::new(11), 0)
            .expect("load second inode");

        assert!(cache.get(victim).is_none());
        assert_eq!(cache.stats().total_entries, 1);
        let device = jbd2_dev.into_inner();
        assert_eq!(device.record(10, 0)[..2], 0o100644u16.to_le_bytes());
    }

    #[test]
    fn initialize_zeroed_replaces_clean_record_without_reading_disk() {
        let mut device = TestBlockDevice::new(32);
        device.set_mode(12, 0, 0o100644);
        let mut jbd2_dev = device_with(device);
        let mut cache = InodeCache::new(4, TEST_INODE_SIZE);
        let number = inode_number(5);
        cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(12), 0)
            .expect("load stale inode");

        cache
            .initialize_zeroed(&mut jbd2_dev, number, AbsoluteBN::new(12), 0)
            .expect("initialize zeroed inode");
        cache
            .initialize_zeroed(&mut jbd2_dev, inode_number(6), AbsoluteBN::new(13), 0)
            .expect("initialize uncached inode");

        let cached = cache.get(number).expect("zeroed inode is cached");
        assert_eq!(cached.inode.i_mode, 0);
        assert!(cached.raw_inode().iter().all(|byte| *byte == 0));
        assert!(!cached.dirty);
        assert_eq!(cache.stats().total_entries, 2);
        assert_eq!(jbd2_dev.into_inner().read_calls, [12]);
    }

    #[test]
    fn initialize_zeroed_rejects_dirty_record_and_out_of_block_offset() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(32));
        let mut cache = InodeCache::new(4, TEST_INODE_SIZE);
        let number = inode_number(2);
        cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(10), 0)
            .expect("load inode");
        cache.mark_dirty(number);
        cache.mark_dirty(inode_number(9));

        let dirty = cache
            .initialize_zeroed(&mut jbd2_dev, number, AbsoluteBN::new(10), 0)
            .expect_err("a dirty record must not be discarded");
        let range = cache
            .initialize_zeroed(
                &mut jbd2_dev,
                inode_number(3),
                AbsoluteBN::new(10),
                BLOCK_SIZE - TEST_INODE_SIZE + 1,
            )
            .expect_err("a record crossing the block end must be rejected");

        assert_eq!(dirty.kind(), Ext4ErrorKind::Corrupted);
        assert_eq!(range.kind(), Ext4ErrorKind::Corrupted);
        assert!(cache.get(number).expect("dirty record stays").dirty);
        assert!(cache.get(inode_number(3)).is_none());
        assert_eq!(cache.stats().dirty_entries, 1);
    }

    #[test]
    fn flush_all_writes_each_dirty_inode_block_once() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(32));
        let mut cache = InodeCache::new(8, TEST_INODE_SIZE);
        for (raw, block, offset) in [(4, 11, 0), (3, 10, TEST_INODE_SIZE), (2, 10, 0)] {
            cache
                .get_or_load(
                    &mut jbd2_dev,
                    inode_number(raw),
                    AbsoluteBN::new(block),
                    offset,
                )
                .expect("load inode");
            cache.mark_dirty(inode_number(raw));
        }
        cache
            .get_or_load(&mut jbd2_dev, inode_number(5), AbsoluteBN::new(12), 0)
            .expect("load clean inode");
        assert_eq!(cache.stats().dirty_entries, 3);

        cache.flush_all(&mut jbd2_dev).expect("flush all inodes");

        let stats = cache.stats();
        assert_eq!((stats.total_entries, stats.dirty_entries), (4, 0));
        assert_eq!(jbd2_dev.into_inner().write_calls, [10, 11]);
    }

    #[test]
    fn write_failures_keep_inode_dirty_and_cached() {
        let mut jbd2_dev = device_with(TestBlockDevice::failing_writes(32));
        let mut cache = InodeCache::new(1, TEST_INODE_SIZE);
        let number = inode_number(2);
        cache
            .get_or_load(&mut jbd2_dev, number, AbsoluteBN::new(10), 0)
            .expect("load inode");
        cache.mark_dirty(number);

        cache
            .flush(&mut jbd2_dev, number)
            .expect_err("flush must report the write failure");
        cache
            .flush_all(&mut jbd2_dev)
            .expect_err("flush_all must report the write failure");
        cache
            .evict(&mut jbd2_dev, number)
            .expect_err("evict must report the write failure");
        cache
            .get_or_load(&mut jbd2_dev, inode_number(3), AbsoluteBN::new(11), 0)
            .expect_err("making room must report the write failure");

        assert!(cache.get(number).expect("inode stays cached").dirty);
        assert!(cache.get(inode_number(3)).is_none());
    }

    #[test]
    fn evict_writes_only_dirty_inodes_and_clear_writes_nothing() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(32));
        let mut cache = InodeCache::default(TEST_INODE_SIZE as u16);
        assert_eq!(cache.stats().max_entries, INODE_CACHE_MAX);
        for (raw, block) in [(2, 10), (3, 11), (4, 12)] {
            cache
                .get_or_load(&mut jbd2_dev, inode_number(raw), AbsoluteBN::new(block), 0)
                .expect("load inode");
        }
        cache.mark_dirty(inode_number(2));
        cache.mark_dirty(inode_number(4));

        cache
            .evict(&mut jbd2_dev, inode_number(2))
            .expect("evict dirty");
        cache
            .evict(&mut jbd2_dev, inode_number(3))
            .expect("evict clean");
        cache
            .evict(&mut jbd2_dev, inode_number(7))
            .expect("evicting an absent inode is a no-op");
        assert_eq!(cache.stats().total_entries, 1);
        cache.clear();

        assert_eq!(cache.stats().total_entries, 0);
        assert_eq!(jbd2_dev.into_inner().write_calls, [10]);
    }
}
