//! Admission control for chat (OWASP API4: unrestricted resource consumption). Every chat
//! turn costs model tokens and maybe web searches, so a request must be admitted before the
//! agent runtime is called at all:
//!
//! 1. A per-user token bucket and a per-guild token bucket (refilled continuously).
//! 2. A global concurrency limit with a bounded wait queue: at most `max_concurrent` turns in
//!    flight, at most `max_queued` waiting, each for at most `queue_timeout`.
//!
//! `ChatService::chat` takes the `ChatPermit` this returns, so the agent can't be reached
//! without admission. A request turned away as busy gets its bucket tokens back. Rejections
//! are reported to a user at most once per `NOTICE_INTERVAL`, so the limiter can't be used
//! to make the bot spam a channel either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::domain::music::{GuildId, UserId};

/// One rejection notice per user per this interval; later rejections are silent.
pub const NOTICE_INTERVAL: Duration = Duration::from_secs(30);
/// Bucket maps are pruned of idle (full) buckets above this size.
const MAX_TRACKED_KEYS: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ChatLimits {
    pub user_per_minute: f64,
    pub user_burst: f64,
    pub guild_per_minute: f64,
    pub guild_burst: f64,
    pub max_concurrent: usize,
    pub max_queued: usize,
    pub queue_timeout: Duration,
}

impl Default for ChatLimits {
    fn default() -> Self {
        Self {
            user_per_minute: 6.0,
            user_burst: 3.0,
            guild_per_minute: 30.0,
            guild_burst: 10.0,
            max_concurrent: 4,
            max_queued: 8,
            queue_timeout: Duration::from_secs(15),
        }
    }
}

/// Who is asking. DMs have no guild: only the user bucket applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requester {
    pub user: UserId,
    pub guild: Option<GuildId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatLimited {
    User {
        retry_after: Duration,
        notify: bool,
    },
    Guild {
        retry_after: Duration,
        notify: bool,
    },
    /// Too many turns in flight and the wait queue is full (or the wait timed out).
    Busy {
        notify: bool,
    },
}

impl ChatLimited {
    /// Whether the user should be told (the first rejection in a `NOTICE_INTERVAL`).
    pub fn notify(&self) -> bool {
        match self {
            Self::User { notify, .. } | Self::Guild { notify, .. } | Self::Busy { notify } => {
                *notify
            }
        }
    }
}

/// Proof of admission; holds a concurrency slot until dropped.
#[derive(Debug)]
pub struct ChatPermit {
    _slot: OwnedSemaphorePermit,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn full(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant, per_minute: f64, capacity: f64) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * per_minute / 60.0).min(capacity);
        self.updated = now;
    }

    fn retry_after(&self, per_minute: f64) -> Duration {
        let missing = (1.0 - self.tokens).max(0.0);
        Duration::from_secs_f64(missing * 60.0 / per_minute.max(f64::MIN_POSITIVE))
    }
}

#[derive(Default)]
struct State {
    users: HashMap<u64, Bucket>,
    guilds: HashMap<u64, Bucket>,
    notices: HashMap<u64, Instant>,
}

pub struct ChatLimiter {
    limits: ChatLimits,
    state: Mutex<State>,
    slots: Arc<Semaphore>,
    waiting: AtomicUsize,
}

