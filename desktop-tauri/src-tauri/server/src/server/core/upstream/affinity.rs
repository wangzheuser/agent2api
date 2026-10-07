//! 会话绑定、原子预占和成功后的迁移；资格与并发过滤由调用方提供。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::server::core::routing::{account_id, max_concurrent_of};

const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const GC_INTERVAL: Duration = Duration::from_secs(60);
const MAX_BINDINGS: usize = 10_000;

#[derive(Clone, Default)]
pub struct Affinity {
    inner: Arc<Mutex<State>>,
}

pub struct Selection {
    pub account: Value,
    pub lease: Option<AffinityLease>,
    pub reason: &'static str,
}

/// 不可克隆：一个请求只释放自己的预占，成功确认与释放分别处理。
pub struct AffinityLease {
    affinity: Affinity,
    id: u64,
}

#[derive(Clone, PartialEq, Eq)]
struct Target {
    id: String,
    identity: Value,
}

impl Target {
    fn of(account: &Value) -> Option<Self> {
        let id = account_id(account)?.to_string();
        // 编排层在内存候选中补充真实 UID/地区/模型/思考档位，绝不保存 token。
        let identity = account
            .get("_affinityIdentity")
            .cloned()
            .unwrap_or_else(|| {
                let fields = ["provider", "uid", "userId", "region", "edition", "variant"];
                Value::Array(
                    fields
                        .iter()
                        .map(|field| account.get(*field).cloned().unwrap_or(Value::Null))
                        .collect(),
                )
            });
        Some(Self { id, identity })
    }

    fn find<'a>(&self, accounts: &'a [Value]) -> Option<&'a Value> {
        accounts.iter().find(|account| {
            account_id(account) == Some(self.id.as_str())
                && Self::of(account).as_ref() == Some(self)
        })
    }
}

struct Binding {
    incarnation: u64,
    generation: u64,
    owner: Option<Target>,
    pending: Option<Target>,
    active: usize,
    last_activity: Instant,
}

struct ActiveLease {
    key: String,
    epoch: u64,
    incarnation: u64,
    generation: u64,
    target: Target,
}

#[derive(Default)]
struct Recent {
    buckets: [(u64, u64); 15],
}

impl Recent {
    fn record(&mut self, minute: u64) {
        let bucket = &mut self.buckets[minute as usize % 15];
        if bucket.0 != minute {
            *bucket = (minute, 0);
        }
        bucket.1 = bucket.1.saturating_add(1);
    }

    fn count(&self, minute: u64) -> u128 {
        self.buckets
            .iter()
            .filter(|(stamp, _)| *stamp <= minute && minute - *stamp < 15)
            .map(|(_, count)| u128::from(*count))
            .sum()
    }
}

struct State {
    bindings: HashMap<String, Binding>,
    leases: HashMap<u64, ActiveLease>,
    recent: HashMap<String, Recent>,
    last_allocation: HashMap<String, u64>,
    sequence: u64,
    epoch: u64,
    started: Instant,
    last_gc: Instant,
}

impl Default for State {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            bindings: HashMap::new(),
            leases: HashMap::new(),
            recent: HashMap::new(),
            last_allocation: HashMap::new(),
            sequence: 0,
            epoch: 0,
            started: now,
            last_gc: now,
        }
    }
}

impl State {
    fn next(&mut self) -> u64 {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("affinity sequence exhausted");
        self.sequence
    }

    fn gc(&mut self, now: Instant, capacity_pressure: bool) {
        if !capacity_pressure && now.duration_since(self.last_gc) < GC_INTERVAL {
            return;
        }
        self.bindings.retain(|_, binding| {
            binding.active > 0 || now.duration_since(binding.last_activity) < IDLE_TTL
        });
        self.last_gc = now;
        let minute = now.duration_since(self.started).as_secs() / 60;
        self.recent.retain(|_, recent| recent.count(minute) > 0);
        // 分配序号仅参与当前账号间的先后；没有绑定/近期使用的历史账号可清理。
        let occupied: HashSet<&str> = self
            .bindings
            .values()
            .flat_map(|binding| {
                binding
                    .owner
                    .iter()
                    .chain(binding.pending.iter())
                    .map(|target| target.id.as_str())
            })
            .collect();
        self.last_allocation
            .retain(|id, _| self.recent.contains_key(id) || occupied.contains(id.as_str()));
    }

