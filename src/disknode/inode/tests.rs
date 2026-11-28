use super::*;

const MAX_RAW: u64 = (1_u64 << 48) - 1;

fn with_mode(mode: u16) -> Ext4Inode {
    Ext4Inode {
        i_mode: mode,
        ..Default::default()
    }
}

#[test]
fn the_embedded_extent_header_fills_the_first_three_words() {
    let mut inode = Ext4Inode::default();
    inode.write_extend_header();
    assert_eq!(inode.i_block[0], 0xF30A);
    assert_eq!(inode.i_block[1], 4);
    assert!(inode.i_block[2..].iter().all(|word| *word == 0));
}

#[test]
fn the_raw_block_count_limit_is_forty_eight_bits() {
    let mut inode = Ext4Inode::default();
    inode.set_blocks_count(MAX_RAW, 1024, true).unwrap();
    assert_eq!(inode.i_flags & Ext4Inode::EXT4_HUGE_FILE_FL, 0);
    assert_eq!(inode.blocks_count(1024, true), MAX_RAW);
}

#[test]
fn block_sizes_must_be_positive_multiples_of_a_sector() {
    let mut inode = Ext4Inode::default();
    assert!(inode.set_blocks_count(8, 0, false).is_err());
    assert!(inode.set_blocks_count(8, 256, false).is_err());
    assert!(inode.set_blocks_count(8, 768, false).is_err());
    assert!(inode.set_blocks_count(8, 512, false).is_ok());
}

#[test]
fn counts_beyond_the_raw_limit_are_stored_in_filesystem_blocks() {
    let mut inode = Ext4Inode::default();
    let sectors = MAX_RAW * 2;
    inode.set_blocks_count(sectors, 1024, true).unwrap();
    assert_ne!(inode.i_flags & Ext4Inode::EXT4_HUGE_FILE_FL, 0);
    assert_eq!(inode.blocks_count(1024, true), sectors);

    let too_many = (MAX_RAW + 1) * 2;
    assert!(inode.set_blocks_count(too_many, 1024, true).is_err());
    // A count that is not a whole number of blocks cannot use the huge form.
    assert!(inode.set_blocks_count(MAX_RAW + 2, 1024, true).is_err());
}

#[test]
fn large_counts_need_the_huge_file_feature() {
    let mut inode = Ext4Inode::default();
    assert!(
        inode
            .set_blocks_count(u64::from(u32::MAX) + 1, 1024, false)
            .is_err()
    );
    inode
        .set_blocks_count(u64::from(u32::MAX) + 1, 1024, true)
        .unwrap();
    assert_eq!(inode.blocks_count(1024, true), u64::from(u32::MAX) + 1);
    inode
        .set_blocks_count(u64::from(u32::MAX), 1024, false)
        .unwrap();
    assert_eq!(inode.blocks_count(1024, false), u64::from(u32::MAX));
}

#[test]
fn the_link_count_sentinel_applies_only_to_directories_with_the_feature() {
    let mut dir = with_mode(Ext4Inode::S_IFDIR);
    dir.i_links_count = 1;
    assert_eq!(dir.incremented_links_count(true).unwrap(), 1);
    assert_eq!(dir.incremented_links_count(false).unwrap(), 2);

    let mut file = with_mode(Ext4Inode::S_IFREG);
    file.i_links_count = 1;
    assert_eq!(file.incremented_links_count(true).unwrap(), 2);
}

#[test]
fn an_indexed_directory_counts_normally_below_the_limit() {
    let mut dir = with_mode(Ext4Inode::S_IFDIR);
    dir.i_flags = Ext4Inode::EXT4_INDEX_FL;
    dir.i_links_count = 2;
    assert_eq!(dir.incremented_links_count(true).unwrap(), 3);
    assert_eq!(dir.incremented_links_count(false).unwrap(), 3);
    dir.i_links_count = 3;
    assert_eq!(dir.incremented_links_count(true).unwrap(), 4);

    let mut plain = with_mode(Ext4Inode::S_IFDIR);
    plain.i_links_count = 2;
    assert_eq!(plain.incremented_links_count(true).unwrap(), 3);

    let mut file = with_mode(Ext4Inode::S_IFREG);
    file.i_flags = Ext4Inode::EXT4_INDEX_FL;
    file.i_links_count = 2;
    assert_eq!(file.incremented_links_count(true).unwrap(), 3);
}

