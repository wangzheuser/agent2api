//! WorkBuddy 积分明细白名单投影，不改变旧资源和整数简报口径。
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;
use serde_json::{json, Value};

use super::commodity::label_of;

const REMAIN: &[&str] = &[
    "SlicePeriodCapacityRemainPrecise",
    "SlicePeriodCapacityRemain",
    "CycleCapacityRemainPrecise",
    "CycleCapacityRemain",
    "CapacityRemainPrecise",
    "CapacityRemain",
    "RemainPrecise",
    "Remain",
    "Remaining",
    "Balance",
];
const TOTAL: &[&str] = &[
    "SlicePeriodCapacitySizePrecise",
    "SlicePeriodCapacitySize",
    "SlicePeriodCapacityPrecise",
    "SlicePeriodCapacity",
    "CycleCapacitySizePrecise",
    "CycleCapacitySize",
    "CycleCapacityPrecise",
    "CycleCapacity",
    "CapacitySizePrecise",
    "CapacitySize",
    "CapacityPrecise",
    "Capacity",
    "TotalCapacityPrecise",
    "TotalCapacity",
    "PackageCapacityPrecise",
    "PackageCapacity",
    "QuotaPrecise",
    "Quota",
    "AmountPrecise",
    "Amount",
];
const ENDS: &[&str] = &[
    "DeductionEndTime",
    "ExpiredTime",
    "SlicePeriodEndTime",
    "PackageEndTime",
    "EndTime",
    "CycleEndTime",
    "ExpireTime",
    "ExpirationTime",
    "ValidEndTime",
    "ValidPeriodEndTime",
    "EndAt",
    "ExpireAt",
];
const STARTS: &[&str] = &[
    "DeductionStartTime",
    "SlicePeriodStartTime",
    "CycleStartTime",
    "StartTime",
    "ValidStartTime",
];

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Segment {
    id: String,
    resource_id: Option<String>,
    package_code: String,
    name: String,
    remaining: Option<f64>,
    total: Option<f64>,
    expires_at: Option<i64>,
    expires_at_text: Option<String>,
    expiry_status: &'static str,
    entitlement_ends_at: Option<i64>,
    state: &'static str,
}

pub(super) fn finite_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Number(value) => value.as_f64(),
        Value::String(value) if !value.trim().is_empty() => value.trim().parse().ok(),
        _ => None,
    }?;
    number.is_finite().then_some(number)
}

fn amount(value: &Value, fields: &[&str], issues: &mut Vec<String>) -> Option<f64> {
    for field in fields {
        let Some(raw) = value.get(*field).filter(|raw| !raw.is_null()) else {
            continue;
        };
        if let Some(number) = finite_number(raw).filter(|number| *number >= 0.0) {
            return Some(number);
        }
        issue(issues, "invalid_amount");
    }
    None
}

pub(super) fn issue(issues: &mut Vec<String>, code: &str) {
    if !issues.iter().any(|item| item == code) {
        issues.push(code.to_string());
    }
}

enum Time {
    Known(i64),
    Unverified(String),
    Unknown,
}

fn time(value: &Value) -> Time {
    if let Some(number) = finite_number(value) {
        let millis = if number < 1e12 {
            number * 1000.0
        } else {
            number
        };
        // 排除 0、1970 哨兵和超出可表示日期范围的数字。
        if millis >= 31_536_000_000.0 && millis <= i64::MAX as f64 {
            let millis = millis as i64;
            if DateTime::from_timestamp_millis(millis).is_some() {
                return Time::Known(millis);
            }
        }
        return Time::Unknown;
    }
    let Some(text) = value.as_str().map(str::trim) else {
        return Time::Unknown;
    };
    if let Ok(date) = DateTime::parse_from_rfc3339(text) {
        return if date.timestamp_millis() >= 31_536_000_000 {
            Time::Known(date.timestamp_millis())
        } else {
            Time::Unknown
        };
    }
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f"));
    if let Ok(date) = naive {
        // 两版脱敏样本的 CycleEndTime 与毫秒 DeductionEndTime 均吻合 UTC+8。
        // 显式偏移确保 UTC 容器与 Windows 主机输出同一绝对时刻。
        let millis = FixedOffset::east_opt(8 * 3600)
            .and_then(|offset| offset.from_local_datetime(&date).single())
            .map(|date| date.timestamp_millis());
        return millis
            .filter(|millis| *millis >= 31_536_000_000)
            .map(Time::Known)
            .unwrap_or(Time::Unknown);
    }
    // 仅日期尚无同类配对证据，不擅自补时刻。
    let valid_naive = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc().timestamp_millis());
    if valid_naive.is_some_and(|millis| millis >= 31_536_000_000) {
        Time::Unverified(text.to_string())
    } else {
        Time::Unknown
    }
}

