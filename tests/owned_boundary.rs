//! Tests for the owned mount boundary (`Ext4`): special inode kinds,
//! directory cursors and readers, permission validation, remount policy, and
//! read-only behavior.

// Miri runs only the tests that reach unsafe code, and this file reaches none.
#![cfg(not(miri))]

use std::{cell::Cell, collections::BTreeMap};

use rsext4::{
    error::{Ext4Error, Ext4Result},
    *,
};

/// In-memory block device that also serves as the mount clock.
struct MockBlockDevice {
    data: Vec<u8>,
    block_size_bytes: u32,
    now: Cell<i64>,
}

impl MockBlockDevice {
    fn new(size: usize) -> Self {
        Self {
            data: vec![0; size],
            block_size_bytes: rsext4::BLOCK_SIZE as u32,
            now: Cell::new(1_700_000_000),
        }
    }
}

impl BlockIo for MockBlockDevice {
    fn read(&mut self, buffer: &mut [u8], sector: rsext4::SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * self.block_size_bytes as usize;
        let end = start + buffer.len();
        if end > self.data.len() {
            return Err(Ext4Error::block_out_of_range(
                sector.to_u32()?,
                (self.data.len() / self.block_size_bytes as usize) as u64,
            ));
        }
        buffer.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write(&mut self, buffer: &[u8], sector: rsext4::SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * self.block_size_bytes as usize;
        let end = start + buffer.len();
        if end > self.data.len() {
            return Err(Ext4Error::block_out_of_range(
                sector.to_u32()?,
                (self.data.len() / self.block_size_bytes as usize) as u64,
            ));
        }
        self.data[start..end].copy_from_slice(buffer);
        Ok(())
    }

    fn geometry(&self) -> rsext4::DeviceGeometry {
        rsext4::DeviceGeometry::new(self.block_size_bytes, {
            (self.data.len() / self.block_size_bytes as usize) as u64
        })
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
        let seconds = self.now.get();
        self.now.set(seconds + 1);
        Ok(Ext4Timestamp::new(seconds, 0))
    }
}

type OwnedFilesystem = Ext4<MockBlockDevice, MountedServices<(), NoopObserver>>;

fn formatted_device() -> MockBlockDevice {
    format(
        MockBlockDevice::new(64 * 1024 * 1024),
        MockBlockDevice::new(0),
        MkfsOptions::default(),
    )
    .expect("mkfs failed")
}

fn mount_device(device: MockBlockDevice, options: MountOptions) -> OwnedFilesystem {
    let services = MountServices::new(MockBlockDevice::new(0), (), NoopObserver);
    Ext4::mount(device, services, options).expect("owned mount failed")
}

fn mounted(options: MountOptions) -> OwnedFilesystem {
    mount_device(formatted_device(), options)
}

fn name(raw: &[u8]) -> FileName<'_> {
    FileName::new(raw).expect("valid raw name")
}

fn permissions(bits: u16) -> FilePermissions {
    FilePermissions::new(bits).expect("valid permissions")
}

fn context() -> MutationContext {
    MutationContext::new(1000, 1001, 0, 0o022)
}

/// Follows `next_cursor` from the start of `directory` until the end and
/// returns every name with its inode number, failing if a cursor repeats.
fn enumerate_with_reader(
    filesystem: &mut OwnedFilesystem,
    directory: InodeNumber,
    batch: usize,
) -> Vec<(Vec<u8>, InodeNumber)> {
    let mut reader = filesystem
        .open_directory_reader(directory)
        .expect("open directory reader");
    assert_eq!(reader.directory(), directory);
    let mut cursor = DirectoryCursor::Start;
    let mut seen_cursors = Vec::new();
    let mut names = Vec::new();
    loop {
        assert!(
            !seen_cursors.contains(&cursor),
            "the reader returned to cursor {cursor:?}"
        );
        seen_cursors.push(cursor);
        let entries = filesystem
            .read_directory_with_reader(&mut reader, cursor, batch)
            .expect("read directory batch");
        let Some(last) = entries.last() else {
            break;
        };
        cursor = last.next_cursor;
        names.extend(entries.into_iter().map(|entry| (entry.name, entry.inode)));
        if cursor == DirectoryCursor::End {
            break;
        }
    }
    names
}

