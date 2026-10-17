// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ownership of a connection's single TDS client.
//!
//! An operation checks the client out as a [`WireLease`], does its I/O
//! without holding the DBC mutex, and hands the lease back. The owner is
//! recorded in the same critical section as the checkout, so a concurrent
//! claim always sees the connection as busy, never as missing its client
//! (AB#49225). A statement can keep the wire reserved between calls while its
//! results are pending on it.

use std::ffi::c_void;
use std::ops::{Deref, DerefMut};

use mssql_tds::connection::tds_client::TdsClient;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireOwner {
    Statement(*mut c_void),
    /// A connection-scoped operation (commit, `USE`, ...).
    Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimPolicy {
    /// The wire must be unreserved.
    IdleOnly,
    /// The wire may also be reserved by the claimant itself.
    IdleOrOwn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaimError {
    /// Reserved by, or checked out to, another owner or another call of the
    /// same one.
    Busy(WireOwner),
    /// No client is installed.
    NoClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseReturn {
    Release,
    /// The statement still has results pending. Treated as `Release` for a
    /// connection-scoped owner.
    KeepReservation,
}

/// Invariants: the client is parked exactly when no lease is out; a lease
/// always has an owner; a connection-scoped owner never outlives its lease.
#[derive(Debug, Default)]
pub(crate) struct ConnectionWire {
    client: Option<TdsClient>,
    owner: Option<WireOwner>,
    lease: Option<u64>,
    next_lease_id: u64,
}

impl ConnectionWire {
    pub(crate) fn install(&mut self, client: TdsClient) {
        debug_assert!(
            self.client.is_none() && self.lease.is_none(),
            "installing a client over a live session"
        );
        self.client = Some(client);
        self.owner = None;
        self.lease = None;
    }

    /// Any lease still out becomes stale.
    pub(crate) fn uninstall(&mut self) -> Option<TdsClient> {
        self.owner = None;
        self.lease = None;
        self.client.take()
    }

    /// The parked client; `None` while a lease is out.
    pub(crate) fn client(&self) -> Option<&TdsClient> {
        self.client.as_ref()
    }

    pub(crate) fn statement_owner(&self) -> Option<*mut c_void> {
        match self.owner {
            Some(WireOwner::Statement(handle)) => Some(handle),
            _ => None,
        }
    }

    pub(crate) fn is_leased_to(&self, owner: WireOwner) -> bool {
        self.lease.is_some() && self.owner == Some(owner)
    }

    /// No-op while a lease is out: its return decides the reservation.
    pub(crate) fn release_reservation(&mut self, statement: *mut c_void) {
        if self.lease.is_none() && self.owner == Some(WireOwner::Statement(statement)) {
            self.owner = None;
        }
    }

    fn check(&self, owner: WireOwner, policy: ClaimPolicy) -> Result<(), ClaimError> {
        if self.lease.is_some() {
            return Err(ClaimError::Busy(self.owner.unwrap_or(owner)));
        }
        match self.owner {
            Some(current) if !(policy == ClaimPolicy::IdleOrOwn && current == owner) => {
                Err(ClaimError::Busy(current))
            }
            _ if self.client.is_none() => Err(ClaimError::NoClient),
            _ => Ok(()),
        }
    }

    pub(crate) fn claim(
        &mut self,
        owner: WireOwner,
        policy: ClaimPolicy,
    ) -> Result<WireLease, ClaimError> {
        self.check(owner, policy)?;
        let client = self.client.take().ok_or(ClaimError::NoClient)?;
        self.next_lease_id = self.next_lease_id.wrapping_add(1);
        self.owner = Some(owner);
        self.lease = Some(self.next_lease_id);
        Ok(WireLease {
            client,
            owner,
            id: self.next_lease_id,
        })
    }

    /// The parked client for a read that completes under the DBC mutex.
    /// Reserves nothing.
    pub(crate) fn parked_client_mut(
        &mut self,
        owner: WireOwner,
        policy: ClaimPolicy,
    ) -> Result<&mut TdsClient, ClaimError> {
        self.check(owner, policy)?;
        self.client.as_mut().ok_or(ClaimError::NoClient)
    }

    /// Only after `parked_client_mut` succeeded for `statement` under the same
    /// lock.
    pub(crate) fn reserve(&mut self, statement: *mut c_void) {
        let owner = WireOwner::Statement(statement);
        debug_assert!(self.check(owner, ClaimPolicy::IdleOrOwn).is_ok());
        self.owner = Some(owner);
    }

    /// Returns `false`, closing the lease's client, if the lease's session has
    /// been torn down since it was issued.
    pub(crate) fn restore(&mut self, lease: WireLease, how: LeaseReturn) -> bool {
        let (client, owner, id) = lease.into_parts();
        if self.lease != Some(id) {
            warn!(?owner, "stale wire lease returned; closing its client");
            return false;
        }
        debug_assert_eq!(self.owner, Some(owner));
        self.lease = None;
        self.client = Some(client);
        self.owner = match (how, owner) {
            (LeaseReturn::KeepReservation, WireOwner::Statement(_)) => Some(owner),
            _ => None,
        };
        true
    }
}

/// Fixture hooks that bypass the invariants.
#[cfg(test)]
impl ConnectionWire {
    pub(crate) fn owner(&self) -> Option<WireOwner> {
        self.owner
    }

    pub(crate) fn is_checked_out(&self) -> bool {
        self.lease.is_some()
    }

    pub(crate) fn put_client_for_test(&mut self, client: TdsClient) {
        self.client = Some(client);
        self.lease = None;
    }

    pub(crate) fn clear_client_for_test(&mut self) -> Option<TdsClient> {
        self.client.take()
    }

    pub(crate) fn set_owner_for_test(&mut self, statement: Option<*mut c_void>) {
        self.owner = statement.map(WireOwner::Statement);
    }

    pub(crate) fn client_mut_for_test(&mut self) -> Option<&mut TdsClient> {
        self.client.as_mut()
    }

    pub(crate) fn take_lease_for_test(&mut self, statement: *mut c_void) -> WireLease {
        self.owner = None;
        self.claim(WireOwner::Statement(statement), ClaimPolicy::IdleOnly)
            .expect("the fixture installed a client")
    }
}

/// A checked-out client, returned with [`ConnectionWire::restore`].
///
/// Dropping it instead closes the client and leaves the wire reserved until
/// `SQLDisconnect`; nothing can return it on drop, because `SQLDisconnect`
/// frees statements, and the leases parked on them, while holding the DBC lock.
#[derive(Debug)]
#[must_use = "a wire lease must be returned to its connection"]
pub(crate) struct WireLease {
    client: TdsClient,
    owner: WireOwner,
    id: u64,
}

impl WireLease {
    pub(crate) fn owner(&self) -> WireOwner {
        self.owner
    }

    fn into_parts(self) -> (TdsClient, WireOwner, u64) {
        (self.client, self.owner, self.id)
    }

    /// Belongs to no wire; returning it anywhere is stale.
    #[cfg(test)]
    pub(crate) fn detached_for_test(client: TdsClient, statement: *mut c_void) -> Self {
        Self {
            client,
            owner: WireOwner::Statement(statement),
            id: 0,
        }
    }
}

impl Deref for WireLease {
    type Target = TdsClient;

    fn deref(&self) -> &TdsClient {
        &self.client
    }
}

impl DerefMut for WireLease {
    fn deref_mut(&mut self) -> &mut TdsClient {
        &mut self.client
    }
}

#[cfg(test)]
mod tests {
    use mssql_tds::test_client_support::tds_client_from_tokens;

    use super::*;

    const STMT_A: *mut c_void = std::ptr::without_provenance_mut(1);
    const STMT_B: *mut c_void = std::ptr::without_provenance_mut(2);
    const COMMIT: WireOwner = WireOwner::Connection;

    fn connected_wire() -> ConnectionWire {
        let mut wire = ConnectionWire::default();
        wire.install(tds_client_from_tokens(Vec::new()));
        wire
    }

    #[test]
    fn claim_reserves_and_checks_out_together() {
        let mut wire = connected_wire();
        let lease = wire
            .claim(WireOwner::Statement(STMT_A), ClaimPolicy::IdleOrOwn)
            .unwrap();
        assert!(wire.is_checked_out());
        assert!(wire.client().is_none());
        assert_eq!(wire.statement_owner(), Some(STMT_A));
        assert!(wire.restore(lease, LeaseReturn::Release));
        assert_eq!(wire.owner(), None);
        assert!(wire.client().is_some());
    }

    #[test]
    fn checked_out_wire_is_busy_for_every_claimant() {
        for holder in [WireOwner::Statement(STMT_A), COMMIT] {
            let mut wire = connected_wire();
            let lease = wire.claim(holder, ClaimPolicy::IdleOnly).unwrap();
            for claimant in [WireOwner::Statement(STMT_B), COMMIT, holder] {
                for policy in [ClaimPolicy::IdleOnly, ClaimPolicy::IdleOrOwn] {
                    let busy = Err(ClaimError::Busy(holder));
                    assert_eq!(wire.claim(claimant, policy).map(|_| ()), busy);
                    assert_eq!(wire.parked_client_mut(claimant, policy).map(|_| ()), busy);
                }
            }
            assert!(wire.restore(lease, LeaseReturn::Release));
        }
    }

    #[test]
    fn kept_reservation_admits_only_its_owner_resuming() {
        let mut wire = connected_wire();
        let owner = WireOwner::Statement(STMT_A);
        let lease = wire.claim(owner, ClaimPolicy::IdleOrOwn).unwrap();
        assert!(wire.restore(lease, LeaseReturn::KeepReservation));
        assert!(!wire.is_checked_out());
        assert!(wire.client().is_some());
        assert_eq!(wire.statement_owner(), Some(STMT_A));

        let busy = Err(ClaimError::Busy(owner));
        let b = WireOwner::Statement(STMT_B);
        assert_eq!(wire.claim(b, ClaimPolicy::IdleOrOwn).map(|_| ()), busy);
        assert_eq!(wire.claim(COMMIT, ClaimPolicy::IdleOnly).map(|_| ()), busy);
        assert_eq!(wire.claim(owner, ClaimPolicy::IdleOnly).map(|_| ()), busy);

        let lease = wire.claim(owner, ClaimPolicy::IdleOrOwn).unwrap();
        assert!(wire.restore(lease, LeaseReturn::Release));
        assert_eq!(wire.owner(), None);
    }

    #[test]
    fn connection_owner_never_keeps_the_wire() {
        let mut wire = connected_wire();
        let lease = wire.claim(COMMIT, ClaimPolicy::IdleOnly).unwrap();
        assert!(wire.restore(lease, LeaseReturn::KeepReservation));
        assert_eq!(wire.owner(), None);
    }

    #[test]
    fn only_the_reserving_statement_releases_an_idle_reservation() {
        let mut wire = connected_wire();
        let owner = WireOwner::Statement(STMT_A);
        let lease = wire.claim(owner, ClaimPolicy::IdleOrOwn).unwrap();
        assert!(wire.is_leased_to(owner));
        wire.release_reservation(STMT_A);
        assert_eq!(wire.owner(), Some(owner));

        assert!(wire.restore(lease, LeaseReturn::KeepReservation));
        assert!(!wire.is_leased_to(owner));
        wire.release_reservation(STMT_B);
        assert_eq!(wire.owner(), Some(owner));
        wire.release_reservation(STMT_A);
        assert_eq!(wire.owner(), None);
    }

    #[test]
    fn parked_read_reserves_only_when_asked() {
        let mut wire = connected_wire();
        let owner = WireOwner::Statement(STMT_A);
        assert!(
            wire.parked_client_mut(owner, ClaimPolicy::IdleOrOwn)
                .is_ok()
        );
        assert_eq!(wire.owner(), None);

        wire.reserve(STMT_A);
        assert_eq!(wire.statement_owner(), Some(STMT_A));
        assert!(!wire.is_checked_out());
        assert!(
            wire.parked_client_mut(owner, ClaimPolicy::IdleOrOwn)
                .is_ok()
        );
        assert_eq!(
            wire.parked_client_mut(WireOwner::Statement(STMT_B), ClaimPolicy::IdleOrOwn)
                .map(|_| ()),
            Err(ClaimError::Busy(owner))
        );
    }

    #[test]
    fn missing_client_is_the_only_no_client_error() {
        let mut wire = ConnectionWire::default();
        let owner = WireOwner::Statement(STMT_A);
        assert_eq!(
            wire.claim(owner, ClaimPolicy::IdleOrOwn).map(|_| ()),
            Err(ClaimError::NoClient)
        );
        assert_eq!(wire.owner(), None);
    }

    #[test]
    fn stale_lease_does_not_replace_the_new_session() {
        let mut wire = connected_wire();
        let stale = wire
            .claim(WireOwner::Statement(STMT_A), ClaimPolicy::IdleOrOwn)
            .unwrap();
        assert!(wire.uninstall().is_none());
        wire.install(tds_client_from_tokens(Vec::new()));
        let current = wire
            .claim(WireOwner::Statement(STMT_B), ClaimPolicy::IdleOrOwn)
            .unwrap();

        assert!(!wire.restore(stale, LeaseReturn::Release));
        assert!(wire.is_checked_out());
        assert_eq!(wire.statement_owner(), Some(STMT_B));
        assert!(wire.restore(current, LeaseReturn::Release));
    }

    #[test]
    fn stale_lease_after_plain_disconnect_is_closed() {
        let mut wire = connected_wire();
        let stale = wire.claim(COMMIT, ClaimPolicy::IdleOnly).unwrap();
        assert!(wire.uninstall().is_none());
        assert!(!wire.restore(stale, LeaseReturn::Release));
        assert!(wire.client().is_none());
        assert_eq!(wire.owner(), None);
    }
}
