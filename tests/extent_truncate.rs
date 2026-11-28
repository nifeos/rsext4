//! Extent truncates that must free whole index nodes and survive a failed write.
//!
//! Every test shrinks an extent-mapped file. One fails the inode write that finishes the truncate
//! and checks that no byte of the file changed. Two fail the read or the write-through of the
//! retained block after the new size was committed, and check that orphan recovery finishes the
//! shrink. The last test shrinks a depth-2 tree past a whole index node and checks that the node
//! is freed.

// Miri runs only the tests that reach unsafe code, and this file reaches none.
#![cfg(not(miri))]

use std::{cell::Cell, rc::Rc};

use rsext4::{error::Ext4Result, *};

/// The most bytes a test accepts from `read_file`: larger than any file these
/// tests write, so only a recorded size that a test sets on purpose reaches it.
const MAXIMUM_READ_BYTES: usize = 1 << 20;

/// In-memory block device whose write of one chosen sector fails after it lands, and whose read
/// of one chosen sector fails once.
struct MockBlockDevice {
    data: Vec<u8>,
    fail_read_sector: Rc<Cell<Option<u64>>>,
    fail_after_write_sector: Rc<Cell<Option<u64>>>,
    now: Cell<i64>,
}

impl MockBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
            fail_read_sector: Rc::new(Cell::new(None)),
            fail_after_write_sector: Rc::new(Cell::new(None)),
            now: Cell::new(1_700_000_000),
        }
    }

    fn with_write_failure_handle(size: usize) -> (Self, Rc<Cell<Option<u64>>>) {
        let device = Self::new(size);
        let handle = Rc::clone(&device.fail_after_write_sector);
        (device, handle)
    }

    /// Returns the handle for a read failure that fires once.
    fn read_failure_handle(&self) -> Rc<Cell<Option<u64>>> {
        Rc::clone(&self.fail_read_sector)
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
        if self.fail_read_sector.get() == Some(sector.raw()) {
            self.fail_read_sector.set(None);
            return Err(Ext4Error::io());
        }
        buffer.copy_from_slice(&self.data[range]);
        Ok(())
    }

    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let range = self.range(sector, buffer.len())?;
        self.data[range].copy_from_slice(buffer);
        if self.fail_after_write_sector.get() == Some(sector.raw()) {
            self.fail_after_write_sector.set(None);
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
fn failed_extent_truncate_keeps_every_byte_of_the_file() {
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

    let data = read_file(
        &mut jbd2_dev,
        &mut fs,
        "/extent-truncate-tail",
        MAXIMUM_READ_BYTES,
    )
    .unwrap();
    assert_eq!(data.len(), original.len());
    let first_changed = data.iter().zip(&original).position(|(now, was)| now != was);
    assert_eq!(
        first_changed, None,
        "a failed truncate must leave every byte of the file as it was"
    );
}

#[cfg(not(feature = "USE_MULTILEVEL_CACHE"))]
#[test]
fn failed_tail_zeroing_write_leaves_the_shrink_to_orphan_recovery() {
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
        .expect_err("a failed tail zeroing write must fail the truncate");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);
    // The new size and the orphan record were published before the tail
    // was zeroed, so the shrink is left to orphan recovery.
    let inode = fs.get_inode_by_num(&mut jbd2_dev, inode_number).unwrap();
    assert_eq!(inode.size(), 100);
    assert_eq!(fs.superblock.s_last_orphan, inode_number.raw());

    // Upstream's test finishes the shrink with the next truncate of the
    // file, which resumes a published orphan only from upstream dde4020be
    // on. This fork's base predates it, so the remount's orphan recovery
    // finishes the shrink here.
    drop(fs);
    let fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("remount failed");
    assert_eq!(fs.superblock.s_last_orphan, 0);
    let mut block = vec![0u8; BLOCK_SIZE];
    jbd2_dev.read_blocks(&mut block, retained_block, 1).unwrap();
    assert_eq!(block[..100], original[..100]);
    assert_eq!(
        block[100..].iter().position(|byte| *byte != 0),
        None,
        "recovery must zero the EOF block past the new size"
    );
}

/// A fresh mount has nothing cached, so zeroing the tail must read the retained block first. The
/// new size and the orphan record are committed before that read, so when it fails, the truncate
/// reports the read's error and leaves the shrink to orphan recovery, which zeroes the tail.
#[test]
fn failed_read_of_the_retained_block_leaves_the_shrink_to_orphan_recovery() {
    let device = MockBlockDevice::new(100 * 1024 * 1024);
    let fail_read_sector = device.read_failure_handle();
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);

    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    mkfile(&mut jbd2_dev, &mut fs, "/tail-read", None, None).expect("file creation failed");
    let original: Vec<u8> = (0..2 * BLOCK_SIZE).map(|i| (i % 251) as u8 + 1).collect();
    write_file(&mut jbd2_dev, &mut fs, "/tail-read", 0, &original).unwrap();
    umount(fs, &mut jbd2_dev).expect("unmount failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("remount failed");
    let (inode_number, mut inode) = dir::get_inode_with_num(&mut fs, &mut jbd2_dev, "/tail-read")
        .unwrap()
        .unwrap();
    let retained_block =
        loopfile::resolve_inode_block(&fs, &mut jbd2_dev, inode_number, &mut inode, 0)
            .unwrap()
            .expect("retained block missing");

    fail_read_sector.set(Some(retained_block.raw()));
    let error = truncate(&mut jbd2_dev, &mut fs, "/tail-read", 100)
        .expect_err("a failed read of the retained block must fail the truncate");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);
    assert_eq!(
        fail_read_sector.get(),
        None,
        "the read must have been the one that failed"
    );
    let inode = fs.get_inode_by_num(&mut jbd2_dev, inode_number).unwrap();
    assert_eq!(inode.size(), 100);
    assert_eq!(fs.superblock.s_last_orphan, inode_number.raw());

    // Mount the device again as after a crash, from what reached it.
    drop(fs);
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, jbd2_dev.into_inner(), true);
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("remount failed");
    assert_eq!(fs.superblock.s_last_orphan, 0);
    let data = read_file(&mut jbd2_dev, &mut fs, "/tail-read", MAXIMUM_READ_BYTES).unwrap();
    assert_eq!(data[..], original[..100]);
    let mut block = vec![0u8; BLOCK_SIZE];
    jbd2_dev.read_blocks(&mut block, retained_block, 1).unwrap();
    assert_eq!(
        block[100..].iter().position(|byte| *byte != 0),
        None,
        "recovery must zero the EOF block past the new size"
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

    let data = read_file(
        &mut jbd2_dev,
        &mut fs,
        "/extent-depth-two",
        MAXIMUM_READ_BYTES,
    )
    .unwrap();
    assert_eq!(data, vec![1u8; 100]);
    // 1499 data blocks and at least the second index node and its
    // leaves are free again.
    assert!(fs.superblock.free_blocks_count() > free_blocks_before + EXTENTS);

    umount(fs, &mut jbd2_dev).expect("unmount failed");
    let mut fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("remount failed");
    let data = read_file(
        &mut jbd2_dev,
        &mut fs,
        "/extent-depth-two",
        MAXIMUM_READ_BYTES,
    )
    .unwrap();
    assert_eq!(data, vec![1u8; 100]);
}
