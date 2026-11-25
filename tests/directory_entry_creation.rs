//! Functional tests for linking new entries into directories.
//!
//! The suite covers linear directory growth, record reuse, creation edge
//! cases and rollback when an inode update fails partway through a create.

// Miri runs only the tests that reach unsafe code, and this file reaches none.
#![cfg(not(miri))]

use std::{cell::Cell, rc::Rc};

use rsext4::{
    checksum::verify_ext4_dirblock_checksum,
    dir::{get_inode_with_num, insert_dir_entry},
    disknode::Ext4Inode,
    entries::Ext4DirEntry2,
    error::{Ext4Error, Ext4Result},
    loopfile::resolve_inode_block,
    superblock::Ext4Superblock,
    *,
};

/// In-memory block device whose clock can be told to fail once.
struct MockBlockDevice {
    data: Vec<u8>,
    block_size: u32,
    now: Cell<i64>,
    clock_failure: Rc<Cell<Option<u32>>>,
}

impl MockBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
            block_size: rsext4::BLOCK_SIZE as u32,
            now: Cell::new(1_700_000_000),
            clock_failure: Rc::new(Cell::new(None)),
        }
    }

    fn range(&self, sector: rsext4::SectorId, len: usize) -> Ext4Result<std::ops::Range<usize>> {
        let start = sector.as_usize()? * self.block_size as usize;
        let end = start + len;
        if end > self.data.len() {
            return Err(Ext4Error::block_out_of_range(
                sector.to_u32()?,
                (self.data.len() / self.block_size as usize) as u64,
            ));
        }
        Ok(start..end)
    }
}

impl BlockIo for MockBlockDevice {
    fn read(&mut self, buffer: &mut [u8], sector: rsext4::SectorId, _count: u32) -> Ext4Result<()> {
        let range = self.range(sector, buffer.len())?;
        buffer.copy_from_slice(&self.data[range]);
        Ok(())
    }

    fn write(&mut self, buffer: &[u8], sector: rsext4::SectorId, _count: u32) -> Ext4Result<()> {
        let range = self.range(sector, buffer.len())?;
        self.data[range].copy_from_slice(buffer);
        Ok(())
    }

    fn geometry(&self) -> rsext4::DeviceGeometry {
        rsext4::DeviceGeometry::new(
            self.block_size,
            (self.data.len() / self.block_size as usize) as u64,
        )
    }

    fn capabilities(&self) -> rsext4::DeviceCapabilities {
        rsext4::DeviceCapabilities {
            flush: true,
            ..rsext4::DeviceCapabilities::default()
        }
    }

    fn flush(&mut self) -> rsext4::Ext4Result<()> {
        Ok(())
    }
}

impl rsext4::Clock for MockBlockDevice {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        // A countdown of one fails this call; larger values count down.
        if let Some(remaining) = self.clock_failure.get() {
            if remaining <= 1 {
                self.clock_failure.set(None);
                return Err(Ext4Error::io());
            }
            self.clock_failure.set(Some(remaining - 1));
        }
        let sec = self.now.get();
        self.now.set(sec + 1);
        Ok(Ext4Timestamp::new(sec, 0))
    }
}

type ClockFailure = Rc<Cell<Option<u32>>>;

fn mounted() -> (Jbd2Dev<MockBlockDevice>, Ext4FileSystem, ClockFailure) {
    let device = MockBlockDevice::new(64 * 1024 * 1024);
    let clock_failure = Rc::clone(&device.clock_failure);
    let mut jbd2_dev = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs(&mut jbd2_dev).expect("mkfs failed");
    let fs = Ext4FileSystem::mount(&mut jbd2_dev).expect("mount failed");
    (jbd2_dev, fs, clock_failure)
}

fn without_dir_index(fs: &mut Ext4FileSystem) {
    fs.superblock.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_DIR_INDEX;
}

/// A 255-byte name; fifteen of them fill one 4 KiB directory block.
fn long_name(index: usize) -> String {
    format!("{index:03}{}", "a".repeat(252))
}

fn fill_directory(
    device: &mut Jbd2Dev<MockBlockDevice>,
    fs: &mut Ext4FileSystem,
    directory: &str,
    entries: std::ops::Range<usize>,
) {
    for index in entries {
        mkfile(
            device,
            fs,
            &format!("{directory}/{}", long_name(index)),
            None,
            None,
        )
        .expect("fill directory");
    }
}

