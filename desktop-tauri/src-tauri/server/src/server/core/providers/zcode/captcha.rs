//! 一次性验证码池：桌面网页或 Docker 独立 SDK 生产，转发层限时等待。
//! 等待不持锁；取消释放等待计数；通知注册后再查库存，防止丢失唤醒。
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;

pub(super) const VERIFY_PARAM_HEADER: &str = "x-aliyun-captcha-verify-param";
pub(super) const VERIFY_REGION_HEADER: &str = "x-aliyun-captcha-verify-region";
pub const TOKEN_TTL_MS: i64 = 95_000;
pub const REFRESH_AGE_MS: i64 = 60_000;
pub const POOL_TARGET: usize = 3;
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(60);
type Proof = (String, String);

struct Entry {
    proof: Proof,
    at: i64,
}

#[derive(Default)]
struct Pool {
    entries: VecDeque<Entry>,
    minted: u64,
    consumed: u64,
    rejected: u64,
    stale: u64,
    timed_out: u64,
    last_challenge_at: i64,
    last_mint_at: i64,
    producer: Value,
}

#[derive(Default)]
struct CaptchaPool {
    inner: Mutex<Pool>,
    available: Notify,
    demand: Notify,
    waiting: AtomicUsize,
}

struct Waiter<'a>(&'a AtomicUsize);
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Pool {
    fn prune(&mut self, now: i64) {
        while self
            .entries
            .front()
            .is_some_and(|e| now.saturating_sub(e.at) >= TOKEN_TTL_MS)
        {
            self.entries.pop_front();
            self.stale = self.stale.saturating_add(1);
        }
    }
}

impl CaptchaPool {
    fn push(&self, param: &str, region: &str, now: i64) -> usize {
        let Ok(mut pool) = self.inner.lock() else {
            return 0;
        };
        pool.prune(now);
        if param.trim().is_empty() || pool.entries.iter().any(|e| e.proof.0 == param.trim()) {
            return pool.entries.len();
        }
        pool.entries.push_back(Entry {
            proof: (param.trim().into(), region.trim().into()),
            at: now,
        });
        // 临期时先补新再淘汰最老的一枚，避免旧令牌一起过期形成空窗。
        while pool.entries.len() > POOL_TARGET {
            pool.entries.pop_front();
        }
        pool.minted = pool.minted.saturating_add(1);
        pool.last_mint_at = now;
        let ready = pool.entries.len();
        drop(pool);
        self.available.notify_waiters();
        ready
    }

    fn take(&self, now: i64) -> Option<Proof> {
        let mut pool = self.inner.lock().ok()?;
        pool.prune(now);
        let entry = pool.entries.pop_front()?;
        pool.consumed = pool.consumed.saturating_add(1);
        drop(pool);
        self.demand.notify_one();
        Some(entry.proof)
    }

    async fn acquire(&self, timeout: Duration) -> Option<Proof> {
        if let Some(proof) = self.take(crate::server::logging::now_ms()) {
            return Some(proof);
        }
        self.waiting.fetch_add(1, Ordering::Relaxed);
        let _guard = Waiter(&self.waiting);
        let result = tokio::time::timeout(timeout, async {
            loop {
                let notified = self.available.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(proof) = self.take(crate::server::logging::now_ms()) {
                    return proof;
                }
                self.demand.notify_one();
                notified.await;
            }
        })
        .await
        .ok();
        if result.is_none() {
            if let Ok(mut pool) = self.inner.lock() {
                pool.timed_out = pool.timed_out.saturating_add(1);
            }
        }
        result
    }
}

fn pool() -> &'static CaptchaPool {
    static POOL: OnceLock<CaptchaPool> = OnceLock::new();
    POOL.get_or_init(CaptchaPool::default)
}

pub fn push(param: &str, region: &str) -> usize {
    pool().push(param, region, crate::server::logging::now_ms())
}
pub fn take() -> Option<Proof> {
    pool().take(crate::server::logging::now_ms())
}
pub async fn acquire() -> Option<Proof> {
    pool().acquire(ACQUIRE_TIMEOUT).await
}
pub async fn wait_for_demand() {
    pool().demand.notified().await;
}

pub fn note_challenge() {
    if let Ok(mut p) = pool().inner.lock() {
        p.rejected = p.rejected.saturating_add(1);
        p.last_challenge_at = crate::server::logging::now_ms();
        p.entries.clear();
    }
    pool().demand.notify_one();
}

pub fn set_producer(status: &str, failures: u64) {
    if let Ok(mut p) = pool().inner.lock() {
        p.producer = json!({"mode": "server", "status": status, "failures": failures,
            "updatedAt": crate::server::logging::now_ms()});
    }
}

pub fn ready() -> usize {
    stats().get("ready").and_then(Value::as_u64).unwrap_or(0) as usize
}

pub fn stats() -> Value {
    let now = crate::server::logging::now_ms();
    let Ok(mut p) = pool().inner.lock() else {
        return json!({"ready": 0});
    };
    p.prune(now);
    let fresh = p
        .entries
        .iter()
        .filter(|e| now.saturating_sub(e.at) < REFRESH_AGE_MS)
        .count();
    json!({
        "ready": p.entries.len(), "fresh": fresh, "ttlMs": TOKEN_TTL_MS,
        "oldestAgeMs": p.entries.front().map(|e| now.saturating_sub(e.at)).unwrap_or(0),
        "minted": p.minted, "consumed": p.consumed, "rejected": p.rejected, "stale": p.stale,
        "lastChallengeAt": p.last_challenge_at, "lastMintAt": p.last_mint_at,
        "waiting": pool().waiting.load(Ordering::Relaxed), "timedOut": p.timed_out,
        "producer": p.producer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn empty_pool_waits_and_wakes_without_reusing_proofs() {
        let p = Arc::new(CaptchaPool::default());
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let p = p.clone();
            tasks.push(tokio::spawn(async move {
                p.acquire(Duration::from_secs(2)).await.unwrap().0
            }));
        }
        while p.waiting.load(Ordering::Relaxed) != 8 {
            tokio::task::yield_now().await;
        }
        for i in 0..8 {
            p.push(
                &format!("proof-{i}"),
                "sgp",
                crate::server::logging::now_ms(),
            );
            while p.inner.lock().unwrap().consumed <= i {
                tokio::task::yield_now().await;
            }
        }
        let mut received = Vec::new();
        for task in tasks {
            received.push(task.await.unwrap());
        }
        received.sort();
        received.dedup();
        assert_eq!(received.len(), 8);
        assert_eq!(p.waiting.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn timeout_and_cancellation_release_waiters() {
        let p = Arc::new(CaptchaPool::default());
        assert!(p.acquire(Duration::from_millis(5)).await.is_none());
        assert_eq!(p.inner.lock().unwrap().timed_out, 1);
        let clone = p.clone();
        let task = tokio::spawn(async move { clone.acquire(Duration::from_secs(60)).await });
        while p.waiting.load(Ordering::Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        assert_eq!(p.waiting.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn expiry_and_replacement_preserve_fresh_inventory() {
        let p = CaptchaPool::default();
        for i in 0..3 {
            p.push(&format!("old-{i}"), "sgp", 0);
        }
        p.push("new", "sgp", REFRESH_AGE_MS);
        assert_eq!(p.inner.lock().unwrap().entries.len(), 3);
        assert_eq!(p.take(TOKEN_TTL_MS).unwrap().0, "new");
        assert!(p.take(TOKEN_TTL_MS).is_none());
        assert_eq!(p.inner.lock().unwrap().stale, 2);
    }
}
