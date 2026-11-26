//! The body of each fuzz target, one function per target over the fuzz input, so another fuzz
//! workspace can run the same targets by calling them. Each file under `fuzz_targets/` is the
//! `fuzz_target!` wrapper of the function of its name.

use rsext4::{
    DirectoryCursor, FileName, MountOptions,
    blockgroup_description::{BlockGroupDescTable, Ext4GroupDesc},
    bmalloc::BGIndex,
    checksum::{verify_ext4_dirblock_checksum, verify_ext4_dx_checksum},
    endian::DiskFormat,
    entries::{DirEntryIterator, classic_dir::list_entries},
    extents_tree::ExtentTree,
    superblock::Ext4Superblock,
};

use crate::{mount, read_inode_fully, walk_directories};

/// Decodes a 1024-byte ext4 superblock and runs every check mount runs on it before it reads
/// anything else.
pub fn superblock(data: &[u8]) {
    if data.len() < 1024 {
        return;
    }
    let superblock = Ext4Superblock::from_disk_bytes(&data[..1024]);
    let _ = superblock.verify_superblock();
    let _ = superblock.checked_block_size();
    let _ = superblock.checked_cluster_size();
    let _ = superblock.checked_block_groups_count();
    let _ = superblock.primary_gdt_byte_offset();
    let _ = superblock.validate_geometry();
    let _ = superblock.descs_per_block();
    let _ = superblock.inode_table_blocks();
    let _ = superblock.blocks_count();
    let _ = superblock.free_blocks_count();
    let _ = superblock.reserved_blocks_count();
}

/// Decodes the group descriptors that follow a superblock. The input is a 1024-byte superblock
/// followed by the descriptor table; the superblock gives the descriptor size and group count, as
/// it does at mount.
pub fn group_descriptor_table(data: &[u8]) {
    if data.len() < 1024 {
        return;
    }
    let (superblock, table) = data.split_at(1024);
    let superblock = Ext4Superblock::from_disk_bytes(superblock);
    if superblock.validate_geometry().is_err() {
        return;
    }
    let desc_size = usize::from(superblock.get_desc_size());
    let Ok(groups) = superblock.checked_block_groups_count() else {
        return;
    };
    let groups = groups.min((table.len() / desc_size.max(1)) as u32);
    for record in table.chunks_exact(desc_size).take(groups as usize) {
        if let Ok(desc) = Ext4GroupDesc::decode_checked(record) {
            let _ = desc.block_bitmap_csum(&superblock);
            let _ = desc.inode_bitmap_csum(&superblock);
            let _ = (desc.is_uninit_bg(), desc.is_inode_table_zeroed());
        }
    }
    let view = BlockGroupDescTable::new(table, desc_size, groups);
    for index in 0..groups {
        let _ = view.get_desc(BGIndex::new(index));
    }
    let _ = (
        view.total_free_blocks(),
        view.total_free_inodes(),
        view.total_used_dirs(),
    );
}

/// Parses one extent-tree node: the 60 bytes of an inode's `i_block`, or a whole index or leaf
/// block.
pub fn extent_tree(data: &[u8]) {
    let _ = ExtentTree::parse_node(data);
}

/// Parses one directory block the way a linear directory read does, and verifies its checksum
/// tail against a fixed superblock. The input is the block; its length is the block size.
pub fn directory_block(data: &[u8]) {
    for (entry, _offset) in DirEntryIterator::new(data) {
        let _ = (entry.name_str(), entry.is_dot(), entry.is_dotdot());
    }
    let _ = list_entries(data);
    let mut superblock = Ext4Superblock::default();
    superblock.s_feature_ro_compat |= Ext4Superblock::EXT4_FEATURE_RO_COMPAT_METADATA_CSUM;
    let _ = verify_ext4_dirblock_checksum(&superblock, 2, 0, data);
    let _ = verify_ext4_dx_checksum(&superblock, 2, 0, data);
}

/// Mounts an image read-only and looks up every name it lists, and one it does not, in every
/// directory, so an indexed directory's hash-tree path parses its index blocks.
pub fn htree_lookup(data: &[u8]) {
    let Some(mut filesystem) = mount(data, MountOptions::read_only_no_journal_replay()) else {
        return;
    };
    walk_directories(&mut filesystem, |filesystem, directory, entries| {
        for entry in entries.iter().take(256) {
            if let Ok(name) = FileName::new(&entry.name) {
                let _ = filesystem.lookup_child(directory, name);
            }
        }
        if let Ok(name) = FileName::new(b"not-in-the-directory") {
            let _ = filesystem.lookup_child(directory, name);
        }
    });
}

/// Mounts an image read-only and reads everything a bounded walk reaches: each directory, each
/// inode's metadata, extents and extended attributes, and the start of each file.
pub fn mount_and_walk(data: &[u8]) {
    let Some(mut filesystem) = mount(data, MountOptions::read_only_no_journal_replay()) else {
        return;
    };
    let _ = filesystem.statfs();
    walk_directories(&mut filesystem, |filesystem, _directory, entries| {
        for entry in entries {
            read_inode_fully(filesystem, entry.inode);
        }
    });
    let _ = filesystem.unmount();
}

/// Mounts an image read-write, which replays its journal when the image needs recovery, then
/// reads the root directory and unmounts.
pub fn journal_replay(data: &[u8]) {
    let Some(mut filesystem) = mount(data, MountOptions::read_write()) else {
        return;
    };
    let root = filesystem.root_inode();
    let _ = filesystem.read_directory(root, DirectoryCursor::Start, 64);
    let _ = filesystem.unmount();
}