#[test]
fn the_link_limit_is_a_sentinel_only_for_indexed_directories() {
    let mut dir = with_mode(Ext4Inode::S_IFDIR);
    dir.i_flags = Ext4Inode::EXT4_INDEX_FL;
    dir.i_links_count = Ext4Inode::EXT4_LINK_MAX;
    assert_eq!(dir.incremented_links_count(true).unwrap(), 1);
    assert!(dir.incremented_links_count(false).is_err());

    let mut plain = with_mode(Ext4Inode::S_IFDIR);
    plain.i_links_count = Ext4Inode::EXT4_LINK_MAX;
    assert!(plain.incremented_links_count(true).is_err());

    plain.i_links_count = Ext4Inode::EXT4_LINK_MAX - 1;
    assert_eq!(
        plain.incremented_links_count(true).unwrap(),
        Ext4Inode::EXT4_LINK_MAX
    );
}

#[test]
fn the_extended_attribute_block_keeps_its_high_half() {
    let mut inode = Ext4Inode::default();
    let block = 0xABCD_1234_5678;
    inode.set_file_acl(block).unwrap();
    assert_eq!(inode.l_i_file_acl_high, 0xABCD);
    assert_eq!(inode.i_file_acl_lo, 0x1234_5678);
    assert_eq!(inode.file_acl(), block);
    assert!(inode.set_file_acl(1 << 48).is_err());
    assert!(inode.set_file_acl((1 << 48) - 1).is_ok());
}

#[test]
fn every_file_type_is_recognized_alone() {
    let types = [
        Ext4Inode::S_IFDIR,
        Ext4Inode::S_IFREG,
        Ext4Inode::S_IFLNK,
        Ext4Inode::S_IFCHR,
        Ext4Inode::S_IFBLK,
        Ext4Inode::S_IFIFO,
        Ext4Inode::S_IFSOCK,
    ];
    for (index, mode) in types.into_iter().enumerate() {
        // Permission bits must not change the answer.
        let inode = with_mode(mode | 0o755);
        let answers = [
            inode.is_dir(),
            inode.is_file(),
            inode.is_symlink(),
            inode.is_character_device(),
            inode.is_block_device(),
            inode.is_fifo(),
            inode.is_socket(),
        ];
        for (other, answer) in answers.into_iter().enumerate() {
            assert_eq!(answer, other == index, "type {mode:#x} predicate {other}");
        }
    }
    assert!(!with_mode(0).is_fifo());
    assert!(!with_mode(0).is_socket());
}

#[test]
fn device_numbers_are_stored_only_in_device_inodes() {
    let device = DeviceNumber::new(8, 1).unwrap();
    let mut file = with_mode(Ext4Inode::S_IFREG);
    assert!(file.set_device_number(device).is_err());

    for mode in [Ext4Inode::S_IFCHR, Ext4Inode::S_IFBLK] {
        let mut inode = with_mode(mode);
        inode.i_flags = Ext4Inode::EXT4_EXTENTS_FL;
        inode.i_block = [0xFFFF_FFFF; 15];
        inode.set_device_number(device).unwrap();
        assert_eq!(inode.i_flags & Ext4Inode::EXT4_EXTENTS_FL, 0);
        assert_eq!(inode.i_block[0], device.encode_legacy());
        assert!(inode.i_block[1..].iter().all(|word| *word == 0));
        assert_eq!(inode.device_number().unwrap(), Some(device));
    }

    let wide = DeviceNumber::new(300, 70_000).unwrap();
    let mut inode = with_mode(Ext4Inode::S_IFBLK);
    inode.set_device_number(wide).unwrap();
    assert_eq!(inode.i_block[0], 0);
    assert_eq!(inode.device_number().unwrap(), Some(wide));
    assert_eq!(file.device_number().unwrap(), None);
}

#[test]
fn executable_means_any_execute_bit() {
    for bit in [Ext4Inode::S_IXUSR, Ext4Inode::S_IXGRP, Ext4Inode::S_IXOTH] {
        assert!(with_mode(Ext4Inode::S_IFREG | bit).is_executable());
    }
    assert!(!with_mode(Ext4Inode::S_IFREG | 0o666).is_executable());
}

