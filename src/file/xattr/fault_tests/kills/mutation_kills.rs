//! Tests that pin the arithmetic, bounds and policy of the extended-attribute codec.

use super::*;
use crate::error::ErrorContext;

fn operation_of(error: Ext4Error) -> Option<&'static str> {
    match error.context() {
        Some(ErrorContext::Operation { op }) => Some(op),
        _ => None,
    }
}

fn stored(name: &[u8], value: &[u8]) -> StoredXattr {
    StoredXattr {
        name: XattrName {
            namespace: XattrNamespace::User,
            name: name.to_vec(),
        },
        value: StoredXattrValue::Local(value.to_vec()),
    }
}

fn ea_inode_superblock() -> Ext4Superblock {
    Ext4Superblock {
        s_first_ino: 11,
        s_inodes_count: 128,
        s_feature_incompat: Ext4Superblock::EXT4_FEATURE_INCOMPAT_EA_INODE,
        ..Ext4Superblock::default()
    }
}

#[test]
fn every_namespace_keeps_its_disk_index() {
    let namespaces = [
        (1, XattrNamespace::User),
        (2, XattrNamespace::PosixAclAccess),
        (3, XattrNamespace::PosixAclDefault),
        (4, XattrNamespace::Trusted),
        (5, XattrNamespace::Lustre),
        (6, XattrNamespace::Security),
        (7, XattrNamespace::System),
        (8, XattrNamespace::RichAcl),
        (9, XattrNamespace::Encryption),
        (10, XattrNamespace::Hurd),
        (200, XattrNamespace::Other(200)),
    ];
    for (index, namespace) in namespaces {
        assert_eq!(namespace.disk_index(), index);
        assert_eq!(XattrNamespace::from_disk_index(index), namespace);
    }
}

#[test]
fn names_longer_than_a_byte_or_holding_a_zero_are_refused() {
    assert!(validate_name(&[b'a'; 255]).is_ok());
    assert!(validate_name(b"").is_ok());
    assert_eq!(
        validate_name(&[b'a'; 256]).expect_err("256 bytes").kind(),
        Ext4ErrorKind::InvalidInput
    );
    assert_eq!(
        validate_name(b"a\0b").expect_err("zero byte").kind(),
        Ext4ErrorKind::InvalidInput
    );
}

#[test]
fn entry_hash_mixes_name_and_value_words() {
    assert_eq!(xattr_entry_hash(b"", b""), 0);
    assert_eq!(xattr_entry_hash(b"ab", b""), 0xc42);
    assert_eq!(
        xattr_entry_hash(b"user", &[1, 2, 3, 4, 5, 6, 7, 8]),
        0x0a3d_42d4
    );
    assert_eq!(
        xattr_entry_hash(b"abc", &[1, 2, 3, 4, 5, 6, 7, 8]),
        0x0a07_8a25
    );
}

#[test]
fn encoding_needs_room_for_the_header_and_the_terminator() {
    let mut none: [StoredXattr; 0] = [];
    assert_eq!(
        encode_xattrs(&mut none, 8, 4, 4, false)
            .expect("exact fit")
            .len(),
        8
    );
    assert_eq!(
        encode_xattrs(&mut none, 36, 32, 0, true)
            .expect("exact block fit")
            .len(),
        36
    );
    for storage_size in [5, 7] {
        let error = encode_xattrs(&mut none, storage_size, 4, 4, false).expect_err("too small");
        assert_eq!(error.kind(), Ext4ErrorKind::NoSpace);
        assert_eq!(operation_of(error), Some("xattr:encode"));
    }
}

#[test]
fn encoding_names_fit_exactly_or_are_refused() {
    // One entry named `a`: 4 header bytes, 20 entry bytes, 4 terminator bytes.
    let mut entries = [stored(b"a", b"")];
    assert_eq!(
        encode_xattrs(&mut entries, 28, 4, 4, false)
            .expect("exact fit")
            .len(),
        28
    );
    let error = encode_xattrs(&mut entries, 27, 4, 4, false).expect_err("one byte short");
    assert_eq!(operation_of(error), Some("xattr:encode_names"));
}

#[test]
fn encoding_values_fit_exactly_or_are_refused() {
    let mut entries = [stored(b"a", b"data")];
    assert_eq!(
        encode_xattrs(&mut entries, 32, 4, 4, false)
            .expect("exact fit")
            .len(),
        32
    );
    let error = encode_xattrs(&mut entries, 31, 4, 4, false).expect_err("value overlaps names");
    assert_eq!(operation_of(error), Some("xattr:encode_value"));
}

#[test]
fn block_hash_chains_the_entry_hashes() {
    let mut entries = [
        stored(b"user", &[1, 2, 3, 4, 5, 6, 7, 8]),
        stored(b"ab", b""),
    ];
    let image = encode_xattrs(&mut entries, 4096, 32, 0, true).expect("encode block");
    assert_eq!(read_u32_le(&image[12..16]), 0x067f_42d4);
}

