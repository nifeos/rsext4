//! Tests for legacy mapping removal on a file mapped only by direct pointers.
//!
//! Every direct mapping costs no pointer edit and no revoke, so a chunk costs
//! exactly two credits per allocation group it frees blocks in, plus two.
//! The fixture maps logical blocks 0-1 in group 1, 2-3 and 5-6 in group 2, and
//! 7 in group 3; logical block 4 is a hole.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::{
    Clock, DeviceCapabilities, DeviceGeometry, Ext4Timestamp, MkfsOptions, SectorId,
    mkfs_with_options,
};

const BLOCK: usize = 1024;
const BLOCKS: usize = 30000;
/// Two credits for the inode and superblock, two per allocation group.
const ONE_GROUP: usize = 4;

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

struct Fixture {
    journal: Jbd2Dev<MemoryDevice>,
    fs: Ext4FileSystem,
    inode_num: InodeNumber,
    inode: Ext4Inode,
    /// The group of each mapped logical block.
    groups: BTreeMap<u32, u32>,
}

/// Allocates `count` blocks, all from `group`, after filling the groups before it.
fn blocks_in_group(
    fs: &mut Ext4FileSystem,
    journal: &mut Jbd2Dev<MemoryDevice>,
    group: u32,
    count: u32,
) -> Vec<AbsoluteBN> {
    for earlier in 0..group as usize {
        let free = fs.group_descs[earlier].free_blocks_count();
        if free > 0 {
            fs.alloc_blocks(journal, free).expect("fill earlier group");
        }
    }
    let blocks = fs.alloc_blocks(journal, count).expect("allocate");
    for block in &blocks {
        let (found, _) = fs.block_allocator.global_to_group(*block).unwrap();
        assert_eq!(found.raw(), group);
    }
    blocks
}

fn fixture() -> Fixture {
    let mut journal = Jbd2Dev::initial_jbd2dev(
        0,
        MemoryDevice {
            bytes: alloc::vec![0; BLOCKS * BLOCK],
        },
        true,
    );
    journal
        .set_filesystem_block_size(BLOCK)
        .expect("block size");
    mkfs_with_options(
        &mut journal,
        MkfsOptions {
            block_size: BLOCK as u32,
            ..MkfsOptions::default()
        },
    )
    .expect("mkfs");
    let mut fs = Ext4FileSystem::mount(&mut journal).expect("mount");
    crate::file::mkfile(&mut journal, &mut fs, "/legacy", None, None).expect("mkfile");
    let (inode_num, _) = crate::dir::get_inode_with_num(&mut fs, &mut journal, "/legacy")
        .expect("lookup")
        .expect("created");

    let layout: [(u32, &[u32]); 3] = [(1, &[0, 1]), (2, &[2, 3, 5, 6]), (3, &[7])];
    let mut pointers = [0u32; 15];
    let mut groups = BTreeMap::new();
    for (group, logicals) in layout {
        let blocks = blocks_in_group(&mut fs, &mut journal, group, logicals.len() as u32);
        for (&logical, block) in logicals.iter().zip(blocks) {
            pointers[logical as usize] = block.to_u32().unwrap();
            groups.insert(logical, group);
        }
    }
    let mapped = groups.len() as u32;
    fs.modify_inode(&mut journal, inode_num, |inode| {
        inode.i_flags &= !Ext4Inode::EXT4_EXTENTS_FL;
        inode.i_block = pointers;
        inode.i_size_lo = 8 * BLOCK as u32;
        inode.i_size_high = 0;
        inode.i_blocks_lo = mapped * (BLOCK / 512) as u32;
    })
    .expect("publish legacy inode");
    let inode = fs.get_inode_by_num(&mut journal, inode_num).expect("inode");
    fs.sync_filesystem(&mut journal).expect("sync");
    let mut fixture = Fixture {
        journal,
        fs,
        inode_num,
        inode,
        groups,
    };
    assert_eq!(fixture.mapped(), [0, 1, 2, 3, 5, 6, 7]);
    fixture
}

impl Fixture {
    fn mapped(&mut self) -> Vec<u32> {
        crate::indirect::resolve_all_legacy_inode_blocks(
            &self.fs,
            &mut self.journal,
            self.inode_num,
            &self.inode,
        )
        .expect("resolve")
        .into_keys()
        .collect()
    }

    fn mapping_chunk(&mut self, start: u64, end: u64, limit: usize) -> Ext4Result<Option<Chunk>> {
        prepare_legacy_mapping_removal_chunk(
            &mut self.journal,
            &mut self.fs,
            self.inode_num,
            &self.inode,
            start,
            end,
            limit,
        )
        .map(|chunk| chunk.map(Chunk::from))
    }

