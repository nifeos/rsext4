//! Tests that pin how a stored extended attribute is placed, sized and accounted for.

use super::*;
use crate::error::ErrorContext;

fn operation_of(error: Ext4Error) -> Option<&'static str> {
    match error.context() {
        Some(ErrorContext::Operation { op }) => Some(op),
        _ => None,
    }
}

fn fixture() -> (Jbd2Dev<FailingMemoryDevice>, Ext4FileSystem, InodeNumber) {
    let (journal, filesystem, inode_number, _) = failing_fixture();
    (journal, filesystem, inode_number)
}

fn failing_fixture() -> (
    Jbd2Dev<FailingMemoryDevice>,
    Ext4FileSystem,
    InodeNumber,
    Rc<Cell<Option<u64>>>,
) {
    let (device, fail_write_sector) = FailingMemoryDevice::new(32 * 1024);
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs(&mut journal).expect("mkfs must succeed");
    let mut filesystem = Ext4FileSystem::mount(&mut journal).expect("mount must succeed");
    mkfile(&mut journal, &mut filesystem, "/victim", None, None).expect("create file");
    let inode_number = dir::get_inode_with_num(&mut filesystem, &mut journal, "/victim")
        .expect("lookup victim")
        .expect("victim must exist")
        .0;
    (journal, filesystem, inode_number, fail_write_sector)
}

fn set(
    journal: &mut Jbd2Dev<FailingMemoryDevice>,
    filesystem: &mut Ext4FileSystem,
    inode_number: InodeNumber,
    name: &[u8],
    value: &[u8],
) -> Ext4Result<()> {
    set_inode_xattr(
        journal,
        filesystem,
        inode_number,
        XattrNamespace::User,
        name,
        value,
        XattrSetMode::Upsert,
    )
}

fn external_block(
    journal: &mut Jbd2Dev<FailingMemoryDevice>,
    filesystem: &mut Ext4FileSystem,
    inode_number: InodeNumber,
) -> Option<ExternalStore> {
    let (inode, _) = filesystem
        .get_inode_record(journal, inode_number)
        .expect("read inode");
    read_external_store(journal, filesystem, &inode).expect("read external store")
}

#[test]
fn a_value_of_the_size_limit_is_too_big_for_the_inode_but_a_larger_one_is_invalid() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    let limit = vec![0u8; XATTR_SIZE_MAX as usize];
    let error = set(&mut journal, &mut filesystem, inode_number, b"big", &limit)
        .expect_err("the limit does not fit");
    assert_eq!(error.kind(), Ext4ErrorKind::NoSpace);
    let over = vec![0u8; XATTR_SIZE_MAX as usize + 1];
    let error = set(&mut journal, &mut filesystem, inode_number, b"big", &over)
        .expect_err("one past the limit");
    assert_eq!(error.kind(), Ext4ErrorKind::InvalidInput);
}

#[test]
fn an_inline_store_that_cannot_be_encoded_is_not_moved_to_a_block() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    set(&mut journal, &mut filesystem, inode_number, b"a", b"x").expect("first xattr");
    let inline_offset = usize::from(Ext4Inode::GOOD_OLD_INODE_SIZE)
        + usize::from(
            filesystem
                .get_inode_record(&mut journal, inode_number)
                .expect("read inode")
                .0
                .i_extra_isize,
        );
    // Turn the stored value into a reference to an inode that holds the value.
    filesystem
        .modify_inode_record(&mut journal, inode_number, |_, raw_inode| {
            let entry = inline_offset + XATTR_IBODY_HEADER_SIZE;
            raw_inode[entry + 4..entry + 8].copy_from_slice(&20u32.to_le_bytes());
            Ok(())
        })
        .expect("rewrite the entry");
    filesystem.superblock.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_EA_INODE;

    let error = set(&mut journal, &mut filesystem, inode_number, b"b", b"y")
        .expect_err("an inode-held value cannot be rewritten");
    assert_eq!(error.kind(), Ext4ErrorKind::UnsupportedFeature);
    assert!(external_block(&mut journal, &mut filesystem, inode_number).is_none());
}

#[test]
fn an_exclusive_external_block_is_rewritten_in_place() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    set(
        &mut journal,
        &mut filesystem,
        inode_number,
        b"v",
        &[1u8; 2_000],
    )
    .expect("first value");
    let first = external_block(&mut journal, &mut filesystem, inode_number)
        .expect("external store")
        .block;
    let free_blocks = filesystem.statfs().free_blocks;

    set(
        &mut journal,
        &mut filesystem,
        inode_number,
        b"v",
        &[2u8; 2_000],
    )
    .expect("second value");

    let store = external_block(&mut journal, &mut filesystem, inode_number).expect("store");
    assert_eq!(store.block, first);
    assert_eq!(filesystem.statfs().free_blocks, free_blocks);
}

