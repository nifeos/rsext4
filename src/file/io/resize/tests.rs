//! Tests for resize: which inodes each purpose accepts, growth limits, the
//! resume decision, shrink publication and the restarted removal paths.
//!
//! The filesystem has 1 KiB blocks in four allocation groups. A test that
//! needs a removal to restart installs a small journal, so that a removal
//! across several groups cannot fit one transaction but one group can.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::Cell;

use super::*;
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    extents_tree::ExtentTree,
    jbd2::jbdstruct::{JBD2_BLOCKTYPE_SUPERBLOCK_V1, JournalSuperBlock},
    mkfs_with_options,
};

const BLOCK: usize = 1024;
const BLOCKS: usize = 30000;
const GIB: u64 = 1 << 30;
/// The highest logical block an extent can map.
const LAST_LOGICAL: u64 = u32::MAX as u64 - 1;
/// The size whose last block is `LAST_LOGICAL`.
const LARGEST_SIZE: u64 = u32::MAX as u64 * BLOCK as u64;
/// Direct, single, double and triple indirect blocks at 1 KiB blocks.
const LEGACY_BLOCKS: u64 = 12 + 256 + 256 * 256 + 256 * 256 * 256;
/// A journal whose transactions hold five credits: one group's removal
/// (four credits) fits, two groups' (six credits) does not.
const FIVE_CREDIT_JOURNAL: u32 = 21;
/// A journal whose transactions hold three credits: a published size (two
/// credits) fits, one group's removal does not.
const THREE_CREDIT_JOURNAL: u32 = 15;

