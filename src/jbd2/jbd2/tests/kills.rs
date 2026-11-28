//! Tests that pin the boundaries of replay parsing and the journal bookkeeping helpers.

use super::*;
use crate::error::ErrorContext;

fn operation<T>(result: Ext4Result<T>) -> &'static str {
    let Err(error) = result else {
        panic!("the call must be refused");
    };
    match error.context() {
        Some(ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

fn system(features: u32) -> JBD2DEVSYSTEM {
    let mut system = transaction_system(Jbd2RunningTransactionPhase::Running);
    system.jbd2_super_block.s_feature_compat = 0;
    system.jbd2_super_block.s_feature_incompat = features;
    if features & (JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_CSUM_V3) != 0 {
        system.jbd2_super_block.s_checksum_type = JBD2_CRC32C_CHKSUM;
    }
    system.jbd2_super_block.s_uuid = [0x5a; JBD2_UUID_SIZE];
    system
}

fn tagged(result: Ext4Result<ReplayDescriptor>) -> Vec<ReplayTag> {
    match result.expect("the descriptor parses") {
        ReplayDescriptor::Tagged { tags, .. } => tags,
        ReplayDescriptor::EmptyTail => panic!("expected tags, found an empty tail"),
    }
}

/// A version 1 tag followed by the optional high half.
fn v1_tag(blocknr: u32, checksum: u16, flags: u16, high: Option<u32>) -> Vec<u8> {
    let mut bytes = vec![0u8; JBD2_TAG_SIZE];
    JournalBlockTagS {
        t_blocknr: blocknr,
        t_checksum: checksum,
        t_flags: flags,
    }
    .to_disk_bytes(&mut bytes);
    if let Some(high) = high {
        bytes.extend_from_slice(&high.to_be_bytes());
    }
    bytes
}

fn v3_tag(blocknr: u32, flags: u32, high: u32, checksum: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; JBD2_TAG3_SIZE];
    JournalBlockTag3S {
        t_blocknr: blocknr,
        t_flags: flags,
        t_blocknr_high: high,
        t_checksum: checksum,
    }
    .to_disk_bytes(&mut bytes);
    bytes
}

/// A descriptor block of exactly the given payload after the header, with `tail` extra bytes.
fn descriptor(payload: &[u8], tail: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; JBD2_DESCRIPTOR_HEADER_SIZE];
    bytes.extend_from_slice(payload);
    bytes.resize(bytes.len() + tail, 0);
    bytes
}

const SAME: u16 = JBD2_FLAG_SAME_UUID;
const LAST: u16 = JBD2_FLAG_LAST_TAG;

#[test]
fn version_one_tags_fill_the_block_exactly_and_a_short_tag_is_refused() {
    let system = system(0);
    let mut payload = v1_tag(7, 0, SAME, None);
    payload.extend(v1_tag(9, 0, SAME, None));
    let tags = tagged(system.parse_replay_tags(&descriptor(&payload, 0)));
    assert_eq!(tags.len(), 2);
    assert_eq!(tags[0].block, AbsoluteBN::new(7));
    assert_eq!(tags[1].block, AbsoluteBN::new(9));
    let short = descriptor(&payload[..payload.len() - 1], 0);
    assert_eq!(
        operation(system.parse_replay_tags(&short)),
        "jbd2:replay_tag_truncated"
    );
}

#[test]
fn a_sixty_four_bit_tag_carries_its_high_half_after_the_tag() {
    let system = system(JBD2_FEATURE_INCOMPAT_64BIT);
    let payload = v1_tag(7, 0, LAST | SAME, Some(3));
    let tags = tagged(system.parse_replay_tags(&descriptor(&payload, 0)));
    assert_eq!(tags[0].block, AbsoluteBN::new((3u64 << 32) | 7));
    let short = descriptor(&payload[..payload.len() - 1], 0);
    assert_eq!(
        operation(system.parse_replay_tags(&short)),
        "jbd2:replay_tag_high_truncated"
    );
}

#[test]
fn a_version_two_tag_is_followed_by_two_bytes_of_padding_before_the_checksum_tail() {
    let system = system(JBD2_FEATURE_INCOMPAT_CSUM_V2);
    let payload = v1_tag(7, 5, LAST | SAME, None);
    let exact = descriptor(&payload, 2 + 4);
    let tags = tagged(system.parse_replay_tags(&exact));
    assert_eq!(tags[0].block, AbsoluteBN::new(7));
    assert!(matches!(tags[0].checksum, Some(ReplayChecksum::CsumV2(5))));
    let short = descriptor(&payload, 1 + 4);
    assert_eq!(
        operation(system.parse_replay_tags(&short)),
        "jbd2:replay_tag_csum_v2_truncated"
    );
}

