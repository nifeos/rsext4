//! Tests that pin the boundaries of the pure planning and encoding helpers.

use super::*;
use crate::error::ErrorContext;

const SEED: [u32; 4] = [0x1234_5678, 0x8765_4321, 0xabcd_ef00, 0x00fe_dcba];

fn operation_of(error: &Ext4Error) -> Option<&'static str> {
    match error.context() {
        Some(ErrorContext::Operation { op }) => Some(op),
        _ => None,
    }
}

fn operation<T>(result: Ext4Result<T>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    operation_of(&error).expect("the error names its operation")
}

fn dx(hash: u32, block: u32) -> Ext4DxEntry {
    Ext4DxEntry { hash, block }
}

fn leaf_entry(inode: u32, name: &[u8], source_record_len: usize) -> LeafEntry {
    LeafEntry {
        hash: inode,
        inode: InodeNumber::new(inode).expect("non-zero inode"),
        file_type: Ext4DirEntry2::EXT4_FT_REG_FILE,
        name: name.to_vec(),
        source_record_len,
    }
}

/// Writes one raw record header and name at `offset`.
fn raw_record(data: &mut [u8], offset: usize, inode: u32, record_len: u16, name: &[u8]) {
    data[offset..offset + 4].copy_from_slice(&inode.to_le_bytes());
    data[offset + 4..offset + 6].copy_from_slice(&record_len.to_le_bytes());
    data[offset + 6] = name.len() as u8;
    data[offset + 7] = Ext4DirEntry2::EXT4_FT_REG_FILE;
    data[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
}

/// A 4096-byte leaf whose records are `(inode, name, record length)` and whose last record is
/// stretched to the end of the entry area.
fn leaf(records: &[(u32, &[u8], u16)], has_checksum: bool) -> Vec<u8> {
    let mut data = vec![0u8; 4096];
    let entries_end = if has_checksum { 4096 - 12 } else { 4096 };
    let mut offset = 0usize;
    for (index, (inode, name, record_len)) in records.iter().enumerate() {
        let record_len = if index + 1 == records.len() {
            (entries_end - offset) as u16
        } else {
            *record_len
        };
        raw_record(&mut data, offset, *inode, record_len, name);
        offset += usize::from(record_len);
    }
    data
}

fn bad_records() -> Vec<(&'static str, Vec<u8>)> {
    let mut too_short = leaf(&[(5, b"a", 12), (6, b"b", 12)], false);
    too_short[4..6].copy_from_slice(&4u16.to_le_bytes());
    let mut unaligned = leaf(&[(5, b"a", 12), (6, b"b", 12)], false);
    unaligned[4..6].copy_from_slice(&10u16.to_le_bytes());
    let mut past_the_end = leaf(&[(5, b"a", 12), (6, b"b", 12)], false);
    past_the_end[4..6].copy_from_slice(&4100u16.to_le_bytes());
    let mut long_name = leaf(&[(5, b"a", 12), (6, b"b", 12)], false);
    long_name[6] = 10;
    vec![
        ("record length below the header", too_short),
        ("record length not a multiple of four", unaligned),
        ("record running past the entry area", past_the_end),
        ("name longer than its record", long_name),
    ]
}

#[test]
fn the_index_limit_needs_room_for_at_least_one_entry() {
    assert_eq!(dx_limit(4096, 32, false).unwrap(), 508);
    assert_eq!(dx_limit(4096, 8, false).unwrap(), 511);
    assert_eq!(dx_limit(4096, 32, true).unwrap(), 507);
    assert_eq!(dx_limit(40, 32, false).unwrap(), 1);
    assert_eq!(operation(dx_limit(39, 32, false)), "htree:index_limit");
    assert_eq!(operation(dx_limit(31, 32, false)), "htree:index_limit");
    assert_eq!(dx_limit(48, 32, true).unwrap(), 1);
    assert_eq!(operation(dx_limit(47, 32, true)), "htree:index_limit");
    assert_eq!(operation(dx_limit(39, 32, true)), "htree:index_limit");
}

#[test]
fn index_entries_are_laid_out_after_the_count_and_limit() {
    let mut data = vec![0u8; 56];
    encode_dx_entries(
        &mut data,
        32,
        false,
        &[dx(0xdead_beef, 7), dx(0x10, 8), dx(0x20, 9)],
    )
    .unwrap();
    let mut expected = vec![0u8; 56];
    expected[32..34].copy_from_slice(&3u16.to_le_bytes());
    expected[34..36].copy_from_slice(&3u16.to_le_bytes());
    expected[36..40].copy_from_slice(&7u32.to_le_bytes());
    expected[40..44].copy_from_slice(&0x10u32.to_le_bytes());
    expected[44..48].copy_from_slice(&8u32.to_le_bytes());
    expected[48..52].copy_from_slice(&0x20u32.to_le_bytes());
    expected[52..56].copy_from_slice(&9u32.to_le_bytes());
    // The first hash slot holds the limit and count, so the first hash is not stored.
    assert_eq!(data, expected);

    let mut one = vec![0u8; 48];
    encode_dx_entries(&mut one, 32, false, &[dx(0, 5)]).unwrap();
    assert_eq!(&one[32..34], &2u16.to_le_bytes());
    assert_eq!(&one[34..36], &1u16.to_le_bytes());
    assert_eq!(&one[36..40], &5u32.to_le_bytes());
}

#[test]
fn index_entries_are_refused_for_each_bad_condition_alone() {
    let refused = |entries: &[Ext4DxEntry]| {
        let mut data = vec![0u8; 56];
        operation(encode_dx_entries(&mut data, 32, false, entries))
    };
    assert_eq!(refused(&[]), "htree:index_entries");
    assert_eq!(
        refused(&[dx(0, 1), dx(1, 2), dx(2, 3), dx(3, 4)]),
        "htree:index_entries"
    );
    assert_eq!(refused(&[dx(0, 0), dx(1, 2)]), "htree:index_entries");
    assert_eq!(
        refused(&[dx(0, DX_BLOCK_MASK + 1), dx(1, 2)]),
        "htree:index_entries"
    );
    assert_eq!(
        refused(&[dx(0, 1), dx(5, 2), dx(4, 3)]),
        "htree:index_order"
    );
    assert_eq!(refused(&[dx(0, 1), dx(0, 0)]), "htree:index_order");
    assert_eq!(
        refused(&[dx(0, 1), dx(0, DX_BLOCK_MASK + 1)]),
        "htree:index_order"
    );
    assert_eq!(
        refused(&[dx(0, 1), dx(1, 2), dx(0, 3)]),
        "htree:index_order"
    );

    // The edges stay valid: the block mask itself, a hash equal to its predecessor, and a table
    // filled to the limit.
    let mut data = vec![0u8; 56];
    encode_dx_entries(
        &mut data,
        32,
        false,
        &[dx(0, DX_BLOCK_MASK), dx(9, DX_BLOCK_MASK), dx(9, 1)],
    )
    .unwrap();
}

#[test]
fn a_block_number_in_the_high_bits_of_a_first_entry_is_refused_not_masked() {
    let mut data = vec![0u8; 56];
    assert_eq!(
        operation(encode_dx_entries(
            &mut data,
            32,
            false,
            &[dx(0, 0x1000_0001)]
        )),
        "htree:index_entries"
    );
}

#[test]
fn leaf_split_needs_two_entries_and_moves_about_half_the_block() {
    let entries = |lens: &[usize]| {
        lens.iter()
            .enumerate()
            .map(|(index, len)| leaf_entry(index as u32 + 1, b"x", *len))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        operation(linux_leaf_split_point(&entries(&[12]), 4096)),
        "htree:split_entry_count"
    );
    assert_eq!(
        operation(linux_leaf_split_point(&entries(&[]), 4096)),
        "htree:split_entry_count"
    );
    assert_eq!(
        linux_leaf_split_point(&entries(&[12, 12]), 4096).unwrap(),
        1
    );
    // Moving everything keeps the first half in place.
    assert_eq!(
        linux_leaf_split_point(&entries(&[12, 12, 12, 12]), 4096).unwrap(),
        2
    );
    assert_eq!(
        linux_leaf_split_point(&entries(&[12, 12, 12, 12, 12]), 4096).unwrap(),
        2
    );
    // The moved half-length of an entry counts, and exactly half the block still moves.
    assert_eq!(
        linux_leaf_split_point(&entries(&[12, 12, 4000]), 4096).unwrap(),
        2
    );
    assert_eq!(
        linux_leaf_split_point(&entries(&[12, 4096]), 4096).unwrap(),
        1
    );
    // An entry too big to move leaves nothing to split off.
    assert_eq!(
        operation(linux_leaf_split_point(&entries(&[12, 4098]), 4096)),
        "htree:split_point"
    );
}

#[test]
fn packing_a_leaf_fills_the_entry_area_and_refuses_overflow() {
    let one = [leaf_entry(3, b"abc", 12)];
    let data = pack_leaf(&one, 4096, true).unwrap();
    assert_eq!(read_u16_le(&data[4..6]), 4096 - 12);
    let empty: [LeafEntry; 0] = [];
    assert_eq!(
        operation(pack_leaf(&empty, 4096, false)),
        "htree:pack_empty_leaf"
    );
    // Sixteen records of 256 bytes fill the area exactly; one more byte does not fit.
    let name = vec![b'n'; 248];
    let fits: Vec<_> = (1..=16)
        .map(|inode| leaf_entry(inode, &name, 256))
        .collect();
    assert!(pack_leaf(&fits, 4096, false).is_ok());
    let mut too_big = fits.clone();
    too_big[15].name.push(b'n');
    assert_eq!(
        operation(pack_leaf(&too_big, 4096, false)),
        "htree:pack_leaf"
    );
    // With a checksum tail the same sixteen records no longer fit.
    assert_eq!(operation(pack_leaf(&fits, 4096, true)), "htree:pack_leaf");
}

#[test]
fn parsing_a_leaf_returns_the_live_entries_in_order() {
    let manager = HashTreeManager::new(SEED);
    let data = leaf(
        &[
            (5, b"alpha", 16),
            (0, b"gone", 12),
            (6, b"beta", 12),
            (7, b"c", 12),
        ],
        false,
    );
    let entries = parse_leaf_entries(&data, false, 1, &manager).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].inode.raw(), 5);
    assert_eq!(entries[0].name, b"alpha");
    assert_eq!(entries[0].source_record_len, 16);
    assert_eq!(entries[1].inode.raw(), 6);
    assert_eq!(entries[1].source_record_len, 12);
    assert_eq!(entries[2].inode.raw(), 7);
    assert_eq!(entries[2].source_record_len, 4096 - 40);
    for entry in &entries {
        let expected = crate::hashtree::calculate_hash(&entry.name, 1, &SEED)
            .unwrap()
            .major;
        assert_eq!(entry.hash, expected);
    }
    let two = leaf(&[(5, b"a", 12), (6, b"b", 12)], true);
    assert_eq!(
        parse_leaf_entries(&two, true, 1, &manager).unwrap().len(),
        2
    );
}