    fn make_room(&mut self) -> bool {
        if self.bindings.len() < MAX_BINDINGS {
            return true;
        }
        let oldest = self
            .bindings
            .iter()
            .filter(|(_, binding)| binding.active == 0)
            .min_by_key(|(_, binding)| (binding.last_activity, binding.incarnation))
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.bindings.remove(&key);
            true
        } else {
            false
        }
    }

    fn choose(
        &mut self,
        candidates: &[Value],
        counts: &HashMap<String, usize>,
        now: Instant,
    ) -> Option<Value> {
        // ponytail: 最多 10000 个绑定的 O(N+A) 压力扫描；实测超 5ms 再改索引计数。
        let mut sessions: HashMap<&str, u128> = HashMap::new();
        for binding in self.bindings.values() {
            if binding.active == 0 && now.duration_since(binding.last_activity) >= IDLE_TTL {
                continue;
            }
            if let Some(owner) = &binding.owner {
                *sessions.entry(&owner.id).or_default() += 1;
            }
            if let Some(pending) = &binding.pending {
                if binding
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner.id != pending.id)
                {
                    *sessions.entry(&pending.id).or_default() += 1;
                }
            }
        }
        let minute = now.duration_since(self.started).as_secs() / 60;
        let score = |account: &Value| {
            let id = account_id(account).unwrap_or_default();
            let s = sessions.get(id).copied().unwrap_or(0);
            let f = counts.get(id).copied().unwrap_or(0) as u128;
            let r = self
                .recent
                .get(id)
                .map(|recent| recent.count(minute))
                .unwrap_or(0);
            (
                s.max(f),
                f,
                r,
                u128::from(max_concurrent_of(account).max(1)),
                self.last_allocation.get(id).copied().unwrap_or(0),
            )
        };
        let chosen = candidates
            .iter()
            .filter(|account| account_id(account).is_some())
            .min_by(|left, right| compare_scores(score(left), score(right)))?
            .clone();
        drop(sessions);
        let sequence = self.next();
        self.last_allocation
            .insert(account_id(&chosen)?.to_string(), sequence);
        Some(chosen)
    }
}

fn compare_scores(
    left: (u128, u128, u128, u128, u64),
    right: (u128, u128, u128, u128, u64),
) -> Ordering {
    (left.0 * right.3)
        .cmp(&(right.0 * left.3))
        .then_with(|| (left.1 * right.3).cmp(&(right.1 * left.3)))
        .then_with(|| (left.2 * right.3).cmp(&(right.2 * left.3)))
        .then_with(|| left.4.cmp(&right.4))
}

impl Affinity {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// candidates 已通过正常并发过滤；reusable 保留健康但繁忙的账号。
    /// 两池都由现有路由层执行权限、提供商/模型、积分、冷却和 tried_ids 检查。
    pub fn select(
        &self,
        session_key: Option<&str>,
        candidates: &[Value],
        reusable: &[Value],
        counts: &HashMap<String, usize>,
    ) -> Option<Selection> {
        self.select_impl(session_key, candidates, reusable, counts, None)
    }

    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// 请求入口捕获 epoch；策略切走后尚未选路的旧请求也不能重建绑定。
    pub fn select_in_epoch(
        &self,
        session_key: Option<&str>,
        candidates: &[Value],
        reusable: &[Value],
        counts: &HashMap<String, usize>,
        expected_epoch: u64,
    ) -> Option<Selection> {
        self.select_impl(
            session_key,
            candidates,
            reusable,
            counts,
            Some(expected_epoch),
        )
    }

