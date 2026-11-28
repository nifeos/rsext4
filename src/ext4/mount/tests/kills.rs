//! Tests that pin each mount decision on a formatted image: the state a mount
//! leaves in the superblock, the events it reports, the layout it loads during
//! recovery, and the damaged images it refuses.
//!
//! The rig formats a four-group filesystem with 1 KiB blocks. Its group
//! descriptors start at block 2 and its journal inode is inode 8 in group 0.

use super::*;
use crate::endian::DiskFormat;
use crate::error::ErrorContext;
use crate::runtime::{Event, FeatureEvent, IntegrityEvent, Observer, RepairEvent};
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    mkfs_with_options,
};

const BLOCK: usize = 1024;
const BLOCKS: usize = 30000;
const DESCRIPTORS: usize = 2 * BLOCK;
const DESCRIPTOR: usize = 64;

#[derive(Clone)]
struct MemoryDevice {
    bytes: Vec<u8>,
}

impl BlockIo for MemoryDevice {
    fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
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

#[derive(Default)]
struct Recorder(Vec<Event>);

impl Observer for Recorder {
    fn event(&mut self, event: Event) {
        self.0.push(event);
    }
}

fn wrap(bytes: Vec<u8>, use_journal: bool) -> Jbd2Dev<MemoryDevice> {
    let mut journal = Jbd2Dev::initial_jbd2dev(0, MemoryDevice { bytes }, use_journal);
    journal
        .set_filesystem_block_size(BLOCK)
        .expect("block size");
    journal
}

/// A freshly formatted image that has never been mounted.
fn image() -> Vec<u8> {
    let mut journal = wrap(vec![0; BLOCKS * BLOCK], true);
    mkfs_with_options(
        &mut journal,
        MkfsOptions {
            block_size: BLOCK as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    journal.into_inner().bytes
}

struct Mounted {
    result: Ext4Result<Ext4FileSystem>,
    events: Vec<Event>,
    journal: Jbd2Dev<MemoryDevice>,
}

impl Mounted {
    fn fs(&self) -> &Ext4FileSystem {
        self.result.as_ref().expect("mount")
    }

    fn operation(&self) -> &'static str {
        let Err(error) = &self.result else {
            panic!("the mount must be refused");
        };
        match error.context() {
            Some(ErrorContext::Operation { op }) => op,
            _ => panic!("the error names its operation"),
        }
    }

    fn bytes(self) -> Vec<u8> {
        self.journal.into_inner().bytes
    }
}

fn mount(bytes: Vec<u8>, options: MountOptions, use_journal: bool) -> Mounted {
    let mut journal = wrap(bytes, use_journal);
    let mut events = Recorder::default();
    let result =
        Ext4FileSystem::mount_with_options_and_observer(&mut journal, options, &mut events);
    Mounted {
        result,
        events: events.0,
        journal,
    }
}

fn read_only() -> MountOptions {
    MountOptions::read_only_no_journal_replay()
}

fn superblock(bytes: &[u8]) -> Ext4Superblock {
    Ext4Superblock::from_disk_bytes(&bytes[BLOCK..2 * BLOCK])
}

fn edit_superblock(bytes: &mut [u8], edit: impl FnOnce(&mut Ext4Superblock)) {
    let mut sb = superblock(bytes);
    edit(&mut sb);
    sb.update_checksum();
    sb.to_disk_bytes(&mut bytes[BLOCK..2 * BLOCK]);
}

fn edit_descriptor(bytes: &mut [u8], group: u32, edit: impl FnOnce(&mut Ext4GroupDesc)) {
    let sb = superblock(bytes);
    let start = DESCRIPTORS + group as usize * DESCRIPTOR;
    let record = &mut bytes[start..start + DESCRIPTOR];
    let mut descriptor = Ext4GroupDesc::decode_checked(record).expect("descriptor");
    edit(&mut descriptor);
    descriptor
        .encode_with_checksum(&sb, group, record, None, None)
        .expect("encode");
}

/// Changes one inode through a writable mount and unmounts cleanly.
fn edit_inode(bytes: Vec<u8>, inode: u32, edit: impl FnOnce(&mut Ext4Inode)) -> Vec<u8> {
    let mut journal = wrap(bytes, true);
    let mut fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    fs.modify_inode(&mut journal, InodeNumber::new(inode).unwrap(), edit)
        .expect("modify inode");
    fs.umount(&mut journal).expect("umount");
    journal.into_inner().bytes
}

fn has(events: &[Event], event: Event) -> bool {
    events.contains(&event)
}

#[test]
fn dirtying_for_mount_clears_only_the_clean_bit_and_counts_the_mount() {
    let mut sb = Ext4Superblock {
        s_state: Ext4Superblock::EXT4_VALID_FS | Ext4Superblock::EXT4_ERROR_FS,
        s_mnt_count: 4,
        ..Default::default()
    };
    Ext4FileSystem::dirty_for_mount(&mut sb);
    assert_eq!(sb.s_state, Ext4Superblock::EXT4_ERROR_FS);
    assert_eq!(sb.s_mnt_count, 5);
}

#[test]
fn a_filesystem_without_a_journal_needs_no_journal_source() {
    let mut sb = Ext4Superblock {
        s_journal_inum: 0,
        ..Default::default()
    };
    sb.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_HAS_JOURNAL;
    assert!(!sb.has_journal());
    assert!(Ext4FileSystem::validate_journal_source(&sb).is_ok());
}

#[test]
fn feature_events_are_reported_only_for_unsupported_features() {
    let mut events = Recorder::default();
    let sb = superblock(&image());
    Ext4FileSystem::check_mount_features(&sb, false, &mut events).expect("supported");
    assert!(events.0.is_empty(), "{:?}", events.0);

    let mut unknown = sb;
    unknown.s_feature_incompat |= 0x8000_0000;
    unknown.s_feature_ro_compat |= 0x8000_0000;
    let _ = Ext4FileSystem::check_mount_features(&unknown, false, &mut events);
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Feature(FeatureEvent::UnsupportedIncompat(_))))
    );
    assert!(
        events
            .0
            .iter()
            .any(|event| matches!(event, Event::Feature(FeatureEvent::ReadOnlyCompat(_))))
    );
}

