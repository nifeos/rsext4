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

fn capture() -> CaptureIo {
    CaptureIo {
        blocks: vec![0; 4096 * BLOCK_SIZE],
        flags: Vec::new(),
    }
}

fn log_block(io: &CaptureIo, relative: u64) -> &[u8] {
    let start = (JOURNAL_START + relative) as usize * BLOCK_SIZE;
    &io.blocks[start..start + BLOCK_SIZE]
}

#[test]
fn revoke_records_split_at_the_capacity_and_keep_every_block_number() {
    for (features, entry) in [(0, 4usize), (JBD2_FEATURE_INCOMPAT_64BIT, 8usize)] {
        let mut writer = system(features);
        writer.jbd2_super_block.s_maxlen = 1024;
        writer.max_len = 1024;
        let capacity = (BLOCK_SIZE - 16) / entry;
        let revoked: Vec<AbsoluteBN> = (0..capacity + 2)
            .map(|index| AbsoluteBN::new(500 + index as u64))
            .collect();
        let mut io = capture();
        writer
            .write_revoke_records(&mut io, &[], 9, BLOCK_SIZE, &revoked)
            .expect("revoke records");
        assert_eq!(writer.head, 3);
        let first = writer.parse_revoke_blocks(log_block(&io, 1)).unwrap();
        let second = writer.parse_revoke_blocks(log_block(&io, 2)).unwrap();
        assert_eq!(first.blocks, revoked[..capacity]);
        assert_eq!(second.blocks, revoked[capacity..]);
        let header = JournalHeaderS::from_disk_bytes(&log_block(&io, 2)[..12]);
        assert_eq!(header.h_blocktype, JBD2_BLOCKTYPE_REVOKE);
        assert_eq!(header.h_sequence, 9);
        // The second record is cleared before it is filled.
        assert!(log_block(&io, 2)[16 + 2 * entry..].iter().all(|b| *b == 0));
    }
}

#[test]
fn revoke_records_need_the_sixty_four_bit_feature_for_a_high_block_and_write_nothing_when_empty() {
    let mut writer = system(0);
    let mut io = capture();
    writer
        .write_revoke_records(&mut io, &[], 1, BLOCK_SIZE, &[])
        .expect("nothing to write");
    assert_eq!(writer.head, 1);
    assert!(io.blocks.iter().all(|b| *b == 0));
    let high = [AbsoluteBN::new(1 << 32)];
    assert_eq!(
        operation(writer.write_revoke_records(&mut io, &[], 1, BLOCK_SIZE, &high)),
        "jbd2:64bit_revoke"
    );
}

#[test]
fn a_checksummed_revoke_block_ends_with_its_own_checksum_and_a_block_too_small_is_refused() {
    let mut writer = system(JBD2_FEATURE_INCOMPAT_CSUM_V3);
    let mut io = capture();
    writer
        .write_revoke_records(&mut io, &[], 3, BLOCK_SIZE, &[AbsoluteBN::new(5)])
        .expect("revoke records");
    let block = log_block(&io, 1);
    let mut zeroed = block.to_vec();
    zeroed[BLOCK_SIZE - 4..].fill(0);
    let expected = jbd2_descriptor_block_csum32(&writer.jbd2_super_block.s_uuid, &zeroed).unwrap();
    assert_eq!(block[BLOCK_SIZE - 4..], expected.to_be_bytes());
    assert_eq!(
        operation(writer.write_revoke_records(&mut io, &[], 3, 8, &[AbsoluteBN::new(5)])),
        "jbd2:revoke_size"
    );
    // Room for the header only: no entry fits.
    assert_eq!(
        operation(writer.write_revoke_records(&mut io, &[], 3, 20, &[AbsoluteBN::new(5)])),
        "jbd2:revoke_capacity"
    );
}

#[test]
fn recording_an_abort_keeps_an_earlier_error_and_writes_with_forced_unit_access() {
    let mut writer = system(0);
    let mut io = capture();
    writer.record_abort_with_mapping(&mut io, &[]).unwrap();
    assert_eq!(writer.jbd2_super_block.s_errno, JBD2_DISK_ERROR_IO);
    assert_eq!(io.flags, vec![WriteFlags::METADATA | WriteFlags::FUA]);
    writer.jbd2_super_block.s_errno = 5;
    writer.record_abort_with_mapping(&mut io, &[]).unwrap();
    assert_eq!(writer.jbd2_super_block.s_errno, 5);
    writer
        .clear_recorded_error_with_mapping(&mut io, &[])
        .unwrap();
    assert_eq!(writer.jbd2_super_block.s_errno, 0);
    assert_eq!(io.flags.len(), 3);
    assert!(
        io.flags
            .iter()
            .all(|f| *f == WriteFlags::METADATA | WriteFlags::FUA)
    );
}