    fn cleanup_chunk(&mut self, start: u64, end: u64, limit: usize) -> Ext4Result<Option<Chunk>> {
        prepare_legacy_metadata_cleanup_chunk(
            &mut self.journal,
            &mut self.fs,
            self.inode_num,
            &self.inode,
            start,
            end,
            limit,
        )
        .map(|chunk| chunk.map(Chunk::from))
    }

    fn group_of(&self, logicals: &[u32]) -> Vec<u32> {
        let mut groups: Vec<u32> = logicals.iter().map(|l| self.groups[l]).collect();
        groups.dedup();
        groups
    }
}

/// What a chunk exposes: where the cursor moves next and which groups it frees in.
#[derive(Debug, PartialEq)]
struct Chunk {
    next_end: u64,
    groups: Vec<u32>,
}

impl From<LegacyMappingChunk> for Chunk {
    fn from(chunk: LegacyMappingChunk) -> Self {
        Self {
            next_end: chunk.next_end,
            groups: chunk
                .transaction
                .footprint
                .allocation_groups
                .iter()
                .map(|group| group.raw())
                .collect(),
        }
    }
}

fn chunk(next_end: u64, groups: &[u32]) -> Option<Chunk> {
    Some(Chunk {
        next_end,
        groups: groups.to_vec(),
    })
}

fn operation(error: Ext4Error) -> &'static str {
    match error.context() {
        Some(crate::error::ErrorContext::Operation { op }) => op,
        _ => panic!("the error names its operation"),
    }
}

/// Runs `work` on its own thread so a chunk loop that never ends fails the
/// test instead of hanging it.
fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .recv_timeout(Duration::from_secs(3))
        .expect("the chunk loop must finish")
}

#[test]
fn a_mapping_chunk_takes_the_last_run_in_one_group_below_the_cursor() {
    let mut f = fixture();
    let free = usize::MAX;
    // Logical 7 is alone in group 3.
    assert_eq!(f.mapping_chunk(0, 8, free).unwrap(), chunk(7, &[3]));
    // The cursor excludes 7; 5-6 run back to the hole at 4.
    assert_eq!(f.mapping_chunk(0, 7, free).unwrap(), chunk(5, &[2]));
    // The hole at 4 is skipped; 2-3 stop where group 1 begins.
    assert_eq!(f.mapping_chunk(0, 5, free).unwrap(), chunk(2, &[2]));
    assert_eq!(f.group_of(&[1, 2]), [1, 2]);
    // The run stops at the start of the range, and includes a mapping on it.
    assert_eq!(f.mapping_chunk(6, 7, free).unwrap(), chunk(6, &[2]));
    assert_eq!(f.mapping_chunk(5, 7, free).unwrap(), chunk(5, &[2]));
    // Nothing is mapped in the hole or past the end.
    assert!(f.mapping_chunk(4, 5, free).unwrap().is_none());
    assert!(f.mapping_chunk(8, 12, free).unwrap().is_none());
}

#[test]
fn a_mapping_chunk_falls_back_to_one_mapping_only_when_the_run_is_too_costly() {
    let mut f = fixture();
    // The run 5-6 costs exactly one group's credits.
    assert_eq!(f.mapping_chunk(0, 7, ONE_GROUP).unwrap(), chunk(5, &[2]));
    assert_eq!(
        f.mapping_chunk(0, 7, ONE_GROUP + 1).unwrap(),
        chunk(5, &[2])
    );
    let error = f.mapping_chunk(0, 7, ONE_GROUP - 1).unwrap_err();
    assert_eq!(operation(error), "indirect:restart_credits");
}

#[test]
fn cleanup_takes_the_whole_range_when_it_fits() {
    let mut f = fixture();
    assert_eq!(
        f.cleanup_chunk(0, 8, usize::MAX).unwrap(),
        chunk(0, &[1, 2, 3])
    );
    assert_eq!(
        f.cleanup_chunk(2, 7, 2 * 2 + 2 - 1).unwrap(),
        chunk(2, &[2]),
        "one group fits below the limit"
    );
    assert!(f.cleanup_chunk(4, 5, usize::MAX).unwrap().is_none());
    assert!(f.cleanup_chunk(5, 5, usize::MAX).unwrap().is_none());
}