fn lookup(
    device: &mut Jbd2Dev<MockBlockDevice>,
    fs: &mut Ext4FileSystem,
    path: &str,
) -> Option<(InodeNumber, Ext4Inode)> {
    get_inode_with_num(fs, device, path).expect("lookup")
}

/// Allocation and directory counters that a failed create must leave alone.
#[derive(Debug, PartialEq, Eq)]
struct Accounting {
    free_inodes: u32,
    free_blocks: u64,
    used_dirs: u32,
    parent_links: u16,
    parent_size: u64,
}

impl Accounting {
    fn read(
        device: &mut Jbd2Dev<MockBlockDevice>,
        fs: &mut Ext4FileSystem,
        parent: InodeNumber,
    ) -> Self {
        let parent_inode = fs.get_inode_by_num(device, parent).expect("read parent");
        Self {
            free_inodes: fs.superblock.s_free_inodes_count,
            free_blocks: fs.superblock.free_blocks_count(),
            used_dirs: fs
                .group_descs
                .iter()
                .map(|descriptor| descriptor.used_dirs_count())
                .sum(),
            parent_links: parent_inode.i_links_count,
            parent_size: parent_inode.size(),
        }
    }
}

#[cfg(test)]
mod linear_directory_insertion_tests {
    use super::*;

    #[test]
    fn full_linear_directory_without_dir_index_appends_a_checksummed_block() {
        let (mut device, mut fs, _) = mounted();
        without_dir_index(&mut fs);
        mkdir(&mut device, &mut fs, "/linear").unwrap();
        fill_directory(&mut device, &mut fs, "/linear", 0..16);

        let (ino, mut inode) = lookup(&mut device, &mut fs, "/linear").unwrap();
        let block_size = fs.superblock.block_size();
        assert_eq!(inode.size(), 2 * block_size);
        assert_eq!(inode.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
        for index in 0..16 {
            let path = format!("/linear/{}", long_name(index));
            assert!(lookup(&mut device, &mut fs, &path).is_some(), "{index}");
        }
        let second = resolve_inode_block(&fs, &mut device, ino, &mut inode, 1)
            .unwrap()
            .expect("appended block is mapped");
        let cached = fs.datablock_cache.get_or_load(&mut device, second).unwrap();
        assert!(verify_ext4_dirblock_checksum(
            &fs.superblock,
            ino.raw(),
            inode.i_generation,
            &cached.data
        ));
        // The appended block holds exactly the sixteenth entry.
        let record_len = u16::from_le_bytes([cached.data[4], cached.data[5]]) as u64;
        assert_eq!(record_len, block_size - 12);
        assert_eq!(cached.data[6], 255);
    }

    #[test]
    fn appended_block_without_metadata_checksums_has_one_record_spanning_the_block() {
        let (mut device, mut fs, _) = mounted();
        without_dir_index(&mut fs);
        fs.superblock.s_feature_ro_compat &= !Ext4Superblock::EXT4_FEATURE_RO_COMPAT_METADATA_CSUM;
        mkdir(&mut device, &mut fs, "/plain").unwrap();
        fill_directory(&mut device, &mut fs, "/plain", 0..16);

        let (ino, mut inode) = lookup(&mut device, &mut fs, "/plain").unwrap();
        let block_size = fs.superblock.block_size();
        assert_eq!(inode.size(), 2 * block_size);
        let second = resolve_inode_block(&fs, &mut device, ino, &mut inode, 1)
            .unwrap()
            .expect("appended block is mapped");
        let cached = fs.datablock_cache.get_or_load(&mut device, second).unwrap();
        let record_len = u16::from_le_bytes([cached.data[4], cached.data[5]]) as u64;
        assert_eq!(record_len, block_size);
        assert_eq!(&cached.data[8..11], &long_name(15).as_bytes()[..3]);
        for index in 0..16 {
            let path = format!("/plain/{}", long_name(index));
            assert!(lookup(&mut device, &mut fs, &path).is_some(), "{index}");
        }
    }

    #[test]
    fn insertion_reuses_a_freed_record_at_the_start_of_a_block() {
        let (mut device, mut fs, _) = mounted();
        without_dir_index(&mut fs);
        mkdir(&mut device, &mut fs, "/reuse").unwrap();
        fill_directory(&mut device, &mut fs, "/reuse", 0..16);
        delete_file(&mut fs, &mut device, &format!("/reuse/{}", long_name(15))).unwrap();
        let size_before = lookup(&mut device, &mut fs, "/reuse").unwrap().1.size();

        fill_directory(&mut device, &mut fs, "/reuse", 16..17);

        let (_, inode) = lookup(&mut device, &mut fs, "/reuse").unwrap();
        assert_eq!(inode.size(), size_before);
        let reused = format!("/reuse/{}", long_name(16));
        assert!(lookup(&mut device, &mut fs, &reused).is_some());
        let removed = format!("/reuse/{}", long_name(15));
        assert!(lookup(&mut device, &mut fs, &removed).is_none());
    }

    #[test]
    fn insertion_into_a_directory_with_an_unmapped_block_reports_corruption() {
        let (mut device, mut fs, _) = mounted();
        mkdir(&mut device, &mut fs, "/sparse").unwrap();
        fill_directory(&mut device, &mut fs, "/sparse", 0..15);
        let (ino, inode) = lookup(&mut device, &mut fs, "/sparse").unwrap();
        let (child, _) =
            lookup(&mut device, &mut fs, &format!("/sparse/{}", long_name(0))).unwrap();
        let mut claims_two_blocks = inode;
        claims_two_blocks.set_size(2 * fs.superblock.block_size());

        let error = insert_dir_entry(
            &mut fs,
            &mut device,
            ino,
            &mut claims_two_blocks,
            child,
            &long_name(99),
            Ext4DirEntry2::EXT4_FT_REG_FILE,
        )
        .unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::Corrupted);
        let extra = format!("/sparse/{}", long_name(99));
        assert!(lookup(&mut device, &mut fs, &extra).is_none());
        assert_eq!(
            lookup(&mut device, &mut fs, "/sparse").unwrap().1.size(),
            inode.size()
        );
    }

