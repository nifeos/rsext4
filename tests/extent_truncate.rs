//! Extent truncates that must free whole index nodes and survive a failed write.
//!
//! Every test shrinks an extent-mapped file. Three fail a write during the truncate: the inode
//! write that finishes it, the write-through that zeroes the tail of the retained block, and the
//! write that restores that tail after the inode write failed. The first two check that no byte of
//! the file changed, and the third that the failed restore is the error reported. The last test
//! shrinks a depth-2 tree past a whole index node and checks that the node is freed.

use std::{cell::Cell, rc::Rc};

use rsext4::{error::Ext4Result, *};

/// In-memory block device whose write of one chosen sector fails after it lands.
struct MockBlockDevice {
    data: Vec<u8>,
    fail_after_write_sector: Rc<Cell<Option<u64>>>,
    /// Takes the place of `fail_after_write_sector` once that failure fires.
    then_fail_after_write_sector: Rc<Cell<Option<u64>>>,
    now: Cell<i64>,
}

impl MockBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
            fail_after_write_sector: Rc::new(Cell::new(None)),
            then_fail_after_write_sector: Rc::new(Cell::new(None)),
            now: Cell::new(1_700_000_000),
        }
    }

    fn with_write_failure_handle(size: usize) -> (Self, Rc<Cell<Option<u64>>>) {
        let device = Self::new(size);
        let handle = Rc::clone(&device.fail_after_write_sector);
        (device, handle)
    }

    /// Returns the handle for a write failure armed once the first one fires.
    #[cfg(not(feature = "USE_MULTILEVEL_CACHE"))]
    fn then_write_failure_handle(&self) -> Rc<Cell<Option<u64>>> {
        Rc::clone(&self.then_fail_after_write_sector)
    }

    fn range(&self, sector: SectorId, length: usize) -> Ext4Result<std::ops::Range<usize>> {
        let start = sector.as_usize()? * BLOCK_SIZE;
        let end = start + length;
        if end > self.data.len() {
            return Err(Ext4Error::block_out_of_range(
                sector.to_u32()?,
                (self.data.len() / BLOCK_SIZE) as u64,
            ));
        }
        Ok(start..end)
    }
}

impl BlockIo for MockBlockDevice {
    fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let range = self.range(sector, buffer.len())?;
        buffer.copy_from_slice(&self.data[range]);
        Ok(())
    }

    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let range = self.range(sector, buffer.len())?;
        self.data[range].copy_from_slice(buffer);
        if self.fail_after_write_sector.get() == Some(sector.raw()) {
            self.fail_after_write_sector
                .set(self.then_fail_after_write_sector.take());
            Err(Ext4Error::io())
        } else {
            Ok(())
        }
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(BLOCK_SIZE as u32, (self.data.len() / BLOCK_SIZE) as u64)
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

impl Clock for MockBlockDevice {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        let sec = self.now.get();
        self.now.set(sec + 1);
        Ok(Ext4Timestamp::new(sec, 0))
    }
}

#[test]
fn failed_extent_truncate_restores_the_zeroed_tail_of_the_retained_block() {
    let (device, fail_after_write_sector) =
        MockBlockDevice::with_write_failure_handle(100 * 1024 * 1024);
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);

    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    mkfile(&mut jbd2_dev, &mut fs, "/extent-truncate-tail", None, None)
        .expect("file creation failed");
    let original: Vec<u8> = (0..2 * BLOCK_SIZE).map(|i| (i % 251) as u8 + 1).collect();
    write_file(
        &mut jbd2_dev,
        &mut fs,
        "/extent-truncate-tail",
        0,
        &original,
    )
    .unwrap();
    jbd2_dev.umount_commit().unwrap();
    jbd2_dev.set_journal_use(false).unwrap();
    let inode_table = fs.group_descs[0].inode_table();

    fail_after_write_sector.set(Some(inode_table));
    let error = truncate(&mut jbd2_dev, &mut fs, "/extent-truncate-tail", 100)
        .expect_err("inode finalize failure must abort the extent truncate");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);

    let data = read_file(&mut jbd2_dev, &mut fs, "/extent-truncate-tail").unwrap();
    assert_eq!(data.len(), original.len());
    let first_changed = data.iter().zip(&original).position(|(now, was)| now != was);
    assert_eq!(
        first_changed, None,
        "a failed truncate must leave every byte of the file as it was"
    );
}

