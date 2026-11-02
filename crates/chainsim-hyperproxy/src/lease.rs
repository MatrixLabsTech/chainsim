//! The HyperCore lock as leases: one holder at a time, by the address it
//! connects from, with an expiry so a holder that dies never blocks the fork.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// How long a lease lasts when its holder never gives it back.
pub const TTL: Duration = Duration::from_secs(120);

struct Lease {
    token: String,
    holder: IpAddr,
    expires: Instant,
}

#[derive(Default)]
pub struct Leases {
    inner: Mutex<Option<Lease>>,
    counter: AtomicU64,
    /// Wakes waiters the moment a lease is given back, so a command that
    /// takes the lease again right away (a replay, candle by candle) does
    /// not keep them out.
    released: Notify,
}

impl Leases {
    fn active(&self) -> Option<(String, IpAddr)> {
        let l = self.inner.lock().expect("lease lock");
        l.as_ref().filter(|l| l.expires > Instant::now()).map(|l| (l.token.clone(), l.holder))
    }

    /// Takes the lease for `peer` when nobody holds it. Returns the token.
    pub fn try_take(&self, peer: IpAddr) -> Option<String> {
        let mut l = self.inner.lock().expect("lease lock");
        if l.as_ref().is_some_and(|l| l.expires > Instant::now()) {
            return None;
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let token = format!("{nanos:x}-{n}");
        *l = Some(Lease { token: token.clone(), holder: peer, expires: Instant::now() + TTL });
        Some(token)
    }

    /// Ends the lease with this token. Returns false for any other token.
    pub fn give_back(&self, token: &str) -> bool {
        let mut l = self.inner.lock().expect("lease lock");
        if l.as_ref().is_some_and(|l| l.token == token) {
            *l = None;
            self.released.notify_waiters();
            return true;
        }
        false
    }

    /// Whether `peer` holds the lease now.
    pub fn holds(&self, peer: IpAddr) -> bool {
        self.active().is_some_and(|(_, holder)| holder == peer)
    }

    /// Whether `peer` may go on: nobody else holds the lease.
    pub fn free_for(&self, peer: IpAddr) -> bool {
        self.active().is_none_or(|(_, holder)| holder == peer)
    }

    /// Waits while someone else holds the lease, at most until it expires.
    pub async fn wait(&self, peer: IpAddr) {
        let until = Instant::now() + TTL;
        loop {
            // Registered before the check, so a give_back in between still wakes it.
            let released = self.released.notified();
            if self.active().is_none_or(|(_, holder)| holder == peer) || Instant::now() >= until {
                return;
            }
            // The poll covers a lease that expires instead of being given back.
            let _ = tokio::time::timeout(Duration::from_millis(20), released).await;
        }
    }

    #[cfg(test)]
    fn expire_now(&self) {
        if let Some(l) = self.inner.lock().unwrap().as_mut() {
            l.expires = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_holder_at_a_time_and_expiry_frees_it() {
        let (cli, other): (IpAddr, IpAddr) = ("10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let l = Leases::default();
        let token = l.try_take(cli).unwrap();
        assert!(l.holds(cli) && !l.holds(other));
        assert!(l.free_for(cli) && !l.free_for(other));
        assert!(l.try_take(other).is_none());
        // The holder does not wait on itself.
        tokio::time::timeout(Duration::from_millis(50), l.wait(cli)).await.unwrap();
        // Another client waits until the lease is given back.
        assert!(tokio::time::timeout(Duration::from_millis(60), l.wait(other)).await.is_err());
        assert!(!l.give_back("not-the-token"));
        assert!(l.give_back(&token));
        tokio::time::timeout(Duration::from_millis(50), l.wait(other)).await.unwrap();
        // An expired lease counts as free.
        let t2 = l.try_take(other).unwrap();
        assert_ne!(t2, token);
        l.expire_now();
        assert!(!l.holds(other));
        assert!(l.try_take(cli).is_some());
    }

    /// With the clock paused, a waiter that only polled would wake 20 ms
    /// later. One woken by the give_back wakes at the same instant.
    #[tokio::test(start_paused = true)]
    async fn a_waiter_wakes_the_moment_the_lease_is_given_back() {
        let (cli, other): (IpAddr, IpAddr) = ("10.0.0.2".parse().unwrap(), "10.0.0.3".parse().unwrap());
        let l = std::sync::Arc::new(Leases::default());
        let token = l.try_take(cli).unwrap();
        let waiter = tokio::spawn({
            let l = l.clone();
            async move {
                l.wait(other).await;
                tokio::time::Instant::now()
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let given = tokio::time::Instant::now();
        assert!(l.give_back(&token));
        assert_eq!(waiter.await.unwrap(), given);
    }
}