struct MemoryDevice {
    bytes: Vec<u8>,
    /// Fails the next write that covers this block, once.
    fail_write: Rc<Cell<Option<u64>>>,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let block = sector.raw();
        let blocks = (buffer.len() / BLOCK) as u64;
        if let Some(target) = self.fail_write.get()
            && (block..block + blocks).contains(&target)
        {
            self.fail_write.set(None);
            return Err(Ext4Error::io());
        }
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

struct Mount {
    dev: Jbd2Dev<MemoryDevice>,
    fs: Ext4FileSystem,
    fail_write: Rc<Cell<Option<u64>>>,
}

fn mount() -> Mount {
    let fail_write = Rc::new(Cell::new(None));
    let mut dev = Jbd2Dev::initial_jbd2dev(
        0,
        MemoryDevice {
            bytes: alloc::vec![0; BLOCKS * BLOCK],
            fail_write: fail_write.clone(),
        },
        true,
    );
    dev.set_filesystem_block_size(BLOCK).expect("block size");
    mkfs_with_options(
        &mut dev,
        MkfsOptions {
            block_size: BLOCK as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    let fs = Ext4FileSystem::mount(&mut dev).expect("mount");
    Mount {
        dev,
        fs,
        fail_write,
    }
}

impl Mount {
    fn number(&mut self, path: &str) -> InodeNumber {
        crate::dir::get_inode_with_num(&mut self.fs, &mut self.dev, path)
            .expect("lookup")
            .expect("exists")
            .0
    }

    /// Creates an empty regular file, mapped by extents or by legacy pointers.
    fn file(&mut self, path: &str, legacy: bool) -> InodeNumber {
        crate::file::mkfile(&mut self.dev, &mut self.fs, path, None, None).expect("mkfile");
        let number = self.number(path);
        if legacy {
            self.fs
                .modify_inode(&mut self.dev, number, |inode| {
                    inode.i_flags &= !Ext4Inode::EXT4_EXTENTS_FL;
                    inode.i_block = [0; 15];
                })
                .expect("legacy inode");
        }
        number
    }

    /// Writes one block of data at `logical`, after filling every group
    /// before `group` so that the block lands in `group` or later.
    fn write(&mut self, number: InodeNumber, logical: u64, group: u32) {
        for earlier in 0..group as usize {
            let free = self.fs.group_descs[earlier].free_blocks_count();
            if free > 0 {
                self.fs
                    .alloc_blocks(&mut self.dev, free)
                    .expect("fill earlier group");
            }
        }
        crate::file::write_inode_data(
            &mut self.dev,
            &mut self.fs,
            number,
            logical * BLOCK as u64,
            &[0x5a; BLOCK],
        )
        .expect("write");
    }

    fn inode(&mut self, number: InodeNumber) -> Ext4Inode {
        self.fs
            .get_inode_by_num(&mut self.dev, number)
            .expect("inode")
    }

    fn size(&mut self, number: InodeNumber) -> u64 {
        self.inode(number).size()
    }

    fn set_size(&mut self, number: InodeNumber, size: u64) {
        self.fs
            .modify_inode(&mut self.dev, number, |inode| {
                inode.i_size_lo = size as u32;
                inode.i_size_high = (size >> 32) as u32;
            })
            .expect("set size");
    }

    /// Every mapped logical block, whatever the file size says.
    fn mapped(&mut self, number: InodeNumber) -> Vec<u64> {
        let mut inode = self.inode(number);
        if !inode.uses_extents() {
            return crate::indirect::resolve_all_legacy_inode_blocks(
                &self.fs,
                &mut self.dev,
                number,
                &inode,
            )
            .expect("resolve")
            .into_keys()
            .map(u64::from)
            .collect();
        }
        let mut tree = ExtentTree::with_filesystem(&mut inode, &self.fs, number);
        tree.all_extents(&mut self.dev)
            .expect("extents")
            .iter()
            .flat_map(|extent| {
                let start = u64::from(extent.ee_block);
                start..start + u64::from(extent.len())
            })
            .collect()
    }

    fn group_of(&mut self, number: InodeNumber, logical: u64) -> u32 {
        let mut inode = self.inode(number);
        let physical = crate::loopfile::resolve_inode_block(
            &self.fs,
            &mut self.dev,
            number,
            &mut inode,
            logical as u32,
        )
        .expect("resolve")
        .expect("mapped");
        self.fs
            .block_allocator
            .global_to_group(physical)
            .expect("group")
            .0
            .raw()
    }

    fn physical(&mut self, number: InodeNumber, logical: u64) -> u64 {
        let mut inode = self.inode(number);
        crate::loopfile::resolve_inode_block(
            &self.fs,
            &mut self.dev,
            number,
            &mut inode,
            logical as u32,
        )
        .expect("resolve")
        .expect("mapped")
        .raw()
    }

    fn pending(&mut self, number: InodeNumber) -> bool {
        self.fs.pending_shrinks.contains(&number)
    }

    fn orphaned(&mut self, number: InodeNumber) -> bool {
        self.fs
            .orphan_contains(&mut self.dev, number)
            .expect("orphan chain")
    }

    /// Checkpoints the journal and installs one of `maxlen` blocks in its place.
    fn install_journal(&mut self, maxlen: u32, credits: usize) {
        self.fs.sync_filesystem(&mut self.dev).expect("sync");
        self.dev.flush().expect("flush");
        let start = self.fs.journal_sb_block_start.expect("journal");
        let mut journal = JournalSuperBlock::default();
        journal.s_header.h_blocktype = JBD2_BLOCKTYPE_SUPERBLOCK_V1;
        journal.s_blocksize = BLOCK as u32;
        journal.s_maxlen = maxlen;
        journal.s_first = 1;
        journal.s_sequence = 1;
        self.dev
            .set_journal_superblock(journal, start)
            .expect("install journal");
        assert_eq!(
            self.dev.transaction_credit_limit().unwrap(),
            Some(credits),
            "journal capacity"
        );
    }

    fn truncate(&mut self, number: InodeNumber, size: u64) -> Ext4Result<()> {
        truncate_inode(&mut self.dev, &mut self.fs, number, size)
    }

    fn recover(&mut self, number: InodeNumber, size: u64) -> Ext4Result<()> {
        truncate_inode_mapping(
            &mut self.dev,
            &mut self.fs,
            number,
            size,
            TruncatePurpose::OrphanRecovery,
        )
    }
}

#[test]
fn a_user_resize_takes_regular_files_only() {
    let mut m = mount();
    let file = m.file("/file", false);
    m.write(file, 0, 0);
    m.truncate(file, 0).expect("a regular file shrinks");
    assert_eq!(m.mapped(file), []);

    crate::dir::mkdir(&mut m.dev, &mut m.fs, "/dir").expect("mkdir");
    let dir = m.number("/dir");
    assert_eq!(
        m.truncate(dir, 0).unwrap_err().kind(),
        Ext4ErrorKind::InvalidInput
    );
    assert_eq!(m.mapped(dir).len(), 1, "the directory keeps its block");

    crate::file::create_symbol_link(&mut m.dev, &mut m.fs, "/file", "/link").expect("symlink");
    let link = m.number("/link");
    assert_eq!(
        m.truncate(link, 0).unwrap_err().kind(),
        Ext4ErrorKind::Unsupported
    );
}

#[test]
fn the_final_reap_takes_a_directory() {
    let mut m = mount();
    crate::dir::mkdir(&mut m.dev, &mut m.fs, "/dir").expect("mkdir");
    let dir = m.number("/dir");
    assert_eq!(m.mapped(dir).len(), 1);
    truncate_inode_for_reap(&mut m.dev, &mut m.fs, dir).expect("reap");
    assert_eq!(m.mapped(dir), []);
}

#[test]
fn growth_publishes_sizes_past_four_gibibytes() {
    let mut m = mount();
    let file = m.file("/file", false);
    m.truncate(file, 5 * GIB + 3).expect("grow");
    assert_eq!(m.size(file), 5 * GIB + 3);
}

#[test]
fn an_extent_file_grows_to_the_last_logical_block_and_no_further() {
    let mut m = mount();
    let file = m.file("/file", false);
    assert_eq!(
        m.truncate(file, LARGEST_SIZE + 1).unwrap_err().kind(),
        Ext4ErrorKind::FileTooLarge
    );
    m.truncate(file, LARGEST_SIZE)
        .expect("past the legacy limit");
    assert_eq!(m.size(file), LARGEST_SIZE);
}

#[test]
fn a_legacy_file_grows_to_its_triple_indirect_limit_and_no_further() {
    let mut m = mount();
    let file = m.file("/legacy", true);
    let limit = LEGACY_BLOCKS * BLOCK as u64;
    assert_eq!(
        m.truncate(file, limit + 1).unwrap_err().kind(),
        Ext4ErrorKind::FileTooLarge
    );
    m.truncate(file, limit).expect("grow");
    assert_eq!(m.size(file), limit);
}

#[test]
fn a_shrink_within_the_last_logical_block_removes_nothing() {
    let mut m = mount();
    let file = m.file("/file", false);
    m.truncate(file, LARGEST_SIZE).expect("grow");
    m.truncate(file, LARGEST_SIZE - 1).expect("shrink");
    assert_eq!(m.size(file), LARGEST_SIZE - 1);
    assert!(!m.pending(file));
}

/// A file of one block whose size hides three more mapped blocks.
fn blocks_past_the_size(m: &mut Mount) -> InodeNumber {
    let file = m.file("/file", false);
    for logical in 0..4 {
        m.write(file, logical, 0);
    }
    m.set_size(file, BLOCK as u64);
    assert_eq!(m.mapped(file), [0, 1, 2, 3]);
    file
}

#[test]
fn a_user_resize_to_the_same_size_keeps_blocks_past_it() {
    let mut m = mount();
    let file = blocks_past_the_size(&mut m);
    m.truncate(file, BLOCK as u64).expect("same size");
    assert_eq!(m.mapped(file), [0, 1, 2, 3]);
}

#[test]
fn a_new_resize_to_the_current_size_keeps_blocks_past_it() {
    let mut m = mount();
    let file = blocks_past_the_size(&mut m);
    let mut resize = InodeResize::new(file, BLOCK as u64);
    resize.resume(&mut m.dev, &mut m.fs).expect("resume");
    assert_eq!(m.mapped(file), [0, 1, 2, 3]);
    assert_eq!(resize.original_size, Some(BLOCK as u64));
    assert!(resize.complete);
}

#[test]
fn a_resumed_shrink_whose_size_is_published_removes_blocks_past_it() {
    let mut m = mount();
    let file = blocks_past_the_size(&mut m);
    let mut resize = InodeResize {
        inode: file,
        target_size: BLOCK as u64,
        original_size: Some(4 * BLOCK as u64),
        complete: false,
    };
    resize.resume(&mut m.dev, &mut m.fs).expect("resume");
    assert_eq!(m.mapped(file), [0]);
    assert_eq!(m.size(file), BLOCK as u64);
}

#[test]
fn a_legacy_shrink_into_a_block_finishes_its_published_intent() {
    let mut m = mount();
    let file = m.file("/legacy", true);
    for logical in 0..4 {
        m.write(file, logical, 0);
    }
    m.truncate(file, 1500).expect("shrink");
    assert_eq!(m.mapped(file), [0, 1]);
    assert_eq!(m.size(file), 1500);
    assert!(!m.pending(file));
    assert!(!m.orphaned(file));
}

#[test]
fn a_shrink_that_fails_to_zero_its_tail_has_published_its_size() {
    let mut m = mount();
    let file = m.file("/file", false);
    let logical = 5 * GIB / BLOCK as u64;
    m.write(file, logical, 0);
    m.write(file, logical + 1, 0);
    m.fs.datablock_cache
        .use_shared_device_cache(&mut m.dev)
        .expect("shared cache");
    let tail = m.physical(file, logical);
    m.fail_write.set(Some(tail));
    let size = logical * BLOCK as u64 + 500;
    assert_eq!(
        m.truncate(file, size).unwrap_err().kind(),
        Ext4ErrorKind::Io
    );
    assert_eq!(
        m.fail_write.get(),
        None,
        "the tail zeroing must have failed"
    );
    assert_eq!(m.size(file), size);
    assert!(m.pending(file));
    assert!(m.orphaned(file));
}

#[test]
fn a_restarted_user_shrink_removes_the_last_logical_block_and_finishes() {
    let mut m = mount();
    let file = m.file("/file", false);
    m.write(file, 0, 0);
    let first = m.group_of(file, 0);
    m.write(file, 1, first + 1);
    m.write(file, LAST_LOGICAL, first + 2);
    assert_eq!(m.group_of(file, LAST_LOGICAL), first + 2);
    m.install_journal(FIVE_CREDIT_JOURNAL, 5);
    m.truncate(file, 0).expect("shrink");
    assert_eq!(m.mapped(file), []);
    assert_eq!(m.size(file), 0);
    assert!(!m.pending(file));
    assert!(!m.orphaned(file));
}

#[test]
fn a_removal_whose_first_step_cannot_fit_publishes_nothing() {
    let mut m = mount();
    let file = m.file("/file", false);
    m.write(file, LAST_LOGICAL, 0);
    m.install_journal(THREE_CREDIT_JOURNAL, 3);
    let error = m.truncate(file, 0).unwrap_err();
    assert_eq!(
        error.context(),
        Some(crate::error::ErrorContext::Operation {
            op: "extent:restart_credits"
        })
    );
    assert_eq!(m.size(file), LARGEST_SIZE);
    assert!(!m.pending(file));
    assert_eq!(m.mapped(file), [LAST_LOGICAL]);
}

#[test]
fn a_restarted_extent_recovery_keeps_a_size_past_four_gibibytes() {
    let mut m = mount();
    let file = m.file("/file", false);
    let size = 5 * GIB;
    let past = size / BLOCK as u64 + 1000;
    m.write(file, past, 0);
    let first = m.group_of(file, past);
    m.write(file, past + 1, first + 1);
    m.set_size(file, size);
    m.install_journal(FIVE_CREDIT_JOURNAL, 5);
    m.recover(file, size).expect("recover");
    assert_eq!(m.mapped(file), []);
    assert_eq!(m.size(file), size);
}

#[test]
fn a_restarted_legacy_recovery_keeps_a_size_past_four_gibibytes() {
    let mut m = mount();
    let file = m.file("/legacy", true);
    let size = 5 * GIB;
    let past = size / BLOCK as u64 + 1000;
    m.write(file, past, 0);
    let first = m.group_of(file, past);
    m.write(file, past + 1, first + 1);
    m.set_size(file, size);
    m.install_journal(FIVE_CREDIT_JOURNAL, 5);
    m.recover(file, size).expect("recover");
    assert_eq!(m.mapped(file), []);
    assert_eq!(m.size(file), size);
}
