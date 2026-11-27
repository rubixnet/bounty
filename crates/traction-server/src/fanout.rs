//! Live fanout: totals pushed to a user's open SSE streams. One node, one
//! `tokio::sync::broadcast` channel per user (no `LISTEN/NOTIFY`, no
//! WebSockets). A subscriber that falls behind gets `Lagged`, which becomes a
//! `Resync` event: a client never silently misses an update.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::broadcast::{self, error::RecvError};

/// What a user's stream carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// The user's current Logged Hours totals, in seconds.
    Totals(Totals),
    /// Updates were missed: refetch the totals.
    Resync,
}

/// Logged Hours: synced, unverified. Never proof (see the glossary).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub human_active_sec: u64,
    pub struggle_credit_sec: u64,
    pub agent_compute_sec: u64,
}

pub trait LiveFanout: Send + Sync {
    /// A receiver for `user`'s events.
    fn subscribe(&self, user: &str) -> broadcast::Receiver<LiveEvent>;
    /// Sends `event` to every open stream of `user`.
    fn publish(&self, user: &str, event: LiveEvent);
}

/// The in-process implementation.
pub struct BroadcastFanout {
    capacity: usize,
    channels: Mutex<HashMap<String, broadcast::Sender<LiveEvent>>>,
}

impl BroadcastFanout {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            channels: Mutex::new(HashMap::new()),
        }
    }

    fn sender(&self, user: &str) -> broadcast::Sender<LiveEvent> {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        channels
            .entry(user.to_owned())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .clone()
    }
}

impl LiveFanout for BroadcastFanout {
    fn subscribe(&self, user: &str) -> broadcast::Receiver<LiveEvent> {
        self.sender(user).subscribe()
    }

    fn publish(&self, user: &str, event: LiveEvent) {
        // No subscriber is not an error.
        let _ = self.sender(user).send(event);
    }
}

/// The next event for a stream: a lag becomes `Resync`. `None` = closed.
pub async fn next_event(receiver: &mut broadcast::Receiver<LiveEvent>) -> Option<LiveEvent> {
    match receiver.recv().await {
        Ok(event) => Some(event),
        Err(RecvError::Lagged(_)) => Some(LiveEvent::Resync),
        Err(RecvError::Closed) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_reach_only_that_users_subscribers() {
        let fanout = BroadcastFanout::new(8);
        let mut a = fanout.subscribe("a");
        let mut b = fanout.subscribe("b");
        let totals = Totals {
            human_active_sec: 5,
            ..Totals::default()
        };
        fanout.publish("a", LiveEvent::Totals(totals));
        assert_eq!(next_event(&mut a).await, Some(LiveEvent::Totals(totals)));
        assert!(b.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_slow_subscriber_is_told_to_resync_never_silently_dropped() {
        let fanout = BroadcastFanout::new(2);
        let mut slow = fanout.subscribe("a");
        for sec in 0..10 {
            fanout.publish(
                "a",
                LiveEvent::Totals(Totals {
                    human_active_sec: sec,
                    ..Totals::default()
                }),
            );
        }
        assert_eq!(next_event(&mut slow).await, Some(LiveEvent::Resync));
        // After the resync the newest events still arrive.
        assert!(matches!(
            next_event(&mut slow).await,
            Some(LiveEvent::Totals(_))
        ));
    }
}