#[test]
fn a_store_shared_by_the_most_inodes_is_still_copied_on_write() {
    let (device, _) = FailingMemoryDevice::new(32 * 1024);
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs(&mut journal).expect("mkfs must succeed");
    let mut filesystem = Ext4FileSystem::mount(&mut journal).expect("mount must succeed");
    let (first, _, shared_block) = create_shared_external_xattr(&mut journal, &mut filesystem);
    let (inode, _) = filesystem
        .get_inode_record(&mut journal, first)
        .expect("read inode");
    let mut image = read_external_store(&mut journal, &filesystem, &inode)
        .expect("read store")
        .expect("external store")
        .raw;
    image[4..8].copy_from_slice(&XATTR_REFCOUNT_MAX.to_le_bytes());
    set_external_block_checksum(&filesystem.superblock, shared_block, &mut image)
        .expect("checksum the block");
    filesystem
        .with_metadata_transaction(&mut journal, 1, |_, journal| {
            journal.write_blocks(&image, shared_block, 1, true)
        })
        .expect("publish the count");

    set(&mut journal, &mut filesystem, first, b"shared", &[3u8; 512]).expect("copy on write");

    let store = external_block(&mut journal, &mut filesystem, first).expect("own store");
    assert_ne!(store.block, shared_block);
    let (inode, _) = filesystem
        .get_inode_record(&mut journal, first)
        .expect("read inode");
    assert_eq!(inode.file_acl(), store.block.raw());
}

#[test]
fn an_external_block_counts_its_sectors_once_and_gives_them_back() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    let block_size = filesystem.block_size() as u32;
    let huge_file = filesystem
        .superblock
        .has_feature_ro_compat(Ext4Superblock::EXT4_FEATURE_RO_COMPAT_HUGE_FILE);
    let sectors = |filesystem: &mut Ext4FileSystem, journal: &mut Jbd2Dev<FailingMemoryDevice>| {
        filesystem
            .get_inode_record(journal, inode_number)
            .expect("read inode")
            .0
            .blocks_count(block_size, huge_file)
    };
    let before = sectors(&mut filesystem, &mut journal);

    set(
        &mut journal,
        &mut filesystem,
        inode_number,
        b"v",
        &[1u8; 2_000],
    )
    .expect("set");
    assert_eq!(
        sectors(&mut filesystem, &mut journal),
        before + u64::from(block_size / 512)
    );

    remove_inode_xattr(
        &mut journal,
        &mut filesystem,
        inode_number,
        XattrNamespace::User,
        b"v",
    )
    .expect("remove");
    assert_eq!(sectors(&mut filesystem, &mut journal), before);
}

#[test]
fn an_inode_whose_extra_size_passes_the_record_cannot_have_xattrs_rewritten() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    set(
        &mut journal,
        &mut filesystem,
        inode_number,
        b"v",
        &[1u8; 2_000],
    )
    .expect("set");
    filesystem
        .modify_inode(&mut journal, inode_number, |inode| {
            inode.i_extra_isize = 4_000
        })
        .expect("grow the extra size");

    let error = remove_inode_xattr(
        &mut journal,
        &mut filesystem,
        inode_number,
        XattrNamespace::User,
        b"v",
    )
    .expect_err("the inline area is outside the record");
    assert_eq!(operation_of(error), Some("xattr:inode_bounds"));
}

#[test]
fn a_block_number_outside_the_filesystem_or_the_device_is_refused() {
    let (mut journal, mut filesystem, _) = fixture();
    let mut inode = Ext4Inode::default();

    // Past the filesystem, inside the device.
    filesystem.superblock.s_blocks_count_lo = 100;
    filesystem.superblock.s_blocks_count_hi = 0;
    inode.set_file_acl(200).expect("set the block");
    let error = read_external_store(&mut journal, &filesystem, &inode)
        .err()
        .expect("past the filesystem");
    assert_eq!(operation_of(error), Some("xattr:block_number"));

    // Inside the filesystem, past the device.
    filesystem.superblock.s_blocks_count_lo = u32::MAX;
    inode
        .set_file_acl(journal.total_blocks() + 1)
        .expect("set the block");
    let error = read_external_store(&mut journal, &filesystem, &inode)
        .err()
        .expect("past the device");
    assert_eq!(operation_of(error), Some("xattr:block_number"));
}

