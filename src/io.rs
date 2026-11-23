//! Synchronous block I/O capabilities required by the portable ext4 core.

use bitflags::bitflags;

use crate::error::{Ext4Error, Ext4Result};

/// Typed logical sector identifier exposed by a block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct SectorId(u64);

impl SectorId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub fn as_usize(self) -> Ext4Result<usize> {
        usize::try_from(self.0).map_err(|_| Ext4Error::overflow())
    }

    pub fn to_u32(self) -> Ext4Result<u32> {
        u32::try_from(self.0).map_err(|_| Ext4Error::overflow())
    }

    pub fn checked_add(self, count: u32) -> Ext4Result<Self> {
        self.0
            .checked_add(u64::from(count))
            .map(Self)
            .ok_or_else(Ext4Error::overflow)
    }
}

/// Immutable geometry of the injected block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceGeometry {
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub block_count: u64,
}

impl DeviceGeometry {
    pub const fn new(logical_block_size: u32, block_count: u64) -> Self {
        Self {
            logical_block_size,
            physical_block_size: logical_block_size,
            block_count,
        }
    }

    /// Overrides the physical block size while preserving logical-sector I/O.
    pub const fn with_physical_block_size(mut self, physical_block_size: u32) -> Self {
        self.physical_block_size = physical_block_size;
        self
    }
}

/// Optional durability and maintenance operations provided by a device.
///
/// Setting `fua` requires [`BlockIo::write_with_flags`] to implement
/// [`WriteFlags::FUA`]. A device that cannot do so must leave `fua` clear; the
/// filesystem-block adapter may then emulate FUA with an advertised `flush`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceCapabilities {
    pub read_only: bool,
    pub flush: bool,
    pub barrier: bool,
    pub fua: bool,
    pub discard: bool,
}

bitflags! {
    /// Durability requirements attached to one block write.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WriteFlags: u8 {
        const FUA = 1 << 0;
        const METADATA = 1 << 1;
    }
}

/// Low-level synchronous sector I/O interface used by the filesystem core.
///
/// Device lifecycle, asynchronous completion, IRQ handling, and OS locking are
/// deliberately outside this boundary. Addresses and counts are always in the
/// device's logical-sector units; ext4 filesystem blocks are translated by the
/// core's private block layer.
pub trait BlockIo {
    /// Writes exactly `count * geometry().logical_block_size` bytes.
    fn write(&mut self, buffer: &[u8], sector: SectorId, count: u32) -> Ext4Result<()>;

    /// Reads exactly `count * geometry().logical_block_size` bytes.
    fn read(&mut self, buffer: &mut [u8], sector: SectorId, count: u32) -> Ext4Result<()>;

    /// Writes with an explicit durability requirement.
    ///
    /// Implementations that advertise [`DeviceCapabilities::fua`] must honor
    /// [`WriteFlags::FUA`] here. The default deliberately rejects FUA instead
    /// of reporting false durability.
    fn write_with_flags(
        &mut self,
        buffer: &[u8],
        sector: SectorId,
        count: u32,
        flags: WriteFlags,
    ) -> Ext4Result<()> {
        if flags.is_empty() || flags == WriteFlags::METADATA {
            self.write(buffer, sector, count)
        } else {
            Err(Ext4Error::unsupported())
        }
    }

    /// Returns immutable device geometry.
    fn geometry(&self) -> DeviceGeometry;

    /// Returns optional operations supported by this device.
    fn capabilities(&self) -> DeviceCapabilities;

    /// Flushes device state to stable storage.
    fn flush(&mut self) -> Ext4Result<()>;

    /// Establishes an ordering barrier when supported by the device.
    fn barrier(&mut self) -> Ext4Result<()> {
        Err(Ext4Error::unsupported())
    }

