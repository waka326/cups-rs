//! Job handles and the identity of the provider that issued them.
//!
//! Every provider numbers its handles from the same starting point, so a
//! number alone cannot say whose handle it is: provider A's first handle and
//! provider B's first handle would be the same value, and B would act on its
//! own job when given A's. A handle therefore carries the identity of the
//! registry that issued it, and a registry refuses any handle that does not
//! carry its own identity — before it looks anything up, changes anything, or
//! lets anything reach CUPS.
//!
//! Both identities are allocated by checked increment. Neither ever wraps, so
//! no identity is ever issued twice within the process; running out is an
//! explicit error, not a silent collision.

use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::provider::error::{ProviderError, ProviderResult};

/// The next registry identity to hand out. Starts at 1 so 0 is never issued.
static NEXT_ISSUER: AtomicU64 = AtomicU64::new(1);

/// Identity of one job registry — in practice, of one provider instance.
///
/// Unique within the process for the life of the process. Deliberately
/// deterministic rather than random: uniqueness is guaranteed, not likely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct IssuerId(NonZeroU64);

impl IssuerId {
    /// Take the next unused identity. Safe to call from any thread.
    pub(crate) fn allocate() -> ProviderResult<Self> {
        allocate_from(&NEXT_ISSUER).map(Self)
    }
}

/// Hand out the counter's current value and advance it, never past
/// `u64::MAX`. Once the last value is gone, every later call fails.
fn allocate_from(counter: &AtomicU64) -> ProviderResult<NonZeroU64> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .ok()
        .and_then(NonZeroU64::new)
        .ok_or_else(|| ProviderError::InternalContractViolation {
            detail: "provider identities exhausted".into(),
        })
}

/// This provider's name for one requested job, allocated before anything is
/// sent to CUPS.
///
/// It exists so a job can be followed even when the scheduler's answer never
/// reached the caller: a create that times out after dispatch still has a
/// handle, and the job id it eventually produces is recorded against it.
///
/// A handle is valid only for the provider that issued it, and that provider
/// is part of the handle: any other provider rejects it as
/// [`ProviderError::UnknownJobHandle`]. It is opaque — its parts cannot be
/// read or chosen — it is not a pointer, and it does not survive the
/// provider. Persist the [`ProviderJobId`](super::ProviderJobId) instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderJobHandle {
    issuer: IssuerId,
    local: NonZeroU64,
}

impl ProviderJobHandle {
    pub(crate) fn issued_by(&self) -> IssuerId {
        self.issuer
    }
}

impl fmt::Display for ProviderJobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "job-handle-{}.{}", self.issuer.0, self.local)
    }
}

/// Issues handles for one registry.
pub(crate) struct HandleIssuer {
    issuer: IssuerId,
    /// The last local number issued; 0 before the first.
    last_local: u64,
}

impl HandleIssuer {
    pub(crate) fn new() -> ProviderResult<Self> {
        Ok(Self {
            issuer: IssuerId::allocate()?,
            last_local: 0,
        })
    }

    pub(crate) fn issuer(&self) -> IssuerId {
        self.issuer
    }

    /// The next handle, or an error once local numbers run out. Never
    /// reuses a number.
    pub(crate) fn next(&mut self) -> ProviderResult<ProviderJobHandle> {
        let local = self
            .last_local
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .ok_or_else(|| ProviderError::InternalContractViolation {
                detail: "job handles exhausted for this provider".into(),
            })?;
        self.last_local = local.get();
        Ok(ProviderJobHandle {
            issuer: self.issuer,
            local,
        })
    }

    #[cfg(test)]
    pub(crate) fn starting_after(last_local: u64) -> ProviderResult<Self> {
        Ok(Self {
            issuer: IssuerId::allocate()?,
            last_local,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuers_are_never_repeated() {
        let first = IssuerId::allocate().expect("identity");
        let second = IssuerId::allocate().expect("identity");
        assert_ne!(first, second);
    }

    #[test]
    fn issuer_allocation_stops_instead_of_wrapping() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_from(&counter).expect("last-but-one").get(),
            u64::MAX - 1
        );
        // u64::MAX is the ceiling: it is never handed out, and nothing after
        // it wraps back to an issued value.
        assert!(allocate_from(&counter).is_err());
        assert!(allocate_from(&counter).is_err());
        assert_eq!(counter.load(Ordering::SeqCst), u64::MAX);
    }

    #[test]
    fn issuer_allocation_never_hands_out_zero() {
        let counter = AtomicU64::new(0);
        // 0 is not a valid identity, so the call fails rather than issue it.
        assert!(allocate_from(&counter).is_err());
    }

    #[test]
    fn local_numbers_stop_instead_of_wrapping() {
        let mut issuer = HandleIssuer::starting_after(u64::MAX - 1).expect("issuer");
        let last = issuer.next().expect("the final number");
        assert!(issuer.next().is_err());
        assert!(issuer.next().is_err());
        assert_eq!(last.local.get(), u64::MAX);
    }

    #[test]
    fn same_local_number_from_two_issuers_is_two_handles() {
        let mut a = HandleIssuer::new().expect("issuer");
        let mut b = HandleIssuer::new().expect("issuer");
        let from_a = a.next().expect("handle");
        let from_b = b.next().expect("handle");
        assert_eq!(
            from_a.local, from_b.local,
            "the collision the old handle had"
        );
        assert_ne!(from_a, from_b);
        assert_ne!(from_a.issued_by(), from_b.issued_by());
    }
}
