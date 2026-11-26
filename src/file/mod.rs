//! File and inode data operations.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    blockdev::*,
    bmalloc::{AbsoluteBN, InodeNumber},
    checksum::update_ext4_dirblock_csum32,
    dir::*,
    disknode::*,
    entries::*,
    error::*,
    ext4::*,
    extents_tree::*,
    loopfile::*,
    metadata::{Ext4DtimeUpdate, Ext4InodeMetadataUpdate},
    superblock::Ext4Superblock,
};

mod blocks;
mod create;
mod delete;
mod extent_map;
mod io;
mod link;
mod read_plan;
mod rename;
mod write;
mod xattr;
mod xattr_extent;

pub use blocks::build_file_block_mapping_with_inode_num;
pub(crate) use create::{
    CreateInodePayload, create_inode_at, discard_unpublished_inode, error_after_cleanup,
};
pub use create::{create_symbol_link, create_symbol_link_with_owner, mkfile, mkfile_with_owner};
pub(crate) use delete::{
    DentryReplacement, find_named_entry_in_parent, preflight_inode_free, remove_named_entry_at,
    replace_named_entry_at, unlink_empty_directory_at, unlink_inode_at,
};
pub use delete::{
    UnlinkOutcome, delete_dir, delete_file, is_dir_empty, reap_unlinked_inode, unlink,
};
pub use extent_map::{
    FileExtent, FileExtentMap, FileExtentState, FileExtentTarget, inspect_inode_extents,
};
pub use io::{
    InodeResize, PreallocationOptions, RangeOperation, ZeroRangeOptions, collapse_range_inode,
    insert_range_inode, operate_inode_range, preallocate_inode, punch_hole_inode, read_file,
    read_inode_data_into, truncate, truncate_inode, zero_range_inode,
};
pub(crate) use io::{
    finish_pending_truncate, recover_linked_truncate_inode, truncate_inode_for_reap,
};
pub use link::link;
pub(crate) use link::link_inode_at;
pub use read_plan::{CompletedFileRead, PreparedFileRead};
pub(crate) use read_plan::{FileBlockRead, FileReadMapping, MAX_READ_BYTES};
pub(crate) use rename::{RenameEntryRequest, rename_inode_at};
pub use rename::{RenameOptions, RenameOutcome, rename};
pub(crate) use write::{CompletedFileWrite, PreparedFileWrite};
pub use write::{write_file, write_inode_data};
pub use xattr::{XattrName, XattrNamespace, XattrSetMode};
pub(crate) use xattr::{get_inode_xattr, list_inode_xattrs, remove_inode_xattr, set_inode_xattr};

pub(crate) use crate::dir::ParentDirEntry;

/// Returns the directory-entry type Linux derives from a recognized inode mode.
pub(crate) const fn directory_entry_type_for_mode(mode: u16) -> Option<u8> {
    match mode & Ext4Inode::S_IFMT {
        Ext4Inode::S_IFREG => Some(Ext4DirEntry2::EXT4_FT_REG_FILE),
        Ext4Inode::S_IFDIR => Some(Ext4DirEntry2::EXT4_FT_DIR),
        Ext4Inode::S_IFCHR => Some(Ext4DirEntry2::EXT4_FT_CHRDEV),
        Ext4Inode::S_IFBLK => Some(Ext4DirEntry2::EXT4_FT_BLKDEV),
        Ext4Inode::S_IFIFO => Some(Ext4DirEntry2::EXT4_FT_FIFO),
        Ext4Inode::S_IFSOCK => Some(Ext4DirEntry2::EXT4_FT_SOCK),
        Ext4Inode::S_IFLNK => Some(Ext4DirEntry2::EXT4_FT_SYMLINK),
        _ => None,
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;

    #[test]
    fn directory_entry_type_follows_the_inode_format_bits() {
        let cases = [
            (Ext4Inode::S_IFREG, Ext4DirEntry2::EXT4_FT_REG_FILE),
            (Ext4Inode::S_IFDIR, Ext4DirEntry2::EXT4_FT_DIR),
            (Ext4Inode::S_IFCHR, Ext4DirEntry2::EXT4_FT_CHRDEV),
            (Ext4Inode::S_IFBLK, Ext4DirEntry2::EXT4_FT_BLKDEV),
            (Ext4Inode::S_IFIFO, Ext4DirEntry2::EXT4_FT_FIFO),
            (Ext4Inode::S_IFSOCK, Ext4DirEntry2::EXT4_FT_SOCK),
            (Ext4Inode::S_IFLNK, Ext4DirEntry2::EXT4_FT_SYMLINK),
        ];
        for (format, entry_type) in cases {
            // Permission and set-id bits must not change the derived type.
            assert_eq!(
                directory_entry_type_for_mode(format | Ext4Inode::S_ISUID | 0o755),
                Some(entry_type),
                "mode={format:#o}"
            );
        }
    }

    #[test]
    fn directory_entry_type_is_absent_for_unrecognized_formats() {
        assert_eq!(directory_entry_type_for_mode(0o644), None);
        assert_eq!(directory_entry_type_for_mode(0o030_000), None);
        assert_eq!(directory_entry_type_for_mode(Ext4Inode::S_IFMT), None);
    }
}