    #[test]
    fn block_mapped_directory_grows_through_direct_pointers_and_stops_at_twelve() {
        let (mut device, mut fs, _) = mounted();
        without_dir_index(&mut fs);
        fs.superblock.s_feature_incompat &= !Ext4Superblock::EXT4_FEATURE_INCOMPAT_EXTENTS;
        mkdir(&mut device, &mut fs, "/direct").unwrap();
        assert!(
            !lookup(&mut device, &mut fs, "/direct")
                .unwrap()
                .1
                .uses_extents()
        );
        fill_directory(&mut device, &mut fs, "/direct", 0..180);
        let (_, full) = lookup(&mut device, &mut fs, "/direct").unwrap();
        assert_eq!(full.size(), 12 * fs.superblock.block_size());
        let free_inodes = fs.superblock.s_free_inodes_count;

        let path = format!("/direct/{}", long_name(180));
        let error = mkfile(&mut device, &mut fs, &path, None, None).unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::Unsupported);
        assert!(lookup(&mut device, &mut fs, &path).is_none());
        assert_eq!(fs.superblock.s_free_inodes_count, free_inodes);
        let (_, after) = lookup(&mut device, &mut fs, "/direct").unwrap();
        assert_eq!(after.size(), full.size());
        assert_eq!(after.i_block, full.i_block);
    }
}

#[cfg(test)]
mod directory_creation_tests {
    use super::*;

    #[test]
    fn mkdir_rejects_empty_root_and_reserved_paths() {
        let (mut device, mut fs, _) = mounted();
        let empty = mkdir(&mut device, &mut fs, "").unwrap_err();
        assert_eq!(empty.kind(), Ext4ErrorKind::InvalidInput);
        let root = mkdir(&mut device, &mut fs, "/").unwrap_err();
        assert_eq!(root.kind(), Ext4ErrorKind::AlreadyExists);
        let free_inodes = fs.superblock.s_free_inodes_count;
        let reserved = mkdir(&mut device, &mut fs, "/missing/.").unwrap_err();
        assert_eq!(reserved.kind(), Ext4ErrorKind::InvalidInput);
        assert_eq!(fs.superblock.s_free_inodes_count, free_inodes);
        assert!(lookup(&mut device, &mut fs, "/missing").is_none());
    }