fn window(value: &Value, fields: &[&str], earliest: bool) -> Time {
    let mut known: Option<i64> = None;
    let mut unverified: Option<String> = None;
    for field in fields {
        match value.get(*field).map(time).unwrap_or(Time::Unknown) {
            Time::Known(millis) => {
                known = Some(match known {
                    Some(old) if earliest => old.min(millis),
                    Some(old) => old.max(millis),
                    None => millis,
                })
            }
            Time::Unverified(text) => {
                if unverified.as_ref().map_or(true, |old| text < *old) {
                    unverified = Some(text);
                }
            }
            Time::Unknown => {}
        }
    }
    // 未知时区候选可能比已知时间更早，不能把其余候选误当最终截止。
    if let Some(text) = unverified {
        Time::Unverified(text)
    } else {
        known.map(Time::Known).unwrap_or(Time::Unknown)
    }
}

fn intersect(a: Time, b: Time, earliest: bool) -> Time {
    match (a, b) {
        (Time::Unverified(text), _) | (_, Time::Unverified(text)) => Time::Unverified(text),
        (Time::Known(a), Time::Known(b)) => Time::Known(if earliest { a.min(b) } else { a.max(b) }),
        (known @ Time::Known(_), _) | (_, known @ Time::Known(_)) => known,
        _ => Time::Unknown,
    }
}

fn text(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    })
}

fn segment(
    parent: &Value,
    period: &Value,
    id: String,
    now: i64,
    issues: &mut Vec<String>,
) -> Segment {
    let package_code = text(parent, &["PackageCode"]).unwrap_or_default();
    let name = text(
        parent,
        &[
            "PackageName",
            "PackageTypeName",
            "AccountName",
            "ProductName",
            "Name",
            "RuleName",
            "Description",
        ],
    )
    .or_else(|| label_of(&package_code).map(str::to_string))
    .unwrap_or_else(|| "积分包".to_string());
    let remaining = amount(period, REMAIN, issues);
    let total = amount(period, TOTAL, issues);
    if remaining.is_none() {
        issue(issues, "invalid_amount");
    }
    if matches!((remaining, total), (Some(left), Some(total)) if left > total + 0.01) {
        issue(issues, "balance_mismatch");
    }
    // 父权益与子周期取交集；金额始终从本周期读取，避免父金额重复。
    let end = intersect(window(parent, ENDS, true), window(period, ENDS, true), true);
    let (expires_at, expires_at_text, expiry_status) = match end {
        Time::Known(millis) => (Some(millis), None, "known"),
        Time::Unverified(text) => {
            issue(issues, "timezone_unverified");
            (None, Some(text), "timezone_unverified")
        }
        Time::Unknown
            if ["NeverExpire", "IsPermanent"].iter().any(|key| {
                period
                    .get(*key)
                    .or_else(|| parent.get(*key))
                    .and_then(Value::as_bool)
                    == Some(true)
            }) =>
        {
            (None, None, "never")
        }
        Time::Unknown => {
            issue(issues, "unknown_expiry");
            (None, None, "unknown")
        }
    };
    let start = intersect(
        window(parent, STARTS, false),
        window(period, STARTS, false),
        false,
    );
    if matches!(start, Time::Unverified(_)) {
        issue(issues, "timezone_unverified");
    }
    let state = if expires_at.is_some_and(|end| end <= now) {
        "expired"
    } else if matches!(start, Time::Known(start) if start > now) {
        "not_started"
    } else if remaining == Some(0.0) {
        "exhausted"
    } else if remaining.is_some() {
        "active"
    } else {
        "unknown"
    };
    let entitlement_ends_at = match window(parent, &["DeductionEndTime", "PackageEndTime"], true) {
        Time::Known(millis) => Some(millis),
        _ => None,
    };
    let resource_id = parent.get("ResourceId").and_then(|value| match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    });
    Segment {
        id,
        resource_id,
        package_code,
        name,
        remaining,
        total,
        expires_at,
        expires_at_text,
        expiry_status,
        entitlement_ends_at,
        state,
    }
}