/// A block device with a chosen block size and no stored data.
struct SizedIo(usize);

impl FilesystemBlockIo for SizedIo {
    fn block_size(&self) -> usize {
        self.0
    }

    fn read(&mut self, buffer: &mut [u8], _block: AbsoluteBN, _count: u32) -> Ext4Result<()> {
        buffer.fill(0);
        Ok(())
    }

    fn write(&mut self, _buffer: &[u8], _block: AbsoluteBN, _count: u32) -> Ext4Result<()> {
        Ok(())
    }

    fn write_with_flags(
        &mut self,
        _buffer: &[u8],
        _block: AbsoluteBN,
        _count: u32,
        _flags: WriteFlags,
    ) -> Ext4Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> Ext4Result<()> {
        Ok(())
    }
}

#[test]
fn the_journal_superblock_needs_a_block_of_at_least_1024_bytes() {
    let mut writer = system(0);
    writer
        .write_journal_superblock_with_mapping_flags(&mut SizedIo(1024), &[], WriteFlags::METADATA)
        .expect("1024 is enough");
    assert_eq!(
        operation(writer.write_journal_superblock_with_mapping_flags(
            &mut SizedIo(1023),
            &[],
            WriteFlags::METADATA
        )),
        "jbd2:small_block"
    );
}

fn commit_one(
    writer: &mut JBD2DEVSYSTEM,
    io: &mut CaptureIo,
    homes: &[(u64, u8)],
    revoked: &[u64],
) {
    writer.running_transaction.updates = homes
        .iter()
        .map(|(home, fill)| {
            Jbd2Update(
                AbsoluteBN::new(*home),
                vec![*fill; BLOCK_SIZE].into_boxed_slice(),
            )
        })
        .collect();
    writer.running_transaction.revoked_blocks =
        revoked.iter().map(|b| AbsoluteBN::new(*b)).collect();
    assert!(writer.start_committing_transaction().unwrap());
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
    writer
        .write_committing_transaction(io, &[], time)
        .expect("commit");
}

fn home(io: &CaptureIo, block: u64) -> u8 {
    io.blocks[block as usize * BLOCK_SIZE]
}

#[test]
fn a_checkpoint_writes_each_image_home_unless_a_later_transaction_replaces_or_revokes_it() {
    let mut writer = system(0);
    writer.jbd2_super_block.s_maxlen = 1024;
    writer.max_len = 1024;
    let mut io = capture();
    commit_one(
        &mut writer,
        &mut io,
        &[(2000, 1), (2001, 1), (2002, 1)],
        &[],
    );
    commit_one(&mut writer, &mut io, &[(2000, 2)], &[2001]);
    commit_one(&mut writer, &mut io, &[(2003, 3)], &[]);
    let second_start = writer.checkpoint_transactions[1].log_start;
    let used = writer.used_log_records;
    let first_records = writer.checkpoint_transactions[0].log_records;
    io.flags.clear();

    // Only the first transaction: its images that the second replaces or
    // revokes stay unwritten.
    assert!(
        writer
            .checkpoint_transactions_with_mapping(&mut io, &[], 1)
            .unwrap()
    );
    assert_eq!(home(&io, 2000), 0);
    assert_eq!(home(&io, 2001), 0);
    assert_eq!(home(&io, 2002), 1);
    assert_eq!(writer.jbd2_super_block.s_start, second_start);
    assert_eq!(writer.jbd2_super_block.s_sequence, 2);
    assert_eq!(writer.used_log_records, used - first_records);
    assert_eq!(writer.checkpoint_transactions.len(), 2);
    assert_eq!(io.flags, vec![WriteFlags::METADATA | WriteFlags::FUA]);

    // Both remaining: the newest image of a block wins, and with nothing left
    // the log is empty.
    assert!(
        writer
            .checkpoint_transactions_with_mapping(&mut io, &[], 5)
            .unwrap()
    );
    assert_eq!(home(&io, 2000), 2);
    assert_eq!(home(&io, 2001), 0);
    assert_eq!(home(&io, 2003), 3);
    assert_eq!(writer.jbd2_super_block.s_start, 0);
    assert_eq!(writer.jbd2_super_block.s_sequence, writer.sequence);
    assert_eq!(writer.used_log_records, 0);
    assert!(
        !writer
            .checkpoint_transactions_with_mapping(&mut io, &[], 5)
            .unwrap()
    );
}

