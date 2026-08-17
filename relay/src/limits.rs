//! Connection caps.
//!
//! DNS for the relay zone is grey-clouded ... it points straight at the relay's
//! address, because a proxying CDN would terminate TLS and that is the one
//! thing this design refuses. So there is no upstream scrubbing service and
//! port 443 is exposed directly on a machine run for free. Caps here are the
//! only thing between that and a bad afternoon.
//!
//! What is possible at this layer, and what is not, is worth stating: the relay
//! sits BELOW TLS, so it can count connections and it cannot inspect requests.
//! No WAF, no per-route rate limits, no request-size caps. Anything shaped like
//! "block this URL" has to happen on the instance.
//!
//! Four things are counted here, and they fail in different ways on purpose:
//!
//! * **a global concurrency ceiling per listener**, which is the backstop. A
//!   rate limit is not a concurrency limit: sixty connections per ten seconds
//!   is unlimited connections if each one is held open forever.
//! * **a per-source rate**, which is cheap to evade from a big address block
//!   and is therefore never the only defence.
//! * **a per-instance concurrency cap**, so one house cannot be flooded off the
//!   relay ... paired with the idle timeout in [`crate::tap`], because a cap on
//!   connections that are never released is a cap on how fast an attacker takes
//!   a house offline permanently.
//! * **a total instance cap**.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_instances: usize,
    pub max_conns_per_instance: usize,
    /// New ingress connections allowed from one source per `rate_window`.
    pub conns_per_ip: usize,
    /// Same, for the control port. Higher, because a busy house dials back
    /// once per browser connection and every one of those comes from the
    /// single address that house happens to have.
    pub control_conns_per_ip: usize,
    pub rate_window: Duration,
    /// Hard ceiling on concurrent ingress connections across all instances,
    /// including ones that have not sent a ClientHello yet. This is the number
    /// that bounds file descriptors and memory when the per-source rate limit
    /// is being evaded, which over IPv6 is free.
    pub max_ingress_conns: usize,
    /// Hard ceiling on concurrent control-port connections. A registered house
    /// holds one for as long as it is connected; the rest are transient.
    pub max_control_conns: usize,
    /// Deadline for the WHOLE pre-routing read: the complete ClientHello on
    /// ingress, the opening line on control. Per-read timeouts do not bound
    /// anything, because a byte every nine seconds resets them forever.
    pub handshake_timeout: Duration,
    /// No bytes in EITHER direction for this long and an established splice is
    /// closed. Idle, not total: a healthy hours-long download keeps moving
    /// bytes and must never be killed for being long.
    pub idle_timeout: Duration,
    /// Ceiling on tracked source buckets per listener. Reached, the window
    /// rolls early rather than sweeping, so the table cannot grow without
    /// bound and no connection ever pays for an O(n) scan.
    pub max_tracked_sources: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_instances: 64,
            max_conns_per_instance: 64,
            conns_per_ip: 60,
            control_conns_per_ip: 120,
            rate_window: Duration::from_secs(10),
            // 64 instances x 64 sessions, plus room for connections still in
            // the handshake. Beyond this the relay stops accepting rather than
            // running the box out of descriptors.
            max_ingress_conns: 8192,
            max_control_conns: 1024,
            handshake_timeout: Duration::from_secs(10),
            // Long enough for an idle SSE stream between events, short enough
            // that a wedged session is not a permanently held slot.
            idle_timeout: Duration::from_secs(120),
            max_tracked_sources: 65_536,
        }
    }
}

/// Rate-limit key.
///
/// IPv4 is counted exactly. IPv6 is counted per **/64**, because that is the
/// smallest thing a residential connection is handed: per-/128 counting means
/// 2^64 free retries and a per-source limit that can never fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    V4(Ipv4Addr),
    /// The high 64 bits of an IPv6 address.
    V6Prefix([u8; 8]),
}

impl Source {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Source::V4(v4),
            IpAddr::V6(v6) => {
                // A v4-mapped address is a v4 address wearing a hat; count it
                // as one, or the /64 bucket would merge every mapped client.
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return Source::V4(v4);
                }
                let o = v6.octets();
                let mut prefix = [0u8; 8];
                prefix.copy_from_slice(&o[..8]);
                Source::V6Prefix(prefix)
            }
        }
    }
}

/// One fixed window of per-source counts.
///
/// Fixed rather than sliding on purpose. A sliding window needs a timestamp
/// list per source and an O(n) sweep to reclaim it, and that sweep ran on
/// every connection on a tokio worker thread while holding a `std` mutex. This
/// holds one `u32` per source and reclaims the whole table in one drop when the
/// window rolls. The price is that a burst spanning a boundary can be up to 2x
/// the cap, which is the right trade for a limit whose job is to keep the
/// pathological case away from the box.
struct Window {
    started: Instant,
    hits: HashMap<Source, u32>,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            hits: HashMap::new(),
        }
    }
}

pub struct State {
    limits: Limits,
    ingress: Mutex<Window>,
    control: Mutex<Window>,
    ingress_slots: Arc<Semaphore>,
    control_slots: Arc<Semaphore>,
}

