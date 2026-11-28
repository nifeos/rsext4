//! Tests that pin the record checks and edge cases of directory entry removal.

use super::*;
use crate::error::ErrorContext;
use alloc::{vec, vec::Vec};

const FILE: u8 = Ext4DirEntry2::EXT4_FT_REG_FILE;

fn operation<T>(result: Ext4Result<T>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    match error.context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

fn record(block: &mut [u8], offset: usize, inode: u32, record_len: u16, name: &[u8]) {
    block[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
    block[offset + 4..offset + 6].copy_from_slice(&record_len.to_le_bytes());
    block[offset + 6] = name.len() as u8;
    block[offset + 7] = FILE;
    block[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
}

// ---- checked_directory_record ----

fn checked(block: &[u8], offset: usize, inode_count: u32) -> Ext4Result<(u32, Vec<u8>, usize)> {
    checked_directory_record(block, offset, inode_count)
        .map(|(inode, name, end)| (inode, name.to_vec(), end))
}

#[test]
fn a_directory_record_must_be_whole_aligned_and_large_enough_for_its_name() {
    let mut block = vec![0u8; 4096];
    // A record that fills its name exactly is accepted.
    record(&mut block, 0, 5, 12, b"four");
    record(&mut block, 12, 6, 4084, b"x");
    assert_eq!(checked(&block, 0, 10).unwrap(), (5, b"four".to_vec(), 12));
    // The inode may equal the count but not pass it.
    record(&mut block, 0, 10, 12, b"four");
    assert!(checked(&block, 0, 10).is_ok());
    record(&mut block, 0, 11, 12, b"four");
    assert_eq!(operation(checked(&block, 0, 10)), "directory:record");
    // A name of five bytes needs sixteen.
    record(&mut block, 0, 5, 12, b"fives");
    assert_eq!(operation(checked(&block, 0, 10)), "directory:record");
    record(&mut block, 0, 5, 16, b"fives");
    record(&mut block, 16, 6, 4080, b"x");
    assert_eq!(checked(&block, 0, 10).unwrap().2, 16);
    // A one-byte name rounds to twelve, so twelve is enough.
    record(&mut block, 0, 5, 12, b"a");
    assert_eq!(checked(&block, 0, 10).unwrap().2, 12);
    // Eight is the smallest length that is not below the header.
    record(&mut block, 0, 5, 8, b"");
    record(&mut block, 8, 6, 4088, b"x");
    assert_eq!(checked(&block, 0, 10).unwrap().2, 8);
    record(&mut block, 0, 5, 4, b"");
    assert_eq!(operation(checked(&block, 0, 10)), "directory:record");
    // Not a multiple of four.
    record(&mut block, 0, 5, 10, b"");
    assert_eq!(operation(checked(&block, 0, 10)), "directory:record");
}

#[test]
fn a_directory_record_in_an_odd_sized_block_is_refused_for_its_alignment() {
    let mut block = vec![0u8; 4098];
    record(&mut block, 0, 5, 0, b"fives");
    assert_eq!(operation(checked(&block, 0, 10)), "directory:record");
}

#[test]
fn a_directory_record_may_not_leave_a_tail_too_small_for_another_record() {
    let mut block = vec![0u8; 64];
    // Ends exactly at the block end.
    record(&mut block, 0, 5, 12, b"a");
    record(&mut block, 12, 6, 52, b"b");
    assert_eq!(checked(&block, 12, 10).unwrap().2, 64);
    // Ends twelve bytes before the end: room for one more record.
    record(&mut block, 12, 6, 40, b"b");
    assert_eq!(checked(&block, 12, 10).unwrap().2, 52);
    // Ends eight bytes before the end: no room for a record.
    record(&mut block, 12, 6, 44, b"b");
    assert_eq!(operation(checked(&block, 12, 10)), "directory:record");
    // Runs past the block end.
    record(&mut block, 12, 6, 56, b"b");
    assert_eq!(operation(checked(&block, 12, 10)), "directory:record");
    // The header itself does not fit.
    assert_eq!(
        operation(checked(&block, 60, 10)),
        "directory:record_header"
    );
}

// ---- remove_dentry_in_dir_block ----

fn plain_superblock() -> Ext4Superblock {
    Ext4Superblock {
        s_feature_ro_compat: 0,
        ..Default::default()
    }
}

fn remove(block: &mut [u8], offset: usize, ino: u32, name: &[u8]) -> Ext4Result<bool> {
    remove_dentry_in_dir_block(
        &plain_superblock(),
        InodeNumber::new(2).unwrap(),
        &Ext4Inode::default(),
        block,
        ParentDirEntry {
            ino: InodeNumber::new(ino).unwrap(),
            phys: AbsoluteBN::new(1),
            offset,
            file_type: FILE,
        },
        name,
    )
}

fn three_records() -> Vec<u8> {
    let mut block = vec![0xa5u8; 64];
    record(&mut block, 0, 11, 12, b"a");
    record(&mut block, 12, 12, 12, b"b");
    record(&mut block, 24, 13, 40, b"c");
    block
}

#[test]
fn removing_an_entry_checks_its_inode_and_name_and_only_stops_at_its_offset() {
    // A mismatched inode or name changes nothing.
    let mut block = three_records();
    let before = block.clone();
    assert!(!remove(&mut block, 12, 99, b"b").unwrap());
    assert!(!remove(&mut block, 12, 12, b"x").unwrap());
    assert_eq!(block, before);
    // An offset between records is never found and the scan stops past it.
    assert!(!remove(&mut block, 16, 12, b"b").unwrap());
    // An offset beyond the entries is never found either.
    assert!(!remove(&mut block, 200, 12, b"b").unwrap());
    assert_eq!(block, before);
    // The middle entry merges into the first.
    assert!(remove(&mut block, 12, 12, b"b").unwrap());
    assert_eq!(u16::from_le_bytes([block[4], block[5]]), 24);
    assert!(block[12..24].iter().all(|byte| *byte == 0));
    // The last entry merges into the one before it.
    let mut block = three_records();
    assert!(remove(&mut block, 24, 13, b"c").unwrap());
    assert_eq!(u16::from_le_bytes([block[16], block[17]]), 52);
}

#[test]
fn removing_the_first_entry_clears_only_its_inode_and_name() {
    let mut block = three_records();
    assert!(remove(&mut block, 0, 11, b"a").unwrap());
    assert_eq!(u32::from_le_bytes(block[0..4].try_into().unwrap()), 0);
    assert_eq!(u16::from_le_bytes([block[4], block[5]]), 12);
    assert!(block[6..12].iter().all(|byte| *byte == 0));
    assert_eq!(u32::from_le_bytes(block[12..16].try_into().unwrap()), 12);
}

#[test]
fn removal_refuses_a_block_whose_records_are_malformed() {
    for (length, label) in [(4u16, "short"), (10, "unaligned"), (60, "past the end")] {
        let mut block = three_records();
        record(&mut block, 12, 12, length, b"b");
        assert_eq!(
            operation(remove(&mut block, 24, 13, b"c")),
            "directory:delete_record",
            "{label}"
        );
    }
    // A name longer than its record.
    let mut block = three_records();
    block[12 + 6] = 40;
    assert_eq!(
        operation(remove(&mut block, 24, 13, b"c")),
        "directory:delete_record"
    );
    // A record of exactly eight bytes is acceptable.
    let mut block = three_records();
    record(&mut block, 12, 12, 8, b"");
    record(&mut block, 20, 13, 44, b"c");
    assert!(remove(&mut block, 20, 13, b"c").unwrap());
    assert_eq!(u16::from_le_bytes([block[16], block[17]]), 52);
    // A block whose length is not a multiple of four cannot hold aligned records.
    let mut odd = vec![0u8; 4098];
    record(&mut odd, 0, 5, 0, b"x");
    assert_eq!(
        operation(remove(&mut odd, 0, 5, b"x")),
        "directory:delete_record"
    );
    // The scan stops at the first record past the wanted offset, so a corrupt
    // record after it is never read.
    let mut block = three_records();
    record(&mut block, 36, 14, 60, b"d");
    block[24 + 4..24 + 6].copy_from_slice(&12u16.to_le_bytes());
    assert!(!remove(&mut block, 16, 12, b"b").unwrap());
    // The header must fit in the block.
    let mut block = vec![0u8; 4];
    assert_eq!(
        operation(remove(&mut block, 0, 1, b"c")),
        "directory:delete_header"
    );
}

// ---- mounted tests ----

mod mounted {
    use alloc::{vec, vec::Vec};

    use super::{FILE, operation};
    use crate::{
        BLOCK_SIZE, DeviceCapabilities, DeviceGeometry, SectorId, create_symbol_link,
        dir::get_inode_with_num, mkdir, mkfile, mkfs, runtime::Clock,
    };

    use super::super::*;

    struct MemoryDevice {
        bytes: Vec<u8>,
    }

    impl BlockIo for MemoryDevice {
        fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * BLOCK_SIZE;
            let end = start + buffer.len();
            self.bytes
                .get_mut(start..end)
                .ok_or_else(Ext4Error::io)?
                .copy_from_slice(buffer);
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * BLOCK_SIZE;
            let end = start + buffer.len();
            buffer.copy_from_slice(self.bytes.get(start..end).ok_or_else(Ext4Error::io)?);
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

    fn mounted() -> (Jbd2Dev<MemoryDevice>, Ext4FileSystem) {
        let device = MemoryDevice {
            bytes: vec![0; 16 * 1024 * BLOCK_SIZE],
        };
        let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
        mkfs(&mut journal).expect("mkfs");
        let filesystem = Ext4FileSystem::mount(&mut journal).expect("mount");
        (journal, filesystem)
    }

    fn lookup(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemoryDevice>,
        path: &str,
    ) -> (InodeNumber, Ext4Inode) {
        get_inode_with_num(fs, dev, path)
            .expect("lookup")
            .expect("present")
    }

    fn first_block(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemoryDevice>,
        number: InodeNumber,
        inode: &mut Ext4Inode,
    ) -> AbsoluteBN {
        *resolve_inode_blocks(fs, dev, number, inode)
            .expect("blocks")
            .get(&0)
            .expect("first block")
    }

    fn rewrite(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemoryDevice>,
        phys: AbsoluteBN,
        number: InodeNumber,
        inode: &Ext4Inode,
        edit: impl FnOnce(&mut [u8]),
    ) {
        let superblock = fs.superblock;
        fs.datablock_cache
            .modify_metadata(dev, phys, |data| {
                edit(data);
                update_ext4_dirblock_csum32(&superblock, number.raw(), inode.i_generation, data);
            })
            .expect("rewrite");
    }

    fn block_bytes(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemoryDevice>,
        phys: AbsoluteBN,
    ) -> Vec<u8> {
        fs.datablock_cache
            .get_or_load(dev, phys)
            .unwrap()
            .data
            .to_vec()
    }

    #[test]
    fn a_directory_is_empty_only_when_it_holds_nothing_but_dot_and_dotdot() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        let (number, mut inode) = lookup(&mut fs, &mut dev, "/d");
        assert!(is_dir_empty(&mut fs, &mut dev, number, &mut inode).unwrap());
        // The smallest size that can hold both records is accepted.
        inode.i_size_lo = 24;
        assert!(is_dir_empty(&mut fs, &mut dev, number, &mut inode).unwrap());
        inode.i_size_lo = 23;
        assert_eq!(
            operation(is_dir_empty(&mut fs, &mut dev, number, &mut inode)),
            "directory:empty_size"
        );
        // A regular file is not a directory.
        mkfile(&mut dev, &mut fs, "/d/f", None, None).unwrap();
        let (file_number, mut file_inode) = lookup(&mut fs, &mut dev, "/d/f");
        assert_eq!(
            is_dir_empty(&mut fs, &mut dev, file_number, &mut file_inode)
                .unwrap_err()
                .kind(),
            Ext4ErrorKind::NotDirectory
        );
        // A live entry makes the directory non-empty.
        let (number, mut inode) = lookup(&mut fs, &mut dev, "/d");
        assert!(!is_dir_empty(&mut fs, &mut dev, number, &mut inode).unwrap());
    }

    #[test]
    fn the_dot_and_dotdot_entries_must_be_what_they_claim_to_be() {
        type Case = (&'static str, fn(&mut [u8]), &'static str);
        let cases: [Case; 4] = [
            (
                "wrong dot inode",
                |d| d[0..4].copy_from_slice(&5u32.to_le_bytes()),
                "directory:dot",
            ),
            ("wrong dot name", |d| d[8] = b'x', "directory:dot"),
            (
                "zero dotdot inode",
                |d| d[12..16].copy_from_slice(&0u32.to_le_bytes()),
                "directory:dotdot",
            ),
            ("wrong dotdot name", |d| d[20] = b'x', "directory:dotdot"),
        ];
        for (label, edit, expected) in cases {
            let (mut dev, mut fs) = mounted();
            mkdir(&mut dev, &mut fs, "/d").unwrap();
            let (number, mut inode) = lookup(&mut fs, &mut dev, "/d");
            let phys = first_block(&mut fs, &mut dev, number, &mut inode);
            rewrite(&mut fs, &mut dev, phys, number, &inode, edit);
            assert_eq!(
                operation(is_dir_empty(&mut fs, &mut dev, number, &mut inode)),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn a_directory_block_needs_its_checksum_tail_and_a_matching_checksum() {
        let (mut dev, mut fs) = mounted();
        assert!(crate::crc32c::ext4_superblock_has_metadata_csum(
            &fs.superblock
        ));
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        let (number, mut inode) = lookup(&mut fs, &mut dev, "/d");
        let phys = first_block(&mut fs, &mut dev, number, &mut inode);
        let original = block_bytes(&mut fs, &mut dev, phys);
        // A bad checksum is refused even though the tail is present.
        fs.datablock_cache
            .modify_metadata(&mut dev, phys, |data| {
                let last = data.len() - 1;
                data[last] ^= 0xff;
            })
            .unwrap();
        assert_eq!(
            operation(is_dir_empty(&mut fs, &mut dev, number, &mut inode)),
            "directory:block"
        );
        // A block with no tail at all is refused even with the records intact.
        fs.datablock_cache
            .modify_metadata(&mut dev, phys, |data| {
                data.copy_from_slice(&original);
                let end = data.len();
                data[end - 12..].fill(0);
            })
            .unwrap();
        assert!(is_dir_empty(&mut fs, &mut dev, number, &mut inode).is_err());
    }

    #[test]
    fn replacing_an_entry_requires_every_field_to_match_and_changes_only_its_target() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        mkfile(&mut dev, &mut fs, "/d/a", None, None).unwrap();
        mkfile(&mut dev, &mut fs, "/d/b", None, None).unwrap();
        let (dir_number, dir_inode) = lookup(&mut fs, &mut dev, "/d");
        let (b, _) = lookup(&mut fs, &mut dev, "/d/b");
        let entry =
            find_named_entry_in_parent(&mut fs, &mut dev, dir_number, &dir_inode, b"a").unwrap();
        let phys = entry.phys;
        let replacement = DentryReplacement {
            inode: b,
            file_type: Ext4DirEntry2::EXT4_FT_SYMLINK,
        };
        let before = block_bytes(&mut fs, &mut dev, phys);
        let run = |fs: &mut Ext4FileSystem,
                   dev: &mut Jbd2Dev<MemoryDevice>,
                   entry: ParentDirEntry,
                   name: &[u8],
                   inode: &Ext4Inode| {
            replace_named_entry_at(fs, dev, dir_number, inode, entry, name, replacement)
        };
        // Wrong inode, wrong name and a name that is a prefix all refuse.
        let wrong_inode = ParentDirEntry { ino: b, ..entry };
        assert!(run(&mut fs, &mut dev, wrong_inode, b"a", &dir_inode).is_err());
        assert!(run(&mut fs, &mut dev, entry, b"b", &dir_inode).is_err());
        assert!(run(&mut fs, &mut dev, entry, b"", &dir_inode).is_err());
        // A location past the block or between records refuses.
        let past = ParentDirEntry {
            offset: 4090,
            ..entry
        };
        assert!(run(&mut fs, &mut dev, past, b"a", &dir_inode).is_err());
        let beyond = ParentDirEntry {
            offset: 9000,
            ..entry
        };
        assert!(run(&mut fs, &mut dev, beyond, b"a", &dir_inode).is_err());
        assert_eq!(block_bytes(&mut fs, &mut dev, phys), before);
        // The matching entry is replaced and nothing else.
        run(&mut fs, &mut dev, entry, b"a", &dir_inode).unwrap();
        let after = block_bytes(&mut fs, &mut dev, phys);
        let at = entry.offset;
        assert_eq!(
            u32::from_le_bytes(after[at..at + 4].try_into().unwrap()),
            b.raw()
        );
        assert_eq!(after[at + 7], Ext4DirEntry2::EXT4_FT_SYMLINK);
        assert_eq!(after[at + 4..at + 7], before[at + 4..at + 7]);
        assert_eq!(after[at + 8..at + 9], *b"a");
        // The first record of a block, at offset zero, is replaceable too.
        let dot =
            find_named_entry_in_parent(&mut fs, &mut dev, dir_number, &dir_inode, b".").unwrap();
        assert_eq!(dot.offset, 0);
        replace_named_entry_at(
            &mut fs,
            &mut dev,
            dir_number,
            &dir_inode,
            dot,
            b".",
            DentryReplacement {
                inode: dir_number,
                file_type: FILE,
            },
        )
        .unwrap();
        assert_eq!(block_bytes(&mut fs, &mut dev, phys)[7], FILE);
    }

    #[test]
    fn replacement_rejects_each_malformed_record_on_its_own() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        mkfile(&mut dev, &mut fs, "/d/a", None, None).unwrap();
        let (dir_number, dir_inode) = lookup(&mut fs, &mut dev, "/d");
        let (a, _) = lookup(&mut fs, &mut dev, "/d/a");
        let entry =
            find_named_entry_in_parent(&mut fs, &mut dev, dir_number, &dir_inode, b"a").unwrap();
        let phys = entry.phys;
        let at = entry.offset;
        let replacement = DentryReplacement {
            inode: dir_number,
            file_type: FILE,
        };
        let original = block_bytes(&mut fs, &mut dev, phys);
        let attempt = |fs: &mut Ext4FileSystem,
                       dev: &mut Jbd2Dev<MemoryDevice>,
                       block: &[u8],
                       name: &[u8]|
         -> bool {
            fs.datablock_cache
                .modify_metadata(dev, phys, |data| data.copy_from_slice(block))
                .unwrap();
            replace_named_entry_at(fs, dev, dir_number, &dir_inode, entry, name, replacement)
                .is_ok()
        };
        // A name longer than its record, matching the supplied name bytes.
        let mut overlong = original.clone();
        overlong[at + 4..at + 6].copy_from_slice(&12u16.to_le_bytes());
        overlong[at + 6] = 10;
        overlong[at + 8..at + 18].copy_from_slice(b"aaaaaaaaaa");
        assert!(!attempt(&mut fs, &mut dev, &overlong, b"aaaaaaaaaa"));
        // A record of exactly eight bytes with an empty name matches.
        let mut eight = original.clone();
        eight[at + 4..at + 6].copy_from_slice(&8u16.to_le_bytes());
        eight[at + 6] = 0;
        assert!(attempt(&mut fs, &mut dev, &eight, b""));
        // A record length of four with no name cannot hold its header.
        let mut four = original.clone();
        four[at + 4..at + 6].copy_from_slice(&4u16.to_le_bytes());
        four[at + 6] = 0;
        assert!(!attempt(&mut fs, &mut dev, &four, b""));
        // Past the end of the block with a name that fits.
        let mut tail = original.clone();
        let start = tail.len() - 40;
        tail[start..start + 4].copy_from_slice(&a.raw().to_le_bytes());
        tail[start + 4..start + 6].copy_from_slice(&200u16.to_le_bytes());
        tail[start + 6] = 1;
        tail[start + 8] = b'a';
        let tail_entry = ParentDirEntry {
            offset: start,
            ..entry
        };
        fs.datablock_cache
            .modify_metadata(&mut dev, phys, |data| data.copy_from_slice(&tail))
            .unwrap();
        assert!(
            replace_named_entry_at(
                &mut fs,
                &mut dev,
                dir_number,
                &dir_inode,
                tail_entry,
                b"a",
                replacement,
            )
            .is_err()
        );
    }

    fn unlinked(
        fs: &mut Ext4FileSystem,
        dev: &mut Jbd2Dev<MemoryDevice>,
        path: &str,
    ) -> UnlinkOutcome {
        unlink(fs, dev, path).expect("unlink")
    }

    fn name(text: &str) -> FileName<'_> {
        FileName::new(text.as_bytes()).expect("name")
    }

    #[test]
    fn reaping_refuses_inodes_that_are_free_linked_or_not_on_the_orphan_list() {
        let (mut dev, mut fs) = mounted();
        mkfile(&mut dev, &mut fs, "/f", Some(b"data"), None).unwrap();
        let (number, _) = lookup(&mut fs, &mut dev, "/f");
        // Linked inode.
        assert_eq!(
            operation(reap_unlinked_inode(&mut fs, &mut dev, number)),
            "orphan:reap_linked"
        );
        // Zero links but never put on the orphan list.
        fs.set_inode_links_count(&mut dev, number, 0).unwrap();
        assert_eq!(
            operation(reap_unlinked_inode(&mut fs, &mut dev, number)),
            "orphan:reap_not_listed"
        );
        // An inode number that was never allocated.
        let free = InodeNumber::new(fs.superblock.s_inodes_count - 1).unwrap();
        assert_eq!(
            operation(reap_unlinked_inode(&mut fs, &mut dev, free)),
            "orphan:reap_unallocated"
        );
    }

    #[test]
    fn reaping_a_file_clears_its_mapping_flags_and_frees_its_blocks() {
        let (mut dev, mut fs) = mounted();
        mkfile(&mut dev, &mut fs, "/f", Some(&[7u8; 9000]), None).unwrap();
        let free_before = fs.statfs();
        let (number, inode) = lookup(&mut fs, &mut dev, "/f");
        assert_ne!(inode.i_flags & Ext4Inode::EXT4_EXTENTS_FL, 0);
        let outcome = unlinked(&mut fs, &mut dev, "/f");
        assert!(outcome.requires_reap());
        assert_eq!(outcome.inode, number);
        // The superblock names the orphan on disk as soon as the unlink commits.
        let mut raw = vec![0u8; BLOCK_SIZE];
        dev.read_blocks(&mut raw, AbsoluteBN::new(0), 1).unwrap();
        let last_orphan = u32::from_le_bytes(raw[1024 + 232..1024 + 236].try_into().unwrap());
        assert_eq!(last_orphan, number.raw());
        reap_unlinked_inode(&mut fs, &mut dev, number).unwrap();
        let reaped = fs.get_inode_by_num(&mut dev, number).unwrap();
        assert_eq!(reaped.i_flags & Ext4Inode::EXT4_EXTENTS_FL, 0);
        assert_eq!(reaped.i_size_lo, 0);
        assert_eq!(reaped.i_blocks_lo, 0);
        assert_eq!(reaped.i_block, [0; 15]);
        let free_after = fs.statfs();
        assert!(free_after.free_blocks >= free_before.free_blocks + 3);
        assert_eq!(free_after.free_inodes, free_before.free_inodes + 1);
    }

    #[test]
    fn removing_a_directory_checks_the_parent_the_target_and_emptiness() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        mkdir(&mut dev, &mut fs, "/d/sub").unwrap();
        mkfile(&mut dev, &mut fs, "/d/f", None, None).unwrap();
        mkdir(&mut dev, &mut fs, "/e").unwrap();
        let (d, _) = lookup(&mut fs, &mut dev, "/d");
        let (f, _) = lookup(&mut fs, &mut dev, "/d/f");
        let (root, _) = lookup(&mut fs, &mut dev, "/");
        // The parent must be a directory.
        assert_eq!(
            unlink_empty_directory_at(&mut fs, &mut dev, f, name("x"))
                .unwrap_err()
                .kind(),
            Ext4ErrorKind::NotDirectory
        );
        // The target must be a directory.
        assert_eq!(
            operation(unlink_empty_directory_at(&mut fs, &mut dev, d, name("f"))),
            "rmdir:target"
        );
        // The target must be empty.
        assert_eq!(
            unlink_empty_directory_at(&mut fs, &mut dev, root, name("d"))
                .unwrap_err()
                .kind(),
            Ext4ErrorKind::NotEmpty
        );
        // An empty directory goes and needs a reap.
        let outcome = unlink_empty_directory_at(&mut fs, &mut dev, root, name("e")).unwrap();
        assert!(outcome.requires_reap());
    }

    fn set_directory_count(fs: &mut Ext4FileSystem, number: InodeNumber, count: u32) {
        let (group, _) = fs.inode_allocator.global_to_group(number).unwrap();
        let descriptor = fs.get_group_desc_mut(group).unwrap();
        descriptor.bg_used_dirs_count_lo = (count & 0xffff) as u16;
        descriptor.bg_used_dirs_count_hi = (count >> 16) as u16;
    }

    fn directory_count(fs: &mut Ext4FileSystem, number: InodeNumber) -> u32 {
        let (group, _) = fs.inode_allocator.global_to_group(number).unwrap();
        fs.get_group_desc_mut(group).unwrap().used_dirs_count()
    }

    #[test]
    fn reaping_a_directory_lowers_its_group_directory_count_across_the_high_half() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/e").unwrap();
        let (root, _) = lookup(&mut fs, &mut dev, "/");
        let (number, _) = lookup(&mut fs, &mut dev, "/e");
        set_directory_count(&mut fs, number, 0x1_0005);
        let outcome = unlink_empty_directory_at(&mut fs, &mut dev, root, name("e")).unwrap();
        reap_unlinked_inode(&mut fs, &mut dev, outcome.inode).unwrap();
        assert_eq!(directory_count(&mut fs, number), 0x1_0004);
        let reaped = fs.get_inode_by_num(&mut dev, number).unwrap();
        assert_eq!(reaped.i_flags & Ext4Inode::EXT4_EXTENTS_FL, 0);
    }

    #[test]
    fn deleting_a_tree_removes_every_entry_and_restores_the_counters() {
        let (mut dev, mut fs) = mounted();
        let (root, root_before) = lookup(&mut fs, &mut dev, "/");
        let before = fs.statfs();
        mkdir(&mut dev, &mut fs, "/t").unwrap();
        mkfile(&mut dev, &mut fs, "/t/f1", Some(&[1u8; 5000]), None).unwrap();
        mkfile(&mut dev, &mut fs, "/t/f2", None, None).unwrap();
        mkdir(&mut dev, &mut fs, "/t/s1").unwrap();
        mkfile(&mut dev, &mut fs, "/t/s1/g", Some(b"g"), None).unwrap();
        mkdir(&mut dev, &mut fs, "/t/s2").unwrap();
        mkdir(&mut dev, &mut fs, "/t/s2/deep").unwrap();
        let (tree, _) = lookup(&mut fs, &mut dev, "/t");
        set_directory_count(&mut fs, tree, 0x1_0010);
        delete_dir(&mut fs, &mut dev, "/t").unwrap();
        assert!(
            get_inode_with_num(&mut fs, &mut dev, "/t")
                .unwrap()
                .is_none()
        );
        let after = fs.statfs();
        assert_eq!(after.free_inodes, before.free_inodes);
        assert_eq!(after.free_blocks, before.free_blocks);
        assert_eq!(directory_count(&mut fs, tree), 0x1_0010 - 4);
        let freed = fs.get_inode_by_num(&mut dev, tree).unwrap();
        assert_eq!(freed.i_flags & Ext4Inode::EXT4_EXTENTS_FL, 0);
        assert_eq!(freed.i_links_count, 0);
        let root_after = fs.get_inode_by_num(&mut dev, root).unwrap();
        assert_eq!(root_after.i_links_count, root_before.i_links_count);
    }

    #[test]
    fn deleting_a_tree_refuses_the_root_a_file_and_a_missing_path() {
        let (mut dev, mut fs) = mounted();
        mkfile(&mut dev, &mut fs, "/f", None, None).unwrap();
        assert_eq!(
            delete_dir(&mut fs, &mut dev, "/").unwrap_err().kind(),
            Ext4ErrorKind::Busy
        );
        assert_eq!(
            delete_dir(&mut fs, &mut dev, "/f").unwrap_err().kind(),
            Ext4ErrorKind::NotDirectory
        );
        assert_eq!(
            delete_dir(&mut fs, &mut dev, "/missing")
                .unwrap_err()
                .kind(),
            Ext4ErrorKind::NotFound
        );
    }

    #[test]
    fn owned_blocks_split_into_data_and_metadata_by_inode_kind() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        mkfile(&mut dev, &mut fs, "/f", Some(&[3u8; 5000]), None).unwrap();
        let long = alloc::format!("/{}", "n".repeat(120));
        mkfile(&mut dev, &mut fs, &long, None, None).unwrap();
        create_symbol_link(&mut dev, &mut fs, &long, "/link").unwrap();
        for (path, metadata, data) in [("/d", 1, 0), ("/f", 0, 2), ("/link", 0, 0)] {
            let (number, mut inode) = lookup(&mut fs, &mut dev, path);
            let owned = inode_owned_blocks(&mut fs, &mut dev, number, &mut inode).unwrap();
            if path == "/link" {
                assert_eq!(owned.revoke_records(), owned.metadata.len());
                assert_eq!(owned.data.len(), 0);
                continue;
            }
            assert_eq!(owned.metadata.len(), metadata, "{path}");
            assert_eq!(owned.data.len(), data, "{path}");
            assert_eq!(owned.revoke_records(), metadata, "{path}");
        }
    }

    #[test]
    fn reap_credits_cover_the_owned_blocks_and_demand_room_for_an_external_xattr() {
        let (_dev, fs) = mounted();
        let mut inode = Ext4Inode::default();
        // No owned blocks at all: the fixed small reservation plus revokes.
        assert_eq!(
            reap_transaction_credits(&fs, &inode, false, 3).unwrap(),
            TransactionCredits::metadata_with_revokes(EMPTY_REAP_TRANSACTION_CREDITS, 3)
        );
        // An external xattr block needs at least one block of sectors.
        inode.i_blocks_lo = 7;
        assert!(reap_transaction_credits(&fs, &inode, true, 0).is_err());
        inode.i_blocks_lo = 8;
        assert_eq!(
            reap_transaction_credits(&fs, &inode, true, 0).unwrap(),
            TransactionCredits::metadata_with_revokes(REAP_BASE_TRANSACTION_CREDITS + 2, 0)
        );
        inode.i_blocks_lo = 16;
        assert!(reap_transaction_credits(&fs, &inode, true, 0).is_ok());
        // Without an external xattr small counts are fine.
        inode.i_blocks_lo = 1;
        assert!(reap_transaction_credits(&fs, &inode, false, 0).is_ok());
        // Large files are capped.
        inode.i_blocks_lo = 8 * 1000;
        assert_eq!(
            reap_transaction_credits(&fs, &inode, false, 0).unwrap(),
            TransactionCredits::metadata_with_revokes(
                REAP_BASE_TRANSACTION_CREDITS + REAP_MAX_TRANSACTION_DATA as usize,
                0
            )
        );
    }

    #[test]
    fn a_record_that_ends_exactly_at_the_block_end_is_replaceable() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        mkfile(&mut dev, &mut fs, "/d/a", None, None).unwrap();
        let (dir_number, dir_inode) = lookup(&mut fs, &mut dev, "/d");
        let (a, _) = lookup(&mut fs, &mut dev, "/d/a");
        let entry =
            find_named_entry_in_parent(&mut fs, &mut dev, dir_number, &dir_inode, b"a").unwrap();
        let start = BLOCK_SIZE - 40;
        fs.datablock_cache
            .modify_metadata(&mut dev, entry.phys, |data| {
                data[start..start + 4].copy_from_slice(&a.raw().to_le_bytes());
                data[start + 4..start + 6].copy_from_slice(&40u16.to_le_bytes());
                data[start + 6] = 1;
                data[start + 7] = FILE;
                data[start + 8] = b'a';
            })
            .unwrap();
        let crafted = ParentDirEntry {
            offset: start,
            ..entry
        };
        let replacement = DentryReplacement {
            inode: dir_number,
            file_type: FILE,
        };
        replace_named_entry_at(
            &mut fs,
            &mut dev,
            dir_number,
            &dir_inode,
            crafted,
            b"a",
            replacement,
        )
        .unwrap();
        let data = block_bytes(&mut fs, &mut dev, entry.phys);
        assert_eq!(
            u32::from_le_bytes(data[start..start + 4].try_into().unwrap()),
            dir_number.raw()
        );
    }

    #[test]
    fn an_indexed_directory_whose_dot_entries_have_no_index_root_is_left_untouched() {
        let (mut dev, mut fs) = mounted();
        mkdir(&mut dev, &mut fs, "/d").unwrap();
        let (dir_number, plain_inode) = lookup(&mut fs, &mut dev, "/d");
        let mut dir_inode = plain_inode;
        dir_inode.i_flags |= Ext4Inode::EXT4_INDEX_FL;
        for dot in [&b"."[..], &b".."[..]] {
            let entry =
                find_named_entry_in_parent(&mut fs, &mut dev, dir_number, &plain_inode, dot)
                    .unwrap();
            let before = block_bytes(&mut fs, &mut dev, entry.phys);
            let result = replace_named_entry_at(
                &mut fs,
                &mut dev,
                dir_number,
                &dir_inode,
                entry,
                dot,
                DentryReplacement {
                    inode: dir_number,
                    file_type: Ext4DirEntry2::EXT4_FT_SYMLINK,
                },
            );
            assert_eq!(operation(result), "directory:stale_entry_location");
            assert_eq!(block_bytes(&mut fs, &mut dev, entry.phys), before);
        }
    }

    #[test]
    fn unlinking_checks_that_the_inode_can_be_freed_only_when_its_last_link_goes() {
        let (mut dev, mut fs) = mounted();
        mkfile(&mut dev, &mut fs, "/a", Some(&[1u8; 5000]), None).unwrap();
        crate::link(&mut fs, &mut dev, "/b", "/a").unwrap();
        let (number, mut inode) = lookup(&mut fs, &mut dev, "/a");
        assert_eq!(inode.i_links_count, 2);
        // Make the inode a legacy mapping whose block pointer lies outside the device.
        inode.i_flags &= !Ext4Inode::EXT4_EXTENTS_FL;
        inode.i_block = [0; 15];
        inode.i_block[0] = 0x7fff_fff0;
        fs.finalize_inode_update(
            &mut dev,
            number,
            &mut inode,
            Ext4InodeMetadataUpdate::link_count_change(),
        )
        .unwrap();
        // Two links: removing one never frees the inode, so nothing is checked.
        assert_eq!(unlinked(&mut fs, &mut dev, "/b").remaining_links, 1);
        // The last link would free it, and the damaged mapping is refused first.
        assert!(unlink(&mut fs, &mut dev, "/a").is_err());
        assert!(
            get_inode_with_num(&mut fs, &mut dev, "/a")
                .unwrap()
                .is_some()
        );
    }
}