    fn select_impl(
        &self,
        session_key: Option<&str>,
        candidates: &[Value],
        reusable: &[Value],
        counts: &HashMap<String, usize>,
        expected_epoch: Option<u64>,
    ) -> Option<Selection> {
        let now = Instant::now();
        let mut state = self.lock();
        if expected_epoch.is_some_and(|epoch| epoch != state.epoch) {
            return None;
        }
        let key = session_key.filter(|key| !key.is_empty());
        let pressure = key.is_some_and(|key| !state.bindings.contains_key(key))
            && state.bindings.len() >= MAX_BINDINGS;
        state.gc(now, pressure);
        // GC 周期内也不复用已经空闲到期的当前键；活跃长流永不到期。
        if let Some(key) = key {
            if state.bindings.get(key).is_some_and(|binding| {
                binding.active == 0 && now.duration_since(binding.last_activity) >= IDLE_TTL
            }) {
                state.bindings.remove(key);
            }
        }
        let Some(key) = key else {
            let account = state.choose(candidates, counts, now)?;
            return Some(selection(account, None, "no_session"));
        };

        let mut selected = None;
        let mut reason = "new_session";
        if let Some(binding) = state.bindings.get(key) {
            // 在途迁移优先复用 pending，不被另一个完成顺序改回旧 owner。
            if let Some(pending) = &binding.pending {
                if let Some(account) = pending.find(candidates) {
                    selected = Some(account.clone());
                    reason = if binding.owner.is_some() {
                        "failover_pending"
                    } else {
                        "new_session"
                    };
                } else if pending.find(reusable).is_some() {
                    selected = Some(state.choose(candidates, counts, now)?);
                    reason = "busy_overflow";
                }
            } else if let Some(owner) = &binding.owner {
                if let Some(account) = owner.find(candidates) {
                    selected = Some(account.clone());
                    reason = "sticky";
                } else if owner.find(reusable).is_some() {
                    selected = Some(state.choose(candidates, counts, now)?);
                    reason = "busy_overflow";
                }
            }
        }

        let account = match selected {
            Some(account) => account,
            None => {
                let account = state.choose(candidates, counts, now)?;
                let target = Target::of(&account)?;
                let generation = state.next();
                if let Some(binding) = state.bindings.get_mut(key) {
                    binding.generation = generation;
                    binding.pending = Some(target);
                    reason = if binding.owner.is_some() {
                        "failover_pending"
                    } else {
                        "new_session"
                    };
                } else if state.make_room() {
                    state.bindings.insert(
                        key.to_string(),
                        Binding {
                            incarnation: generation,
                            generation,
                            owner: None,
                            pending: Some(target),
                            active: 0,
                            last_activity: now,
                        },
                    );
                } else {
                    // 全表活跃时保持服务能力，不创建第 10001 个持久绑定。
                    return Some(selection(account, None, "capacity_overflow"));
                }
                account
            }
        };
        let target = Target::of(&account)?;
        let id = state.next();
        let epoch = state.epoch;
        let binding = state.bindings.get_mut(key)?;
        binding.active += 1;
        binding.last_activity = now;
        let record = ActiveLease {
            key: key.to_string(),
            epoch,
            incarnation: binding.incarnation,
            generation: binding.generation,
            target,
        };
        state.leases.insert(id, record);
        Some(selection(
            account,
            Some(AffinityLease {
                affinity: self.clone(),
                id,
            }),
            reason,
        ))
    }

    /// 每次实际开始发送上游请求时调用；选路、预览和仅刷新凭证不调用。
    pub fn record_send(&self, account_id: &str) {
        self.record_send_impl(account_id, None);
    }

    pub fn record_send_for_epoch(&self, account_id: &str, expected_epoch: u64) {
        self.record_send_impl(account_id, Some(expected_epoch));
    }

    fn record_send_impl(&self, account_id: &str, expected_epoch: Option<u64>) {
        if account_id.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut state = self.lock();
        if expected_epoch.is_some_and(|epoch| epoch != state.epoch) {
            return;
        }
        state.gc(now, false);
        let minute = now.duration_since(state.started).as_secs() / 60;
        state
            .recent
            .entry(account_id.to_string())
            .or_default()
            .record(minute);
    }

    /// 切离本策略时清空绑定，旧 lease 仍独立释放，epoch 禁止旧请求回填。
    pub fn reset(&self) {
        let mut state = self.lock();
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("affinity epoch exhausted");
        state.bindings.clear();
        state.recent.clear();
        state.last_allocation.clear();
    }
}

fn selection(mut account: Value, lease: Option<AffinityLease>, reason: &'static str) -> Selection {
    if let Some(object) = account.as_object_mut() {
        object.remove("_affinityIdentity");
    }
    Selection {
        account,
        lease,
        reason,
    }
}