    /// Discards a logical device-sector range when supported by the device.
    fn discard(&mut self, _start: SectorId, _count: u32) -> Ext4Result<()> {
        Err(Ext4Error::unsupported())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use crate::Ext4ErrorKind;

    struct MemoryIo {
        data: Vec<u8>,
    }

    impl BlockIo for MemoryIo {
        fn write(&mut self, buffer: &[u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * 512;
            self.data[start..start + buffer.len()].copy_from_slice(buffer);
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8], sector: SectorId, _count: u32) -> Ext4Result<()> {
            let start = sector.as_usize()? * 512;
            buffer.copy_from_slice(&self.data[start..start + buffer.len()]);
            Ok(())
        }

        fn geometry(&self) -> DeviceGeometry {
            DeviceGeometry::new(512, (self.data.len() / 512) as u64)
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

    #[test]
    fn optional_durability_operations_never_fake_success() {
        let mut io = MemoryIo {
            data: alloc::vec![0; 4096],
        };
        let data = [0x5a; 512];

        assert_eq!(
            io.write_with_flags(&data, SectorId::new(0), 1, WriteFlags::FUA)
                .expect_err("FUA requires an explicit backend implementation")
                .kind(),
            Ext4ErrorKind::Unsupported
        );
        assert_eq!(
            io.barrier()
                .expect_err("barrier requires an explicit backend implementation")
                .kind(),
            Ext4ErrorKind::Unsupported
        );
        assert_eq!(
            io.discard(SectorId::new(0), 1)
                .expect_err("discard requires an explicit backend implementation")
                .kind(),
            Ext4ErrorKind::Unsupported
        );
        io.flush().expect("declared memory flush must succeed");
    }

    #[test]
    fn plain_and_metadata_writes_reach_the_backend_write() {
        let mut io = MemoryIo {
            data: alloc::vec![0; 4096],
        };
        let plain = [0x11; 512];
        let metadata = [0x22; 512];

        io.write_with_flags(&plain, SectorId::new(1), 1, WriteFlags::empty())
            .expect("an unflagged write needs no durability support");
        io.write_with_flags(&metadata, SectorId::new(2), 1, WriteFlags::METADATA)
            .expect("a metadata hint alone needs no durability support");
        assert_eq!(
            io.write_with_flags(
                &metadata,
                SectorId::new(3),
                1,
                WriteFlags::METADATA | WriteFlags::FUA
            )
            .expect_err("FUA with a metadata hint still requires backend support")
            .kind(),
            Ext4ErrorKind::Unsupported
        );

        let mut sectors = [0u8; 1536];
        io.read(&mut sectors, SectorId::new(1), 3)
            .expect("memory read must succeed");
        assert_eq!(&sectors[..512], &plain);
        assert_eq!(&sectors[512..1024], &metadata);
        assert!(sectors[1024..].iter().all(|&byte| byte == 0));
    }

    #[test]
    fn sector_id_conversions_report_overflow_instead_of_truncating() {
        let small = SectorId::new(41);
        assert_eq!(small.raw(), 41);
        assert_eq!(small.as_usize(), Ok(41));
        assert_eq!(small.to_u32(), Ok(41));
        assert_eq!(small.checked_add(1), Ok(SectorId::new(42)));

        let wide = SectorId::new(u64::from(u32::MAX) + 1);
        assert_eq!(wide.to_u32().unwrap_err().kind(), Ext4ErrorKind::Overflow);
        assert_eq!(
            SectorId::new(u64::MAX).checked_add(1).unwrap_err().kind(),
            Ext4ErrorKind::Overflow
        );
        assert_eq!(
            SectorId::new(u64::MAX - 2).checked_add(2),
            Ok(SectorId::new(u64::MAX))
        );
    }

    #[test]
    fn physical_block_size_override_keeps_logical_geometry() {
        let geometry = DeviceGeometry::new(512, 8).with_physical_block_size(4096);
        assert_eq!(geometry.logical_block_size, 512);
        assert_eq!(geometry.physical_block_size, 4096);
        assert_eq!(geometry.block_count, 8);
        assert_eq!(DeviceGeometry::new(512, 8).physical_block_size, 512);
    }
}
