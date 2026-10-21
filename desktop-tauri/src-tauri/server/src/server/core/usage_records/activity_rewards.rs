//! 国际版活动积分包的观察记录，随余额快照原子落盘，不把时间相关当成领奖归因。
use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::server::core::billing::commodity;

pub(crate) fn is_international(account: &Value) -> bool {
    account["provider"] == "workbuddy-intl"
        || (account["provider"] == "workbuddy" && account["edition"] == "intl")
}

fn resource_key(resource: &Value) -> Option<(String, i64)> {
    let id = match resource.get("resourceId")? {
        Value::String(id) if !id.trim().is_empty() => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => return None,
    };
    Some((id, resource.get("grantAt")?.as_i64()?))
}

/// 只从完整分页快照新增记录；过期、耗尽、查询失败不会删除已有的发放证据。
pub(super) fn reconcile(previous: Option<&Value>, usage: &mut Value, queried_at: i64) {
    let mut ledger = previous.and_then(|value| value.get("activityRewards")).cloned()
        .unwrap_or_else(|| json!({"baselineAt": 0, "checkedAt": 0, "resources": []}));
    let baseline_at = ledger["baselineAt"].as_i64().unwrap_or(0);
    if queried_at > 0 && usage.pointer("/creditDetails/complete") == Some(&Value::Bool(true)) {
        if let Some(credits) = usage.get("credits").and_then(Value::as_array) {
            let mut resources = BTreeMap::new();
            for resource in ledger["resources"].as_array().into_iter().flatten() {
                if let Some(key) = resource_key(resource) {
                    resources.insert(key, resource.clone());
                }
            }
            for credit in credits {
                let Some(code) = credit.get("packageCode").and_then(Value::as_str) else { continue };
                if ![commodity::codes::ACTIVITY, commodity::codes::BONUS_INTL].contains(&code) {
                    continue;
                }
                let grant_at = credit["grantAt"].as_i64().unwrap_or(0);
                if grant_at <= 0 || grant_at > queried_at
                    || !credit["total"].as_f64().is_some_and(|amount| amount.is_finite() && amount > 0.0)
                {
                    continue;
                }
                let mut resource = json!({
                    "resourceId": credit.get("id"),
                    "packageCode": code,
                    "amount": credit["total"],
                    "grantAt": grant_at,
                    "firstObservedAt": queried_at,
                    // 首次见到的存量包只是基线；以后首次出现也不等于由某次签到触发。
                    "observation": if baseline_at > 0 { "newly_observed" } else { "baseline" },
                    "attribution": "unconfirmed",
                });
                if let Some(reason) = credit["grantReason"].as_str().filter(|text| !text.trim().is_empty()) {
                    resource["grantReason"] = json!(reason);
                }
                if let Some(expiry) = credit["expireAt"].as_i64().filter(|at| *at > 0) {
                    resource["expireAt"] = json!(expiry);
                }
                let Some(key) = resource_key(&resource) else { continue };
                if let Some(old) = resources.get(&key) {
                    resource["firstObservedAt"] = old["firstObservedAt"].clone();
                    resource["observation"] = old["observation"].clone();
                    for field in ["grantReason", "expireAt"] {
                        if resource.get(field).is_none() {
                            if let Some(value) = old.get(field) { resource[field] = value.clone(); }
                        }
                    }
                }
                resources.insert(key, resource);
            }
            let mut rows: Vec<Value> = resources.into_values().collect();
            rows.sort_by_key(|row| std::cmp::Reverse(row["grantAt"].as_i64().unwrap_or(0)));
            ledger = json!({
                "baselineAt": if baseline_at > 0 { baseline_at } else { queried_at },
                "checkedAt": queried_at,
                "resources": rows,
            });
        }
    }
    if let Some(object) = usage.as_object_mut() {
        object.insert("activityRewards".into(), ledger);
    }
}