impl AffinityLease {
    pub(crate) fn matches_identity(&self, identity: &Value) -> bool {
        self.affinity.lock().leases.get(&self.id)
            .is_some_and(|record| record.target.identity == *identity)
    }
    /// 仅真实成功终态调用，不因 HTTP 200、网关补发 DONE 或空流确认。
    pub fn confirm(&mut self) {
        let mut state = self.affinity.lock();
        let Some(record) = state.leases.get(&self.id) else {
            return;
        };
        if record.epoch != state.epoch {
            return;
        }
        let key = record.key.clone();
        let incarnation = record.incarnation;
        let generation = record.generation;
        let target = record.target.clone();
        if let Some(binding) = state.bindings.get_mut(&key) {
            if binding.incarnation == incarnation
                && binding.generation == generation
                && binding.pending.as_ref() == Some(&target)
            {
                binding.owner = Some(target);
                binding.pending = None;
                binding.last_activity = Instant::now();
            }
        }
    }
}

impl Drop for AffinityLease {
    fn drop(&mut self) {
        let mut state = self.affinity.lock();
        // 先释放实际 lease，generation 不匹配也不能跳过这一步。
        let Some(record) = state.leases.remove(&self.id) else {
            return;
        };
        if record.epoch != state.epoch {
            return;
        }
        let pending_active = state.leases.values().any(|other| {
            other.epoch == record.epoch
                && other.key == record.key
                && other.incarnation == record.incarnation
                && other.generation == record.generation
                && other.target == record.target
        });
        let mut remove = false;
        if let Some(binding) = state.bindings.get_mut(&record.key) {
            if binding.incarnation != record.incarnation {
                return;
            }
            binding.active -= 1;
            binding.last_activity = Instant::now();
            if binding.generation == record.generation
                && !pending_active
                && binding.pending.as_ref() == Some(&record.target)
            {
                binding.pending = None;
            }
            remove = binding.owner.is_none() && binding.pending.is_none() && binding.active == 0;
        }
        if remove {
            state.bindings.remove(&record.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Barrier;
    use std::thread;

    fn accounts(capacities: &[u64]) -> Vec<Value> {
        capacities
            .iter()
            .enumerate()
            .map(|(index, capacity)| {
                json!({
                    "id": (["A", "B", "C"][index]), "provider": "workbuddy",
                    "uid": format!("uid-{index}"), "maxConcurrent": capacity,
                })
            })
            .collect()
    }

    fn pick(affinity: &Affinity, key: Option<&str>, pool: &[Value]) -> Selection {
        affinity
            .select(key, pool, pool, &HashMap::new())
            .expect("eligible account")
    }

    fn complete(mut selected: Selection) -> String {
        if let Some(lease) = &mut selected.lease {
            lease.confirm();
        }
        account_id(&selected.account).unwrap().to_string()
    }

    fn distribution(affinity: &Affinity) -> [usize; 3] {
        let state = affinity.lock();
        let mut result = [0; 3];
        for binding in state.bindings.values() {
            let target = binding.owner.as_ref().or(binding.pending.as_ref()).unwrap();
            result[["A", "B", "C"]
                .iter()
                .position(|id| *id == target.id)
                .unwrap()] += 1;
        }
        result
    }

    #[test]
    fn affinity_old_session_stays_and_new_sessions_balance() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        assert_eq!(complete(pick(&affinity, Some("old"), &pool)), "A");
        for _ in 0..100 {
            let selected = pick(&affinity, Some("old"), &pool);
            assert_eq!(selected.reason, "sticky");
            assert_eq!(complete(selected), "A");
        }
        assert_eq!(complete(pick(&affinity, Some("new"), &pool)), "B");
        assert_eq!(complete(pick(&affinity, Some("third"), &pool)), "C");
        assert_eq!(distribution(&affinity), [1, 1, 1]);
        assert!(affinity.lock().leases.is_empty());
    }

    #[test]
    fn affinity_capacity_weights_and_parallel_reservations() {
        let affinity = Affinity::default();
        let pool = accounts(&[2, 1, 1]);
        for index in 0..8 {
            complete(pick(&affinity, Some(&format!("s-{index}")), &pool));
        }
        assert_eq!(distribution(&affinity), [4, 2, 2]);

        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        for (index, count) in [3, 1, 2].iter().enumerate() {
            for sequence in 0..*count {
                complete(pick(
                    &affinity,
                    Some(&format!("load-{index}-{sequence}")),
                    &pool[index..=index],
                ));
            }
        }
        assert_eq!(complete(pick(&affinity, Some("least-used"), &pool)), "B");

        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        let barrier = Arc::new(Barrier::new(30));
        thread::scope(|scope| {
            for index in 0..30 {
                let affinity = affinity.clone();
                let pool = &pool;
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    complete(pick(&affinity, Some(&format!("p-{index}")), pool));
                });
            }
        });
        assert_eq!(distribution(&affinity), [10, 10, 10]);
        assert!(affinity.lock().leases.is_empty());
    }