#[test]
fn a_version_three_tag_fills_its_sixteen_bytes_and_keeps_both_block_halves() {
    let system = system(JBD2_FEATURE_INCOMPAT_CSUM_V3 | JBD2_FEATURE_INCOMPAT_64BIT);
    let payload = v3_tag(7, u32::from(LAST | SAME), 3, 11);
    let tags = tagged(system.parse_replay_tags(&descriptor(&payload, 4)));
    assert_eq!(tags[0].block, AbsoluteBN::new((3u64 << 32) | 7));
    assert!(matches!(tags[0].checksum, Some(ReplayChecksum::CsumV3(11))));
    let short = descriptor(&payload[..payload.len() - 1], 4);
    assert_eq!(
        operation(system.parse_replay_tags(&short)),
        "jbd2:replay_tag3_truncated"
    );
}

#[test]
fn a_version_three_high_half_needs_the_sixty_four_bit_feature() {
    let payload = v3_tag(7, u32::from(LAST | SAME), 3, 11);
    let narrow = system(JBD2_FEATURE_INCOMPAT_CSUM_V3);
    assert_eq!(
        operation(narrow.parse_replay_tags(&descriptor(&payload, 4))),
        "jbd2:replay_tag3_block_high"
    );
    let payload = v3_tag(7, u32::from(LAST | SAME), 0, 11);
    let tags = tagged(narrow.parse_replay_tags(&descriptor(&payload, 4)));
    assert_eq!(tags[0].block, AbsoluteBN::new(7));
}

#[test]
fn a_tag_without_the_same_uuid_flag_is_followed_by_a_uuid() {
    let system = system(0);
    let payload = v1_tag(7, 0, LAST, None);
    let tags = tagged(system.parse_replay_tags(&descriptor(&payload, JBD2_UUID_SIZE)));
    assert_eq!(tags.len(), 1);
    assert_eq!(
        operation(system.parse_replay_tags(&descriptor(&payload, JBD2_UUID_SIZE - 1))),
        "jbd2:replay_uuid_truncated"
    );
}

#[test]
fn a_zero_tag_in_a_zero_block_is_an_empty_tail_but_any_nonzero_field_makes_it_a_tag() {
    let plain = system(0);
    let wide = system(JBD2_FEATURE_INCOMPAT_64BIT);
    let v3 = system(JBD2_FEATURE_INCOMPAT_CSUM_V3 | JBD2_FEATURE_INCOMPAT_64BIT);
    let zero = descriptor(&v1_tag(0, 0, 0, None), JBD2_UUID_SIZE);
    assert!(matches!(
        plain.parse_replay_tags(&zero).unwrap(),
        ReplayDescriptor::EmptyTail
    ));
    // Each case has exactly one nonzero field; the uuid bytes that follow are zero.
    let uuid = JBD2_UUID_SIZE;
    let cases: Vec<(&JBD2DEVSYSTEM, Vec<u8>, usize)> = vec![
        (&plain, v1_tag(1, 0, 0, None), uuid),
        (&plain, v1_tag(0, 1, 0, None), uuid),
        (&plain, v1_tag(0, 0, LAST, None), uuid),
        (&wide, v1_tag(0, 0, 0, Some(1)), uuid),
        (&v3, v3_tag(1, 0, 0, 0), uuid + 4),
        (&v3, v3_tag(0, u32::from(LAST), 0, 0), uuid + 4),
        (&v3, v3_tag(0, 0, 1, 0), uuid + 4),
        (&v3, v3_tag(0, 0, 0, 1), uuid + 4),
    ];
    for (index, (system, payload, tail)) in cases.iter().enumerate() {
        let tags = tagged(system.parse_replay_tags(&descriptor(payload, *tail)));
        assert_eq!(tags.len(), 1, "case {index}");
    }
}

#[test]
fn a_revoke_block_shorter_than_its_header_is_refused() {
    let system = system(0);
    assert_eq!(
        operation(system.parse_revoke_blocks(&[0u8; 8])),
        "jbd2:replay_revoke_header"
    );
}

