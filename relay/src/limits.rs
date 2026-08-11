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

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_instances: usize,
    pub max_conns_per_instance: usize,
    /// New connections allowed from one address per `rate_window`.
    pub conns_per_ip: usize,
    pub rate_window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_instances: 64,
            max_conns_per_instance: 64,
            conns_per_ip: 60,
            rate_window: Duration::from_secs(10),
        }
    }
}

pub struct State {
    limits: Limits,
    recent: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl State {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            recent: Mutex::new(HashMap::new()),
        }
    }

    /// Sliding window per source address. Pruned on touch, and the whole table
    /// is swept whenever it grows past a threshold so an address-spraying
    /// client cannot turn this into a memory leak.
    pub fn check_rate(&self, ip: IpAddr, now: Instant) -> Result<()> {
        let mut recent = self.recent.lock().expect("rate table poisoned");
        let window = self.limits.rate_window;

        if recent.len() > 10_000 {
            recent.retain(|_, hits| {
                hits.retain(|t| now.duration_since(*t) < window);
                !hits.is_empty()
            });
        }

        let hits = recent.entry(ip).or_default();
        hits.retain(|t| now.duration_since(*t) < window);
        if hits.len() >= self.limits.conns_per_ip {
            bail!("rate limit: {} connections in {:?}", hits.len(), window);
        }
        hits.push(now);
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
            assert!(state.check_rate(ip(), t0).is_ok(), "connection {i}");
        }
        assert!(
            state.check_rate(ip(), t0).is_err(),
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
        assert!(state.check_rate(ip(), t0).is_ok());
        assert!(state.check_rate(ip(), t0).is_err());
        assert!(state.check_rate(ip(), t0 + Duration::from_secs(11)).is_ok());
    }

    #[test]
    fn addresses_are_counted_separately() {
        let limits = Limits {
            conns_per_ip: 1,
            ..Default::default()
        };
        let state = State::new(limits);
        let t0 = Instant::now();
        assert!(state.check_rate(ip(), t0).is_ok());
        let other: IpAddr = "198.51.100.7".parse().expect("test address");
        assert!(state.check_rate(other, t0).is_ok());
    }
}