#[test]
fn an_empty_cleanup_range_reads_no_mapping() {
    let mut f = fixture();
    // A single-indirect pointer past the end of the device fails any plan.
    f.inode.i_block[12] = u32::MAX - 1;
    assert!(f.cleanup_chunk(0, 8, usize::MAX).is_err());
    assert!(f.cleanup_chunk(5, 5, usize::MAX).unwrap().is_none());
}

#[test]
fn cleanup_bisects_toward_the_upper_mappings_until_a_chunk_fits() {
    let mut f = fixture();
    let (start, end) = (0, 8);
    let result = bounded(move || f.cleanup_chunk(start, end, ONE_GROUP));
    assert_eq!(result.unwrap(), chunk(7, &[3]));

    let mut f = fixture();
    let result = bounded(move || f.cleanup_chunk(2, 8, ONE_GROUP));
    assert_eq!(result.unwrap(), chunk(7, &[3]));

    // The upper half of 0..4 is 2..4, still mapped in group 2.
    let mut f = fixture();
    let result = bounded(move || f.cleanup_chunk(0, 4, ONE_GROUP));
    assert_eq!(result.unwrap(), chunk(2, &[2]));

    // The upper half of 0..2 is 1..2, so the range narrows to 0..1 and stays in group 1.
    let mut f = fixture();
    let result = bounded(move || f.cleanup_chunk(0, 2, ONE_GROUP));
    assert_eq!(result.unwrap(), chunk(0, &[1]));
}

#[test]
fn cleanup_refuses_a_single_block_that_does_not_fit() {
    for (start, end) in [(0, 8), (3, 7), (7, 8)] {
        let mut f = fixture();
        let result = bounded(move || f.cleanup_chunk(start, end, ONE_GROUP - 1));
        assert_eq!(operation(result.unwrap_err()), "indirect:restart_credits");
    }
}

#[test]
fn a_restart_limit_is_chosen_only_when_the_plan_exceeds_the_journal() {
    let mut f = fixture();
    let plan = crate::indirect::plan_legacy_inode_range_removal(
        &f.fs,
        &mut f.journal,
        f.inode_num,
        &f.inode,
        0,
        8,
    )
    .unwrap();
    let transaction = build_legacy_mapping_transaction(&f.fs, plan).unwrap();
    let limit = legacy_mapping_restart_limit(
        &mut f.journal,
        &mut f.fs,
        f.inode_num,
        &f.inode,
        0,
        8,
        &transaction,
    )
    .unwrap();
    assert_eq!(limit, None);
}

#[test]
fn removal_with_restarts_clears_the_range_in_chunks() {
    let mut f = bounded(|| {
        let mut f = fixture();
        let mut inode = f.inode;
        remove_legacy_mapping_with_restarts(
            &mut f.journal,
            &mut f.fs,
            f.inode_num,
            &mut inode,
            1,
            7,
            ONE_GROUP,
        )
        .expect("remove");
        f.inode = inode;
        f
    });
    assert_eq!(f.mapped(), [0, 7]);
}

#[test]
fn punching_removes_only_whole_blocks_inside_the_range() {
    let mut f = fixture();
    let inode = f.inode;
    // Bytes from the middle of block 1 to the middle of block 6.
    punch_legacy_blocks(
        &mut f.journal,
        &mut f.fs,
        f.inode_num,
        inode,
        BLOCK as u64 + 10,
        6 * BLOCK as u64 + 10,
    )
    .expect("punch");
    f.inode = f.fs.get_inode_by_num(&mut f.journal, f.inode_num).unwrap();
    assert_eq!(f.mapped(), [0, 1, 6, 7]);
}

#[test]
fn a_committed_removal_publishes_a_size_past_four_gibibytes() {
    let mut f = fixture();
    let plan = crate::indirect::plan_legacy_inode_range_removal(
        &f.fs,
        &mut f.journal,
        f.inode_num,
        &f.inode,
        7,
        8,
    )
    .unwrap();
    let transaction = build_legacy_mapping_transaction(&f.fs, plan).unwrap();
    let size = (5u64 << 32) | 0x1234;
    let mut inode = f.inode;
    commit_legacy_mapping_removal(
        &mut f.journal,
        &mut f.fs,
        f.inode_num,
        &mut inode,
        Ext4InodeMetadataUpdate::default(),
        Some(size),
        MetadataTransactionStep {
            start: MetadataTransactionStart::Join,
            payload: transaction,
        },
    )
    .expect("commit");
    assert_eq!(inode.i_size_lo, 0x1234);
    assert_eq!(inode.i_size_high, 5);
}
