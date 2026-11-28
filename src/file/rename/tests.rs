//! Tests for rename: the transaction reservations, the option checks, the
//! parent link counts, the `..` rewrites and the source reload after a
//! directory restructures.

use alloc::{format, string::String, vec, vec::Vec};

use super::*;
use crate::{
    BLOCK_SIZE, DeviceCapabilities, DeviceGeometry, Ext4ErrorKind, Ext4Timestamp, SectorId,
    dir::get_inode_with_num,
    error::ErrorContext,
    jbd2::jbdstruct::{JBD2_BLOCKTYPE_SUPERBLOCK_V1, JournalSuperBlock},
    mkdir, mkfile, mkfs,
    runtime::Clock,
};

struct MemoryDevice {
    bytes: Vec<u8>,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK_SIZE;
        self.bytes
            .get_mut(start..start + buffer.len())
            .ok_or_else(Ext4Error::io)?
            .copy_from_slice(buffer);
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK_SIZE;
        buffer.copy_from_slice(
            self.bytes
                .get(start..start + buffer.len())
                .ok_or_else(Ext4Error::io)?,
        );
        Ok(())
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(BLOCK_SIZE as u32, (self.bytes.len() / BLOCK_SIZE) as u64)
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

type Device = Jbd2Dev<MemoryDevice>;

fn mounted() -> (Device, Ext4FileSystem) {
    let device = MemoryDevice {
        bytes: vec![0; 16 * 1024 * BLOCK_SIZE],
    };
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs(&mut journal).expect("mkfs");
    let filesystem = Ext4FileSystem::mount(&mut journal).expect("mount");
    (journal, filesystem)
}

fn lookup(fs: &mut Ext4FileSystem, dev: &mut Device, path: &str) -> (InodeNumber, Ext4Inode) {
    get_inode_with_num(fs, dev, path)
        .expect("lookup")
        .expect("present")
}

fn exists(fs: &mut Ext4FileSystem, dev: &mut Device, path: &str) -> bool {
    get_inode_with_num(fs, dev, path).expect("lookup").is_some()
}

fn links(fs: &mut Ext4FileSystem, dev: &mut Device, path: &str) -> u16 {
    lookup(fs, dev, path).1.i_links_count
}

/// The inode the `..` entry of the directory at `path` names.
fn dotdot(fs: &mut Ext4FileSystem, dev: &mut Device, path: &str) -> InodeNumber {
    let (number, inode) = lookup(fs, dev, path);
    find_named_entry_in_parent(fs, dev, number, &inode, b"..")
        .expect("dotdot")
        .ino
}

fn kind<T: core::fmt::Debug>(result: Ext4Result<T>) -> Ext4ErrorKind {
    result.expect_err("the rename must be refused").kind()
}

fn operation<T: core::fmt::Debug>(result: Ext4Result<T>) -> &'static str {
    match result.expect_err("the rename must be refused").context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

/// Checkpoints the journal and installs the smallest one whose transactions
/// hold `credits` credits.
fn install_journal(fs: &mut Ext4FileSystem, dev: &mut Device, credits: usize) {
    fs.sync_filesystem(dev).expect("sync");
    dev.flush().expect("flush");
    let start = fs.journal_sb_block_start.expect("journal");
    for maxlen in 15..1000 {
        let mut journal = JournalSuperBlock::default();
        journal.s_header.h_blocktype = JBD2_BLOCKTYPE_SUPERBLOCK_V1;
        journal.s_blocksize = BLOCK_SIZE as u32;
        journal.s_maxlen = maxlen;
        journal.s_first = 1;
        journal.s_sequence = 1;
        dev.set_journal_superblock(journal, start)
            .expect("install journal");
        if dev.transaction_credit_limit().unwrap() == Some(credits) {
            return;
        }
    }
    panic!("no journal holds {credits} credits");
}

/// A filesystem with the directories `/a` and `/b`, the files `/a/x` and
/// `/b/y`, and a journal whose transactions hold `credits` credits.
fn two_files_with_journal(credits: usize) -> (Device, Ext4FileSystem) {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/b").unwrap();
    mkfile(&mut dev, &mut fs, "/a/x", Some(b"x"), None).unwrap();
    mkfile(&mut dev, &mut fs, "/b/y", Some(b"y"), None).unwrap();
    install_journal(&mut fs, &mut dev, credits);
    (dev, fs)
}

#[test]
fn a_rename_reserves_its_62_credits() {
    let (mut dev, mut fs) = two_files_with_journal(62);
    let _ = rename(&mut dev, &mut fs, "/a/x", "/b/z", RenameOptions::REPLACE).unwrap();
    assert!(exists(&mut fs, &mut dev, "/b/z"));

    let (mut dev, mut fs) = two_files_with_journal(61);
    let refused = rename(&mut dev, &mut fs, "/a/x", "/b/z", RenameOptions::REPLACE);
    assert_eq!(operation(refused), "jbd2:handle_credits");
    assert!(exists(&mut fs, &mut dev, "/a/x"));
}

#[test]
fn an_exchange_reserves_its_74_credits() {
    let (mut dev, mut fs) = two_files_with_journal(74);
    let _ = rename(&mut dev, &mut fs, "/a/x", "/b/y", RenameOptions::EXCHANGE).unwrap();

    let (mut dev, mut fs) = two_files_with_journal(73);
    let refused = rename(&mut dev, &mut fs, "/a/x", "/b/y", RenameOptions::EXCHANGE);
    assert_eq!(operation(refused), "jbd2:handle_credits");
}

#[test]
fn each_option_reports_only_its_own_flag() {
    assert!(RenameOptions::NO_REPLACE.no_replace());
    assert!(!RenameOptions::NO_REPLACE.exchange());
    assert!(RenameOptions::EXCHANGE.exchange());
    assert!(!RenameOptions::EXCHANGE.whiteout());
    assert!(RenameOptions::WHITEOUT.whiteout());
    assert!(RenameOptions::WHITEOUT_NO_REPLACE.whiteout());
    assert!(RenameOptions::WHITEOUT_NO_REPLACE.no_replace());
    assert_eq!(RenameOptions::default(), RenameOptions::REPLACE);
}

#[test]
fn no_replace_refuses_an_existing_destination() {
    let (mut dev, mut fs) = two_files_with_journal(62);
    let refused = rename(&mut dev, &mut fs, "/a/x", "/b/y", RenameOptions::NO_REPLACE);
    assert_eq!(kind(refused), Ext4ErrorKind::AlreadyExists);
    assert!(exists(&mut fs, &mut dev, "/a/x"));
}

#[test]
fn whiteout_is_refused_as_unsupported() {
    let (mut dev, mut fs) = mounted();
    mkfile(&mut dev, &mut fs, "/x", Some(b"x"), None).unwrap();
    let refused = rename(&mut dev, &mut fs, "/x", "/z", RenameOptions::WHITEOUT);
    assert_eq!(operation(refused), "rename:whiteout");
    assert!(exists(&mut fs, &mut dev, "/x"));
}

#[test]
fn an_exchange_swaps_the_two_entries() {
    let (mut dev, mut fs) = two_files_with_journal(74);
    let (x, _) = lookup(&mut fs, &mut dev, "/a/x");
    let (y, _) = lookup(&mut fs, &mut dev, "/b/y");
    let _ = rename(&mut dev, &mut fs, "/a/x", "/b/y", RenameOptions::EXCHANGE).unwrap();
    assert_eq!(lookup(&mut fs, &mut dev, "/a/x").0, y);
    assert_eq!(lookup(&mut fs, &mut dev, "/b/y").0, x);
}

#[test]
fn a_damaged_destination_directory_is_reported_and_not_read_as_missing() {
    let (mut dev, mut fs) = two_files_with_journal(74);
    let (number, mut inode) = lookup(&mut fs, &mut dev, "/b");
    let block = *crate::file::resolve_inode_blocks(&mut fs, &mut dev, number, &mut inode)
        .expect("blocks")
        .get(&0)
        .expect("first block");
    // One flipped byte, so the block no longer matches its checksum.
    fs.datablock_cache
        .modify_metadata(&mut dev, block, |data| data[100] ^= 1)
        .expect("damage");
    let refused = rename(&mut dev, &mut fs, "/a/x", "/b/y", RenameOptions::EXCHANGE);
    assert_eq!(operation(refused), "directory:lookup_block");
}

#[test]
fn a_directory_cannot_move_beneath_itself() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/a/b").unwrap();
    mkdir(&mut dev, &mut fs, "/c").unwrap();
    let refused = rename(&mut dev, &mut fs, "/a", "/a/b/a", RenameOptions::REPLACE);
    assert_eq!(operation(refused), "rename:directory_cycle");
    // Moving it beside itself is fine.
    let _ = rename(&mut dev, &mut fs, "/a", "/c/a", RenameOptions::REPLACE).unwrap();
    assert!(exists(&mut fs, &mut dev, "/c/a/b"));
}