#[test]
fn parsing_a_leaf_refuses_each_bad_record_and_too_few_entries() {
    let manager = HashTreeManager::new(SEED);
    for (what, data) in bad_records() {
        assert_eq!(
            operation(parse_leaf_entries(&data, false, 1, &manager)),
            "htree:split_leaf_record",
            "{what}"
        );
    }
    let mut checksum_area = leaf(&[(5, b"a", 12), (6, b"b", 12)], true);
    checksum_area[4..6].copy_from_slice(&4090u16.to_le_bytes());
    checksum_area[12 + 4..12 + 6].copy_from_slice(&4084u16.to_le_bytes());
    assert_eq!(
        operation(parse_leaf_entries(&checksum_area, true, 1, &manager)),
        "htree:split_leaf_record"
    );
    let single = leaf(&[(5, b"a", 12), (0, b"b", 12)], false);
    assert_eq!(
        operation(parse_leaf_entries(&single, false, 1, &manager)),
        "htree:split_leaf_coverage"
    );
    let empty_name = leaf(&[(5, b"a", 12), (6, b"", 12)], false);
    assert_eq!(
        operation(parse_leaf_entries(&empty_name, false, 1, &manager)),
        "htree:split_leaf_empty_name"
    );
}

#[test]
fn inserting_into_a_leaf_refuses_each_bad_record_and_reports_a_full_leaf() {
    for (what, mut data) in bad_records() {
        let name = FileName::new(b"new").unwrap();
        let ino = InodeNumber::new(9).unwrap();
        assert_eq!(
            operation(insert_into_leaf(&mut data, false, ino, name, 1)),
            "htree:leaf_record",
            "{what}"
        );
    }
    let ino = InodeNumber::new(9).unwrap();
    let name = FileName::new(b"new").unwrap();
    // A leaf with no slack at all is full, with and without the checksum tail.
    let name_of_record = vec![b'n'; 248];
    let full = |has_checksum: bool| {
        let mut records: Vec<(u32, &[u8], u16)> = (1..=15)
            .map(|inode| (inode, name_of_record.as_slice(), 256u16))
            .collect();
        // The last record ends where the entry area ends: 244 bytes with a checksum tail.
        let last_name = if has_checksum {
            &name_of_record[..236]
        } else {
            &name_of_record[..]
        };
        records.push((16, last_name, 256));
        leaf(&records, has_checksum)
    };
    let mut data = full(false);
    assert!(!insert_into_leaf(&mut data, false, ino, name, 1).unwrap());
    let mut data = full(true);
    assert!(!insert_into_leaf(&mut data, true, ino, name, 1).unwrap());
}