#[test]
fn special_inode_kinds_report_their_type_in_inode_and_directory_entry() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let block_number = DeviceNumber::new(8, 1).expect("valid device number");
    let character_number = DeviceNumber::new(4, 64).expect("valid device number");
    let kinds = [
        (
            b"block".as_slice(),
            SpecialInodeKind::BlockDevice(block_number),
            DirectoryEntryType::BlockDevice,
            Some(block_number),
        ),
        (
            b"character",
            SpecialInodeKind::CharacterDevice(character_number),
            DirectoryEntryType::CharacterDevice,
            Some(character_number),
        ),
        (
            b"fifo",
            SpecialInodeKind::Fifo,
            DirectoryEntryType::Fifo,
            None,
        ),
        (
            b"socket",
            SpecialInodeKind::Socket,
            DirectoryEntryType::Socket,
            None,
        ),
    ];
    for (raw_name, kind, file_type, device_number) in kinds {
        let inode = filesystem
            .create_special_inode(context(), root, name(raw_name), permissions(0o666), kind)
            .expect("create special inode");
        assert_eq!(inode.file_type(), file_type);
        assert!(!inode.is_directory());
        assert_eq!(inode.mode & 0o7777, 0o644);
        assert_eq!(inode.device_number, device_number);
    }
    let file = filesystem
        .create_regular_file(context(), root, name(b"regular"), permissions(0o644))
        .expect("create regular file");
    assert_eq!(file.file_type(), DirectoryEntryType::RegularFile);
    let link = filesystem
        .create_symlink(context(), root, name(b"link"), b"regular")
        .expect("create symlink");
    assert_eq!(link.file_type(), DirectoryEntryType::Symlink);

    let types = filesystem
        .read_directory(root, DirectoryCursor::Start, 64)
        .expect("read root")
        .into_iter()
        .map(|entry| (entry.name, entry.file_type))
        .collect::<BTreeMap<_, _>>();
    for (raw_name, _, file_type, _) in kinds {
        assert_eq!(types.get(raw_name), Some(&file_type));
    }
    assert_eq!(
        types.get(b"regular".as_slice()),
        Some(&DirectoryEntryType::RegularFile)
    );
    assert_eq!(
        types.get(b"link".as_slice()),
        Some(&DirectoryEntryType::Symlink)
    );
    assert_eq!(
        types.get(b".".as_slice()),
        Some(&DirectoryEntryType::Directory)
    );
}

#[test]
fn file_permissions_reject_bits_outside_the_permission_mask() {
    assert_eq!(
        FilePermissions::new(0o10000)
            .expect_err("file type bits are not permissions")
            .kind(),
        Ext4ErrorKind::InvalidInput
    );
    assert_eq!(permissions(0o7777).bits(), 0o7777);
    assert_eq!(permissions(0o4755).bits(), 0o4755);
}

#[test]
fn directory_entry_points_reject_non_directory_inodes() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let file = filesystem
        .create_regular_file(context(), root, name(b"plain"), permissions(0o644))
        .expect("create regular file");

    let open = filesystem
        .open_directory_reader(file.number)
        .expect_err("a regular file has no directory reader");
    let read = filesystem
        .read_directory(file.number, DirectoryCursor::Start, 8)
        .expect_err("a regular file cannot be enumerated");
    let end = filesystem
        .directory_end_cursor(file.number)
        .expect_err("a regular file has no directory end");

    for error in [open, read, end] {
        assert_eq!(error.kind(), Ext4ErrorKind::NotDirectory);
    }
}