#[test]
fn the_last_logical_block_follows_the_superblock_and_the_mapping() {
    let mapped = |count: usize| vec![AbsoluteBN::new(1); count];
    let last = |maxlen: u32, first: u32, count: usize| {
        let mut system = system(0);
        system.jbd2_super_block.s_maxlen = maxlen;
        system.jbd2_super_block.s_first = first;
        system.last_logical_block(&mapped(count))
    };
    // Without a mapping the superblock length decides.
    assert_eq!(last(0, 1, 0), None);
    assert_eq!(last(1, 1, 0), None);
    assert_eq!(last(2, 1, 0), Some(1));
    assert_eq!(last(16, 1, 0), Some(15));
    // With a mapping the shorter of the two lengths wins.
    assert_eq!(last(5, 1, 10), Some(4));
    assert_eq!(last(16, 1, 10), Some(9));
    assert_eq!(last(10, 10, 20), None);
    assert_eq!(last(8, 8, 20), None);
    // A zero or too small superblock length is replaced by the mapping.
    assert_eq!(last(0, 0, 10), Some(9));
    assert_eq!(last(0, 1, 10), Some(9));
    assert_eq!(last(5, 8, 20), Some(19));
}

#[test]
fn a_journal_payload_needs_a_whole_block_of_at_least_four_bytes() {
    let update = |len: usize| Jbd2Update(AbsoluteBN::new(1), vec![0x11; len].into_boxed_slice());
    assert!(journal_payload(&update(4), 4).is_ok());
    assert_eq!(
        operation(journal_payload(&update(4), 8)),
        "jbd2:update_block_size"
    );
    assert_eq!(
        operation(journal_payload(&update(2), 2)),
        "jbd2:update_block_size"
    );
    assert!(!journal_payload(&update(8), 8).unwrap().is_escaped());
    let mut magic = update(8);
    magic.1[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
    let payload = journal_payload(&magic, 8).unwrap();
    assert!(payload.is_escaped());
    assert_eq!(&payload.bytes()[..4], &[0; 4]);
}

#[test]
fn a_commit_time_accepts_the_largest_nanosecond_count_and_refuses_the_rest() {
    let at = |sec: i64, nsec: u32| Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec, nsec });
    let largest = at(5, Ext4Timestamp::MAX_NSEC).unwrap();
    assert_eq!(largest.seconds, 5);
    assert_eq!(largest.nanoseconds, Ext4Timestamp::MAX_NSEC);
    assert_eq!(
        operation(at(5, Ext4Timestamp::MAX_NSEC + 1)),
        "jbd2:commit_time_nanoseconds"
    );
    assert_eq!(operation(at(-1, 0)), "jbd2:commit_time_seconds");
}

#[test]
fn the_replay_index_keeps_the_newest_image_per_home_and_stops_at_its_budget() {
    let mut index = ReplayIndex::default();
    index
        .push(AbsoluteBN::new(5), AbsoluteBN::new(100), false)
        .unwrap();
    assert_eq!(index.locations.capacity(), 256);
    index
        .push(AbsoluteBN::new(3), AbsoluteBN::new(101), true)
        .unwrap();
    index
        .push(AbsoluteBN::new(5), AbsoluteBN::new(102), false)
        .unwrap();
    index.finish();
    assert_eq!(
        index.get(AbsoluteBN::new(5)),
        Some((AbsoluteBN::new(102), false))
    );
    assert_eq!(
        index.get(AbsoluteBN::new(3)),
        Some((AbsoluteBN::new(101), true))
    );
    let homes: Vec<u64> = index.home_blocks().map(AbsoluteBN::raw).collect();
    assert_eq!(homes, vec![3, 5]);

    let maximum = 4 * 1024 * 1024 / core::mem::size_of::<ReplayLocation>();
    let mut index = ReplayIndex::default();
    for home in 0..maximum {
        index
            .push(AbsoluteBN::new(home as u64), AbsoluteBN::new(0), false)
            .unwrap();
    }
    assert_eq!(
        operation(index.push(AbsoluteBN::new(0), AbsoluteBN::new(0), false)),
        "jbd2:preview_index_budget"
    );
}

/// Keeps every block written so a test can read the log back.
struct CaptureIo {
    blocks: Vec<u8>,
    flags: Vec<WriteFlags>,
}

impl FilesystemBlockIo for CaptureIo {
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read(&mut self, buffer: &mut [u8], block: AbsoluteBN, _count: u32) -> Ext4Result<()> {
        let start = block.as_usize()? * BLOCK_SIZE;
        buffer.copy_from_slice(&self.blocks[start..start + buffer.len()]);
        Ok(())
    }

    fn write(&mut self, buffer: &[u8], block: AbsoluteBN, _count: u32) -> Ext4Result<()> {
        let start = block.as_usize()? * BLOCK_SIZE;
        self.blocks[start..start + buffer.len()].copy_from_slice(buffer);
        Ok(())
    }