#[test]
fn a_writable_mount_dirties_the_superblock_and_a_read_only_one_does_not() {
    let bytes = image();
    let clean = superblock(&bytes);
    assert_ne!(clean.s_state & Ext4Superblock::EXT4_VALID_FS, 0);

    let read = mount(bytes.clone(), read_only(), true);
    assert!(!read.fs().superblock_dirty);
    assert_eq!(read.fs().superblock.s_state, clean.s_state);
    assert_eq!(read.fs().superblock.s_mnt_count, clean.s_mnt_count);
    assert!(
        !read
            .fs()
            .superblock
            .has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER),
        "a read-only mount never claims the journal"
    );
    assert!(read.bytes() == bytes, "a read-only mount writes nothing");

    let write = mount(bytes, MountOptions::read_write(), true);
    let on_disk = superblock(&write.bytes());
    assert_eq!(on_disk.s_state & Ext4Superblock::EXT4_VALID_FS, 0);
    assert_eq!(on_disk.s_mnt_count, clean.s_mnt_count + 1);
    assert!(on_disk.has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER));
}

#[test]
fn a_clean_mount_reports_no_corruption_and_an_error_state_does() {
    let clean = mount(image(), read_only(), true);
    assert!(
        !has(
            &clean.events,
            Event::Integrity(IntegrityEvent::CorruptionDetected)
        ),
        "{:?}",
        clean.events
    );

    let mut bytes = image();
    edit_superblock(&mut bytes, |sb| sb.s_state |= Ext4Superblock::EXT4_ERROR_FS);
    let errored = mount(bytes, read_only(), true);
    assert!(has(
        &errored.events,
        Event::Integrity(IntegrityEvent::CorruptionDetected)
    ));
}

#[test]
fn a_mount_without_block_validity_keeps_no_system_zones() {
    let clean = mount(
        image(),
        MountOptions::read_write().with_block_validity(false),
        true,
    );
    assert!(clean.fs().system_zones.is_empty());

    // A recovery flag without replay loads the ordinary layout.
    let mut bytes = image();
    edit_superblock(&mut bytes, |sb| {
        sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER;
    });
    let unreplayed = mount(bytes, read_only().with_block_validity(false), true);
    assert!(unreplayed.fs().system_zones.is_empty());
}

#[test]
fn a_device_without_journal_use_loads_no_journal_superblock() {
    let mounted = mount(image(), MountOptions::read_write(), false);
    assert_eq!(mounted.fs().journal_sb_block_start, None);
    let mounted = mount(image(), MountOptions::read_write(), true);
    assert!(mounted.fs().journal_sb_block_start.is_some());
}