#[test]
fn linear_directory_cursor_resumes_after_each_returned_entry() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let size = filesystem.inode(root).expect("inspect root").size;
    let all = filesystem
        .read_directory(root, DirectoryCursor::Start, 64)
        .expect("read whole root");
    assert!(all.len() >= 3);

    let mut cursor = DirectoryCursor::Start;
    let mut resumed = Vec::new();
    loop {
        let batch = filesystem
            .read_directory(root, cursor, 1)
            .expect("read one entry");
        let Some(entry) = batch.into_iter().next() else {
            break;
        };
        cursor = entry.next_cursor;
        resumed.push(entry);
    }
    assert_eq!(resumed, all);
    // The last record's cursor stops before the block's checksum tail, and
    // resuming there returns nothing because the tail is never reported.
    assert!(matches!(cursor, DirectoryCursor::Linear { offset } if offset < size));
    assert_eq!(
        filesystem.directory_end_cursor(root).expect("root end"),
        DirectoryCursor::Linear { offset: size }
    );

    for empty_cursor in [
        DirectoryCursor::End,
        DirectoryCursor::Linear { offset: size },
        DirectoryCursor::Linear {
            offset: size + 4096,
        },
    ] {
        assert!(
            filesystem
                .read_directory(root, empty_cursor, 8)
                .expect("read past the end")
                .is_empty()
        );
    }
    assert!(
        filesystem
            .read_directory(root, DirectoryCursor::Start, 0)
            .expect("read zero entries")
            .is_empty()
    );
    let error = filesystem
        .read_directory(
            root,
            DirectoryCursor::HTree {
                major: 0,
                minor: 0,
                collision: 0,
            },
            8,
        )
        .expect_err("a linear directory rejects hash cursors");
    assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
}

#[test]
fn indexed_directory_reader_returns_every_name_once_across_batches() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let directory = filesystem
        .create_directory(context(), root, name(b"indexed"), permissions(0o755))
        .expect("create directory")
        .number;
    let mut expected = BTreeMap::new();
    for index in 0..40 {
        let raw_name = format!("{index:03}{}", "n".repeat(240)).into_bytes();
        let file = filesystem
            .create_regular_file(context(), directory, name(&raw_name), permissions(0o644))
            .expect("create long name");
        expected.insert(raw_name, file.number);
    }
    let info = filesystem.inode(directory).expect("inspect directory");
    assert!(info.flags.contains(InodeFlags::DIRECTORY_INDEX));
    assert_eq!(
        filesystem
            .directory_end_cursor(directory)
            .expect("indexed end"),
        DirectoryCursor::End
    );

    let listed = enumerate_with_reader(&mut filesystem, directory, 7);
    let children = listed
        .iter()
        .filter(|(raw_name, _)| raw_name != b"." && raw_name != b"..")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(children.len(), expected.len());
    assert_eq!(children.into_iter().collect::<BTreeMap<_, _>>(), expected);

    let unbatched = filesystem
        .read_directory(directory, DirectoryCursor::Start, 128)
        .expect("read indexed directory at once");
    assert_eq!(
        unbatched
            .into_iter()
            .map(|entry| (entry.name, entry.inode))
            .collect::<Vec<_>>(),
        listed
    );
}

#[test]
fn indexed_directory_rejects_linear_and_odd_hash_cursors() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let directory = filesystem
        .create_directory(context(), root, name(b"indexed"), permissions(0o755))
        .expect("create directory")
        .number;
    for index in 0..40 {
        let raw_name = format!("{index:03}{}", "c".repeat(240)).into_bytes();
        filesystem
            .create_regular_file(context(), directory, name(&raw_name), permissions(0o644))
            .expect("create long name");
    }

    for cursor in [
        DirectoryCursor::Linear { offset: 0 },
        DirectoryCursor::HTree {
            major: 1,
            minor: 0,
            collision: 0,
        },
    ] {
        let error = filesystem
            .read_directory(directory, cursor, 8)
            .expect_err("an indexed directory accepts only even hash cursors");
        assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
    }
}

#[test]
fn reset_reader_rereads_indexed_ranges_after_directory_changes() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let directory = filesystem
        .create_directory(context(), root, name(b"indexed"), permissions(0o755))
        .expect("create directory")
        .number;
    for index in 0..40 {
        let raw_name = format!("{index:03}{}", "r".repeat(240)).into_bytes();
        filesystem
            .create_regular_file(context(), directory, name(&raw_name), permissions(0o644))
            .expect("create long name");
    }
    let mut reader = filesystem
        .open_directory_reader(directory)
        .expect("open reader");
    let first = filesystem
        .read_directory_with_reader(&mut reader, DirectoryCursor::Start, 4)
        .expect("read first batch");
    assert_eq!(first.len(), 4);

    filesystem
        .create_regular_file(context(), directory, name(b"late"), permissions(0o644))
        .expect("create after the first batch");
    let changed = filesystem
        .read_directory_with_reader(&mut reader, first[3].next_cursor, 128)
        .expect("continue after a change");
    reader.reset();
    let after_reset = filesystem
        .read_directory_with_reader(&mut reader, first[3].next_cursor, 128)
        .expect("continue after reset");
    let fresh = filesystem
        .read_directory(directory, first[3].next_cursor, 128)
        .expect("continue without reader state");

    assert_eq!(changed, fresh);
    assert_eq!(after_reset, fresh);
    assert_eq!(
        changed.last().map(|entry| entry.next_cursor),
        Some(DirectoryCursor::End)
    );
}