#[test]
fn setid_bits_are_cleared_by_content_changes_and_chown() {
    let all = Ext4Inode::S_IFREG | Ext4Inode::S_ISUID | Ext4Inode::S_ISGID | 0o644;
    let mut inode = with_mode(all);
    inode.clear_setid_bits_for_content_change();
    assert_eq!(
        inode.i_mode,
        Ext4Inode::S_IFREG | Ext4Inode::S_ISGID | 0o644
    );

    let mut executable = with_mode(all | Ext4Inode::S_IXGRP);
    executable.clear_setid_bits_for_content_change();
    assert_eq!(
        executable.i_mode,
        Ext4Inode::S_IFREG | 0o644 | Ext4Inode::S_IXGRP
    );

    let mut chowned = with_mode(all | Ext4Inode::S_ISVTX);
    chowned.clear_setid_bits_for_chown();
    assert_eq!(
        chowned.i_mode,
        Ext4Inode::S_IFREG | 0o644 | Ext4Inode::S_ISVTX
    );
}

#[test]
fn changing_the_mode_keeps_the_file_type() {
    let mut inode = with_mode(Ext4Inode::S_IFDIR | 0o700);
    inode.set_mode_preserve_type(Ext4Inode::S_IFREG | 0o644);
    assert_eq!(inode.i_mode, Ext4Inode::S_IFDIR | 0o644);
    assert_eq!(inode.permissions(), 0o644);
}

#[test]
fn the_plain_time_setters_store_their_value() {
    let mut inode = Ext4Inode::default();
    inode.set_mtime(11);
    inode.set_ctime(22);
    inode.set_atime(33);
    assert_eq!((inode.i_mtime, inode.i_ctime, inode.i_atime), (11, 22, 33));
}

#[test]
fn the_high_version_word_is_read_when_the_field_fits() {
    let mut inode = Ext4Inode {
        l_i_version: 5,
        i_version_hi: 3,
        i_extra_isize: Ext4Inode::required_extra_isize(Ext4Inode::FIELD_END_I_VERSION_HI),
        ..Default::default()
    };
    assert_eq!(inode.version(256), (3_u64 << 32) | 5);
    assert_eq!(inode.version(128), 5);
    inode.i_extra_isize = 0;
    assert_eq!(inode.version(256), 5);
}

#[test]
fn extra_time_carries_the_epoch_bits_and_nanoseconds() {
    let size = Ext4Inode::LARGE_INODE_SIZE;
    for epoch in 0..4_i64 {
        let mut inode = Ext4Inode {
            i_extra_isize: 32,
            ..Default::default()
        };
        let stamp = Ext4Timestamp::new((epoch << 32) + 1_000, 123_456_789);
        inode.set_mtime_ts(size, stamp);
        assert_eq!(inode.i_mtime_extra & 3, epoch as u32);
        assert_eq!(inode.mtime_ts(size), stamp);
    }
    // Raw extra words decode with their epoch bits regardless of nanoseconds.
    let raw = Ext4Inode::decode_extra_time(7, (5 << 2) | 3);
    assert_eq!(raw, Ext4Timestamp::new((3_i64 << 32) + 7, 5));
    let plain = Ext4Inode::decode_extra_time(7, 5 << 2);
    assert_eq!(plain, Ext4Timestamp::new(7, 5));
}

#[test]
fn a_timestamp_with_bit_thirty_one_set_encodes_the_next_epoch() {
    let size = Ext4Inode::LARGE_INODE_SIZE;
    let mut inode = Ext4Inode {
        i_extra_isize: 32,
        ..Default::default()
    };
    let stamp = Ext4Timestamp::new(1_i64 << 31, 0);
    inode.set_atime_ts(size, stamp);
    assert_eq!(inode.i_atime_extra, 1);
    assert_eq!(inode.atime_ts(size), stamp);
    // An extra word that holds only the epoch bits, all set and no nanoseconds.
    assert_eq!(
        Ext4Inode::decode_extra_time(0, 3),
        Ext4Timestamp::new(3_i64 << 32, 0)
    );
}