    #[test]
    fn affinity_parallel_same_key_uses_one_pending_target() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        let start = Arc::new(Barrier::new(16));
        let reserved = Arc::new(Barrier::new(16));
        thread::scope(|scope| {
            for _ in 0..16 {
                let affinity = affinity.clone();
                let pool = &pool;
                let start = start.clone();
                let reserved = reserved.clone();
                scope.spawn(move || {
                    start.wait();
                    let selected = pick(&affinity, Some("shared"), pool);
                    assert_eq!(account_id(&selected.account), Some("A"));
                    reserved.wait();
                    complete(selected);
                });
            }
        });
        assert_eq!(distribution(&affinity), [1, 0, 0]);
        assert!(affinity.lock().leases.is_empty());
    }

    #[test]
    fn affinity_failed_and_cancelled_leases_only_release_their_own_reservation() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0]);
        let first = pick(&affinity, Some("shared"), &pool);
        let second = pick(&affinity, Some("shared"), &pool);
        drop(first);
        assert_eq!(affinity.lock().bindings["shared"].active, 1);
        assert!(affinity.lock().bindings["shared"].pending.is_some());
        assert_eq!(complete(second), "A");
        drop(pick(&affinity, Some("failed-new"), &pool));
        assert!(!affinity.lock().bindings.contains_key("failed-new"));
        assert_eq!(affinity.lock().bindings["shared"].active, 0);
        assert!(affinity.lock().leases.is_empty());
    }

    #[test]
    fn affinity_busy_overflow_never_rebinds_and_protects_owner() {
        let affinity = Affinity::default();
        let pool = accounts(&[1, 1]);
        complete(pick(&affinity, Some("session"), &pool));
        let overflow = affinity
            .select(
                Some("session"),
                &pool[1..],
                &pool,
                &HashMap::from([("A".to_string(), 1)]),
            )
            .unwrap();
        assert_eq!(overflow.reason, "busy_overflow");
        assert_eq!(affinity.lock().bindings["session"].active, 1);
        {
            let mut state = affinity.lock();
            state.bindings.get_mut("session").unwrap().last_activity -= IDLE_TTL;
            state.gc(Instant::now(), true);
            assert!(state.bindings.contains_key("session"));
        }
        assert_eq!(complete(overflow), "B");
        assert_eq!(complete(pick(&affinity, Some("session"), &pool)), "A");
        assert_eq!(distribution(&affinity), [1, 0, 0]);
        assert!(affinity
            .select(Some("session"), &[], &pool, &HashMap::new())
            .is_none());
    }

    #[test]
    fn affinity_failover_is_pending_until_success_and_old_generation_cleans_up() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        complete(pick(&affinity, Some("session"), &pool));
        let mut old = pick(&affinity, Some("session"), &pool);
        let replacement = affinity
            .select(Some("session"), &pool[1..], &pool[1..], &HashMap::new())
            .unwrap();
        assert_eq!(replacement.reason, "failover_pending");
        assert_eq!(
            affinity.lock().bindings["session"]
                .owner
                .as_ref()
                .unwrap()
                .id,
            "A"
        );
        assert_eq!(
            affinity.lock().bindings["session"]
                .pending
                .as_ref()
                .unwrap()
                .id,
            "B"
        );
        let shared = affinity
            .select(Some("session"), &pool[1..], &pool[1..], &HashMap::new())
            .unwrap();
        drop(replacement);
        assert!(affinity.lock().bindings["session"].pending.is_some());
        assert_eq!(complete(shared), "B");
        old.lease.as_mut().unwrap().confirm();
        assert_eq!(
            affinity.lock().bindings["session"]
                .owner
                .as_ref()
                .unwrap()
                .id,
            "B"
        );
        drop(old);
        assert_eq!(affinity.lock().bindings["session"].active, 0);
        assert!(affinity.lock().leases.is_empty());

        let failed = affinity
            .select(Some("session"), &pool[2..], &pool[2..], &HashMap::new())
            .unwrap();
        drop(failed);
        assert!(affinity.lock().bindings["session"].pending.is_none());
        assert_eq!(
            affinity.lock().bindings["session"]
                .owner
                .as_ref()
                .unwrap()
                .id,
            "B"
        );
    }

    #[test]
    fn affinity_replaced_pending_cannot_be_confirmed_by_stale_success() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        let mut old = pick(&affinity, Some("session"), &pool);
        let next = affinity
            .select(Some("session"), &pool[1..], &pool[1..], &HashMap::new())
            .unwrap();
        assert_eq!(complete(next), "B");
        old.lease.as_mut().unwrap().confirm();
        drop(old);
        assert_eq!(distribution(&affinity), [0, 1, 0]);
        assert!(affinity.lock().leases.is_empty());
    }

    #[test]
    fn affinity_identity_change_migrates_but_token_refresh_does_not() {
        let affinity = Affinity::default();
        let mut pool = accounts(&[0, 0]);
        complete(pick(&affinity, Some("session"), &pool));
        pool[0]["tokenTail"] = json!("refreshed");
        assert_eq!(pick(&affinity, Some("session"), &pool).reason, "sticky");
        pool[0]["uid"] = json!("different-identity");
        let next = pick(&affinity, Some("session"), &pool);
        assert_eq!(next.reason, "failover_pending");
        assert_eq!(complete(next), "B");

        let affinity = Affinity::default();
        pool[0]["_affinityIdentity"] = json!(["uid", "cn", "model", "high"]);
        let selected = pick(&affinity, Some("internal"), &pool);
        assert!(selected.account.get("_affinityIdentity").is_none());
        complete(selected);
        pool[0]["_affinityIdentity"] = json!(["uid", "international", "model", "high"]);
        assert_eq!(
            pick(&affinity, Some("internal"), &pool).reason,
            "failover_pending"
        );
    }

    #[test]
    fn affinity_no_session_does_not_persist_and_unknown_ids_are_ignored() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        let selected: Vec<String> = (0..6)
            .map(|_| complete(pick(&affinity, None, &pool)))
            .collect();
        assert_eq!(selected, ["A", "B", "C", "A", "B", "C"]);
        assert!(affinity.lock().bindings.is_empty());
        assert!(affinity.lock().leases.is_empty());
        assert!(affinity
            .select(
                Some("unknown"),
                &[json!({"id": ""}), json!({})],
                &[],
                &HashMap::new()
            )
            .is_none());
        assert!(affinity
            .select(Some("unknown"), &[], &[], &HashMap::new())
            .is_none());
        assert!(pick(&affinity, Some(""), &pool).lease.is_none());
    }

    #[test]
    fn affinity_recent_send_window_and_inflight_tie_break_are_exact() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0]);
        affinity.record_send("A");
        assert_eq!(complete(pick(&affinity, None, &pool)), "B");
        {
            let mut state = affinity.lock();
            state.started -= Duration::from_secs(15 * 60);
            state.last_allocation.clear();
        }
        assert_eq!(complete(pick(&affinity, None, &pool)), "A");
        complete(pick(&affinity, Some("on-a"), &pool[..1]));
        complete(pick(&affinity, Some("on-b"), &pool[1..]));
        let chosen = affinity
            .select(
                Some("new"),
                &pool,
                &pool,
                &HashMap::from([("A".to_string(), 1)]),
            )
            .unwrap();
        assert_eq!(account_id(&chosen.account), Some("B"));
        assert_eq!(
            compare_scores(
                (usize::MAX as u128, 0, 0, u64::MAX as u128, 0),
                (usize::MAX as u128 - 1, 0, 0, u64::MAX as u128, 0)
            ),
            Ordering::Greater
        );
        assert_eq!(
            compare_scores((0, 0, 3, 2, 0), (0, 0, 2, 1, 0)),
            Ordering::Less
        );
    }

    fn populate(state: &mut State, pool: &[Value], count: usize, now: Instant) {
        for index in 0..count {
            let generation = state.next();
            state.bindings.insert(
                format!("seed-{index}"),
                Binding {
                    incarnation: generation,
                    generation,
                    owner: Target::of(&pool[index % pool.len()]),
                    pending: None,
                    active: 0,
                    last_activity: now,
                },
            );
        }
    }

    #[test]
    fn affinity_idle_ttl_and_capacity_never_evict_active_leases() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        complete(pick(&affinity, Some("idle"), &pool));
        let active = pick(&affinity, Some("active"), &pool);
        {
            let mut state = affinity.lock();
            for binding in state.bindings.values_mut() {
                binding.last_activity -= IDLE_TTL;
            }
            state.gc(Instant::now(), true);
            assert!(!state.bindings.contains_key("idle"));
            assert!(state.bindings.contains_key("active"));
            populate(&mut state, &pool, MAX_BINDINGS - 1, Instant::now());
        }
        complete(pick(&affinity, Some("replacement"), &pool));
        assert_eq!(affinity.lock().bindings.len(), MAX_BINDINGS);
        assert!(affinity.lock().bindings.contains_key("active"));
        drop(active);
        assert!(affinity.lock().leases.is_empty());

        // 所有记录都活跃时不淘汰；合成计数仅用于填满容量边界。
        {
            let mut state = affinity.lock();
            populate(&mut state, &pool, MAX_BINDINGS, Instant::now());
            while state.bindings.len() > MAX_BINDINGS {
                state.bindings.remove("replacement");
            }
            for binding in state.bindings.values_mut() {
                binding.active = 1;
            }
        }
        let overflow = pick(&affinity, Some("capacity-overflow"), &pool);
        assert_eq!(overflow.reason, "capacity_overflow");
        assert!(overflow.lease.is_none());
        assert!(!affinity.lock().bindings.contains_key("capacity-overflow"));
    }

    #[test]
    fn affinity_reset_fences_old_leases_but_still_releases_them() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0]);
        let mut old = pick(&affinity, Some("same"), &pool);
        affinity.record_send("A");
        affinity.reset();
        let current = pick(&affinity, Some("same"), &pool[1..]);
        old.lease.as_mut().unwrap().confirm();
        drop(old);
        assert_eq!(affinity.lock().bindings["same"].active, 1);
        assert_eq!(affinity.lock().leases.len(), 1);
        assert_eq!(complete(current), "B");
        assert!(affinity.lock().leases.is_empty());
        assert!(affinity.lock().recent.is_empty());
        assert_eq!(distribution(&affinity), [0, 1, 0]);
    }

    #[test]
    fn affinity_reset_rejects_old_request_reservations_and_send_counters() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0]);
        let old_epoch = affinity.epoch();
        affinity.reset();
        assert!(affinity
            .select_in_epoch(Some("old"), &pool, &pool, &HashMap::new(), old_epoch)
            .is_none());
        affinity.record_send_for_epoch("A", old_epoch);
        let state = affinity.lock();
        assert!(state.bindings.is_empty());
        assert!(state.leases.is_empty());
        assert!(state.recent.is_empty());
        assert!(state.last_allocation.is_empty());
        drop(state);
        let current = affinity
            .select_in_epoch(Some("new"), &pool, &pool, &HashMap::new(), affinity.epoch())
            .unwrap();
        assert_eq!(complete(current), "A");
    }

    #[test]
    fn affinity_measure_10000_binding_selection_cost() {
        let affinity = Affinity::default();
        let pool = accounts(&[0, 0, 0]);
        populate(&mut affinity.lock(), &pool, MAX_BINDINGS, Instant::now());
        let mut fresh = Vec::new();
        let mut sticky = Vec::new();
        for index in 0..200 {
            let key = format!("measured-{index}");
            let start = Instant::now();
            complete(pick(&affinity, Some(&key), &pool));
            fresh.push(start.elapsed().as_micros());
            let start = Instant::now();
            complete(pick(&affinity, Some(&key), &pool));
            sticky.push(start.elapsed().as_micros());
        }
        fresh.sort_unstable();
        sticky.sort_unstable();
        eprintln!("affinity bindings=10000 samples=200 selection+confirm+drop_us new_p50={} new_p95={} new_max={} sticky_p50={} sticky_p95={} sticky_max={}",
            fresh[99], fresh[189], fresh[199], sticky[99], sticky[189], sticky[199]);
        assert_eq!(affinity.lock().bindings.len(), MAX_BINDINGS);
        assert!(affinity.lock().leases.is_empty());
    }
}
