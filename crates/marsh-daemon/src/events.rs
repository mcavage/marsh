//! `marsh events --json`: change notifications for host subscribers.
//!
//! Every journaled job transition, split change, and checkpoint change gets
//! the next sequence number and is offered to each subscriber's bounded
//! queue without blocking the publisher. A subscriber whose queue is full is
//! dropped: it receives `{"type":"resync"}` after what was already queued and
//! its stream ends, so it re-lists state and subscribes again.

use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

/// Lines a subscriber may fall behind before it is dropped.
const QUEUE: usize = 1024;
/// Subscriptions at once. Each holds one of the daemon's 128 connections for
/// its lifetime, so the rest stay free for ordinary and lifecycle requests.
pub const SUBSCRIBERS: usize = 8;

#[derive(Debug, Default)]
pub struct Bus(Mutex<Inner>);

#[derive(Debug, Default)]
struct Inner {
    seq: u64,
    subscribers: Vec<SyncSender<Arc<str>>>,
    active: usize,
}

impl Bus {
    /// Number the change and offer it to every subscriber.
    pub fn publish(&self, kind: &str, value: &Value) {
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        inner.seq += 1;
        if inner.subscribers.is_empty() {
            return;
        }
        let line: Arc<str> = json!({"seq": inner.seq, "type": kind, kind: value})
            .to_string()
            .into();
        inner
            .subscribers
            .retain(|subscriber| match subscriber.try_send(Arc::clone(&line)) {
                Ok(()) => true,
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            });
    }

    /// A new subscription, or `None` when `SUBSCRIBERS` are connected.
    pub fn subscribe(self: &Arc<Self>) -> Option<Subscription> {
        let (sender, receiver) = sync_channel(QUEUE);
        let mut inner = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.active >= SUBSCRIBERS {
            return None;
        }
        inner.active += 1;
        inner.subscribers.push(sender);
        Some(Subscription {
            seq: inner.seq,
            receiver,
            bus: Arc::clone(self),
        })
    }
}

/// One `marsh events` connection: the sequence number of the last change
/// before it, and its queue. Dropping it frees its place.
pub struct Subscription {
    pub seq: u64,
    pub receiver: Receiver<Arc<str>>,
    bus: Arc<Bus>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut inner = self.bus.0.lock().unwrap_or_else(PoisonError::into_inner);
        inner.active -= 1;
    }
}
