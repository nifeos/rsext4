//! Bitmap cache helpers.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use crate::{
    BITMAP_CACHE_MAX,
    blockdev::*,
    bmalloc::{AbsoluteBN, BGIndex},
    config::USE_MULTILEVEL_CACHE,
    error::*,
};

/// Type of bitmap stored in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BitmapType {
    /// Block bitmap.
    Block,
    /// Inode bitmap.
    Inode,
}

/// Cache key for one bitmap in one block group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheKey {
    pub group_id: BGIndex,
    pub bitmap_type: BitmapType,
}

impl CacheKey {
    pub fn new_block(group_id: BGIndex) -> Self {
        Self {
            group_id,
            bitmap_type: BitmapType::Block,
        }
    }

    pub fn new_inode(group_id: BGIndex) -> Self {
        Self {
            group_id,
            bitmap_type: BitmapType::Inode,
        }
    }
}

/// Cached bitmap payload.
#[derive(Debug, Clone)]
pub struct CachedBitmap {
    /// Bitmap bytes.
    pub data: Arc<Vec<u8>>,
    /// Whether the cache entry is dirty.
    pub dirty: bool,
    /// Physical block storing the bitmap.
    pub block_num: AbsoluteBN,
    /// Access timestamp for LRU eviction.
    pub last_access: u64,
    /// Generation counter bumped on every access.
    pub generation: u64,
}

impl CachedBitmap {
    pub fn new(data: Vec<u8>, block_num: AbsoluteBN) -> Self {
        Self {
            data: Arc::new(data),
            dirty: false,
            block_num,
            last_access: 0,
            generation: 0,
        }
    }