#[test]
fn inline_header_bounds_are_exact() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    let (inode, raw_inode) = filesystem
        .get_inode_record(&mut journal, inode_number)
        .expect("read inode");
    let offset = usize::from(Ext4Inode::GOOD_OLD_INODE_SIZE) + usize::from(inode.i_extra_isize);
    let mut raw = raw_inode[..offset + XATTR_IBODY_HEADER_SIZE + XATTR_TERMINATOR_SIZE].to_vec();
    raw[offset..].fill(0);
    raw[offset..offset + 4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());

    let exact = parse_inline_store(&filesystem, &inode, &raw).expect("exact fit");
    assert_eq!(exact.expect("an empty store").len(), 0);
    let short = &raw[..raw.len() - 1];
    assert!(
        parse_inline_store(&filesystem, &inode, short)
            .expect("short")
            .is_none()
    );
    let header_only = &raw[..offset + XATTR_IBODY_HEADER_SIZE];
    assert!(
        parse_inline_store(&filesystem, &inode, header_only)
            .expect("header")
            .is_none()
    );
}

#[test]
fn the_superblock_is_written_only_when_the_feature_flag_was_missing() {
    for missing in [false, true] {
        let (mut journal, mut filesystem, inode_number, fail_write_sector) = failing_fixture();
        set(&mut journal, &mut filesystem, inode_number, b"a", b"x").expect("first xattr");
        filesystem
            .sync_filesystem(&mut journal)
            .expect("sync baseline");
        journal.umount_commit().expect("commit baseline");
        journal
            .set_journal_use(false)
            .expect("direct metadata writes");
        if missing {
            filesystem.superblock.s_feature_compat &= !Ext4Superblock::EXT4_FEATURE_COMPAT_EXT_ATTR;
        }
        fail_write_sector.set(Some(0));

        let outcome = set(&mut journal, &mut filesystem, inode_number, b"b", b"y");

        assert_eq!(outcome.is_err(), missing, "feature missing: {missing}");
    }
}

#[test]
fn removing_the_last_external_value_publishes_the_block_bitmap() {
    let (mut journal, mut filesystem, inode_number, fail_write_sector) = failing_fixture();
    set(
        &mut journal,
        &mut filesystem,
        inode_number,
        b"v",
        &[1u8; 2_000],
    )
    .expect("set");
    filesystem
        .sync_filesystem(&mut journal)
        .expect("sync baseline");
    journal.umount_commit().expect("commit baseline");
    let block = external_block(&mut journal, &mut filesystem, inode_number)
        .expect("external store")
        .block;
    let (group, _) = filesystem
        .block_allocator
        .global_to_group(block)
        .expect("locate the group");
    let block_bitmap = filesystem
        .get_group_desc(group)
        .expect("group descriptor")
        .block_bitmap();
    journal
        .set_journal_use(false)
        .expect("direct metadata writes");
    fail_write_sector.set(Some(block_bitmap));

    let error = remove_inode_xattr(
        &mut journal,
        &mut filesystem,
        inode_number,
        XattrNamespace::User,
        b"v",
    )
    .expect_err("the bitmap write fails");
    assert_eq!(error.kind(), Ext4ErrorKind::Io);
}

#[test]
fn create_refuses_an_existing_name_and_replace_refuses_a_missing_one() {
    let (mut journal, mut filesystem, inode_number) = fixture();
    let mode_set = |journal: &mut Jbd2Dev<FailingMemoryDevice>,
                    filesystem: &mut Ext4FileSystem,
                    mode: XattrSetMode| {
        set_inode_xattr(
            journal,
            filesystem,
            inode_number,
            XattrNamespace::User,
            b"a",
            b"x",
            mode,
        )
    };
    let error = mode_set(&mut journal, &mut filesystem, XattrSetMode::Replace)
        .expect_err("replace needs an existing name");
    assert_eq!(error.kind(), Ext4ErrorKind::NotFound);
    assert_eq!(operation_of(error), Some("xattr:replace"));
    mode_set(&mut journal, &mut filesystem, XattrSetMode::Create).expect("create a new name");
    let error = mode_set(&mut journal, &mut filesystem, XattrSetMode::Create)
        .expect_err("create needs a missing name");
    assert_eq!(error.kind(), Ext4ErrorKind::AlreadyExists);
    assert_eq!(operation_of(error), Some("xattr:create"));
    mode_set(&mut journal, &mut filesystem, XattrSetMode::Replace).expect("replace existing");
}