#[test]
fn a_checkpoint_of_two_transactions_does_not_write_the_older_image_over_the_newer() {
    let mut writer = system(0);
    writer.jbd2_super_block.s_maxlen = 1024;
    writer.max_len = 1024;
    let mut io = capture();
    commit_one(&mut writer, &mut io, &[(2000, 1), (2005, 1)], &[]);
    commit_one(&mut writer, &mut io, &[(2000, 2)], &[2005]);
    assert!(
        writer
            .checkpoint_transactions_with_mapping(&mut io, &[], 2)
            .unwrap()
    );
    assert_eq!(home(&io, 2000), 2);
    assert_eq!(home(&io, 2005), 0);
}

/// Commits one image with the compat checksum, lets `edit` change the commit
/// block, replays, and returns the failure operation if there is one.
fn compat_replay(edit: impl Fn(&mut CommitHeader)) -> (Option<&'static str>, u8) {
    let mut writer = system(0);
    writer.jbd2_super_block.s_feature_compat = JBD2_FEATURE_COMPAT_CHECKSUM;
    writer.jbd2_super_block.s_maxlen = 1024;
    writer.max_len = 1024;
    let mut io = capture();
    commit_one(&mut writer, &mut io, &[(2000, 7)], &[]);
    let at = (JOURNAL_START as usize + 3) * BLOCK_SIZE;
    let mut commit = CommitHeader::from_disk_bytes(&io.blocks[at..at + BLOCK_SIZE]);
    assert_eq!(commit.h_header.h_blocktype, JBD2_BLOCKTYPE_COMMIT);
    edit(&mut commit);
    commit.to_disk_bytes(&mut io.blocks[at..at + BLOCK_SIZE]);
    let mut replayer = system(0);
    replayer.jbd2_super_block.s_feature_compat = JBD2_FEATURE_COMPAT_CHECKSUM;
    replayer.jbd2_super_block.s_maxlen = 1024;
    replayer.jbd2_super_block.s_start = 1;
    replayer.jbd2_super_block.s_sequence = 1;
    let status = replayer.replay_with_mapping(&mut io, &[]);
    let op = status
        .failure()
        .map(|failure| match failure.cause().context() {
            Some(ErrorContext::Operation { op }) => op,
            _ => "none",
        });
    (op, home(&io, 2000))
}

#[test]
fn a_compat_checksum_must_match_in_type_size_and_value_unless_the_whole_field_is_unused() {
    assert_eq!(compat_replay(|_| {}), (None, 7));
    // The field may be left entirely unused.
    assert_eq!(
        compat_replay(|c| {
            c.h_chksum_type = 0;
            c.h_chksum_size = 0;
            c.h_chksum[0] = 0;
        }),
        (None, 7)
    );
    let bad = Some("jbd2:replay_compat_checksum");
    assert_eq!(compat_replay(|c| c.h_chksum[0] ^= 1), (bad, 0));
    assert_eq!(compat_replay(|c| c.h_chksum_type = 0), (bad, 0));
    assert_eq!(compat_replay(|c| c.h_chksum_size = 0), (bad, 0));
    assert_eq!(compat_replay(|c| c.h_chksum_type = 9), (bad, 0));
    assert_eq!(compat_replay(|c| c.h_chksum_size = 9), (bad, 0));
    // Unused type and size with a stray value is not unused.
    assert_eq!(
        compat_replay(|c| {
            c.h_chksum_type = 0;
            c.h_chksum_size = 0;
        }),
        (bad, 0)
    );
    assert_eq!(
        compat_replay(|c| {
            c.h_chksum_type = 0;
            c.h_chksum[0] = 0;
        }),
        (bad, 0)
    );
    assert_eq!(
        compat_replay(|c| {
            c.h_chksum_size = 0;
            c.h_chksum[0] = 0;
        }),
        (bad, 0)
    );
}

#[test]
fn replay_failure_accessors_report_what_was_stored() {
    let failure = ReplayFailure::at(
        JournalReplayPhase::Scan,
        Ext4Error::corrupted().with_operation("x"),
        7,
    );
    assert_eq!(failure.restart_rel(), Some(7));
    assert!(failure.persistence_error().is_none());
    let failure = failure.with_persistence_error(Ext4Error::busy());
    assert!(failure.persistence_error().is_some());
}

