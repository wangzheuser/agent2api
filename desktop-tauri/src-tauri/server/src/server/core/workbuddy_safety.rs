//! WorkBuddy 账号安全暂停：独立于转发开关、限额和凭据，人工确认后才能恢复。

use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{account_store::AccountStore, providers::workbuddy::Region, workbuddy_policy};
use crate::server::{errors::GatewayError, logging};

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    paused: bool,
    revision: u64,
    observed_at: i64,
}

fn identity(value: &Value) -> Option<String> {
    let provider = value.get("provider").and_then(Value::as_str);
    let region = match provider {
        Some(provider) => Region::from_provider_id(provider)?,
        None => Region::from_edition_id(value.get("edition").and_then(Value::as_str)),
    };
    workbuddy_policy::identity(value)
        .or_else(|| Some(format!("{}:default", region.provider_id())))
}

fn key(identity: &str) -> String {
    format!("workbuddySafety:{identity}")
}

fn unavailable() -> GatewayError {
    GatewayError::with_status(503, "WorkBuddy 安全状态不可用，已停止上游请求")
}

fn read(conn: &rusqlite::Connection, key: &str) -> Result<State, GatewayError> {
    let text: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key=?1", [key], |row| row.get(0))
        .optional()
        .map_err(|_| unavailable())?;
    match text {
        Some(text) => serde_json::from_str(&text).map_err(|_| unavailable()),
        None => Ok(State::default()),
    }
}

fn view(identity: &str, state: &State) -> Value {
    json!({
        "identity": identity,
        "paused": state.paused,
        "revision": state.revision,
        "observedAt": state.observed_at,
        "upstreamCode": if state.paused { Some(11140) } else { None },
        "reason": if state.paused {
            "上游已明确限制账号；转发及后台请求已暂停，请确认上游恢复后手动解除"
        } else { "" },
    })
}

/// 接受会话或公开账号形态；非 WorkBuddy 账号不改变其既有行为。
pub fn status(store: &AccountStore, session: &Value) -> Result<Value, GatewayError> {
    let Some(identity) = identity(session) else {
        return Ok(json!({"paused": false}));
    };
    let db = store.database().ok_or_else(unavailable)?;
    db.with(|conn| read(conn, &key(&identity)).map(|state| view(&identity, &state)))
        .ok_or_else(unavailable)?
}

pub fn check(store: &AccountStore, session: &Value) -> Result<(), GatewayError> {
    let value = status(store, session)?;
    if value["paused"] == true {
        return Err(GatewayError::with_status(
            403,
            "WorkBuddy 账号处于安全暂停状态，请确认上游限制已解除后手动恢复",
        ).upstream_code(11140));
    }
    Ok(())
}

/// 仅供已明确分类的账号限制调用；不持久化上游原文或凭据。
pub fn pause(store: &AccountStore, session: &Value) -> Result<(), GatewayError> {
    change(store, session, None).map(|_| ())
}

pub fn resume(
    store: &AccountStore,
    session: &Value,
    expected_identity: &str,
    expected_revision: u64,
) -> Result<Value, GatewayError> {
    change(store, session, Some((expected_identity, expected_revision)))
}

fn change(
    store: &AccountStore,
    session: &Value,
    resume: Option<(&str, u64)>,
) -> Result<Value, GatewayError> {
    let identity = identity(session)
        .ok_or_else(|| GatewayError::with_status(400, "该账号不是 WorkBuddy 账号"))?;
    let db = store.database().ok_or_else(unavailable)?;
    db.with_mut(|conn| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| unavailable())?;
        let key = key(&identity);
        let mut state = read(&tx, &key)?;
        if let Some((expected_identity, expected_revision)) = resume {
            if expected_identity != identity || expected_revision != state.revision {
                return Err(GatewayError::with_status(409, "账号身份或安全状态已变化，请刷新后重新确认"));
            }
        }
        state.revision = state.revision.checked_add(1).ok_or_else(unavailable)?;
        state.paused = resume.is_none();
        if state.paused {
            state.observed_at = logging::now_ms();
        }
        let text = serde_json::to_string(&state).map_err(|_| unavailable())?;
        tx.execute(
            "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, text],
        ).map_err(|_| unavailable())?;
        tx.commit().map_err(|_| unavailable())?;
        Ok(view(&identity, &state))
    }).ok_or_else(unavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::db::test_temp::TempDb;

    #[test]
    fn safety_pause_persists_and_stale_resume_cannot_clear_new_restriction() {
        let (db, _file) = TempDb::open("workbuddy-safety");
        let store = AccountStore::with_db(Some(db.clone()));
        let session = json!({"provider":"workbuddy","account":{"uid":"fixture"}});
        assert!(check(&store, &session).is_ok());
        pause(&store, &session).unwrap();
        let first = status(&store, &session).unwrap();
        let reopened = AccountStore::with_db(Some(crate::server::db::Db::open(_file.path()).unwrap()));
        assert_eq!(check(&reopened, &session).unwrap_err().status_code, 403);
        pause(&reopened, &session).unwrap();
        assert_eq!(resume(&reopened, &session, first["identity"].as_str().unwrap(), 1)
            .unwrap_err().status_code, 409);
        let current = status(&reopened, &session).unwrap();
        resume(&reopened, &session, current["identity"].as_str().unwrap(), 2).unwrap();
        assert!(check(&reopened, &session).is_ok());
        pause(&reopened, &session).unwrap();
        assert!(resume(&reopened, &session, current["identity"].as_str().unwrap(), 2).is_err());
    }

    #[test]
    fn safety_is_region_scoped_and_default_identity_survives_token_change() {
        let (db, _file) = TempDb::open("workbuddy-safety-region");
        let store = AccountStore::with_db(Some(db));
        let cn = json!({"provider":"workbuddy","account":{"uid":"same"}});
        let intl = json!({"provider":"workbuddy-intl","account":{"uid":"same"}});
        pause(&store, &cn).unwrap();
        assert!(check(&store, &intl).is_ok());
        let first = json!({"provider":"workbuddy-intl","auth":{"accessToken":"fixture-a"}});
        let refreshed = json!({"provider":"workbuddy-intl","auth":{"accessToken":"fixture-b"}});
        pause(&store, &first).unwrap();
        assert!(check(&store, &refreshed).is_err());
        assert!(check(&store, &json!({"provider":"qoder"})).is_ok());
        assert!(check(&AccountStore::with_db(None), &cn).is_err());
    }
}