    #[test]
    fn mkdir_recreates_lost_and_found_through_its_bootstrap_path() {
        let (mut device, mut fs, _) = mounted();
        delete_dir(&mut fs, &mut device, "/lost+found").unwrap();
        assert!(lookup(&mut device, &mut fs, "/lost+found").is_none());

        let created = mkdir(&mut device, &mut fs, "/lost+found").unwrap();

        assert!(created.is_dir());
        let (_, found) = lookup(&mut device, &mut fs, "/lost+found").unwrap();
        assert!(found.is_dir());
        assert_eq!(found.i_generation, created.i_generation);
    }

    #[test]
    fn mkdir_inherits_the_project_of_a_project_inheriting_parent() {
        let (mut device, mut fs, _) = mounted();
        fs.superblock.s_feature_ro_compat |= Ext4Superblock::EXT4_FEATURE_RO_COMPAT_PROJECT;
        mkdir(&mut device, &mut fs, "/project").unwrap();
        mkdir(&mut device, &mut fs, "/plain").unwrap();
        let (project, _) = lookup(&mut device, &mut fs, "/project").unwrap();
        fs.modify_inode(&mut device, project, |inode| {
            inode.i_flags |= Ext4Inode::EXT4_PROJINHERIT_FL;
            inode.i_projid = 42;
        })
        .unwrap();

        let inherited = mkdir(&mut device, &mut fs, "/project/child").unwrap();
        let plain = mkdir(&mut device, &mut fs, "/plain/child").unwrap();

        assert_eq!(inherited.i_projid, 42);
        assert_ne!(inherited.i_flags & Ext4Inode::EXT4_PROJINHERIT_FL, 0);
        assert_eq!(plain.i_projid, 0);
    }

    #[test]
    fn mkdir_rolls_back_when_the_parent_cannot_grow() {
        let (mut device, mut fs, _) = mounted();
        without_dir_index(&mut fs);
        mkdir(&mut device, &mut fs, "/full").unwrap();
        fill_directory(&mut device, &mut fs, "/full", 0..15);
        // Leave exactly the one block the child directory needs for itself.
        while fs.superblock.free_blocks_count() > 1 {
            fs.alloc_block(&mut device).unwrap();
        }
        let (parent, _) = lookup(&mut device, &mut fs, "/full").unwrap();
        let before = Accounting::read(&mut device, &mut fs, parent);

        // The long child name no longer fits in the parent's only block.
        let child = format!("/full/{}", long_name(15));
        let error = mkdir(&mut device, &mut fs, &child).unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::NoSpace);
        assert_eq!(Accounting::read(&mut device, &mut fs, parent), before);
        assert!(lookup(&mut device, &mut fs, &child).is_none());
    }

    #[test]
    fn mkdir_reports_a_corrupt_parent_block_before_allocating() {
        let (mut device, mut fs, _) = mounted();
        mkdir(&mut device, &mut fs, "/damaged").unwrap();
        let (parent, mut inode) = lookup(&mut device, &mut fs, "/damaged").unwrap();
        let block = resolve_inode_block(&fs, &mut device, parent, &mut inode, 0)
            .unwrap()
            .expect("directory block is mapped");
        // A record length of zero cannot be walked past.
        fs.datablock_cache
            .modify(&mut device, block, |data| {
                data[4] = 0;
                data[5] = 0;
            })
            .unwrap();
        let before = Accounting::read(&mut device, &mut fs, parent);

        let error = mkdir(&mut device, &mut fs, "/damaged/child").unwrap_err();

        assert!(error.is_corruption(), "{error:?}");
        assert_eq!(Accounting::read(&mut device, &mut fs, parent), before);
    }

    #[test]
    fn mkdir_releases_the_new_inode_when_no_block_is_left() {
        let (mut device, mut fs, _) = mounted();
        mkdir(&mut device, &mut fs, "/parent").unwrap();
        while fs.superblock.free_blocks_count() > 0 {
            fs.alloc_block(&mut device).unwrap();
        }
        let (parent, _) = lookup(&mut device, &mut fs, "/parent").unwrap();
        let before = Accounting::read(&mut device, &mut fs, parent);

        let error = mkdir(&mut device, &mut fs, "/parent/child").unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::NoSpace);
        assert_eq!(Accounting::read(&mut device, &mut fs, parent), before);
        assert!(lookup(&mut device, &mut fs, "/parent/child").is_none());
    }

    #[test]
    fn mkdir_refuses_a_parent_at_the_link_limit_without_dir_nlink() {
        let (mut device, mut fs, _) = mounted();
        fs.superblock.s_feature_ro_compat &= !Ext4Superblock::EXT4_FEATURE_RO_COMPAT_DIR_NLINK;
        mkdir(&mut device, &mut fs, "/parent").unwrap();
        let (parent, _) = lookup(&mut device, &mut fs, "/parent").unwrap();
        fs.modify_inode(&mut device, parent, |inode| {
            inode.i_links_count = Ext4Inode::EXT4_LINK_MAX;
        })
        .unwrap();
        let before = Accounting::read(&mut device, &mut fs, parent);

        let error = mkdir(&mut device, &mut fs, "/parent/child").unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::TooManyLinks);
        assert_eq!(Accounting::read(&mut device, &mut fs, parent), before);
        assert!(lookup(&mut device, &mut fs, "/parent/child").is_none());
    }

    #[test]
    fn mkdir_leaves_accounting_unchanged_when_any_clock_read_fails() {
        let (mut device, mut fs, clock_failure) = mounted();
        mkdir(&mut device, &mut fs, "/parent").unwrap();
        let (parent, _) = lookup(&mut device, &mut fs, "/parent").unwrap();
        for countdown in 1.. {
            let before = Accounting::read(&mut device, &mut fs, parent);
            clock_failure.set(Some(countdown));
            let result = mkdir(&mut device, &mut fs, "/parent/child");
            let fired = clock_failure.get().is_none();
            clock_failure.set(None);
            if !fired {
                result.expect("mkdir succeeds once no clock read fails");
                break;
            }
            let error = result.expect_err("a failed clock read fails mkdir");
            assert_eq!(error.kind(), Ext4ErrorKind::Io, "countdown {countdown}");
            assert_eq!(
                Accounting::read(&mut device, &mut fs, parent),
                before,
                "countdown {countdown}"
            );
            assert!(lookup(&mut device, &mut fs, "/parent/child").is_none());
        }
        let (_, child) = lookup(&mut device, &mut fs, "/parent/child").unwrap();
        assert!(child.is_dir());
    }
}