#[test]
fn a_record_of_exactly_eight_bytes_is_valid_and_a_free_record_of_the_right_size_is_reused() {
    let manager = HashTreeManager::new(SEED);
    let ino = InodeNumber::new(9).unwrap();
    let name = FileName::new(b"new").unwrap();
    // A free record of eight bytes is well formed; it is too small for the new entry.
    let mut data = leaf(&[(0, b"", 8), (6, b"b", 12)], false);
    assert!(insert_into_leaf(&mut data, false, ino, name, 1).unwrap());
    assert_eq!(read_u32_le(&data[0..4]), 0);
    let parsed = parse_leaf_entries(
        &leaf(&[(0, b"", 8), (6, b"b", 12), (7, b"c", 12)], false),
        false,
        1,
        &manager,
    )
    .expect("an eight-byte free record parses");
    assert_eq!(parsed.len(), 2);
    // A free record exactly as long as the new entry is reused in place.
    let mut data = leaf(&[(0, b"", 12), (6, b"b", 12)], false);
    assert!(insert_into_leaf(&mut data, false, ino, name, 1).unwrap());
    assert_eq!(read_u32_le(&data[0..4]), 9);
    assert_eq!(read_u16_le(&data[4..6]), 12);
}

#[test]
fn an_index_with_one_entry_cannot_be_split() {
    let one = super::super::lookup::HashTreeFrame {
        source_block: 3,
        entries: vec![dx(0, 5)],
        selected: 0,
    };
    let parent = super::super::lookup::HashTreeFrame {
        source_block: 0,
        entries: vec![dx(0, 3)],
        selected: 0,
    };
    assert_eq!(
        operation(plan_index_split(&parent, &one, 9)),
        "htree:index_split_count"
    );
    let two = super::super::lookup::HashTreeFrame {
        source_block: 3,
        entries: vec![dx(0, 5), dx(0x40, 6)],
        selected: 1,
    };
    let plan = plan_index_split(&parent, &two, 9).unwrap();
    assert_eq!(plan.left.len(), 1);
    assert_eq!(plan.right.len(), 1);
    assert_eq!(plan.right[0].block, 6);
}