/// Decrements the waiting count even if the waiting future is dropped.
struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ChatLimiter {
    pub fn new(limits: ChatLimits) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(limits.max_concurrent.max(1))),
            limits,
            state: Mutex::new(State::default()),
            waiting: AtomicUsize::new(0),
        }
    }

    pub fn limits(&self) -> &ChatLimits {
        &self.limits
    }

    /// Admits a chat turn or says why not. Never calls anything outside the process.
    pub async fn admit(&self, who: Requester) -> Result<ChatPermit, ChatLimited> {
        self.take_tokens(who)?;
        if let Ok(slot) = self.slots.clone().try_acquire_owned() {
            return Ok(ChatPermit { _slot: slot });
        }
        if self.waiting.fetch_add(1, Ordering::SeqCst) >= self.limits.max_queued {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(self.busy(who));
        }
        let _waiting = Waiting(&self.waiting);
        match tokio::time::timeout(
            self.limits.queue_timeout,
            self.slots.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(slot)) => Ok(ChatPermit { _slot: slot }),
            _ => Err(self.busy(who)),
        }
    }

    fn take_tokens(&self, who: Requester) -> Result<(), ChatLimited> {
        let l = &self.limits;
        let now = Instant::now();
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut user = *s
            .users
            .entry(who.user.0)
            .or_insert_with(|| Bucket::full(l.user_burst, now));
        user.refill(now, l.user_per_minute, l.user_burst);
        let mut guild = who.guild.map(|g| {
            let mut b = *s
                .guilds
                .entry(g.0)
                .or_insert_with(|| Bucket::full(l.guild_burst, now));
            b.refill(now, l.guild_per_minute, l.guild_burst);
            b
        });
        // Check both before taking from either, so a rejection costs nothing.
        let rejection = if user.tokens < 1.0 {
            Some((true, user.retry_after(l.user_per_minute)))
        } else {
            guild
                .filter(|g| g.tokens < 1.0)
                .map(|g| (false, g.retry_after(l.guild_per_minute)))
        };
        if rejection.is_none() {
            user.tokens -= 1.0;
            if let Some(g) = guild.as_mut() {
                g.tokens -= 1.0;
            }
        }
        s.users.insert(who.user.0, user);
        if let (Some(id), Some(b)) = (who.guild, guild) {
            s.guilds.insert(id.0, b);
        }
        Self::prune(&mut s, l, now);
        match rejection {
            None => Ok(()),
            Some((is_user, retry_after)) => {
                let notify = Self::notice(&mut s, who.user, now);
                Err(if is_user {
                    ChatLimited::User {
                        retry_after,
                        notify,
                    }
                } else {
                    ChatLimited::Guild {
                        retry_after,
                        notify,
                    }
                })
            }
        }
    }

    /// Turned away for lack of capacity: give the bucket tokens back.
    fn busy(&self, who: Requester) -> ChatLimited {
        let l = &self.limits;
        let now = Instant::now();
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = s.users.get_mut(&who.user.0) {
            b.tokens = (b.tokens + 1.0).min(l.user_burst);
        }
        if let Some(b) = who.guild.and_then(|g| s.guilds.get_mut(&g.0)) {
            b.tokens = (b.tokens + 1.0).min(l.guild_burst);
        }
        ChatLimited::Busy {
            notify: Self::notice(&mut s, who.user, now),
        }
    }

    fn notice(s: &mut State, user: UserId, now: Instant) -> bool {
        match s.notices.get(&user.0) {
            Some(last) if now.saturating_duration_since(*last) < NOTICE_INTERVAL => false,
            _ => {
                s.notices.insert(user.0, now);
                true
            }
        }
    }

    fn prune(s: &mut State, l: &ChatLimits, now: Instant) {
        if s.users.len() > MAX_TRACKED_KEYS {
            s.users.retain(|_, b| {
                let mut b = *b;
                b.refill(now, l.user_per_minute, l.user_burst);
                b.tokens < l.user_burst
            });
        }
        if s.guilds.len() > MAX_TRACKED_KEYS {
            s.guilds.retain(|_, b| {
                let mut b = *b;
                b.refill(now, l.guild_per_minute, l.guild_burst);
                b.tokens < l.guild_burst
            });
        }
        if s.notices.len() > MAX_TRACKED_KEYS {
            s.notices
                .retain(|_, t| now.saturating_duration_since(*t) < NOTICE_INTERVAL);
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> (usize, usize) {
        let s = self.state.lock().unwrap();
        (s.users.len(), s.guilds.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who(user: u64, guild: Option<u64>) -> Requester {
        Requester {
            user: UserId(user),
            guild: guild.map(GuildId),
        }
    }

    fn limits() -> ChatLimits {
        ChatLimits {
            user_per_minute: 6.0, // one token every 10 s
            user_burst: 3.0,
            guild_per_minute: 60.0,
            guild_burst: 5.0,
            max_concurrent: 2,
            max_queued: 1,
            queue_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn spam_from_one_user_is_cut_off_after_the_burst() {
        let limiter = ChatLimiter::new(limits());
        let mut admitted = Vec::new();
        let mut rejected = Vec::new();
        for _ in 0..20 {
            match limiter.admit(who(1, Some(9))).await {
                Ok(permit) => {
                    drop(permit); // the turn finished
                    admitted.push(());
                }
                Err(e) => rejected.push(e),
            }
        }
        assert_eq!(admitted.len(), 3);
        assert_eq!(rejected.len(), 17);
        assert!(matches!(
            rejected[0],
            ChatLimited::User { notify: true, .. }
        ));
        // Only the first rejection tells the user; the rest are silent.
        assert!(rejected[1..].iter().all(|r| !r.notify()));
        let ChatLimited::User { retry_after, .. } = rejected[0] else {
            unreachable!()
        };
        assert!(retry_after <= Duration::from_secs(10), "{retry_after:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn tokens_come_back_with_time() {
        let limiter = ChatLimiter::new(limits());
        for _ in 0..3 {
            limiter.admit(who(1, None)).await.unwrap();
        }
        assert!(limiter.admit(who(1, None)).await.is_err());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(
            limiter.admit(who(1, None)).await.is_ok(),
            "one token after 10 s"
        );
        assert!(limiter.admit(who(1, None)).await.is_err());
        tokio::time::advance(Duration::from_secs(60)).await;
        for _ in 0..3 {
            assert!(limiter.admit(who(1, None)).await.is_ok(), "burst refilled");
        }
        // Notices resume once the interval has passed.
        let again = limiter.admit(who(1, None)).await.unwrap_err();
        assert!(again.notify());
    }

    #[tokio::test(start_paused = true)]
    async fn one_guild_is_limited_across_its_users_and_others_are_not() {
        let limiter = ChatLimiter::new(limits());
        for user in 0..5 {
            limiter.admit(who(user, Some(9))).await.unwrap();
        }
        let err = limiter.admit(who(99, Some(9))).await.unwrap_err();
        assert!(matches!(err, ChatLimited::Guild { notify: true, .. }));
        // The rejected user's own bucket was not charged.
        tokio::time::advance(Duration::from_secs(1)).await; // one guild token back
        assert!(limiter.admit(who(99, Some(9))).await.is_ok());
        // Another guild and DMs are unaffected.
        assert!(limiter.admit(who(50, Some(10))).await.is_ok());
        assert!(limiter.admit(who(51, None)).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_is_bounded_with_a_bounded_queue() {
        let limiter = Arc::new(ChatLimiter::new(limits()));
        let a = limiter.admit(who(1, None)).await.unwrap();
        let b = limiter.admit(who(2, None)).await.unwrap();
        // Third waits in the queue (max_queued = 1)...
        let waiter = {
            let limiter = limiter.clone();
            tokio::spawn(async move { limiter.admit(who(3, None)).await })
        };
        tokio::task::yield_now().await;
        // ...a fourth finds the queue full and is turned away at once, tokens refunded.
        let full = limiter.admit(who(4, None)).await.unwrap_err();
        assert!(matches!(full, ChatLimited::Busy { notify: true }));
        // A slot frees: the queued request gets it.
        drop(a);
        assert!(waiter.await.unwrap().is_ok());
        drop(b);
        // User 4 was refunded: still has the full burst of 3.
        for _ in 0..2 {
            let p = limiter.admit(who(4, None)).await.unwrap();
            drop(p);
        }
        let p = limiter.admit(who(4, None)).await;
        assert!(p.is_ok(), "the busy rejection cost no tokens");
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_request_times_out_as_busy() {
        let limiter = ChatLimiter::new(limits());
        let _a = limiter.admit(who(1, None)).await.unwrap();
        let _b = limiter.admit(who(2, None)).await.unwrap();
        let started = Instant::now();
        let err = limiter.admit(who(3, None)).await.unwrap_err();
        assert!(matches!(err, ChatLimited::Busy { .. }));
        assert_eq!(started.elapsed(), Duration::from_secs(5));
        assert_eq!(limiter.waiting.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn tracked_keys_stay_bounded() {
        let limiter = ChatLimiter::new(limits());
        for user in 0..(MAX_TRACKED_KEYS as u64 + 50) {
            drop(limiter.admit(who(user, None)).await.unwrap());
            if user % 1000 == 0 {
                // Let old buckets refill so they can be pruned.
                tokio::time::advance(Duration::from_secs(60)).await;
            }
        }
        let (users, _) = limiter.tracked();
        assert!(users <= MAX_TRACKED_KEYS + 1, "{users}");
    }
}