#[cfg(test)]
mod file_creation_tests {
    use super::*;

    #[test]
    fn symbolic_link_rejects_existing_destination_and_unusable_parents() {
        let (mut device, mut fs, _) = mounted();
        mkfile(&mut device, &mut fs, "/target", Some(b"data"), None).unwrap();
        mkfile(&mut device, &mut fs, "/taken", None, None).unwrap();
        let free_inodes = fs.superblock.s_free_inodes_count;

        for destination in ["/taken", "/missing/link", "/target/link"] {
            let error = create_symbol_link(&mut device, &mut fs, "/target", destination)
                .expect_err(destination);
            assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput, "{destination}");
        }

        assert_eq!(fs.superblock.s_free_inodes_count, free_inodes);
        assert!(lookup(&mut device, &mut fs, "/missing").is_none());
        assert!(lookup(&mut device, &mut fs, "/taken").unwrap().1.is_file());
    }

    #[test]
    fn symbolic_link_with_a_relative_destination_is_created_in_the_root() {
        let (mut device, mut fs, _) = mounted();
        mkfile(&mut device, &mut fs, "/target", None, None).unwrap();

        create_symbol_link(&mut device, &mut fs, "/target", "relative").unwrap();

        let (_, link) = lookup(&mut device, &mut fs, "/relative").unwrap();
        assert!(link.is_symlink());
        assert_eq!(link.size(), "/target".len() as u64);
    }

    #[test]
    fn mkfile_symlink_type_stores_an_inline_target_or_an_empty_one() {
        let (mut device, mut fs, _) = mounted();
        let symlink = Some(Ext4DirEntry2::EXT4_FT_SYMLINK);

        let inline = mkfile(&mut device, &mut fs, "/inline", Some(b"target"), symlink).unwrap();
        let empty = mkfile(&mut device, &mut fs, "/empty", None, symlink).unwrap();

        assert!(inline.is_symlink());
        assert_eq!(inline.size(), 6);
        assert_eq!(&inline.i_block[0].to_le_bytes(), b"targ");
        assert_eq!(&inline.i_block[1].to_le_bytes()[..2], b"et");
        assert!(empty.is_symlink());
        assert_eq!(empty.size(), 0);
        assert_eq!(empty.i_block, [0; 15]);
        assert!(
            lookup(&mut device, &mut fs, "/empty")
                .unwrap()
                .1
                .is_symlink()
        );
    }

    #[test]
    fn mkfile_without_extents_refuses_data_past_twelve_direct_blocks() {
        let (mut device, mut fs, _) = mounted();
        fs.superblock.s_feature_incompat &= !Ext4Superblock::EXT4_FEATURE_INCOMPAT_EXTENTS;
        let data = vec![0x5a; 12 * BLOCK_SIZE + 1];
        let root = fs.root_inode;
        let before = Accounting::read(&mut device, &mut fs, root);

        let error = mkfile(&mut device, &mut fs, "/too-big", Some(&data), None).unwrap_err();

        assert_eq!(error.kind(), Ext4ErrorKind::Unsupported);
        assert_eq!(Accounting::read(&mut device, &mut fs, root), before);
        assert!(lookup(&mut device, &mut fs, "/too-big").is_none());
    }

    #[test]
    fn mkfile_leaves_accounting_unchanged_when_any_clock_read_fails() {
        let (mut device, mut fs, clock_failure) = mounted();
        let root = fs.root_inode;
        let data = vec![0xa5; 2 * BLOCK_SIZE];
        for countdown in 1.. {
            let before = Accounting::read(&mut device, &mut fs, root);
            clock_failure.set(Some(countdown));
            let result = mkfile(&mut device, &mut fs, "/file", Some(&data), None);
            let fired = clock_failure.get().is_none();
            clock_failure.set(None);
            if !fired {
                result.expect("mkfile succeeds once no clock read fails");
                break;
            }
            let error = result.expect_err("a failed clock read fails mkfile");
            assert_eq!(error.kind(), Ext4ErrorKind::Io, "countdown {countdown}");
            assert_eq!(
                Accounting::read(&mut device, &mut fs, root),
                before,
                "countdown {countdown}"
            );
            assert!(lookup(&mut device, &mut fs, "/file").is_none());
        }
        assert_eq!(read_file(&mut device, &mut fs, "/file").unwrap(), data);
    }
}