#[test]
fn the_lost_and_found_hint_is_adopted_from_the_directory_when_missing() {
    let mut bytes = image();
    edit_superblock(&mut bytes, |sb| sb.s_lpf_ino = 0);
    // Without journal use any superblock write would reach the device.
    let read = mount(bytes.clone(), read_only(), false);
    assert_eq!(read.fs().superblock.s_lpf_ino, 11);
    assert!(has(
        &read.events,
        Event::Repair(RepairEvent::DirectoryIndexFallback)
    ));
    assert!(read.bytes() == bytes, "a read-only mount writes nothing");

    let write = mount(bytes, MountOptions::read_write(), true);
    assert_eq!(superblock(&write.bytes()).s_lpf_ino, 11);
}

#[test]
fn the_lost_and_found_hint_must_name_a_live_directory() {
    let mounted = mount(image(), MountOptions::read_write(), true);
    let mut journal = mounted.journal;
    let mut fs = mounted.result.expect("mount");
    fs.superblock.s_lpf_ino = 11;
    assert!(fs.valid_lost_found_hint(&mut journal).unwrap());
    fs.superblock.s_lpf_ino = JOURNAL_FILE_INODE as u32;
    assert!(!fs.valid_lost_found_hint(&mut journal).unwrap());
}

fn superblock_volume(bytes: &[u8]) -> [u8; 16] {
    superblock(bytes).s_volume_name
}

/// Commits a superblock change to the journal without checkpointing it, so
/// the next replaying mount finds a transaction to recover.
fn pending_superblock_change(edit: impl FnOnce(&mut Ext4Superblock)) -> Vec<u8> {
    let mut journal = wrap(image(), true);
    let fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    journal.read_block(AbsoluteBN::new(1)).expect("read");
    let mut block = journal.buffer().to_vec();
    let mut sb = Ext4Superblock::from_disk_bytes(&block);
    edit(&mut sb);
    sb.update_checksum();
    sb.to_disk_bytes(&mut block);
    journal
        .write_blocks(&block, AbsoluteBN::new(1), 1, true)
        .expect("journal write");
    journal.commit().expect("commit");
    drop(fs);
    journal.into_inner().bytes
}

#[test]
fn replay_applies_a_committed_superblock_even_on_a_device_without_journal_use() {
    let name = *b"replayed-volume\0";
    let bytes = pending_superblock_change(|sb| sb.s_volume_name = name);
    assert_ne!(superblock_volume(&bytes), name, "still only in the journal");
    let mounted = mount(bytes, MountOptions::read_write(), false);
    assert_eq!(mounted.fs().superblock.s_volume_name, name);
    assert!(
        !mounted
            .fs()
            .superblock
            .has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER)
    );
}

#[test]
fn replay_refuses_each_change_of_filesystem_geometry() {
    let edits: [fn(&mut Ext4Superblock); 3] = [
        |sb| sb.s_uuid[0] ^= 1,
        |sb| sb.s_inodes_per_group -= 8,
        |sb| sb.s_desc_size = 128,
    ];
    for edit in edits {
        let mounted = mount(
            pending_superblock_change(edit),
            MountOptions::read_write(),
            true,
        );
        assert_eq!(
            mounted.operation(),
            "journal:geometry_changed_during_replay"
        );
    }
}

#[test]
fn a_recorded_journal_error_marks_the_filesystem_in_error() {
    let mut bytes = image();
    let start = mount(bytes.clone(), read_only(), true)
        .fs()
        .journal_sb_block_start
        .expect("journal superblock")
        .raw() as usize
        * BLOCK;
    let record = &mut bytes[start..start + BLOCK];
    let mut journal_superblock = JournalSuperBlock::decode_checked(record).expect("decode");
    journal_superblock.s_errno = 5;
    crate::checksum::jbd2_update_superblock_checksum(&mut journal_superblock);
    journal_superblock.to_disk_bytes(record);

    let mounted = mount(bytes, MountOptions::read_write(), true);
    let state = mounted.fs().superblock.s_state;
    assert_ne!(state & Ext4Superblock::EXT4_ERROR_FS, 0);
    assert!(has(
        &mounted.events,
        Event::Integrity(IntegrityEvent::CorruptionDetected)
    ));
}

fn inode_record(bytes: &[u8], inode: u32) -> core::ops::Range<usize> {
    let table = mount(bytes.to_vec(), read_only(), true).fs().group_descs[0].inode_table();
    let size = superblock(bytes).s_inode_size as usize;
    let start = table as usize * BLOCK + (inode as usize - 1) * size;
    start..start + size
}

#[test]
fn a_mount_refuses_a_missing_journal_inode() {
    let mut bytes = image();
    let record = inode_record(&bytes, JOURNAL_FILE_INODE as u32);
    bytes[record].fill(0);
    let mounted = mount(bytes, MountOptions::read_write(), true);
    assert_eq!(mounted.operation(), "journal:missing_inode");
}