    fn write_with_flags(
        &mut self,
        buffer: &[u8],
        block: AbsoluteBN,
        count: u32,
        flags: WriteFlags,
    ) -> Ext4Result<()> {
        self.flags.push(flags);
        self.write(buffer, block, count)
    }

    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }
}

#[test]
fn a_committed_descriptor_names_its_block_type_and_the_transaction_sequence() {
    let mut system = system(0);
    system.sequence = 41;
    assert!(system.start_committing_transaction().unwrap());
    let mut io = CaptureIo {
        blocks: vec![0; 256 * BLOCK_SIZE],
        flags: Vec::new(),
    };
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 9, nsec: 0 }).unwrap();
    system
        .write_committing_transaction(&mut io, &[], time)
        .expect("commit");
    let descriptor = JOURNAL_START as usize * BLOCK_SIZE + BLOCK_SIZE;
    let header = JournalHeaderS::from_disk_bytes(&io.blocks[descriptor..descriptor + 12]);
    assert_eq!(header.h_magic, JBD2_MAGIC);
    assert_eq!(header.h_blocktype, JBD2_BLOCKTYPE_DESCRIPTOR);
    assert_eq!(header.h_sequence, 41);
}

#[test]
fn a_revoke_block_may_hold_only_its_header_and_each_entry_must_fit_in_the_count() {
    let system = system(0);
    let revoke = |count: u32| {
        let mut bytes = vec![0u8; 16 + 8];
        bytes[12..16].copy_from_slice(&count.to_be_bytes());
        bytes[16..20].copy_from_slice(&77u32.to_be_bytes());
        bytes
    };
    let header_only = system.parse_revoke_blocks(&revoke(16)[..16]).unwrap();
    assert!(header_only.blocks.is_empty());
    let one = system.parse_revoke_blocks(&revoke(20)).unwrap();
    assert_eq!(one.blocks, vec![AbsoluteBN::new(77)]);
    assert_eq!(
        operation(system.parse_revoke_blocks(&revoke(18))),
        "jbd2:replay_revoke_entry"
    );
}

/// Commits three images, then replays the log into a second device and
/// compares every home block.
fn commit_then_replay(features: u32, compat: u32, homes: [u64; 3]) {
    let mut writer = system(features);
    writer.jbd2_super_block.s_feature_compat = compat;
    writer.jbd2_super_block.s_maxlen = JOURNAL_LEN;
    let mut images = Vec::new();
    for (index, home) in homes.iter().enumerate() {
        let mut image = vec![index as u8 + 1; BLOCK_SIZE];
        if index == 1 {
            // A leading journal magic forces the escape path.
            image[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }
        images.push((*home, image.clone()));
        writer
            .running_transaction
            .updates
            .push(Jbd2Update(AbsoluteBN::new(*home), image.into_boxed_slice()));
    }
    writer.running_transaction.updates.remove(0);
    assert!(writer.start_committing_transaction().unwrap());
    let mut io = CaptureIo {
        blocks: vec![0; 512 * BLOCK_SIZE],
        flags: Vec::new(),
    };
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 5, nsec: 6 }).unwrap();
    writer
        .write_committing_transaction(&mut io, &[], time)
        .expect("commit");
    // Descriptor, three payloads and the commit block.
    assert_eq!(writer.used_log_records, 5);
    assert_eq!(writer.sequence, 2);
    let mut replayer = system(features);
    replayer.jbd2_super_block.s_feature_compat = compat;
    replayer.jbd2_super_block.s_maxlen = JOURNAL_LEN;
    replayer.jbd2_super_block.s_start = 1;
    replayer.jbd2_super_block.s_first = 1;
    replayer.jbd2_super_block.s_sequence = 1;
    let status = replayer.replay_with_mapping(&mut io, &[]);
    assert!(status.failure().is_none(), "replay completes");
    for (home, image) in images {
        let start = home as usize * BLOCK_SIZE;
        assert_eq!(&io.blocks[start..start + BLOCK_SIZE], image.as_slice());
    }
}

#[test]
fn a_committed_transaction_replays_to_the_same_images_in_every_checksum_mode() {
    commit_then_replay(0, 0, [10, 11, 12]);
    commit_then_replay(0, JBD2_FEATURE_COMPAT_CHECKSUM, [10, 11, 12]);
    commit_then_replay(JBD2_FEATURE_INCOMPAT_64BIT, 0, [10, 11, 12]);
    commit_then_replay(JBD2_FEATURE_INCOMPAT_CSUM_V2, 0, [10, 11, 12]);
    commit_then_replay(
        JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_CSUM_V3,
        0,
        [10, 11, 12],
    );
}

