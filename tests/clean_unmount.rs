//! Clean publication must follow the durable journal checkpoint tail.
//!
//! An unmount checkpoints every journal transaction, resets the journal
//! start to zero and clears the `RECOVER` flag in the ext4 superblock. If
//! the cleared flag reaches the disk first, a power cut between the two writes leaves
//! a volume that claims no recovery is needed while the journal still holds
//! a transaction. These tests cut power after every write of an unmount in
//! turn and check the image that is left.

// Miri runs only the tests that reach unsafe code, and this file reaches none.
#![cfg(not(miri))]

use std::{cell::RefCell, rc::Rc};

use rsext4::{endian::DiskFormat, superblock::Ext4Superblock, *};

const DEVICE_BYTES: usize = 64 * 1024 * 1024;
const FILE_PATH: &str = "/clean-order";
const FILE_CONTENTS: &[u8] = b"persisted";
/// Byte offset of `s_start` in the journal superblock (big-endian).
const JOURNAL_START_OFFSET: usize = 28;

struct Storage {
    bytes: Vec<u8>,
    /// Writes that still reach the medium; `None` means no power cut is armed.
    writes_before_cut: Option<usize>,
    /// Writes that reached the medium since the counter was last reset.
    writes_landed: usize,
}

#[derive(Clone)]
struct CutDevice(Rc<RefCell<Storage>>);

impl CutDevice {
    fn new(bytes: Vec<u8>) -> Self {
        Self(Rc::new(RefCell::new(Storage {
            bytes,
            writes_before_cut: None,
            writes_landed: 0,
        })))
    }
}

impl BlockIo for CutDevice {
    fn read(&mut self, output: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let start = sector.as_usize()? * BLOCK_SIZE;
        output.copy_from_slice(&self.0.borrow().bytes[start..start + output.len()]);
        Ok(())
    }

    fn write(&mut self, input: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
        let mut storage = self.0.borrow_mut();
        match storage.writes_before_cut {
            Some(0) => return Err(Ext4Error::io()),
            Some(remaining) => storage.writes_before_cut = Some(remaining - 1),
            None => {}
        }
        let start = sector.as_usize()? * BLOCK_SIZE;
        storage.bytes[start..start + input.len()].copy_from_slice(input);
        storage.writes_landed += 1;
        Ok(())
    }

    fn geometry(&self) -> DeviceGeometry {
        DeviceGeometry::new(
            BLOCK_SIZE as u32,
            (self.0.borrow().bytes.len() / BLOCK_SIZE) as u64,
        )
    }

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            flush: true,
            ..Default::default()
        }
    }

    fn flush(&mut self) -> Ext4Result<()> {
        if self.0.borrow().writes_before_cut == Some(0) {
            return Err(Ext4Error::io());
        }
        Ok(())
    }
}

impl Clock for CutDevice {
    fn now(&self) -> Ext4Result<Ext4Timestamp> {
        Ok(Ext4Timestamp::new(1_700_000_000, 0))
    }
}

fn formatted_image() -> Vec<u8> {
    let device = CutDevice::new(vec![0; DEVICE_BYTES]);
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device.clone(), true);
    mkfs(&mut journal).expect("mkfs");
    device.0.borrow().bytes.clone()
}

fn recover_flag_is_set(bytes: &[u8]) -> bool {
    let offset = SUPERBLOCK_OFFSET as usize;
    let superblock = Ext4Superblock::from_disk_bytes(&bytes[offset..offset + SUPERBLOCK_SIZE]);
    superblock.s_feature_incompat & Ext4Superblock::EXT4_FEATURE_INCOMPAT_RECOVER != 0
}

fn journal_start(bytes: &[u8], journal_superblock_offset: usize) -> u32 {
    let field = journal_superblock_offset + JOURNAL_START_OFFSET;
    u32::from_be_bytes(bytes[field..field + 4].try_into().unwrap())
}

/// Mounts `image`, writes one file, then unmounts with power cut after
/// `writes_before_cut` writes of the unmount (`None`: no cut). Returns the
/// image left on the medium, the byte offset of the journal superblock, the
/// number of unmount writes that landed and whether the unmount succeeded.
fn unmount_with_cut(
    image: &[u8],
    writes_before_cut: Option<usize>,
) -> (Vec<u8>, usize, usize, bool) {
    let device = CutDevice::new(image.to_vec());
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device.clone(), true);
    let mut filesystem = Ext4FileSystem::mount(&mut journal).expect("mount");
    mkfile(
        &mut journal,
        &mut filesystem,
        FILE_PATH,
        Some(FILE_CONTENTS),
        None,
    )
    .expect("mkfile");
    let journal_superblock_offset = filesystem
        .journal_sb_block_start
        .expect("journal superblock block")
        .raw() as usize
        * filesystem.superblock.block_size() as usize;
    {
        let mut storage = device.0.borrow_mut();
        storage.writes_landed = 0;
        storage.writes_before_cut = writes_before_cut;
    }
    let unmounted = filesystem.umount(&mut journal).is_ok();
    let storage = device.0.borrow();
    (
        storage.bytes.clone(),
        journal_superblock_offset,
        storage.writes_landed,
        unmounted,
    )
}

#[test]
fn clean_unmount_clears_recover_and_the_journal_start() {
    let (bytes, journal_superblock_offset, writes, unmounted) =
        unmount_with_cut(&formatted_image(), None);
    assert!(unmounted, "an unmount without a power cut must succeed");
    assert_ne!(writes, 0, "the unmount must write");
    assert!(
        !recover_flag_is_set(&bytes),
        "a clean unmount clears RECOVER"
    );
    assert_eq!(
        journal_start(&bytes, journal_superblock_offset),
        0,
        "a clean unmount leaves an empty journal"
    );
}

#[test]
fn power_cut_during_unmount_never_clears_recover_before_the_journal_start() {
    let image = formatted_image();
    let (_, _, unmount_writes, _) = unmount_with_cut(&image, None);
    assert_ne!(unmount_writes, 0, "the unmount must write");

    for cut in 0..unmount_writes {
        let (bytes, journal_superblock_offset, landed, unmounted) =
            unmount_with_cut(&image, Some(cut));
        assert_eq!(
            landed, cut,
            "cut {cut}: the device must stop after {cut} writes"
        );
        assert!(
            !unmounted,
            "cut {cut}: an unmount that lost writes must fail"
        );

        let start = journal_start(&bytes, journal_superblock_offset);
        assert!(
            recover_flag_is_set(&bytes) || start == 0,
            "cut {cut}: RECOVER is clear while the journal still starts at block {start}"
        );

        // The image left by the cut must mount, replaying what the journal
        // holds, and every committed file must read back whole.
        let device = CutDevice::new(bytes);
        let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
        let mut filesystem = Ext4FileSystem::mount(&mut journal)
            .unwrap_or_else(|error| panic!("cut {cut}: mount after the cut failed: {error:?}"));
        match read_file(&mut journal, &mut filesystem, FILE_PATH) {
            Ok(contents) => assert_eq!(contents, FILE_CONTENTS, "cut {cut}: file contents"),
            Err(error) => assert_eq!(
                error.kind(),
                Ext4ErrorKind::NotFound,
                "cut {cut}: reading the file failed: {error:?}"
            ),
        }
    }
}