#[cfg(not(feature = "USE_MULTILEVEL_CACHE"))]
#[test]
fn failed_tail_zeroing_write_restores_the_retained_block() {
    let (device, fail_after_write_sector) =
        MockBlockDevice::with_write_failure_handle(100 * 1024 * 1024);
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);

    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    mkfile(&mut jbd2_dev, &mut fs, "/tail-zeroing-write", None, None)
        .expect("file creation failed");
    let original: Vec<u8> = (0..2 * BLOCK_SIZE).map(|i| (i % 251) as u8 + 1).collect();
    write_file(&mut jbd2_dev, &mut fs, "/tail-zeroing-write", 0, &original).unwrap();
    jbd2_dev.umount_commit().unwrap();
    jbd2_dev.set_journal_use(false).unwrap();
    let (inode_number, mut inode) =
        dir::get_inode_with_num(&mut fs, &mut jbd2_dev, "/tail-zeroing-write")
            .unwrap()
            .unwrap();
    let retained_block =
        loopfile::resolve_inode_block(&fs, &mut jbd2_dev, inode_number, &mut inode, 0)
            .unwrap()
            .expect("retained block missing");

    fail_after_write_sector.set(Some(retained_block.raw()));
    let error = truncate(&mut jbd2_dev, &mut fs, "/tail-zeroing-write", 100)
        .expect_err("a failed tail zeroing write must abort the truncate");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);

    let data = read_file(&mut jbd2_dev, &mut fs, "/tail-zeroing-write").unwrap();
    assert_eq!(data.len(), original.len());
    let first_changed = data.iter().zip(&original).position(|(now, was)| now != was);
    assert_eq!(
        first_changed, None,
        "a failed truncate must leave every byte of the file as it was"
    );
}

#[cfg(not(feature = "USE_MULTILEVEL_CACHE"))]
#[test]
fn failed_tail_restore_after_failed_truncate_commit_is_reported() {
    let (device, fail_after_write_sector) =
        MockBlockDevice::with_write_failure_handle(100 * 1024 * 1024);
    let then_fail_after_write_sector = device.then_write_failure_handle();
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);

    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    mkfile(&mut jbd2_dev, &mut fs, "/tail-restore-write", None, None)
        .expect("file creation failed");
    let original: Vec<u8> = (0..2 * BLOCK_SIZE).map(|i| (i % 251) as u8 + 1).collect();
    write_file(&mut jbd2_dev, &mut fs, "/tail-restore-write", 0, &original).unwrap();
    jbd2_dev.umount_commit().unwrap();
    jbd2_dev.set_journal_use(false).unwrap();
    let (inode_number, mut inode) =
        dir::get_inode_with_num(&mut fs, &mut jbd2_dev, "/tail-restore-write")
            .unwrap()
            .unwrap();
    let retained_block =
        loopfile::resolve_inode_block(&fs, &mut jbd2_dev, inode_number, &mut inode, 0)
            .unwrap()
            .expect("retained block missing");
    let inode_table = fs.group_descs[0].inode_table();

    fail_after_write_sector.set(Some(inode_table));
    then_fail_after_write_sector.set(Some(retained_block.raw()));
    let error = truncate(&mut jbd2_dev, &mut fs, "/tail-restore-write", 100)
        .expect_err("inode finalize failure must abort the truncate");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);
    assert_eq!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "rollback:truncate_tail_restore"
        }),
        "a failed tail restore must be reported in place of the commit error"
    );
    assert_eq!(
        fail_after_write_sector.get(),
        None,
        "the restore write must have been the one that failed"
    );
}

/// Writing every other block makes one single-block extent per block, so
/// 1500 of them need a depth-2 tree whose root holds two level-1 index
/// nodes. Shrinking to one block removes every leaf under the second
/// index node, which must then be freed rather than written back empty.
#[test]
fn shrinking_extent_inode_past_a_whole_index_node_frees_that_node() {
    const EXTENTS: u64 = 1500;
    let device = MockBlockDevice::new(100 * 1024 * 1024);
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);

    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    mkfile(&mut jbd2_dev, &mut fs, "/extent-depth-two", None, None).expect("file creation failed");
    for index in 0..EXTENTS {
        let block = vec![(index % 251) as u8 + 1; BLOCK_SIZE];
        write_file(
            &mut jbd2_dev,
            &mut fs,
            "/extent-depth-two",
            2 * index * BLOCK_SIZE as u64,
            &block,
        )
        .unwrap();
    }
    let (inode_number, inode) =
        dir::get_inode_with_num(&mut fs, &mut jbd2_dev, "/extent-depth-two")
            .unwrap()
            .unwrap();
    // The inline root header is eh_magic, eh_entries, eh_max, eh_depth.
    let root_entries = inode.i_block[0] >> 16;
    let root_depth = inode.i_block[1] >> 16;
    assert_eq!(root_depth, 2, "the fixture must build a depth-2 tree");
    assert!(root_entries >= 2, "the root must hold two index nodes");
    let free_blocks_before = fs.superblock.free_blocks_count();

    truncate_inode(&mut jbd2_dev, &mut fs, inode_number, 100)
        .expect("shrinking a depth-2 tree must succeed");

    let data = read_file(&mut jbd2_dev, &mut fs, "/extent-depth-two").unwrap();
    assert_eq!(data, vec![1u8; 100]);
    // 1499 data blocks and at least the second index node and its
    // leaves are free again.
    assert!(fs.superblock.free_blocks_count() > free_blocks_before + EXTENTS);

    umount(fs, &mut jbd2_dev).expect("unmount failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("remount failed");
    let data = read_file(&mut jbd2_dev, &mut fs, "/extent-depth-two").unwrap();
    assert_eq!(data, vec![1u8; 100]);
}