#[test]
fn a_commit_block_with_the_wrong_magic_or_sequence_ends_the_scan_without_replaying() {
    assert_eq!(compat_replay(|c| c.h_header.h_magic ^= 1), (None, 0));
    assert_eq!(compat_replay(|c| c.h_header.h_sequence = 99), (None, 0));
}

#[test]
fn a_version_three_commit_block_may_carry_the_checksum_of_its_header_alone() {
    let features = JBD2_FEATURE_INCOMPAT_CSUM_V3;
    let mut writer = system(features);
    writer.jbd2_super_block.s_maxlen = 1024;
    writer.max_len = 1024;
    let mut io = capture();
    commit_one(&mut writer, &mut io, &[(2000, 7)], &[]);
    // Stray bytes after the header change the full checksum but not the
    // header-only one that the block carries.
    let at = (JOURNAL_START as usize + 3) * BLOCK_SIZE;
    io.blocks[at + 200] = 1;
    let mut replayer = system(features);
    replayer.jbd2_super_block.s_maxlen = 1024;
    replayer.jbd2_super_block.s_start = 1;
    replayer.jbd2_super_block.s_sequence = 1;
    assert!(
        replayer
            .replay_with_mapping(&mut io, &[])
            .failure()
            .is_none()
    );
    assert_eq!(home(&io, 2000), 7);
}

#[test]
fn replay_accepts_a_block_exactly_as_large_as_the_commit_header_and_refuses_a_smaller_one() {
    // The scan accepts the exact size and then stops at the first block; the
    // follow-up superblock write is what refuses a block this small.
    for (size, expected) in [
        (JBD2_COMMIT_HEADER_SIZE, "jbd2:small_block"),
        (JBD2_COMMIT_HEADER_SIZE - 1, "jbd2:replay_block_size"),
    ] {
        let mut replayer = system(0);
        replayer.jbd2_super_block.s_maxlen = 16;
        replayer.jbd2_super_block.s_start = 1;
        replayer.jbd2_super_block.s_sequence = 1;
        let failure = replayer
            .replay_with_mapping(&mut SizedIo(size), &[])
            .failure()
            .expect("a block this small cannot be replayed");
        assert_eq!(
            operation(Err::<(), _>(failure.cause())),
            expected,
            "size {size}"
        );
    }
}

#[test]
fn committing_an_empty_transaction_does_nothing_and_a_full_one_commits() {
    let time = Jbd2CommitTimestamp::try_from(Ext4Timestamp { sec: 1, nsec: 0 }).unwrap();
    let mut io = capture();
    let mut writer = system(0);
    assert!(
        writer
            .commit_transaction_with_mapping(&mut io, &[], time)
            .unwrap()
    );
    assert_eq!(writer.checkpoint_transactions.len(), 1);
    writer.running_transaction.updates.clear();
    assert!(
        !writer
            .commit_transaction_with_mapping(&mut io, &[], time)
            .unwrap()
    );
    assert_eq!(writer.checkpoint_transactions.len(), 1);
}

mod mounted {
    use super::*;
    use crate::{
        BLOCK_SIZE as FS_BLOCK_SIZE, DeviceCapabilities, DeviceGeometry, Ext4FileSystem,
        Ext4Timestamp as Timestamp, SectorId, mkfs, runtime::Clock,
    };

    struct MemoryDevice {
        bytes: Vec<u8>,
    }

    impl BlockIo for MemoryDevice {
        fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * FS_BLOCK_SIZE;
            let end = start + buffer.len();
            self.bytes
                .get_mut(start..end)
                .ok_or_else(Ext4Error::io)?
                .copy_from_slice(buffer);
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * FS_BLOCK_SIZE;
            let end = start + buffer.len();
            buffer.copy_from_slice(self.bytes.get(start..end).ok_or_else(Ext4Error::io)?);
            Ok(())
        }

        fn geometry(&self) -> DeviceGeometry {
            DeviceGeometry::new(
                FS_BLOCK_SIZE as u32,
                (self.bytes.len() / FS_BLOCK_SIZE) as u64,
            )
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
        fn now(&self) -> Ext4Result<Timestamp> {
            Ok(Timestamp::new(1_700_000_000, 0))
        }
    }

