//! Rewrites of an extended attribute with the value it already stores.

use super::*;

#[test]
fn setting_a_shared_external_xattr_to_its_stored_value_copies_nothing() {
    let (device, _) = FailingMemoryDevice::new(32 * 1024);
    let mut journal = Jbd2Dev::initial_jbd2dev(0, device, true);
    mkfs(&mut journal).expect("mkfs must succeed");
    let mut filesystem = Ext4FileSystem::mount(&mut journal).expect("mount must succeed");
    let (first, _, shared_block) = create_shared_external_xattr(&mut journal, &mut filesystem);
    let before_free_blocks = filesystem.statfs().free_blocks;

    set_inode_xattr(
        &mut journal,
        &mut filesystem,
        first,
        XattrNamespace::User,
        b"shared",
        &vec![0x5a; 512],
        XattrSetMode::Replace,
    )
    .expect("set the stored value again");

    let first_inode = filesystem
        .get_inode_by_num(&mut journal, first)
        .expect("read first inode");
    let first_store = read_external_store(&mut journal, &filesystem, &first_inode)
        .expect("read first external store")
        .expect("first xattr remains external");
    assert_eq!(first_store.block, shared_block);
    assert_eq!(first_store.refcount, 2);
    assert_eq!(filesystem.statfs().free_blocks, before_free_blocks);
}