// ---- directories on a mounted filesystem ----

mod mounted {
    use super::*;
    use crate::{
        BLOCK_SIZE, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, SectorId, dir, mkdir,
        mkfile, mkfs, runtime::Clock,
    };

    pub(super) struct MemoryDevice {
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

    pub(super) type Mounted = (Jbd2Dev<MemoryDevice>, Ext4FileSystem);

    pub(super) fn mounted() -> Mounted {
        let device = MemoryDevice {
            bytes: vec![0; 16 * 1024 * BLOCK_SIZE],
        };
        let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
        mkfs(&mut journal).expect("mkfs");
        let filesystem = Ext4FileSystem::mount(&mut journal).expect("mount");
        (journal, filesystem)
    }

    fn ascii_name(index: usize) -> alloc::string::String {
        alloc::format!("{index:03}{}", "a".repeat(252))
    }

    /// A 255-byte name whose bytes past the ASCII range make the signed and unsigned hashes differ.
    fn accented_name(index: usize) -> alloc::string::String {
        alloc::format!("{index:03}{}", "\u{e9}".repeat(126))
    }

    fn fill(
        (journal, filesystem): &mut Mounted,
        names: impl Iterator<Item = alloc::string::String>,
    ) {
        for name in names {
            mkfile(
                journal,
                filesystem,
                &alloc::format!("/d/{name}"),
                None,
                None,
            )
            .expect("create entry");
        }
    }

    fn directory((journal, filesystem): &mut Mounted) -> (InodeNumber, Ext4Inode) {
        dir::get_inode_with_num(filesystem, journal, "/d")
            .expect("lookup")
            .expect("directory exists")
    }

    fn read_logical(
        (journal, filesystem): &mut Mounted,
        ino: InodeNumber,
        inode: &mut Ext4Inode,
        logical: u32,
    ) -> Vec<u8> {
        let physical = resolve_inode_block(filesystem, journal, ino, inode, logical)
            .expect("map")
            .expect("mapped");
        filesystem
            .datablock_cache
            .get_or_load(journal, physical)
            .expect("load")
            .data
            .as_ref()
            .clone()
    }