#[test]
fn a_commit_that_fills_the_log_exactly_succeeds_and_one_more_record_is_refused() {
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
    let ring = (JOURNAL_LEN - 1) as usize;
    // One update needs a descriptor, a payload and a commit block.
    for (used, fits) in [(ring - 3, true), (ring - 2, false)] {
        let mut writer = system(0);
        writer.jbd2_super_block.s_maxlen = JOURNAL_LEN;
        writer.used_log_records = used;
        assert!(writer.start_committing_transaction().unwrap());
        let mut io = CaptureIo {
            blocks: vec![0; 512 * BLOCK_SIZE],
            flags: Vec::new(),
        };
        let result = writer.write_committing_transaction(&mut io, &[], time);
        if fits {
            result.expect("the commit fits");
        } else {
            assert_eq!(operation(result), "jbd2:log_space");
        }
    }
}

#[test]
fn the_commit_block_is_written_as_forced_unit_access_metadata() {
    let mut writer = system(0);
    assert!(writer.start_committing_transaction().unwrap());
    let mut io = CaptureIo {
        blocks: vec![0; 256 * BLOCK_SIZE],
        flags: Vec::new(),
    };
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
    writer
        .write_committing_transaction(&mut io, &[], time)
        .expect("commit");
    assert_eq!(io.flags, vec![WriteFlags::METADATA | WriteFlags::FUA]);
}

#[test]
fn a_high_block_number_needs_the_sixty_four_bit_feature_and_is_kept_whole_with_it() {
    let high_home = (3u64 << 32) | 7;
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
    for (features, accepted) in [
        (0, false),
        (JBD2_FEATURE_INCOMPAT_64BIT, true),
        (
            JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_CSUM_V3,
            true,
        ),
    ] {
        let mut writer = system(features);
        writer.running_transaction.updates[0].0 = AbsoluteBN::new(high_home);
        assert!(writer.start_committing_transaction().unwrap());
        let mut io = CaptureIo {
            blocks: vec![0; 256 * BLOCK_SIZE],
            flags: Vec::new(),
        };
        let result = writer.write_committing_transaction(&mut io, &[], time);
        if !accepted {
            assert_eq!(operation(result), "jbd2:64bit_block_number");
            continue;
        }
        result.expect("commit");
        let at = (JOURNAL_START as usize + 1) * BLOCK_SIZE;
        let tags = tagged(writer.parse_replay_tags(&io.blocks[at..at + BLOCK_SIZE]));
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].block.raw(), high_home);
    }
}

#[test]
fn a_transaction_larger_than_one_descriptor_continues_in_a_clean_second_descriptor() {
    for features in [
        0,
        JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_CSUM_V3,
    ] {
        let mut writer = system(features);
        writer.jbd2_super_block.s_maxlen = 1024;
        writer.max_len = 1024;
        let capacity = writer
            .jbd2_super_block
            .descriptor_tag_capacity(BLOCK_SIZE)
            .unwrap();
        let count = capacity + 3;
        writer.running_transaction.updates.clear();
        for index in 0..count {
            let image = vec![(index % 251) as u8 + 1; BLOCK_SIZE];
            writer.running_transaction.updates.push(Jbd2Update(
                AbsoluteBN::new(2000 + index as u64),
                image.into_boxed_slice(),
            ));
        }
        assert!(writer.start_committing_transaction().unwrap());
        let mut io = CaptureIo {
            blocks: vec![0; 4096 * BLOCK_SIZE],
            flags: Vec::new(),
        };
        let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
        writer
            .write_committing_transaction(&mut io, &[], time)
            .expect("commit");
        assert_eq!(writer.used_log_records, count + 2 + 1);
        let mut replayer = system(features);
        replayer.jbd2_super_block.s_maxlen = 1024;
        replayer.jbd2_super_block.s_start = 1;
        replayer.jbd2_super_block.s_first = 1;
        replayer.jbd2_super_block.s_sequence = 1;
        let status = replayer.replay_with_mapping(&mut io, &[]);
        assert!(status.failure().is_none(), "replay completes");
        for index in 0..count {
            let start = (2000 + index) * BLOCK_SIZE;
            assert_eq!(
                io.blocks[start..start + BLOCK_SIZE],
                vec![(index % 251) as u8 + 1; BLOCK_SIZE][..]
            );
        }
    }
}