impl State {
    pub fn new(limits: Limits) -> Self {
        let now = Instant::now();
        Self {
            ingress: Mutex::new(Window::new(now)),
            control: Mutex::new(Window::new(now)),
            ingress_slots: Arc::new(Semaphore::new(limits.max_ingress_conns)),
            control_slots: Arc::new(Semaphore::new(limits.max_control_conns)),
            limits,
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// A slot on the ingress listener, held for the life of the connection
    /// including the splice. `None` means the relay is full and the caller
    /// must drop the connection.
    pub fn ingress_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.ingress_slots.clone().try_acquire_owned().ok()
    }

    pub fn control_slot(&self) -> Option<OwnedSemaphorePermit> {
        self.control_slots.clone().try_acquire_owned().ok()
    }

    pub fn check_ingress(&self, ip: IpAddr, now: Instant) -> Result<()> {
        self.check(&self.ingress, self.limits.conns_per_ip, ip, now)
    }

    pub fn check_control(&self, ip: IpAddr, now: Instant) -> Result<()> {
        self.check(&self.control, self.limits.control_conns_per_ip, ip, now)
    }

    fn check(&self, table: &Mutex<Window>, cap: usize, ip: IpAddr, now: Instant) -> Result<()> {
        let src = Source::of(ip);

        // Everything expensive happens outside the lock: the only work under
        // it is one hash lookup and one increment. `rolled` carries the old
        // table out so its drop is not charged to a lock holder.
        let (allowed, rolled) = {
            // A poisoned table is not a reason to stop rate limiting.
            let mut w = table.lock().unwrap_or_else(|e| e.into_inner());
            let expired = now.duration_since(w.started) >= self.limits.rate_window;
            let full =
                w.hits.len() >= self.limits.max_tracked_sources && !w.hits.contains_key(&src);
            let rolled = if expired || full {
                w.started = now;
                Some(std::mem::take(&mut w.hits))
            } else {
                None
            };
            let n = w.hits.entry(src).or_insert(0);
            let allowed = (*n as usize) < cap;
            if allowed {
                *n += 1;
            }
            (allowed, rolled)
        };
        drop(rolled);

        if !allowed {
            bail!(
                "rate limit: {cap} connections in {:?}",
                self.limits.rate_window
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.0.2.10".parse().expect("test address")
    }

    #[test]
    fn allows_up_to_the_cap_then_refuses() {
        let limits = Limits {
            conns_per_ip: 3,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(state.check_ingress(ip(), t0).is_ok(), "connection {i}");
        }
        assert!(
            state.check_ingress(ip(), t0).is_err(),
            "fourth must be refused"
        );
    }

    #[test]
    fn the_window_slides() {
        let limits = Limits {
            conns_per_ip: 1,
            rate_window: Duration::from_secs(10),
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        assert!(state.check_ingress(ip(), t0).is_ok());
        assert!(state.check_ingress(ip(), t0).is_err());
        assert!(state
            .check_ingress(ip(), t0 + Duration::from_secs(11))
            .is_ok());
    }

    #[test]
    fn addresses_are_counted_separately() {
        let limits = Limits {
            conns_per_ip: 1,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        assert!(state.check_ingress(ip(), t0).is_ok());
        let other: IpAddr = "198.51.100.7".parse().expect("test address");
        assert!(state.check_ingress(other, t0).is_ok());
    }

    #[test]
    fn ingress_and_control_are_counted_separately() {
        let limits = Limits {
            conns_per_ip: 1,
            control_conns_per_ip: 1,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        assert!(state.check_ingress(ip(), t0).is_ok());
        assert!(state.check_ingress(ip(), t0).is_err());
        // A house whose browsers are being refused must still be able to hold
        // its control connection.
        assert!(state.check_control(ip(), t0).is_ok());
    }

    /// The whole point of bucketing: a /64 is what one subscriber is handed,
    /// so counting /128s means the limit never fires.
    #[test]
    fn ipv6_is_counted_per_64_not_per_address() {
        let limits = Limits {
            conns_per_ip: 2,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        let a: IpAddr = "2001:db8:dead:beef::1".parse().expect("test address");
        let b: IpAddr = "2001:db8:dead:beef:ffff:ffff:ffff:ffff"
            .parse()
            .expect("test address");
        let c: IpAddr = "2001:db8:dead:beef:1:2:3:4".parse().expect("test address");
        assert!(state.check_ingress(a, t0).is_ok());
        assert!(state.check_ingress(b, t0).is_ok());
        assert!(
            state.check_ingress(c, t0).is_err(),
            "a fresh address in the same /64 must not buy a fresh budget"
        );

        // A different /64 is a different subscriber.
        let d: IpAddr = "2001:db8:dead:bee0::1".parse().expect("test address");
        assert!(state.check_ingress(d, t0).is_ok());
    }

    /// Address spraying must cost the relay bounded memory and no per
    /// connection sweep. Before this, crossing the threshold made every
    /// subsequent connection walk the whole table under a `std` mutex on a
    /// tokio worker.
    #[test]
    fn the_source_table_stays_bounded_under_spraying() {
        let limits = Limits {
            conns_per_ip: 1,
            max_tracked_sources: 512,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        for i in 0..50_000u32 {
            let ip = IpAddr::from(std::net::Ipv6Addr::new(
                0x2001,
                0xdb8,
                (i >> 16) as u16,
                i as u16,
                0,
                0,
                0,
                1,
            ));
            assert!(state.check_ingress(ip, t0).is_ok());
        }
        let tracked = state.ingress.lock().expect("rate table").hits.len();
        assert!(tracked <= 512, "table grew to {tracked}");
    }

    #[test]
    fn v4_mapped_addresses_are_counted_as_v4() {
        let mapped: IpAddr = "::ffff:192.0.2.10".parse().expect("test address");
        assert_eq!(Source::of(mapped), Source::of(ip()));
    }

    #[test]
    fn the_global_ceiling_refuses_rather_than_queues() {
        let state = State::new(Limits {
            max_ingress_conns: 2,
            ..Default::default()
        });
        let a = state.ingress_slot().expect("first");
        let _b = state.ingress_slot().expect("second");
        assert!(state.ingress_slot().is_none(), "third must be refused");
        drop(a);
        assert!(state.ingress_slot().is_some(), "a closed session frees one");
    }
}