#[test]
fn an_exchange_cannot_put_a_directory_beneath_itself() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/a/b").unwrap();
    mkdir(&mut dev, &mut fs, "/a/b/c").unwrap();
    let refused = rename(&mut dev, &mut fs, "/a", "/a/b/c", RenameOptions::EXCHANGE);
    assert_eq!(operation(refused), "rename:directory_cycle");
    let refused = rename(&mut dev, &mut fs, "/a/b/c", "/a", RenameOptions::EXCHANGE);
    assert_eq!(operation(refused), "rename:directory_cycle");
}

#[test]
fn replacing_checks_that_the_target_can_be_freed_only_when_its_last_link_goes() {
    let (mut dev, mut fs) = mounted();
    mkfile(&mut dev, &mut fs, "/s", Some(b"s"), None).unwrap();
    mkfile(&mut dev, &mut fs, "/t", Some(&[1u8; 5000]), None).unwrap();
    crate::link(&mut fs, &mut dev, "/u", "/t").unwrap();
    let (number, mut inode) = lookup(&mut fs, &mut dev, "/t");
    // Make the target a legacy mapping whose block pointer lies outside the device.
    inode.i_flags &= !Ext4Inode::EXT4_EXTENTS_FL;
    inode.i_block = [0; 15];
    inode.i_block[0] = 0x7fff_fff0;
    fs.finalize_inode_update(
        &mut dev,
        number,
        &mut inode,
        crate::metadata::Ext4InodeMetadataUpdate::link_count_change(),
    )
    .unwrap();
    // Two links: replacing one never frees the inode, so nothing is checked.
    let outcome = rename(&mut dev, &mut fs, "/s", "/u", RenameOptions::REPLACE).unwrap();
    assert_eq!(outcome.replaced.unwrap().remaining_links, 1);
    // The last link would free it, and the damaged mapping is refused first.
    mkfile(&mut dev, &mut fs, "/s", Some(b"s"), None).unwrap();
    assert!(rename(&mut dev, &mut fs, "/s", "/t", RenameOptions::REPLACE).is_err());
    assert!(exists(&mut fs, &mut dev, "/s"));
}