#[test]
fn a_mount_refuses_an_unlinked_or_encrypted_journal_inode() {
    let unlinked = edit_inode(image(), JOURNAL_FILE_INODE as u32, |inode| {
        inode.i_links_count = 0
    });
    let encrypted = edit_inode(image(), JOURNAL_FILE_INODE as u32, |inode| {
        inode.i_flags |= Ext4Inode::EXT4_ENCRYPT_FL
    });
    for bytes in [unlinked, encrypted] {
        let mounted = mount(bytes, read_only(), true);
        assert_eq!(mounted.operation(), "journal:invalid_inode");
    }
}

#[test]
fn a_read_only_mount_refuses_a_root_that_is_not_a_directory() {
    let bytes = edit_inode(image(), 2, |inode| {
        inode.i_mode = Ext4Inode::S_IFREG | 0o644;
    });
    assert!(mount(bytes, read_only(), true).result.is_err());
}

#[test]
fn a_valid_lost_and_found_hint_needs_no_repair() {
    let mounted = mount(image(), read_only(), true);
    assert!(
        !has(
            &mounted.events,
            Event::Repair(RepairEvent::DirectoryIndexFallback)
        ),
        "{:?}",
        mounted.events
    );
}

#[test]
fn a_lost_and_found_that_is_not_a_directory_is_refused() {
    let mut bytes = edit_inode(image(), 11, |inode| {
        inode.i_mode = Ext4Inode::S_IFREG | 0o700;
    });
    edit_superblock(&mut bytes, |sb| sb.s_lpf_ino = 0);
    assert!(mount(bytes, read_only(), true).result.is_err());
}

#[test]
fn a_deleted_lost_and_found_is_recreated_only_by_a_writable_mount() {
    let mut journal = wrap(image(), true);
    let mut fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    // The walk below follows whatever entries the directory blocks hold, and
    // on damaged blocks it need not end, so first check that lost+found reads
    // as a sound directory.
    assert!(matches!(
        get_file_inode(&mut fs, &mut journal, "/lost+found/lost+found"),
        Ok(None)
    ));
    crate::file::delete_dir(&mut fs, &mut journal, "/lost+found").expect("delete");
    fs.umount(&mut journal).expect("umount");
    let mut bytes = journal.into_inner().bytes;
    // The freed inode keeps its mode, so drop the hint as well.
    edit_superblock(&mut bytes, |sb| sb.s_lpf_ino = 0);

    let read = mount(bytes.clone(), read_only(), true);
    assert!(!has(
        &read.events,
        Event::Repair(RepairEvent::LostFoundRecreated)
    ));
    assert!(read.bytes() == bytes, "a read-only mount writes nothing");

    let write = mount(bytes, MountOptions::read_write(), true);
    assert!(has(
        &write.events,
        Event::Repair(RepairEvent::LostFoundRecreated)
    ));
}

#[test]
fn a_mount_checks_both_bitmaps_of_the_first_group() {
    let bytes = image();
    let descriptor = mount(bytes.clone(), read_only(), true).fs().group_descs[0];
    for (bitmap, byte) in [
        (descriptor.inode_bitmap(), 40),
        (descriptor.block_bitmap(), 900),
    ] {
        let mut damaged = bytes.clone();
        damaged[bitmap as usize * BLOCK + byte] ^= 0x10;
        let mounted = mount(damaged, read_only(), true);
        assert!(mounted.result.is_err());
        assert!(has(
            &mounted.events,
            Event::Integrity(IntegrityEvent::ChecksumMismatch)
        ));
    }
}

#[test]
fn reloading_after_replay_dirties_only_a_writable_mount() {
    let mut bytes = image();
    // Spare device capacity beyond the filesystem is allowed.
    bytes.extend(core::iter::repeat_n(0, 16 * BLOCK));
    let mounted = mount(bytes, read_only(), true);
    let mut journal = mounted.journal;
    let mut fs = mounted.result.expect("mount");
    let clean = fs.superblock;

    fs.reload_after_journal_replay(&mut journal, true, true)
        .expect("read-only reload");
    assert_eq!(fs.superblock.s_state, clean.s_state);
    assert_eq!(fs.superblock.s_mnt_count, clean.s_mnt_count);
    assert!(!fs.superblock_dirty);

    fs.reload_after_journal_replay(&mut journal, false, true)
        .expect("writable reload");
    assert_eq!(
        fs.superblock.s_state,
        clean.s_state & !Ext4Superblock::EXT4_VALID_FS
    );
    assert_eq!(fs.superblock.s_mnt_count, clean.s_mnt_count + 1);
    assert!(fs.superblock_dirty);
}