    /// Checks a root with two leaves and returns the separator hash.
    fn check_two_leaf_tree(
        m: &mut Mounted,
        base_version: u8,
        effective_version: u8,
        expected_names: usize,
    ) -> u32 {
        let (ino, mut inode) = directory(m);
        assert_ne!(inode.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
        assert_eq!(inode.size(), 3 * 4096);
        let root = read_logical(m, ino, &mut inode, 0);
        assert_eq!(root[28], base_version);
        assert_eq!(root[29], Ext4DxRootInfo::INFO_LENGTH);
        assert_eq!(root[30], 0);
        assert_eq!(read_u16_le(&root[32..34]), 507);
        assert_eq!(read_u16_le(&root[34..36]), 2);
        assert_eq!(read_u32_le(&root[36..40]), 1);
        let separator = read_u32_le(&root[40..44]);
        assert_eq!(read_u32_le(&root[44..48]), 2);
        let manager = HashTreeManager::new(m.1.superblock.s_hash_seed);
        let left = read_logical(m, ino, &mut inode, 1);
        let right = read_logical(m, ino, &mut inode, 2);
        let left = parse_leaf_entries(&left, true, effective_version, &manager).expect("left");
        let right = parse_leaf_entries(&right, true, effective_version, &manager).expect("right");
        assert_eq!(left.len() + right.len(), expected_names);
        assert!(left.iter().all(|entry| entry.hash < separator));
        assert!(right.iter().all(|entry| entry.hash >= separator));
        separator
    }

    #[test]
    fn a_full_directory_becomes_a_root_and_two_leaves_split_at_the_first_right_hash() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..16).map(ascii_name));
        let separator = check_two_leaf_tree(&mut m, 1, 1, 16);
        let (ino, mut inode) = directory(&mut m);
        let manager = HashTreeManager::new(m.1.superblock.s_hash_seed);
        let right = read_logical(&mut m, ino, &mut inode, 2);
        let right = parse_leaf_entries(&right, true, 1, &manager).expect("right");
        let lowest = right.iter().map(|entry| entry.hash).min().expect("entries");
        assert_eq!(separator, lowest);
    }

    #[test]
    fn the_hash_flavor_follows_the_superblock_for_signed_and_unsigned_bytes() {
        let unsigned = Ext4Superblock::EXT4_FLAGS_UNSIGNED_HASH;
        let signed = Ext4Superblock::EXT4_FLAGS_SIGNED_HASH;
        for (base, flags, effective) in [
            (Ext4DxRootInfo::DX_HASH_HALF_MD4, unsigned, 4),
            (Ext4DxRootInfo::DX_HASH_HALF_MD4, signed, 1),
            (Ext4DxRootInfo::DX_HASH_TEA, unsigned, 5),
            (Ext4DxRootInfo::DX_HASH_TEA, signed, 2),
            (Ext4DxRootInfo::DX_HASH_TEA, signed | 0x4, 2),
            (Ext4DxRootInfo::DX_HASH_TEA, unsigned | 0x4, 5),
        ] {
            let mut m = mounted();
            m.1.superblock.s_def_hash_version = base;
            m.1.superblock.s_flags = flags;
            mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
            fill(&mut m, (0..16).map(accented_name));
            check_two_leaf_tree(&mut m, base, effective, 16);
        }
    }

    #[test]
    fn a_hash_version_past_tea_is_refused_before_anything_changes() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..15).map(ascii_name));
        m.1.superblock.s_def_hash_version = Ext4DxRootInfo::DX_HASH_TEA + 1;
        let result = mkfile(
            &mut m.0,
            &mut m.1,
            &alloc::format!("/d/{}", ascii_name(15)),
            None,
            None,
        );
        assert_eq!(operation(result), "htree:make_indexed_hash_version");
        let (_, inode) = directory(&mut m);
        assert_eq!(inode.size(), 4096);
        assert_eq!(inode.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
    }

    fn convert_directly(
        m: &mut Mounted,
        inode: &mut Ext4Inode,
        ino: InodeNumber,
    ) -> Ext4Result<()> {
        if dir::get_inode_with_num(&mut m.1, &mut m.0, "/x")
            .expect("lookup")
            .is_none()
        {
            mkfile(&mut m.0, &mut m.1, "/x", None, None).expect("create the new entry's inode");
        }
        let child = dir::get_inode_with_num(&mut m.1, &mut m.0, "/x")
            .expect("lookup")
            .expect("exists")
            .0;
        make_indexed_directory(
            &mut m.1,
            &mut m.0,
            ino,
            inode,
            child,
            FileName::new(b"zz").expect("name"),
            Ext4DirEntry2::EXT4_FT_REG_FILE,
        )
    }

    #[test]
    fn only_a_one_block_directory_inode_can_be_converted() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..2).map(ascii_name));
        let (ino, inode) = directory(&mut m);
        let mut regular = inode;
        regular.i_mode = 0x8000 | 0o644;
        assert_eq!(
            operation(convert_directly(&mut m, &mut regular, ino)),
            "htree:make_indexed_geometry"
        );
        let mut two_blocks = inode;
        two_blocks.set_size(2 * 4096);
        assert_eq!(
            operation(convert_directly(&mut m, &mut two_blocks, ino)),
            "htree:make_indexed_geometry"
        );
        let mut half_block = inode;
        half_block.set_size(2048);
        assert_eq!(
            operation(convert_directly(&mut m, &mut half_block, ino)),
            "htree:make_indexed_geometry"
        );
    }

    #[test]
    fn a_directory_needs_two_entries_besides_dot_and_dotdot_to_be_split() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..1).map(ascii_name));
        let (ino, mut inode) = directory(&mut m);
        assert_eq!(
            operation(convert_directly(&mut m, &mut inode, ino)),
            "htree:make_indexed_entries"
        );
        fill(&mut m, (1..2).map(ascii_name));
        let (ino, mut inode) = directory(&mut m);
        convert_directly(&mut m, &mut inode, ino).expect("two entries are enough");
        assert_ne!(inode.i_flags & Ext4Inode::EXT4_INDEX_FL, 0);
    }

    #[test]
    fn the_first_two_records_must_be_dot_of_this_directory_and_dotdot() {
        let corrupt = |offset: usize, patch: &[u8]| {
            let mut m = mounted();
            mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
            fill(&mut m, (0..3).map(ascii_name));
            let (ino, mut inode) = directory(&mut m);
            let physical = resolve_inode_block(&m.1, &mut m.0, ino, &mut inode, 0)
                .expect("map")
                .expect("mapped");
            let superblock = m.1.superblock;
            let generation = inode.i_generation;
            m.1.datablock_cache
                .modify_metadata(&mut m.0, physical, |data| {
                    data[offset..offset + patch.len()].copy_from_slice(patch);
                    update_ext4_dirblock_csum32(&superblock, ino.raw(), generation, data);
                })
                .expect("patch the block");
            operation(convert_directly(&mut m, &mut inode, ino))
        };
        // The dot record names another inode.
        assert_eq!(corrupt(0, &999u32.to_le_bytes()), "htree:make_indexed_dot");
        // The dot record has the right inode but another name.
        assert_eq!(corrupt(8, b"x"), "htree:make_indexed_dot");
        // The second record is not dot-dot.
        assert_eq!(corrupt(21, b"x"), "htree:make_indexed_dotdot");
    }

    #[test]
    fn names_with_one_hash_keep_together_and_the_separator_moves_past_them() {
        let mut m = mounted();
        m.1.superblock.s_hash_seed = SEED;
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        // These two names have the same half-MD4 hash under the seed above.
        fill(
            &mut m,
            ["c97300", "c106408"]
                .into_iter()
                .map(alloc::string::String::from),
        );
        let (ino, mut inode) = directory(&mut m);
        convert_directly(&mut m, &mut inode, ino).expect("convert");
        let (ino, mut inode) = directory(&mut m);
        let root = read_logical(&mut m, ino, &mut inode, 0);
        assert_eq!(read_u32_le(&root[40..44]), 0xd141_d3f1);
    }

    #[test]
    fn a_leaf_split_records_the_first_hash_of_the_new_leaf() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..16).map(ascii_name));
        for index in 16..40 {
            let (ino, mut inode) = directory(&mut m);
            let root = read_logical(&mut m, ino, &mut inode, 0);
            if read_u16_le(&root[34..36]) == 3 {
                break;
            }
            fill(&mut m, core::iter::once(ascii_name(index)));
        }
        let (ino, mut inode) = directory(&mut m);
        let root = read_logical(&mut m, ino, &mut inode, 0);
        assert_eq!(read_u16_le(&root[34..36]), 3);
        let manager = HashTreeManager::new(m.1.superblock.s_hash_seed);
        let mut previous = 0u32;
        for slot in 0..3usize {
            let block = read_u32_le(&root[36 + slot * 8..40 + slot * 8]);
            let separator = if slot == 0 {
                0
            } else {
                read_u32_le(&root[32 + slot * 8..36 + slot * 8])
            };
            let leaf = read_logical(&mut m, ino, &mut inode, block);
            let entries = parse_leaf_entries(&leaf, true, 1, &manager).expect("leaf");
            let lowest = entries
                .iter()
                .map(|entry| entry.hash)
                .min()
                .expect("entries");
            if slot != 0 {
                assert_eq!(separator, lowest);
            }
            assert!(entries.iter().all(|entry| entry.hash >= separator));
            assert!(separator >= previous);
            previous = separator;
        }
    }

    fn legacy_directory_inode(m: &mut Mounted, blocks: u64) -> (InodeNumber, Ext4Inode) {
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        let (ino, mut inode) = directory(m);
        inode.i_flags &= !Ext4Inode::EXT4_EXTENTS_FL;
        inode.i_block = [0; 15];
        inode.set_size(blocks * 4096);
        (ino, inode)
    }

    #[test]
    fn a_block_mapped_directory_grows_up_to_twelve_blocks() {
        let mut m = mounted();
        let (ino, mut inode) = legacy_directory_inode(&mut m, 11);
        let (logical, block) =
            append_directory_block(&mut m.1, &mut m.0, ino, &mut inode).expect("the twelfth block");
        assert_eq!(logical, 11);
        assert_eq!(
            inode.i_block[11],
            block.to_u32().expect("small block number")
        );
        assert_eq!(inode.size(), 12 * 4096);

        let mut m = mounted();
        let (ino, mut inode) = legacy_directory_inode(&mut m, 12);
        assert_eq!(
            operation(append_directory_block(&mut m.1, &mut m.0, ino, &mut inode)),
            "htree:legacy_directory_growth"
        );
    }

    #[test]
    fn an_extent_directory_grows_through_the_extent_tree() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        let (ino, mut inode) = directory(&mut m);
        let (logical, block) =
            append_directory_block(&mut m.1, &mut m.0, ino, &mut inode).expect("append");
        assert_eq!(logical, 1);
        let mapped = resolve_inode_block(&m.1, &mut m.0, ino, &mut inode, 1)
            .expect("map")
            .expect("mapped");
        assert_eq!(mapped, block);
    }

    #[test]
    fn an_index_that_cannot_be_split_below_its_parent_is_refused_before_any_write() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        let (ino, mut inode) = directory(&mut m);
        let frame = |source_block: u32, entries: Vec<Ext4DxEntry>, selected: usize| {
            super::super::super::lookup::HashTreeFrame {
                source_block,
                entries,
                selected,
            }
        };
        let mut run = |parent: &super::super::super::lookup::HashTreeFrame,
                       index: &super::super::super::lookup::HashTreeFrame| {
            split_index_below_parent(&mut m.1, &mut m.0, ino, &mut inode, parent, index, true)
        };
        let good_parent = frame(0, vec![dx(0, 7), dx(0x100, 8)], 1);
        let good_index = frame(8, vec![dx(0, 20), dx(0x200, 21)], 0);
        // Each condition alone refuses the split.
        let wrong_selection = frame(0, vec![dx(0, 7), dx(0x100, 8)], 0);
        assert_eq!(
            operation(run(&wrong_selection, &good_index)),
            "htree:index_split_count"
        );
        let missing_selection = frame(0, vec![dx(0, 7), dx(0x100, 8)], 2);
        assert_eq!(
            operation(run(&missing_selection, &good_index)),
            "htree:index_split_count"
        );
        let root_as_index = frame(0, vec![dx(0, 20), dx(0x200, 21)], 0);
        let parent_of_root = frame(0, vec![dx(0, 0), dx(0x100, 8)], 1);
        assert_eq!(
            operation(run(&parent_of_root, &root_as_index)),
            "htree:index_split_count"
        );
        let one_entry = frame(8, vec![dx(0, 20)], 0);
        assert_eq!(
            operation(run(&good_parent, &one_entry)),
            "htree:index_split_count"
        );
        // A valid pair passes the checks and fails later, on the unmapped blocks.
        assert_ne!(
            operation(run(&good_parent, &good_index)),
            "htree:index_split_count"
        );
    }

    #[test]
    fn a_directory_of_many_leaves_splits_leaves_in_the_middle_of_the_index() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..80).map(ascii_name));
        let (ino, mut inode) = directory(&mut m);
        let root = read_logical(&mut m, ino, &mut inode, 0);
        let count = usize::from(read_u16_le(&root[34..36]));
        assert!(count >= 5);
        let manager = HashTreeManager::new(m.1.superblock.s_hash_seed);
        let mut total = 0;
        let mut previous = 0u32;
        for slot in 0..count {
            let block = read_u32_le(&root[36 + slot * 8..40 + slot * 8]);
            let low = if slot == 0 {
                0
            } else {
                read_u32_le(&root[32 + slot * 8..36 + slot * 8])
            };
            let high = if slot + 1 == count {
                u32::MAX
            } else {
                read_u32_le(&root[40 + slot * 8..44 + slot * 8])
            };
            assert!(low >= previous);
            previous = low;
            let leaf = read_logical(&mut m, ino, &mut inode, block);
            let entries = parse_leaf_entries(&leaf, true, 1, &manager).expect("leaf");
            total += entries.len();
            assert!(
                entries
                    .iter()
                    .all(|entry| entry.hash >= low && entry.hash <= high)
            );
        }
        assert_eq!(total, 80);
    }

    fn assert_checksummed(m: &Mounted, ino: InodeNumber, inode: &Ext4Inode, data: &[u8]) {
        let mut copy = data.to_vec();
        assert!(update_ext4_dx_checksum(
            &m.1.superblock,
            ino.raw(),
            inode.i_generation,
            &mut copy
        ));
        assert_eq!(copy, data);
    }

    #[test]
    fn a_full_root_is_promoted_once_and_a_lower_index_then_splits_below_it() {
        let mut m = mounted();
        mkdir(&mut m.0, &mut m.1, "/d").expect("mkdir");
        fill(&mut m, (0..16).map(ascii_name));
        let (ino, mut inode) = directory(&mut m);
        let root = read_logical(&mut m, ino, &mut inode, 0);
        let separator = read_u32_le(&root[40..44]);
        let root_frame =
            |entries: Vec<Ext4DxEntry>, selected| super::super::super::lookup::HashTreeFrame {
                source_block: 0,
                entries,
                selected,
            };
        let two = vec![dx(0, 1), dx(separator, 2)];
        // Only the root block can be promoted.
        let not_root = super::super::super::lookup::HashTreeFrame {
            source_block: 1,
            entries: two.clone(),
            selected: 0,
        };
        assert_eq!(
            operation(promote_full_root(
                &mut m.1, &mut m.0, ino, &mut inode, &not_root, true
            )),
            "htree:root_source"
        );
        promote_full_root(
            &mut m.1,
            &mut m.0,
            ino,
            &mut inode,
            &root_frame(two.clone(), 0),
            true,
        )
        .expect("promote");
        assert_eq!(
            operation(promote_full_root(
                &mut m.1,
                &mut m.0,
                ino,
                &mut inode,
                &root_frame(vec![dx(0, 3)], 0),
                true
            )),
            "htree:max_depth"
        );
        let root = read_logical(&mut m, ino, &mut inode, 0);
        assert_eq!(root[30], 1);
        assert_eq!(read_u16_le(&root[34..36]), 1);
        assert_eq!(read_u32_le(&root[36..40]), 3);
        assert_checksummed(&m, ino, &inode, &root);
        let index = read_logical(&mut m, ino, &mut inode, 3);
        assert_eq!(read_u16_le(&index[10..12]), 2);
        assert_eq!(read_u32_le(&index[12..16]), 1);
        assert_eq!(read_u32_le(&index[16..20]), separator);
        assert_eq!(read_u32_le(&index[20..24]), 2);
        assert_checksummed(&m, ino, &inode, &index);

        let path = super::super::super::lookup::HashTreePath {
            frames: vec![
                root_frame(vec![dx(0, 3)], 0),
                super::super::super::lookup::HashTreeFrame {
                    source_block: 3,
                    entries: two,
                    selected: 0,
                },
            ],
        };
        grow_full_index_for_leaf_split(&mut m.1, &mut m.0, ino, &mut inode, &path, true)
            .expect("split below the root");
        let root = read_logical(&mut m, ino, &mut inode, 0);
        assert_eq!(read_u16_le(&root[34..36]), 2);
        assert_eq!(read_u32_le(&root[36..40]), 3);
        assert_eq!(read_u32_le(&root[40..44]), separator);
        assert_eq!(read_u32_le(&root[44..48]), 4);
        assert_checksummed(&m, ino, &inode, &root);
        let left = read_logical(&mut m, ino, &mut inode, 3);
        assert_eq!(read_u16_le(&left[10..12]), 1);
        assert_eq!(read_u32_le(&left[12..16]), 1);
        assert_checksummed(&m, ino, &inode, &left);
        let right = read_logical(&mut m, ino, &mut inode, 4);
        assert_eq!(read_u16_le(&right[10..12]), 1);
        assert_eq!(read_u32_le(&right[12..16]), 2);
        assert_checksummed(&m, ino, &inode, &right);
    }
}