pub(super) fn personal(accounts: &[Value], fetched_at: i64, mut issues: Vec<String>) -> Value {
    let mut segments = Vec::new();
    let mut trusted_remaining = 0.0;
    let mut all_amounts_known = true;
    let mut unattributed = 0.0;
    let mut identities = std::collections::HashSet::new();
    for (index, parent) in accounts.iter().enumerate() {
        let resource = parent.get("ResourceId").and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_i64().map(|id| id.to_string()))
        });
        let mut base = resource
            .map(|id| format!("resource:{id}"))
            .unwrap_or_else(|| format!("row:{index}"));
        if !identities.insert(base.clone()) {
            base = format!("{base}:row:{index}");
        }
        let parent_remaining = amount(parent, REMAIN, &mut issues);
        let slices = parent
            .get("SlicePeriodUsageDetails")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty());
        if let Some(slices) = slices {
            let child_amounts: Vec<Option<f64>> = slices
                .iter()
                .map(|slice| amount(slice, REMAIN, &mut issues))
                .collect();
            if child_amounts.iter().all(Option::is_some) {
                let mut sum = 0.0;
                let mut period_ids = std::collections::HashSet::new();
                for (slice_index, slice) in slices.iter().enumerate() {
                    let mut identity = period_key(slice, slice_index);
                    if !period_ids.insert(identity.clone()) {
                        identity = format!("{identity}:slice:{slice_index}");
                    }
                    let item = segment(
                        parent,
                        slice,
                        format!("{base}:{identity}"),
                        fetched_at,
                        &mut issues,
                    );
                    if !matches!(item.state, "expired" | "not_started") {
                        sum += item.remaining.unwrap_or(0.0);
                    }
                    segments.push(item);
                }
                let parent_inactive = matches!(window(parent, ENDS, true), Time::Known(end) if end <= fetched_at)
                    || matches!(window(parent, STARTS, false), Time::Known(start) if start > fetched_at);
                let available_parent = if parent_inactive {
                    Some(0.0)
                } else {
                    parent_remaining
                };
                trusted_remaining += available_parent.unwrap_or(sum);
                if let Some(left) = available_parent {
                    if sum > left + 0.01 {
                        issue(&mut issues, "balance_mismatch");
                    } else if left > sum + 0.01 {
                        let delta = left - sum;
                        unattributed += delta;
                        let mut extra = segment(
                            parent,
                            &json!({"Remaining":delta}),
                            format!("{base}:unattributed"),
                            fetched_at,
                            &mut issues,
                        );
                        extra.name = format!("{}（未分配到具体周期）", extra.name);
                        extra.expires_at = None;
                        extra.expires_at_text = None;
                        extra.expiry_status = "unknown";
                        extra.state = "active";
                        issue(&mut issues, "unknown_expiry");
                        segments.push(extra);
                    }
                }
            } else {
                issue(&mut issues, "invalid_amount");
                // 部分子周期缺金额时仅保留父聚合一次，无法确定的周期分配不伪造。
                let mut aggregate = segment(
                    parent,
                    parent,
                    format!("{base}:aggregate"),
                    fetched_at,
                    &mut issues,
                );
                // 子余额无法分配时，父长期权益不能冒充这份积分的周期截止。
                aggregate.expires_at = None;
                aggregate.expires_at_text = None;
                aggregate.expiry_status = "unknown";
                aggregate.name = format!("{}（未分配到具体周期）", aggregate.name);
                issue(&mut issues, "unknown_expiry");
                if let Some(left) = parent_remaining {
                    if !matches!(aggregate.state, "expired" | "not_started") {
                        trusted_remaining += left;
                        unattributed += left;
                    }
                } else {
                    all_amounts_known = false;
                }
                segments.push(aggregate);
            }
        } else {
            let item = segment(parent, parent, base, fetched_at, &mut issues);
            match item.remaining {
                Some(left) if !matches!(item.state, "expired" | "not_started") => {
                    trusted_remaining += left
                }
                None => all_amounts_known = false,
                _ => {}
            }
            segments.push(item);
        }
    }
    if !trusted_remaining.is_finite() || !unattributed.is_finite() {
        issue(&mut issues, "invalid_amount");
        all_amounts_known = false;
    }
    segments.sort_by(|a, b| {
        a.expires_at
            .unwrap_or(i64::MAX)
            .cmp(&b.expires_at.unwrap_or(i64::MAX))
            .then(a.id.cmp(&b.id))
    });
    let complete = all_amounts_known
        && !issues.iter().any(|code| {
            matches!(
                code.as_str(),
                "truncated" | "invalid_amount" | "balance_mismatch"
            )
        });
    let remaining = (all_amounts_known && !issues.iter().any(|code| code == "truncated"))
        .then_some(trusted_remaining);
    json!({"version":1,"kind":"personal","fetchedAt":fetched_at,"complete":complete,"remaining":remaining,"unlimited":false,"unattributedRemaining":if all_amounts_known {Some(unattributed)} else {None},"issues":issues,"segments":segments})
}