/// 旧快照可只读展示为历史基线；持久化仍只发生在既有余额写入入口。
pub(crate) fn ledger(usage: &Value, queried_at: i64) -> Value {
    if let Some(ledger) = usage.get("activityRewards") {
        return ledger.clone();
    }
    let mut snapshot = usage.clone();
    reconcile(None, &mut snapshot, queried_at);
    snapshot.get("activityRewards").cloned().unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{identity, row_to_record, write_result_in, UsageOutcome, RECORD_COLUMNS};
    use crate::server::db::Db;

    fn pack(id: &str, at: i64) -> Value {
        json!({"id": id, "packageCode": commodity::codes::ACTIVITY, "grantAt": at,
            "total": 30.5, "remain": 0, "grantReason": "official activity", "expireAt": 9000})
    }

    fn usage(packs: Vec<Value>) -> Value {
        json!({"totalLeft": 0, "creditDetails": {"complete": true}, "credits": packs})
    }

    #[test]
    fn reward_ledger_retains_baseline_deduplicates_and_preserves_consumed_resources() {
        let mut first = usage(vec![pack("old", 100)]);
        reconcile(None, &mut first, 1000);
        assert_eq!(first["activityRewards"]["resources"][0]["observation"], "baseline");
        let mut next = usage(vec![pack("new", 1200), pack("old", 100), pack("new", 1200)]);
        reconcile(Some(&first), &mut next, 2000);
        let rows = next["activityRewards"]["resources"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["amount"], 30.5, "consumption must not erase a grant");
        assert_eq!(rows[0]["firstObservedAt"], 2000);
        assert_eq!(rows[0]["observation"], "newly_observed");
        assert_eq!(rows[0]["attribution"], "unconfirmed");
        assert_eq!(rows[1]["firstObservedAt"], 1000);
        let mut empty = usage(vec![]);
        reconcile(Some(&next), &mut empty, 3000);
        assert_eq!(empty["activityRewards"]["resources"], next["activityRewards"]["resources"]);
        assert_eq!(empty["activityRewards"]["checkedAt"], 3000);
        // 上游复用资源 ID 时，不吞掉不同发放时刻的记录。
        let mut reused = usage(vec![pack("old", 3100)]);
        reconcile(Some(&empty), &mut reused, 4000);
        assert_eq!(reused["activityRewards"]["resources"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn reward_ledger_rejects_incomplete_or_unidentified_evidence() {
        let mut first = usage(vec![pack("old", 100)]);
        reconcile(None, &mut first, 1000);
        let mut partial = usage(vec![pack("new", 1200)]);
        partial["creditDetails"]["complete"] = json!(false);
        partial["activityRewards"] = json!({"resources": ["untrusted"]});
        reconcile(Some(&first), &mut partial, 2000);
        assert_eq!(partial["activityRewards"], first["activityRewards"]);
        let mut invalid = usage(vec![pack("", 100), pack("future", 3001), pack("zero", 0)]);
        invalid["credits"].as_array_mut().unwrap().extend([
            json!({"id": "subscription", "packageCode": "subscription", "grantAt": 100, "total": 30}),
            json!({"id": "empty", "packageCode": commodity::codes::ACTIVITY, "grantAt": 100, "total": 0}),
        ]);
        reconcile(None, &mut invalid, 3000);
        assert_eq!(invalid["activityRewards"]["resources"], json!([]));
    }

    #[test]
    fn reward_ledger_sqlite_preserves_history_across_failure_restart_and_identity_change() {
        let path = std::env::temp_dir().join(format!("reward-ledger-{}-{}.db",
            std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let db = Db::open(&path).unwrap();
        let account = json!({"id": "intl", "provider": "workbuddy-intl", "uid": "first"});
        let success = |packs| UsageOutcome { usage: Some(usage(packs)), error: None, code: None };
        db.with(|conn| {
            assert!(write_result_in(conn, &account, &success(vec![pack("old", 100)]), 1000, 1000, false).unwrap());
            // 模拟升级前只有 credits 的旧快照，升级后的首次刷新不能重新发现历史包。
            conn.execute("UPDATE account_usage_records SET usage=?1 WHERE account_id='intl'",
                [usage(vec![pack("old", 100)]).to_string()]).unwrap();
            assert!(write_result_in(conn, &account, &success(vec![pack("new", 1500)]), 2000, 2000, false).unwrap());
            assert!(!write_result_in(conn, &account, &success(vec![pack("stale", 100)]), 900, 900, false).unwrap());
            assert!(write_result_in(conn, &account, &UsageOutcome { usage: None, error: Some("offline".into()), code: None }, 3000, 3000, false).unwrap());
        }).unwrap();
        let reopened = crate::server::db::Db::open(db.file()).unwrap();
        reopened.with(|conn| {
            let record = conn.query_row(&format!("SELECT {RECORD_COLUMNS} FROM account_usage_records WHERE account_id='intl'"), [], row_to_record).unwrap();
            assert_eq!(record.identity, identity(&account));
            assert_eq!(record.last_success_at, 2000);
            assert_eq!(record.error.as_deref(), Some("offline"));
            let ledger = &record.usage.unwrap()["activityRewards"];
            assert_eq!(ledger["resources"].as_array().unwrap().len(), 2);
            assert_eq!(ledger["resources"][1]["firstObservedAt"], 1000);
            assert_eq!(ledger["resources"][1]["observation"], "baseline");
            assert_eq!(ledger["resources"][0]["observation"], "newly_observed");
            let mut partial = usage(vec![pack("unverified", 3100)]);
            partial["creditDetails"]["complete"] = json!(false);
            assert!(write_result_in(conn, &account, &UsageOutcome { usage: Some(partial), error: None, code: None }, 3500, 3500, false).unwrap());
            let partial = conn.query_row(&format!("SELECT {RECORD_COLUMNS} FROM account_usage_records WHERE account_id='intl'"), [], row_to_record).unwrap();
            assert_eq!(&partial.usage.unwrap()["activityRewards"], ledger);
            // 显式构造新身份，不能把同 ID 的旧账号历史带入新账号。
            let mut changed = account.clone();
            changed["uid"] = json!("second");
            assert!(write_result_in(conn, &changed, &success(vec![]), 4000, 4000, false).unwrap());
            let record = conn.query_row(&format!("SELECT {RECORD_COLUMNS} FROM account_usage_records WHERE account_id='intl'"), [], row_to_record).unwrap();
            assert_eq!(record.usage.unwrap()["activityRewards"]["resources"], json!([]));
        }).unwrap();
        drop(reopened);
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