#[test]
fn clearing_recovery_state_clears_only_the_recovery_flag() {
    let mounted = mount(image(), MountOptions::read_write(), true);
    let mut fs = mounted.result.expect("mount");
    let incompat = fs.superblock.s_feature_incompat;
    assert_ne!(incompat & Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER, 0);
    fs.superblock_dirty = false;
    fs.clear_recovery_state();
    assert_eq!(
        fs.superblock.s_feature_incompat,
        incompat & !Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER
    );
    assert!(fs.superblock_dirty);
}

#[test]
fn remounting_read_only_marks_the_filesystem_clean_and_back_reopens_it() {
    for error in [0, Ext4Superblock::EXT4_ERROR_FS] {
        let mounted = mount(image(), MountOptions::read_write(), true);
        let mut journal = mounted.journal;
        let mut fs = mounted.result.expect("mount");
        fs.superblock.s_state = error;
        fs.remount_read_only(&mut journal).expect("read only");
        assert_eq!(fs.superblock.s_state, error | Ext4Superblock::EXT4_VALID_FS);
        assert!(
            !fs.superblock
                .has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER)
        );

        fs.remount_read_write(&mut journal, &mut Recorder::default())
            .expect("read write");
        assert_eq!(fs.superblock.s_state & Ext4Superblock::EXT4_VALID_FS, 0);
        assert!(
            fs.superblock
                .has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER)
        );
    }
}

fn layout(
    bytes: Vec<u8>,
) -> (
    Ext4Result<Vec<Ext4GroupDesc>>,
    Ext4Result<Vec<Ext4GroupDesc>>,
) {
    let sb = superblock(&bytes);
    let groups = sb.checked_block_groups_count().unwrap();
    let mut journal = wrap(bytes, true);
    let bootstrap = Ext4FileSystem::load_journal_bootstrap_layout(&mut journal, &sb, groups)
        .map(|(descriptors, _)| descriptors);
    let ordinary = Ext4FileSystem::load_group_descriptors(&mut journal, &sb, groups);
    (bootstrap, ordinary)
}

fn table(descriptors: &[Ext4GroupDesc]) -> Vec<(u64, u64, u64, u16)> {
    descriptors
        .iter()
        .map(|d| {
            (
                d.block_bitmap(),
                d.inode_bitmap(),
                d.inode_table(),
                d.bg_flags,
            )
        })
        .collect()
}

#[test]
fn the_recovery_layout_matches_the_ordinary_layout_on_a_clean_image() {
    let (bootstrap, ordinary) = layout(image());
    let ordinary = ordinary.expect("ordinary");
    assert_eq!(ordinary.len(), 4);
    assert_eq!(table(&bootstrap.expect("bootstrap")), table(&ordinary));
}

#[test]
fn the_recovery_layout_drops_a_damaged_descriptor_outside_the_journal_group() {
    let mut bytes = image();
    bytes[DESCRIPTORS + 3 * DESCRIPTOR + 0x1E] ^= 0xFF;
    let (bootstrap, ordinary) = layout(bytes);
    assert!(ordinary.is_err());
    let bootstrap = bootstrap.expect("bootstrap");
    assert_eq!(table(&bootstrap[3..]), table(&[Ext4GroupDesc::default()]));
    assert_ne!(table(&bootstrap[2..3]), table(&[Ext4GroupDesc::default()]));
}

#[test]
fn the_recovery_layout_refuses_a_damaged_journal_group_descriptor() {
    let mut bytes = image();
    bytes[DESCRIPTORS + 0x1E] ^= 0xFF;
    let sb = superblock(&bytes);
    let mut journal = wrap(bytes, true);
    let Err(error) = Ext4FileSystem::load_journal_bootstrap_layout(&mut journal, &sb, 4) else {
        panic!("a damaged journal group must be refused");
    };
    assert!(matches!(
        error.context(),
        Some(ErrorContext::Operation {
            op: "journal:group_descriptor_checksum"
        })
    ));
}

#[test]
fn only_the_first_group_must_have_initialized_bitmaps() {
    let mut bytes = image();
    edit_descriptor(&mut bytes, 1, |d| {
        d.bg_flags |= Ext4GroupDesc::EXT4_BG_BLOCK_UNINIT
    });
    assert!(layout(bytes).1.is_ok());

    for flag in [
        Ext4GroupDesc::EXT4_BG_BLOCK_UNINIT,
        Ext4GroupDesc::EXT4_BG_INODE_UNINIT,
    ] {
        let mut bytes = image();
        edit_descriptor(&mut bytes, 0, |d| d.bg_flags |= flag);
        assert!(layout(bytes).1.is_err());
    }
}
