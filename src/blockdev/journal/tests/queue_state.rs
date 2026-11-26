//! How the queued transactions decide what a read sees, what a write costs and when the journal
//! may be switched off.

use super::*;

fn journaled_device() -> Jbd2Dev<MemBlockDev> {
    let mut dev = Jbd2Dev::initial_jbd2dev(0, MemBlockDev::new(256), true);
    dev.set_journal_superblock(small_journal_superblock(), AbsoluteBN::new(128))
        .expect("install small journal");
    dev
}

#[test]
fn a_read_sees_the_update_of_a_transaction_left_in_commit() {
    let mut dev = Jbd2Dev::initial_jbd2dev(0, MemBlockDev::with_failing_flush(256), true);
    dev.set_journal_superblock(small_journal_superblock(), AbsoluteBN::new(128))
        .expect("install small journal");
    dev.write_blocks(&vec![0x5a; BLOCK_SIZE], AbsoluteBN::new(10), 1, true)
        .expect("queue metadata update");
    dev.umount_commit()
        .expect_err("the failing flush must stop the commit");
    assert!(
        dev.system
            .as_ref()
            .is_some_and(|system| system.committing_transaction.is_some())
    );

    dev.read_block(AbsoluteBN::new(10))
        .expect("read the block the failed commit still owns");
    assert_eq!(dev.buffer(), &vec![0x5a; BLOCK_SIZE][..]);
}

#[test]
fn a_revoke_costs_a_credit_unless_the_transaction_also_updates_its_block() {
    let mut dev = journaled_device();
    let system = dev.system.as_mut().expect("journal state");
    system.running_transaction.updates.push(Jbd2Update(
        AbsoluteBN::new(10),
        vec![0; BLOCK_SIZE].into_boxed_slice(),
    ));
    system
        .running_transaction
        .revoked_blocks
        .push(AbsoluteBN::new(20));
    assert_eq!(
        Jbd2Dev::<MemBlockDev>::running_transaction_credits(system, 1).expect("count credits"),
        2
    );

    system
        .running_transaction
        .revoked_blocks
        .push(AbsoluteBN::new(10));
    assert_eq!(
        Jbd2Dev::<MemBlockDev>::running_transaction_credits(system, 1).expect("count credits"),
        2
    );
}

#[test]
fn the_journal_stays_on_while_any_queue_holds_work() {
    let mut dev = journaled_device();
    dev.write_blocks(&vec![0x5a; BLOCK_SIZE], AbsoluteBN::new(10), 1, true)
        .expect("queue metadata update");
    assert_eq!(
        dev.set_journal_use(false)
            .expect_err("a queued update keeps the journal on")
            .kind(),
        crate::Ext4ErrorKind::Busy
    );

    let mut dev = journaled_device();
    dev.system
        .as_mut()
        .expect("journal state")
        .running_transaction
        .revoked_blocks
        .push(AbsoluteBN::new(20));
    assert_eq!(
        dev.set_journal_use(false)
            .expect_err("a queued revoke keeps the journal on")
            .kind(),
        crate::Ext4ErrorKind::Busy
    );
}

#[test]
fn a_multi_block_write_lands_in_one_transaction() {
    let mut dev = journaled_device();
    let capacity = dev
        .journal_transaction_capacity()
        .expect("transaction capacity");
    for block in 0..capacity - 1 {
        dev.write_blocks(
            &vec![0x11; BLOCK_SIZE],
            AbsoluteBN::new(10 + block as u64),
            1,
            true,
        )
        .expect("fill the running transaction");
    }

    dev.write_blocks(&vec![0x22; 2 * BLOCK_SIZE], AbsoluteBN::new(200), 2, true)
        .expect("write two blocks");

    let updates = &dev
        .system
        .as_ref()
        .expect("journal state")
        .running_transaction
        .updates;
    assert!(
        updates
            .iter()
            .any(|update| update.0 == AbsoluteBN::new(200))
    );
    assert!(
        updates
            .iter()
            .any(|update| update.0 == AbsoluteBN::new(201))
    );
}

#[test]
fn rewriting_a_queued_block_in_a_full_transaction_commits_nothing() {
    let mut dev = journaled_device();
    let capacity = dev
        .journal_transaction_capacity()
        .expect("transaction capacity");
    for block in 0..capacity {
        dev.write_blocks(
            &vec![0x11; BLOCK_SIZE],
            AbsoluteBN::new(10 + block as u64),
            1,
            true,
        )
        .expect("fill the running transaction");
    }

    dev.write_blocks(&vec![0x33; BLOCK_SIZE], AbsoluteBN::new(10), 1, true)
        .expect("rewrite a queued block");

    let updates = &dev
        .system
        .as_ref()
        .expect("journal state")
        .running_transaction
        .updates;
    assert_eq!(updates.len(), capacity);
    assert_eq!(updates[0].1[0], 0x33);
}