#[cfg(test)]
mod resolved_parent_creation_tests {
    use super::*;

    /// Clock for the owned mount, which needs a clock it can send.
    struct OwnedClock(Cell<i64>);

    impl rsext4::Clock for OwnedClock {
        fn now(&self) -> Ext4Result<Ext4Timestamp> {
            let sec = self.0.get();
            self.0.set(sec + 1);
            Ok(Ext4Timestamp::new(sec, 0))
        }
    }

    #[test]
    fn creation_at_a_resolved_parent_rejects_reserved_existing_and_file_parents() {
        let (mut device, fs, _) = mounted();
        umount(fs, &mut device).expect("umount failed");
        let services = MountServices::new(OwnedClock(Cell::new(1_800_000_000)), (), NoopObserver);
        let mut filesystem =
            Ext4::mount(device.into_inner(), services, MountOptions::read_write()).expect("mount");
        let context = MutationContext::new(1000, 1001, 0, 0o022);
        let root = filesystem.root_inode();
        let permissions = FilePermissions::new(0o755).unwrap();
        let name = |bytes| FileName::new(bytes).unwrap();
        let file = filesystem
            .create_regular_file(context, root, name(b"file"), permissions)
            .unwrap()
            .number;

        for (parent, entry, kind) in [
            (root, b".".as_slice(), Ext4ErrorKind::InvalidInput),
            (root, b"lost+found", Ext4ErrorKind::AlreadyExists),
            (file, b"child", Ext4ErrorKind::NotDirectory),
        ] {
            let directory = filesystem
                .create_directory(context, parent, name(entry), permissions)
                .unwrap_err();
            assert_eq!(directory.kind(), kind, "directory {entry:?}");
            let regular = filesystem
                .create_regular_file(context, parent, name(entry), permissions)
                .unwrap_err();
            assert_eq!(regular.kind(), kind, "file {entry:?}");
        }

        let found = filesystem.lookup_child(root, name(b"lost+found")).unwrap();
        assert!(found.is_some_and(|entry| entry.number != file));
        assert!(
            filesystem
                .lookup_child(root, name(b"child"))
                .unwrap()
                .is_none()
        );
    }
}