fn period_key(period: &Value, index: usize) -> String {
    let identity = text(period, &["SlicePeriodId", "PeriodId"]);
    if let Some(identity) = identity {
        return format!("period:{identity}");
    }
    let key = |time: Time| match time {
        Time::Known(millis) => millis.to_string(),
        Time::Unverified(text) => text,
        Time::Unknown => String::new(),
    };
    let start = key(window(period, STARTS, false));
    let end = key(window(period, ENDS, true));
    if start.is_empty() && end.is_empty() {
        format!("slice:{index}")
    } else {
        format!("period:{start}:{end}")
    }
}

pub(super) fn enterprise(data: &Value, fetched_at: i64) -> Value {
    let limit = data.get("limitNum").and_then(finite_number);
    let credit = data
        .get("credit")
        .and_then(finite_number)
        .filter(|value| *value >= 0.0);
    let unlimited = limit == Some(-1.0);
    let remaining = limit
        .filter(|limit| *limit >= 0.0)
        .zip(credit)
        .map(|(limit, used)| (limit - used).max(0.0));
    let mut issues = Vec::new();
    let mut period = json!({"Remaining": remaining,"Amount":limit.filter(|limit| *limit >= 0.0), "EndTime":data.get("cycleResetTime")});
    if unlimited {
        period["Remaining"] = json!(0);
    }
    let mut item = segment(
        &json!({"PackageName":"企业周期额度"}),
        &period,
        "enterprise:cycle".to_string(),
        fetched_at,
        &mut issues,
    );
    if unlimited {
        item.remaining = None;
        item.state = "active";
    }
    let complete = unlimited || remaining.is_some();
    if !complete {
        issue(&mut issues, "invalid_amount");
    }
    json!({"version":1,"kind":"enterprise","fetchedAt":fetched_at,"complete":complete,"remaining":remaining,"unlimited":unlimited,"unattributedRemaining":if complete {Some(0.0)} else {None},"issues":issues,"segments":[item]})
}

#[cfg(test)]
mod tests;