#[test]
fn moving_a_directory_moves_one_link_between_the_parents_and_rewrites_dotdot() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/b").unwrap();
    mkdir(&mut dev, &mut fs, "/a/d").unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 3);
    let _ = rename(&mut dev, &mut fs, "/a/d", "/b/d", RenameOptions::REPLACE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 2);
    assert_eq!(links(&mut fs, &mut dev, "/b"), 3);
    let (b, _) = lookup(&mut fs, &mut dev, "/b");
    assert_eq!(dotdot(&mut fs, &mut dev, "/b/d"), b);

    // Within one parent nothing changes.
    let _ = rename(&mut dev, &mut fs, "/b/d", "/b/e", RenameOptions::REPLACE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/b"), 3);
    assert_eq!(dotdot(&mut fs, &mut dev, "/b/e"), b);
}

#[test]
fn replacing_an_empty_directory_in_the_same_parent_drops_one_parent_link() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/a/d").unwrap();
    mkdir(&mut dev, &mut fs, "/a/e").unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 4);
    let _ = rename(&mut dev, &mut fs, "/a/d", "/a/e", RenameOptions::REPLACE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 3);
}

#[test]
fn moving_a_file_to_another_parent_with_the_same_name_moves_it() {
    let (mut dev, mut fs) = two_files_with_journal(62);
    let _ = rename(&mut dev, &mut fs, "/a/x", "/b/x", RenameOptions::REPLACE).unwrap();
    assert!(!exists(&mut fs, &mut dev, "/a/x"));
    assert!(exists(&mut fs, &mut dev, "/b/x"));
    assert_eq!(links(&mut fs, &mut dev, "/a"), 2);
    assert_eq!(links(&mut fs, &mut dev, "/b"), 2);
}

#[test]
fn exchanging_a_directory_with_a_file_across_parents_moves_one_link() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/b").unwrap();
    mkdir(&mut dev, &mut fs, "/a/d").unwrap();
    mkfile(&mut dev, &mut fs, "/b/f", Some(b"f"), None).unwrap();
    let _ = rename(&mut dev, &mut fs, "/a/d", "/b/f", RenameOptions::EXCHANGE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 2);
    assert_eq!(links(&mut fs, &mut dev, "/b"), 3);
    let (b, _) = lookup(&mut fs, &mut dev, "/b");
    assert_eq!(dotdot(&mut fs, &mut dev, "/b/f"), b);

    // And back: the file goes to /b and the directory to /a.
    let _ = rename(&mut dev, &mut fs, "/a/d", "/b/f", RenameOptions::EXCHANGE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 3);
    assert_eq!(links(&mut fs, &mut dev, "/b"), 2);
    let (a, _) = lookup(&mut fs, &mut dev, "/a");
    assert_eq!(dotdot(&mut fs, &mut dev, "/a/d"), a);
}

