// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::{cell::Cell, collections::BTreeMap, time::Duration};

/// Simple caller-visible concurrency limiter for storage operations.
#[derive(Debug)]
pub struct ConcurrencyLimiter {
    limit: usize,
    in_flight: Cell<usize>,
}

impl ConcurrencyLimiter {
    /// Creates a limiter with the given maximum in-flight operations.
    pub fn new(limit: usize) -> Self {
        assert!(limit != 0, "concurrency limit must be nonzero");
        Self {
            limit,
            in_flight: Cell::new(0),
        }
    }

    /// Attempts to acquire a permit without blocking.
    pub fn try_acquire(&self) -> Option<ConcurrencyPermit<'_>> {
        let current = self.in_flight.get();
        if current >= self.limit {
            return None;
        }
        self.in_flight.set(current + 1);
        Some(ConcurrencyPermit { limiter: self })
    }

    /// Returns the number of currently in-flight operations.
    pub fn in_flight(&self) -> usize {
        self.in_flight.get()
    }
}

/// Permit returned by [`ConcurrencyLimiter`].
pub struct ConcurrencyPermit<'a> {
    limiter: &'a ConcurrencyLimiter,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        self.limiter
            .in_flight
            .set(self.limiter.in_flight.get().saturating_sub(1));
    }
}

/// Minimal idle pool keyed by destination.
#[derive(Debug, Default)]
pub struct IdlePool<T> {
    max_idle_per_host: usize,
    idle: BTreeMap<String, Vec<IdleEntry<T>>>,
}

#[derive(Debug)]
struct IdleEntry<T> {
    checked_in_at: Duration,
    item: T,
}

impl<T> IdlePool<T> {
    /// Creates an idle pool.
    pub fn new(max_idle_per_host: usize) -> Self {
        Self {
            max_idle_per_host,
            idle: BTreeMap::new(),
        }
    }

    /// Returns an idle item for a destination.
    pub fn checkout(&mut self, destination: &str) -> Option<T> {
        self.checkout_at(destination, Duration::ZERO, Duration::MAX)
    }

    /// Returns a non-expired idle item for a destination.
    pub fn checkout_at(
        &mut self,
        destination: &str,
        now: Duration,
        idle_timeout: Duration,
    ) -> Option<T> {
        let idle = self.idle.get_mut(destination)?;
        while let Some(entry) = idle.pop() {
            if now.saturating_sub(entry.checked_in_at) <= idle_timeout {
                return Some(entry.item);
            }
        }
        None
    }

    /// Returns an item to the idle pool if capacity allows.
    pub fn checkin(&mut self, destination: impl Into<String>, item: T) -> bool {
        self.checkin_at(destination, item, Duration::ZERO)
    }

    /// Returns an item to the idle pool at a caller-supplied monotonic timestamp.
    pub fn checkin_at(
        &mut self,
        destination: impl Into<String>,
        item: T,
        checked_in_at: Duration,
    ) -> bool {
        let idle = self.idle.entry(destination.into()).or_default();
        if idle.len() >= self.max_idle_per_host {
            return false;
        }
        idle.push(IdleEntry {
            checked_in_at,
            item,
        });
        true
    }

    /// Returns idle count for a destination.
    pub fn idle_len(&self, destination: &str) -> usize {
        self.idle.get(destination).map_or(0, Vec::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_limiter_bounds_in_flight_operations() {
        let limiter = ConcurrencyLimiter::new(1);
        let permit = limiter.try_acquire().unwrap();

        assert_eq!(limiter.in_flight(), 1);
        assert!(limiter.try_acquire().is_none());
        drop(permit);
        assert_eq!(limiter.in_flight(), 0);
        assert!(limiter.try_acquire().is_some());
    }

    #[test]
    fn idle_pool_reuses_and_bounds_per_destination_items() {
        let mut pool = IdlePool::new(1);

        assert!(pool.checkin("host", 7));
        assert!(!pool.checkin("host", 8));
        assert_eq!(pool.idle_len("host"), 1);
        assert_eq!(pool.checkout("host"), Some(7));
        assert_eq!(pool.checkout("host"), None);
    }

    #[test]
    fn idle_pool_expires_entries_after_timeout() {
        let mut pool = IdlePool::new(2);

        assert!(pool.checkin_at("host", 7, Duration::from_secs(1)));
        assert_eq!(
            pool.checkout_at("host", Duration::from_secs(3), Duration::from_secs(1)),
            None
        );

        assert!(pool.checkin_at("host", 8, Duration::from_secs(3)));
        assert_eq!(
            pool.checkout_at("host", Duration::from_secs(4), Duration::from_secs(1)),
            Some(8)
        );
    }
}