    fn mounted() -> (Jbd2Dev<MemoryDevice>, Ext4FileSystem) {
        let device = MemoryDevice {
            bytes: vec![0; 16 * 1024 * FS_BLOCK_SIZE],
        };
        let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
        mkfs(&mut journal).expect("mkfs");
        let filesystem = Ext4FileSystem::mount(&mut journal).expect("mount");
        (journal, filesystem)
    }

    #[test]
    fn the_created_journal_has_a_described_superblock_and_an_empty_log() {
        let (mut journal, mut filesystem) = mounted();
        let first = filesystem.journal_sb_block_start.expect("journal mapped");
        let data = filesystem
            .datablock_cache
            .get_or_load(&mut journal, first)
            .unwrap()
            .data
            .clone();
        let superblock = JournalSuperBlock::decode_checked(&data).unwrap();
        assert_eq!(superblock.s_maxlen, CREATED_JOURNAL_BLOCK_COUNT);
        assert_eq!(superblock.s_first, 1);
        assert_eq!(superblock.s_start, 0);
        assert_eq!(superblock.s_sequence, 1);
        assert_eq!(superblock.s_blocksize as usize, FS_BLOCK_SIZE);
        assert_eq!(superblock.s_uuid, filesystem.superblock.s_uuid);
        assert_ne!(
            superblock.s_feature_incompat & JBD2_FEATURE_INCOMPAT_REVOKE,
            0
        );
        let has_64bit = filesystem
            .superblock
            .has_feature_incompat(crate::superblock::Ext4Superblock::EXT4_FEATURE_INCOMPAT_64BIT);
        assert_eq!(
            superblock.s_feature_incompat & JBD2_FEATURE_INCOMPAT_64BIT != 0,
            has_64bit
        );
        let checksummed = ext4_superblock_has_metadata_csum(&filesystem.superblock);
        assert_eq!(
            superblock.s_feature_incompat & JBD2_FEATURE_INCOMPAT_CSUM_V3 != 0,
            checksummed
        );
        assert_eq!(
            superblock.s_checksum_type,
            if checksummed { JBD2_CRC32C_CHKSUM } else { 0 }
        );
        let inode = filesystem
            .get_inode_by_num(
                &mut journal,
                InodeNumber::new(JOURNAL_FILE_INODE as u32).unwrap(),
            )
            .unwrap();
        let size = u64::from(CREATED_JOURNAL_BLOCK_COUNT) * FS_BLOCK_SIZE as u64;
        assert_eq!(u64::from(inode.i_size_lo), size);
        assert_eq!(u64::from(inode.i_blocks_lo), size / 512);
        assert_eq!(inode.i_links_count, 1);
        assert_eq!(inode.i_mode & Ext4Inode::S_IFREG, Ext4Inode::S_IFREG);
        assert_eq!(inode.i_mode & 0o777, 0o600);
        // Blocks the log has not reached start clean.
        let mut block = vec![0xffu8; FS_BLOCK_SIZE];
        for offset in [200u32, 1000, CREATED_JOURNAL_BLOCK_COUNT - 1] {
            journal
                .read_blocks(&mut block, first.checked_add(offset).unwrap(), 1)
                .unwrap();
            assert!(block.iter().all(|byte| *byte == 0), "block {offset}");
        }
    }

    #[test]
    fn zeroing_block_runs_clears_each_run_and_leaves_the_gaps() {
        let (mut journal, _filesystem) = mounted();
        let junk = vec![0xaau8; FS_BLOCK_SIZE];
        for block in 14_000u64..14_016 {
            journal
                .write_blocks_durable(&junk, AbsoluteBN::new(block), 1)
                .unwrap();
        }
        let blocks: Vec<AbsoluteBN> = [14_000u64, 14_001, 14_002, 14_010, 14_011]
            .iter()
            .map(|block| AbsoluteBN::new(*block))
            .collect();
        zero_allocated_block_runs(&mut journal, &blocks).expect("zero");
        let mut data = vec![0u8; FS_BLOCK_SIZE];
        for block in 14_000u64..14_016 {
            journal
                .read_blocks(&mut data, AbsoluteBN::new(block), 1)
                .unwrap();
            let zeroed = [14_000, 14_001, 14_002, 14_010, 14_011].contains(&block);
            assert_eq!(data[0], if zeroed { 0 } else { 0xaa }, "block {block}");
        }
        zero_allocated_block_runs(&mut journal, &[]).expect("an empty list is a no-op");
    }
}