    /// Marks the bitmap entry dirty.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

/// Bitmap cache owned exclusively by one mounted filesystem.
///
/// The cache deliberately has no internal lock. Callers need mutable access to
/// the mounted filesystem before changing cache state; an OS adapter may place
/// its own sleepable lock around that owner.
#[derive(Clone)]
pub struct BitmapCache {
    cache: BTreeMap<CacheKey, CachedBitmap>,
    max_entries: usize,
    access_counter: u64,
}

impl BitmapCache {
    /// Creates a bitmap cache.
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: BTreeMap::new(),
            max_entries,
            access_counter: 0,
        }
    }

    /// Creates a bitmap cache with the default size.
    pub fn create_default() -> Self {
        Self::new(BITMAP_CACHE_MAX)
    }

    /// Returns a cached bitmap, loading it from disk on demand.
    pub fn get_or_load<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        key: CacheKey,
        block_num: AbsoluteBN,
    ) -> Ext4Result<CachedBitmap> {
        self.ensure_loaded(block_dev, key, block_num)?;
        self.touch(key);
        self.cache.get(&key).cloned().ok_or(Ext4Error::corrupted())
    }

    fn ensure_loaded<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        key: CacheKey,
        block_num: AbsoluteBN,
    ) -> Ext4Result<()> {
        if self.cache.contains_key(&key) {
            return Ok(());
        }

        // Read first so a read failure cannot evict a valid cached entry.
        let mut data = alloc::vec![0u8; block_dev.block_size() as usize];
        block_dev.read_blocks(&mut data, block_num, 1)?;

        if self.cache.len() >= self.max_entries
            && let Some(victim_key) = self.lru_key()
        {
            let victim = self
                .cache
                .get(&victim_key)
                .cloned()
                .ok_or(Ext4Error::corrupted())?;
            if victim.dirty {
                Self::write_bitmap_static(block_dev, victim.block_num, &victim.data)?;
            }
            self.cache.remove(&victim_key);
        }

        self.cache.insert(key, CachedBitmap::new(data, block_num));
        Ok(())
    }

    fn touch(&mut self, key: CacheKey) {
        self.access_counter = self.access_counter.saturating_add(1);
        if let Some(bitmap) = self.cache.get_mut(&key) {
            bitmap.last_access = self.access_counter;
            bitmap.generation = bitmap.generation.saturating_add(1);
        }
    }

    fn lru_key(&self) -> Option<CacheKey> {
        self.cache
            .iter()
            .min_by_key(|(_, bitmap)| bitmap.last_access)
            .map(|(key, _)| *key)
    }

    /// Returns a cached bitmap without loading from disk.
    pub fn get(&self, key: &CacheKey) -> Option<CachedBitmap> {
        self.cache.get(key).cloned()
    }

    /// Returns an owned mutable-view snapshot and refreshes its LRU state.
    pub fn get_mut(&mut self, key: &CacheKey) -> Option<CachedBitmap> {
        self.touch(*key);
        self.cache.get(key).cloned()
    }

    /// Marks a cached bitmap dirty.
    pub fn mark_dirty(&mut self, key: &CacheKey) {
        if let Some(bitmap) = self.cache.get_mut(key) {
            bitmap.mark_dirty();
            bitmap.generation = bitmap.generation.saturating_add(1);
        }
    }

    /// Modifies one cached bitmap and marks it dirty.
    pub fn modify<B, F>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        key: CacheKey,
        block_num: AbsoluteBN,
        f: F,
    ) -> Ext4Result<()>
    where
        B: BlockIo,
        F: FnOnce(&mut [u8]),
    {
        self.ensure_loaded(block_dev, key, block_num)?;
        self.touch(key);

        let bitmap = self.cache.get_mut(&key).ok_or(Ext4Error::corrupted())?;
        f(Arc::make_mut(&mut bitmap.data).as_mut_slice());
        bitmap.mark_dirty();
        bitmap.generation = bitmap.generation.saturating_add(1);

        if !USE_MULTILEVEL_CACHE {
            let data = bitmap.data.clone();
            let block_num = bitmap.block_num;
            Self::write_bitmap_static(block_dev, block_num, &data)?;
            let bitmap = self.cache.get_mut(&key).ok_or(Ext4Error::corrupted())?;
            bitmap.dirty = false;
            bitmap.generation = bitmap.generation.saturating_add(1);
        }
        Ok(())
    }

    /// Evicts one cached bitmap.
    pub fn evict<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        key: &CacheKey,
    ) -> Ext4Result<()> {
        let Some(bitmap) = self.cache.get(key).cloned() else {
            return Ok(());
        };
        if bitmap.dirty {
            Self::write_bitmap_static(block_dev, bitmap.block_num, &bitmap.data)?;
        }
        self.cache.remove(key);
        Ok(())
    }

    /// Flushes all dirty bitmaps to disk.
    pub fn flush_all<B: BlockIo>(&mut self, block_dev: &mut Jbd2Dev<B>) -> Ext4Result<()> {
        let mut dirty = self
            .cache
            .iter()
            .filter(|(_, bitmap)| bitmap.dirty)
            .map(|(key, bitmap)| (*key, bitmap.block_num, bitmap.data.clone()))
            .collect::<Vec<_>>();
        dirty.sort_by_key(|(_, block_num, _)| *block_num);

        for (_, block_num, data) in &dirty {
            Self::write_bitmap_static(block_dev, *block_num, data)?;
        }
        for (key, ..) in dirty {
            if let Some(bitmap) = self.cache.get_mut(&key) {
                bitmap.dirty = false;
                bitmap.generation = bitmap.generation.saturating_add(1);
            }
        }
        Ok(())
    }

    /// Flushes one bitmap to disk.
    pub fn flush<B: BlockIo>(
        &mut self,
        block_dev: &mut Jbd2Dev<B>,
        key: &CacheKey,
    ) -> Ext4Result<()> {
        let Some(bitmap) = self.cache.get(key).cloned() else {
            return Ok(());
        };
        if bitmap.dirty {
            Self::write_bitmap_static(block_dev, bitmap.block_num, &bitmap.data)?;
            let bitmap = self.cache.get_mut(key).ok_or(Ext4Error::corrupted())?;
            bitmap.dirty = false;
            bitmap.generation = bitmap.generation.saturating_add(1);
        }
        Ok(())
    }

    /// Clears the cache without flushing.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Returns cache statistics.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            total_entries: self.cache.len(),
            dirty_entries: self.cache.values().filter(|bitmap| bitmap.dirty).count(),
            max_entries: self.max_entries,
        }
    }

    fn write_bitmap_static<B: BlockIo>(
        block_dev: &mut Jbd2Dev<B>,
        block_num: AbsoluteBN,
        data: &[u8],
    ) -> Ext4Result<()> {
        let block_size = block_dev.block_size() as usize;
        let mut buffer = alloc::vec![0u8; block_size];
        block_dev.read_blocks(&mut buffer, block_num, 1)?;
        let len = core::cmp::min(data.len(), block_size);
        buffer[..len].copy_from_slice(&data[..len]);
        block_dev.write_blocks(&buffer, block_num, 1, true)
    }
}