#[test]
fn remount_rejects_changing_the_journal_replay_policy() {
    let mut filesystem = mounted(MountOptions::read_write());
    let error = filesystem
        .remount(MountOptions {
            replay_journal: false,
            ..MountOptions::read_write()
        })
        .expect_err("replay policy is fixed at mount");

    assert_eq!(error.kind(), Ext4ErrorKind::Unsupported);
    assert_eq!(filesystem.options(), MountOptions::read_write());
}

#[test]
fn read_only_mount_syncs_without_mmp_and_refuses_mutation() {
    let mut filesystem = mounted(MountOptions::read_only_no_journal_replay());
    let root = filesystem.root_inode();

    filesystem
        .sync()
        .expect("read-only sync flushes the device");
    assert_eq!(
        filesystem
            .refresh_mmp(core::time::Duration::from_secs(5))
            .expect("read-only refresh"),
        None
    );
    assert_eq!(filesystem.mmp_refresh_interval(), None);
    let error = filesystem
        .create_regular_file(context(), root, name(b"refused"), permissions(0o644))
        .expect_err("read-only mount refuses creation");
    assert_eq!(error.kind(), Ext4ErrorKind::ReadOnly);
    filesystem.unmount().expect("read-only unmount");
}

#[test]
fn runtime_failure_report_is_ignored_without_active_mmp() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    assert_eq!(filesystem.mmp_refresh_interval(), None);

    filesystem.report_mmp_runtime_failure(Ext4Error::io());

    filesystem
        .create_regular_file(context(), root, name(b"allowed"), permissions(0o644))
        .expect("no MMP (multiple mount protection) owner means nothing is latched");
    assert_eq!(
        filesystem
            .refresh_mmp(core::time::Duration::ZERO)
            .expect("refresh without MMP"),
        None
    );
}

#[test]
fn truncate_through_the_owned_boundary_shortens_file_data() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let file = filesystem
        .create_regular_file(context(), root, name(b"truncated"), permissions(0o644))
        .expect("create file");
    filesystem
        .write_inode(file.number, 0, &[0x5a; 10_000])
        .expect("write file");

    filesystem
        .truncate_inode(file.number, 100)
        .expect("truncate file");

    assert_eq!(filesystem.inode(file.number).expect("inspect").size, 100);
    let mut output = [0u8; 200];
    let copied = filesystem
        .read_inode(file.number, 0, &mut output)
        .expect("read file");
    assert_eq!(copied, 100);
    assert!(output[..100].iter().all(|byte| *byte == 0x5a));
}

#[test]
fn metadata_update_applies_owner_permissions_and_device_number() {
    let mut filesystem = mounted(MountOptions::read_write());
    let root = filesystem.root_inode();
    let original = DeviceNumber::new(4, 1).expect("valid device number");
    let replacement = DeviceNumber::new(259, 7).expect("valid device number");
    let inode = filesystem
        .create_special_inode(
            context(),
            root,
            name(b"device"),
            permissions(0o666),
            SpecialInodeKind::CharacterDevice(original),
        )
        .expect("create device inode");

    let updated = filesystem
        .update_inode_metadata(
            inode.number,
            InodeMetadataUpdate {
                permissions: Some(permissions(0o600)),
                owner: Some((5, 6)),
                device_number: Some(replacement),
                ..InodeMetadataUpdate::default()
            },
        )
        .expect("update metadata");

    assert_eq!(updated.mode & 0o7777, 0o600);
    assert_eq!((updated.uid, updated.gid), (5, 6));
    assert_eq!(updated.device_number, Some(replacement));
    assert_eq!(updated.file_type(), DirectoryEntryType::CharacterDevice);
    assert_eq!(filesystem.inode(inode.number).expect("inspect"), updated);
}