#[test]
fn exchanging_two_directories_across_parents_keeps_the_links_and_swaps_dotdot() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/b").unwrap();
    mkdir(&mut dev, &mut fs, "/a/d").unwrap();
    mkdir(&mut dev, &mut fs, "/b/e").unwrap();
    let _ = rename(&mut dev, &mut fs, "/a/d", "/b/e", RenameOptions::EXCHANGE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 3);
    assert_eq!(links(&mut fs, &mut dev, "/b"), 3);
    let (a, _) = lookup(&mut fs, &mut dev, "/a");
    let (b, _) = lookup(&mut fs, &mut dev, "/b");
    assert_eq!(dotdot(&mut fs, &mut dev, "/a/d"), a);
    assert_eq!(dotdot(&mut fs, &mut dev, "/b/e"), b);
}

#[test]
fn exchanging_a_directory_with_a_file_in_one_parent_keeps_the_links() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    mkdir(&mut dev, &mut fs, "/a/d").unwrap();
    mkfile(&mut dev, &mut fs, "/a/f", Some(b"f"), None).unwrap();
    let _ = rename(&mut dev, &mut fs, "/a/d", "/a/f", RenameOptions::EXCHANGE).unwrap();
    assert_eq!(links(&mut fs, &mut dev, "/a"), 3);
}

#[test]
fn a_reserved_name_on_either_side_is_refused() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/a").unwrap();
    let (a, _) = lookup(&mut fs, &mut dev, "/a");
    for (old, new) in [(&b".."[..], &b"z"[..]), (b"z", b".")] {
        let refused = rename_inode_at(
            &mut fs,
            &mut dev,
            RenameEntryRequest {
                old_parent: a,
                old_name: FileName::new(old).unwrap(),
                new_parent: a,
                new_name: FileName::new(new).unwrap(),
                options: RenameOptions::REPLACE,
            },
        );
        assert_eq!(operation(refused), "rename:reserved_name");
    }
}

#[test]
fn either_parent_must_be_a_directory() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/d").unwrap();
    mkfile(&mut dev, &mut fs, "/d/x", Some(b"x"), None).unwrap();
    mkfile(&mut dev, &mut fs, "/f", Some(b"f"), None).unwrap();
    let refused = rename(&mut dev, &mut fs, "/f/x", "/d/y", RenameOptions::REPLACE);
    assert_eq!(kind(refused), Ext4ErrorKind::NotDirectory);
    let refused = rename(&mut dev, &mut fs, "/d/x", "/f/y", RenameOptions::REPLACE);
    assert_eq!(kind(refused), Ext4ErrorKind::NotDirectory);
}

#[test]
fn the_root_cannot_be_renamed_or_replaced() {
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/d").unwrap();
    for (old, new) in [("/", "/d/r"), ("/d", "/")] {
        let refused = rename(&mut dev, &mut fs, old, new, RenameOptions::REPLACE);
        let error = refused.expect_err("the root is refused");
        assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
        assert!(error.context().is_none(), "{old} -> {new}");
    }
    assert!(exists(&mut fs, &mut dev, "/d"));
}

/// The names `n000`, `n001` and so on, each padded to 200 bytes.
fn long_name(index: usize) -> String {
    format!("{index:0>200}")
}

#[test]
fn a_rename_that_restructures_its_directory_removes_the_source_where_it_now_is() {
    // Find how many long names fill the first block of a directory.
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/d").unwrap();
    let mut fit = 0;
    loop {
        mkfile(
            &mut dev,
            &mut fs,
            &format!("/d/{}", long_name(fit)),
            None,
            None,
        )
        .unwrap();
        if lookup(&mut fs, &mut dev, "/d").1.size() > BLOCK_SIZE as u64 {
            break;
        }
        fit += 1;
    }

    // Fill one block exactly, then rename an entry to a new name: the new
    // entry does not fit beside the old one, so the directory is indexed and
    // its entries move before the old entry is removed.
    let (mut dev, mut fs) = mounted();
    mkdir(&mut dev, &mut fs, "/d").unwrap();
    for index in 0..fit {
        mkfile(
            &mut dev,
            &mut fs,
            &format!("/d/{}", long_name(index)),
            None,
            None,
        )
        .unwrap();
    }
    let old = format!("/d/{}", long_name(0));
    let new = format!("/d/{}", long_name(fit));
    let _ = rename(&mut dev, &mut fs, &old, &new, RenameOptions::REPLACE).unwrap();
    let (_, directory) = lookup(&mut fs, &mut dev, "/d");
    assert_ne!(directory.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
    assert!(!exists(&mut fs, &mut dev, &old));
    assert!(exists(&mut fs, &mut dev, &new));
    for index in 1..fit {
        assert!(exists(
            &mut fs,
            &mut dev,
            &format!("/d/{}", long_name(index))
        ));
    }
}