/// Cache statistics.
#[derive(Debug, Clone, Copy)]
pub struct CacheStats {
    pub total_entries: usize,
    pub dirty_entries: usize,
    pub max_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::BLOCK_SIZE, disknode::Ext4Timestamp};

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

        fn fill_block(&mut self, block: usize, value: u8) {
            self.data[block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE].fill(value);
        }

        fn block(&self, block: usize) -> &[u8] {
            &self.data[block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE]
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
            let source = self
                .data
                .get(start..start + buffer.len())
                .ok_or_else(Ext4Error::io)?;
            buffer.copy_from_slice(source);
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

    #[test]
    fn create_default_uses_configured_bitmap_capacity() {
        let cache = BitmapCache::create_default();
        let stats = cache.stats();
        assert_eq!(stats.max_entries, BITMAP_CACHE_MAX);
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.dirty_entries, 0);
    }

    #[test]
    fn get_or_load_reads_each_bitmap_once_and_refreshes_its_access_state() {
        let mut device = TestBlockDevice::new(64);
        device.fill_block(7, 0xa5);
        device.fill_block(8, 0x5a);
        let mut jbd2_dev = device_with(device);
        let mut cache = BitmapCache::new(4);
        let block_key = CacheKey::new_block(BGIndex::new(0));
        let inode_key = CacheKey::new_inode(BGIndex::new(0));
        assert_ne!(block_key, inode_key);

        let first = cache
            .get_or_load(&mut jbd2_dev, block_key, AbsoluteBN::new(7))
            .expect("load block bitmap");
        assert_eq!(first.block_num, AbsoluteBN::new(7));
        assert!(first.data.iter().all(|byte| *byte == 0xa5));
        assert!(!first.dirty);
        assert_eq!((first.last_access, first.generation), (1, 1));

        let inode_bitmap = cache
            .get_or_load(&mut jbd2_dev, inode_key, AbsoluteBN::new(8))
            .expect("load inode bitmap");
        assert!(inode_bitmap.data.iter().all(|byte| *byte == 0x5a));

        let again = cache
            .get_or_load(&mut jbd2_dev, block_key, AbsoluteBN::new(7))
            .expect("reuse cached block bitmap");
        assert_eq!((again.last_access, again.generation), (3, 2));
        assert_eq!(
            cache.get(&block_key).map(|bitmap| bitmap.generation),
            Some(2)
        );
        assert_eq!(
            cache.get_mut(&block_key).map(|bitmap| bitmap.last_access),
            Some(4)
        );
        assert!(cache.get(&CacheKey::new_block(BGIndex::new(1))).is_none());
        assert!(
            cache
                .get_mut(&CacheKey::new_block(BGIndex::new(1)))
                .is_none()
        );
        assert_eq!(cache.stats().total_entries, 2);
        assert_eq!(jbd2_dev.into_inner().read_calls, [7, 8]);
    }

    #[test]
    fn failed_bitmap_read_keeps_cached_entry_resident() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(16));
        let mut cache = BitmapCache::new(1);
        let resident = CacheKey::new_block(BGIndex::new(0));
        cache
            .get_or_load(&mut jbd2_dev, resident, AbsoluteBN::new(3))
            .expect("load resident bitmap");

        cache
            .get_or_load(
                &mut jbd2_dev,
                CacheKey::new_block(BGIndex::new(1)),
                AbsoluteBN::new(1000),
            )
            .expect_err("a bitmap beyond the device must fail to load");

        assert!(cache.get(&resident).is_some());
        assert_eq!(cache.stats().total_entries, 1);
    }

    #[test]
    fn full_cache_writes_back_dirty_least_recently_used_bitmap() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(64));
        let mut cache = BitmapCache::new(2);
        let first = CacheKey::new_block(BGIndex::new(0));
        let second = CacheKey::new_block(BGIndex::new(1));
        cache
            .get_or_load(&mut jbd2_dev, first, AbsoluteBN::new(10))
            .expect("load first");
        cache
            .get_or_load(&mut jbd2_dev, second, AbsoluteBN::new(11))
            .expect("load second");
        cache
            .modify(&mut jbd2_dev, first, AbsoluteBN::new(10), |data| {
                data[0] = 0xff;
            })
            .expect("modify first");
        cache
            .get_or_load(&mut jbd2_dev, second, AbsoluteBN::new(11))
            .expect("touch second");

        cache
            .get_or_load(
                &mut jbd2_dev,
                CacheKey::new_block(BGIndex::new(2)),
                AbsoluteBN::new(12),
            )
            .expect("load third and evict first");

        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
        assert_eq!(cache.stats().total_entries, 2);
        let device = jbd2_dev.into_inner();
        assert_eq!(device.block(10)[0], 0xff);
        assert!(device.block(10)[1..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn dirty_eviction_write_failure_keeps_victim_cached() {
        let mut jbd2_dev = device_with(TestBlockDevice::failing_writes(64));
        let mut cache = BitmapCache::new(1);
        let victim = CacheKey::new_inode(BGIndex::new(0));
        cache
            .get_or_load(&mut jbd2_dev, victim, AbsoluteBN::new(10))
            .expect("load victim");
        cache.mark_dirty(&victim);

        cache
            .get_or_load(
                &mut jbd2_dev,
                CacheKey::new_inode(BGIndex::new(1)),
                AbsoluteBN::new(11),
            )
            .expect_err("writeback failure must abort the load");

        assert!(cache.get(&victim).expect("victim stays").dirty);
        assert_eq!(cache.stats().total_entries, 1);
    }

    #[test]
    fn modify_defers_writeback_until_flush_when_multilevel_cache_is_enabled() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(64));
        let mut cache = BitmapCache::new(4);
        let key = CacheKey::new_block(BGIndex::new(0));
        cache
            .modify(&mut jbd2_dev, key, AbsoluteBN::new(20), |data| {
                data[..2].copy_from_slice(&[0x0f, 0xf0]);
            })
            .expect("modify bitmap");

        let cached = cache.get(&key).expect("modified bitmap is cached");
        assert_eq!(cached.data[..2], [0x0f, 0xf0]);
        assert_eq!(cached.dirty, USE_MULTILEVEL_CACHE);
        assert_eq!(
            cache.stats().dirty_entries,
            usize::from(USE_MULTILEVEL_CACHE)
        );

        cache.flush(&mut jbd2_dev, &key).expect("flush bitmap");
        cache
            .flush(&mut jbd2_dev, &CacheKey::new_inode(BGIndex::new(0)))
            .expect("flushing an absent bitmap is a no-op");
        assert_eq!(cache.stats().dirty_entries, 0);
        assert!(!cache.get(&key).expect("still cached").dirty);
        let device = jbd2_dev.into_inner();
        assert_eq!(device.block(20)[..2], [0x0f, 0xf0]);
        assert_eq!(device.write_calls, [20]);
    }

    #[test]
    fn failed_flush_keeps_bitmap_dirty() {
        let mut jbd2_dev = device_with(TestBlockDevice::failing_writes(64));
        let mut cache = BitmapCache::new(4);
        let key = CacheKey::new_block(BGIndex::new(0));
        cache
            .get_or_load(&mut jbd2_dev, key, AbsoluteBN::new(5))
            .expect("load bitmap");
        cache.mark_dirty(&key);

        cache
            .flush(&mut jbd2_dev, &key)
            .expect_err("flush must report the write failure");
        cache
            .flush_all(&mut jbd2_dev)
            .expect_err("flush_all must report the write failure");
        cache
            .evict(&mut jbd2_dev, &key)
            .expect_err("evict must report the write failure");

        assert!(cache.get(&key).expect("bitmap stays cached").dirty);
        assert_eq!(cache.stats().dirty_entries, 1);
    }

    #[test]
    fn flush_all_writes_dirty_bitmaps_in_block_order_and_cleans_them() {
        let mut device = TestBlockDevice::new(64);
        device.fill_block(30, 0x30);
        device.fill_block(20, 0x20);
        let mut jbd2_dev = device_with(device);
        let mut cache = BitmapCache::new(4);
        let high = CacheKey::new_block(BGIndex::new(0));
        let low = CacheKey::new_block(BGIndex::new(1));
        let clean = CacheKey::new_inode(BGIndex::new(0));
        for (key, block) in [(high, 30), (low, 20), (clean, 25)] {
            cache
                .get_or_load(&mut jbd2_dev, key, AbsoluteBN::new(block))
                .expect("load bitmap");
        }
        cache.mark_dirty(&high);
        cache.mark_dirty(&low);
        cache.mark_dirty(&CacheKey::new_inode(BGIndex::new(9)));
        assert_eq!(cache.stats().dirty_entries, 2);

        cache.flush_all(&mut jbd2_dev).expect("flush all bitmaps");

        assert_eq!(cache.stats().dirty_entries, 0);
        assert_eq!(cache.stats().total_entries, 3);
        let device = jbd2_dev.into_inner();
        assert_eq!(device.write_calls, [20, 30]);
        assert!(device.block(20).iter().all(|byte| *byte == 0x20));
        assert!(device.block(30).iter().all(|byte| *byte == 0x30));
    }

    #[test]
    fn evict_writes_only_dirty_bitmaps_before_dropping_them() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(64));
        let mut cache = BitmapCache::new(4);
        let dirty = CacheKey::new_block(BGIndex::new(0));
        let clean = CacheKey::new_block(BGIndex::new(1));
        cache
            .get_or_load(&mut jbd2_dev, dirty, AbsoluteBN::new(40))
            .expect("load dirty");
        cache
            .get_or_load(&mut jbd2_dev, clean, AbsoluteBN::new(41))
            .expect("load clean");
        cache.mark_dirty(&dirty);

        cache.evict(&mut jbd2_dev, &dirty).expect("evict dirty");
        cache.evict(&mut jbd2_dev, &clean).expect("evict clean");
        cache
            .evict(&mut jbd2_dev, &CacheKey::new_inode(BGIndex::new(0)))
            .expect("evicting an absent bitmap is a no-op");

        assert_eq!(cache.stats().total_entries, 0);
        assert_eq!(jbd2_dev.into_inner().write_calls, [40]);
    }

    #[test]
    fn clear_drops_dirty_bitmaps_without_writing_them() {
        let mut jbd2_dev = device_with(TestBlockDevice::new(64));
        let mut cache = BitmapCache::new(4);
        let key = CacheKey::new_block(BGIndex::new(0));
        cache
            .get_or_load(&mut jbd2_dev, key, AbsoluteBN::new(6))
            .expect("load bitmap");
        cache.mark_dirty(&key);

        cache.clear();

        assert!(cache.get(&key).is_none());
        assert_eq!(cache.stats().total_entries, 0);
        assert!(jbd2_dev.into_inner().write_calls.is_empty());
    }
}