#[test]
fn block_hash_is_zero_when_an_entry_hash_is_zero() {
    let mut entries = [stored(b"ab", b""), stored(b"", b"")];
    let image = encode_xattrs(&mut entries, 4096, 32, 0, true).expect("encode block");
    assert_eq!(read_u32_le(&image[12..16]), 0);
}

fn checksum_superblock() -> Ext4Superblock {
    Ext4Superblock {
        s_feature_ro_compat: Ext4Superblock::EXT4_FEATURE_RO_COMPAT_METADATA_CSUM,
        ..Ext4Superblock::default()
    }
}

fn valid_header(length: usize, refcount: u32) -> Vec<u8> {
    let mut block = vec![0u8; length];
    block[0..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
    block[4..8].copy_from_slice(&refcount.to_le_bytes());
    block[8..12].copy_from_slice(&1u32.to_le_bytes());
    block
}

#[test]
fn block_checksum_covers_a_block_with_only_a_header() {
    let mut block = valid_header(XATTR_BLOCK_HEADER_SIZE, 1);
    set_external_block_checksum(&checksum_superblock(), AbsoluteBN::new(9), &mut block)
        .expect("a header-sized block is checksummed");
    assert_ne!(read_u32_le(&block[16..20]), 0);

    let mut short = vec![0u8; XATTR_BLOCK_HEADER_SIZE - 1];
    let error = set_external_block_checksum(&checksum_superblock(), AbsoluteBN::new(9), &mut short)
        .expect_err("a block shorter than its header");
    assert_eq!(error.kind(), Ext4ErrorKind::Corrupted);
}

#[test]
fn block_checksum_round_trips_through_validation() {
    let superblock = checksum_superblock();
    let mut block = valid_header(4096, 1);
    block[100] = 7;
    set_external_block_checksum(&superblock, AbsoluteBN::new(9), &mut block).expect("checksum");
    validate_external_header(&superblock, AbsoluteBN::new(9), &block).expect("matches");
    block[200] ^= 1;
    let error = validate_external_header(&superblock, AbsoluteBN::new(9), &block)
        .expect_err("a changed byte is caught");
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
}

#[test]
fn external_header_accepts_the_smallest_valid_block_and_the_largest_refcount() {
    let superblock = Ext4Superblock::default();
    let smallest = XATTR_BLOCK_HEADER_SIZE + XATTR_TERMINATOR_SIZE;
    validate_external_header(&superblock, AbsoluteBN::new(1), &valid_header(smallest, 1))
        .expect("smallest block");
    validate_external_header(
        &superblock,
        AbsoluteBN::new(1),
        &valid_header(smallest, XATTR_REFCOUNT_MAX),
    )
    .expect("largest refcount");
    let error = validate_external_header(
        &superblock,
        AbsoluteBN::new(1),
        &valid_header(smallest - 1, 1),
    )
    .expect_err("one byte short");
    assert_eq!(error.kind(), Ext4ErrorKind::Corrupted);
}

#[test]
fn external_header_refuses_each_bad_field_alone() {
    let superblock = Ext4Superblock::default();
    let smallest = XATTR_BLOCK_HEADER_SIZE + XATTR_TERMINATOR_SIZE;
    let mut bad_magic = valid_header(smallest, 1);
    bad_magic[0] ^= 1;
    let mut bad_count = valid_header(smallest, 1);
    bad_count[8..12].copy_from_slice(&2u32.to_le_bytes());
    for block in [bad_magic, bad_count] {
        let error = validate_external_header(&superblock, AbsoluteBN::new(1), &block)
            .expect_err("a bad header field");
        assert_eq!(operation_of(error), Some("xattr:block_header"));
    }
}

fn entry_storage(value_offset: u16, value_inode: u32, value_size: u32, length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length];
    bytes[..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
    bytes[4] = 3;
    bytes[5] = XattrNamespace::User.disk_index();
    bytes[6..8].copy_from_slice(&value_offset.to_le_bytes());
    bytes[8..12].copy_from_slice(&value_inode.to_le_bytes());
    bytes[12..16].copy_from_slice(&value_size.to_le_bytes());
    bytes[20..23].copy_from_slice(b"key");
    bytes
}

fn parse_with(
    superblock: &Ext4Superblock,
    root: u32,
    storage: &[u8],
) -> Ext4Result<Vec<StoredXattr>> {
    parse_xattrs(
        superblock,
        InodeNumber::new(root).expect("root inode"),
        storage,
        XATTR_IBODY_HEADER_SIZE,
        XATTR_IBODY_HEADER_SIZE,
    )
}

#[test]
fn value_may_start_right_after_the_names() {
    // The entry ends at 24 and the terminator at 28; value base 4 puts offset 24 at 28.
    let mut storage = entry_storage(24, 0, 4, 64);
    storage[28..32].copy_from_slice(b"data");
    let entries = parse_with(&Ext4Superblock::default(), 2, &storage).expect("parse");
    assert_eq!(entries[0].value, StoredXattrValue::Local(b"data".to_vec()));
}

#[test]
fn value_padding_must_stay_inside_the_storage() {
    // One byte at the last position of the storage, whose padding would pass the end.
    let storage = entry_storage(59, 0, 1, 64);
    let error =
        parse_with(&Ext4Superblock::default(), 2, &storage).expect_err("padding passes end");
    assert_eq!(operation_of(error), Some("xattr:value_bounds"));
}

#[test]
fn value_inode_entries_accept_the_edges_of_the_valid_range() {
    let superblock = ea_inode_superblock();
    for (inode, size) in [(11, 1), (20, 1), (128, 1), (20, 1 << 24)] {
        let storage = entry_storage(0, inode, size, 64);
        let entries = parse_with(&superblock, 2, &storage).expect("valid value inode");
        assert_eq!(entries[0].value, StoredXattrValue::EaInode { inode, size });
    }
}

#[test]
fn value_inode_entries_are_refused_for_each_bad_condition_alone() {
    let mut without_feature = ea_inode_superblock();
    without_feature.s_feature_incompat = 0;
    let cases = [
        ("feature missing", without_feature, 2, 20, 1),
        ("the root inode", ea_inode_superblock(), 20, 20, 1),
        ("before the first inode", ea_inode_superblock(), 2, 5, 1),
        ("past the inode count", ea_inode_superblock(), 2, 129, 1),
        ("zero size", ea_inode_superblock(), 2, 20, 0),
    ];
    for (label, superblock, root, inode, size) in cases {
        let storage = entry_storage(0, inode, size, 64);
        let error = parse_with(&superblock, root, &storage).expect_err(label);
        assert_eq!(operation_of(error), Some("xattr:value_inode"), "{label}");
    }
    let storage = entry_storage(0, 20, (1 << 24) + 1, 64);
    let error = parse_with(&ea_inode_superblock(), 2, &storage).expect_err("size past the limit");
    assert_eq!(operation_of(error), Some("xattr:value_size"));
}

#[test]
fn shared_store_count_is_the_revoke_count() {
    let store = |refcount| ExternalStore {
        block: AbsoluteBN::new(1),
        refcount,
        entries: Vec::new(),
        raw: Vec::new(),
    };
    assert_eq!(external_store_revoke_records(None), 0);
    assert_eq!(external_store_revoke_records(Some(&store(2))), 0);
    assert_eq!(external_store_revoke_records(Some(&store(1))), 1);
}

#[test]
fn the_smallest_block_still_has_its_checksum_checked() {
    let superblock = checksum_superblock();
    let smallest = XATTR_BLOCK_HEADER_SIZE + XATTR_TERMINATOR_SIZE;
    let mut block = valid_header(smallest, 1);
    set_external_block_checksum(&superblock, AbsoluteBN::new(9), &mut block).expect("checksum");
    validate_external_header(&superblock, AbsoluteBN::new(9), &block).expect("matches");
    block[smallest - 1] ^= 1;
    let error = validate_external_header(&superblock, AbsoluteBN::new(9), &block)
        .expect_err("a changed byte is caught");
    assert_eq!(error.kind(), Ext4ErrorKind::ChecksumMismatch);
}

#[test]
fn the_terminator_may_end_exactly_at_the_end_of_the_storage() {
    // The entry ends at 24 and the terminator fills 24..28.
    let storage = entry_storage(0, 0, 0, 28);
    let entries = parse_with(&Ext4Superblock::default(), 2, &storage).expect("exact fit");
    assert_eq!(entries.len(), 1);
    let storage = entry_storage(0, 0, 0, 27);
    let error = parse_with(&Ext4Superblock::default(), 2, &storage).expect_err("one byte short");
    assert_eq!(operation_of(error), Some("xattr:name"));
}

#[test]
fn several_entries_are_laid_out_back_to_back_and_read_back() {
    let make = |name: &[u8], value: &[u8]| StoredXattr {
        name: XattrName {
            namespace: XattrNamespace::User,
            name: name.to_vec(),
        },
        value: StoredXattrValue::Local(value.to_vec()),
    };
    let mut entries = vec![
        make(b"a", b"1"),
        make(b"bcdef", b"22"),
        make(b"ghijklmn", b"333"),
    ];
    // The header takes 4 bytes, the entries 20, 24 and 24, the terminator 4, the values 12.
    let storage_size = 4 + 20 + 24 + 24 + 4 + 12;
    let image = encode_xattrs(
        &mut entries,
        storage_size,
        XATTR_IBODY_HEADER_SIZE,
        XATTR_IBODY_HEADER_SIZE,
        false,
    )
    .expect("the entries fit exactly");
    assert_eq!(image.len(), storage_size);
    assert_eq!(image[4], 1);
    assert_eq!(image[24], 5);
    assert_eq!(image[48], 8);
    assert_eq!(&image[72..76], &[0, 0, 0, 0]);
    let read = parse_with(&Ext4Superblock::default(), 2, &image).expect("read back");
    assert_eq!(read.len(), 3);
    assert_eq!(read[0].value, StoredXattrValue::Local(b"1".to_vec()));
    assert_eq!(read[1].name.name, b"bcdef");
    assert_eq!(read[1].value, StoredXattrValue::Local(b"22".to_vec()));
    assert_eq!(read[2].name.name, b"ghijklmn");
    assert_eq!(read[2].value, StoredXattrValue::Local(b"333".to_vec()));
}
