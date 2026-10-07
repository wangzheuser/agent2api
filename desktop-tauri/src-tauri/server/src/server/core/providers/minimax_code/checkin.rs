//! MiniMax Code 每日签到：领取确认与余额核验分开，核验失败不重复领取。
use super::credentials::Credentials;
use super::{auth, balance, refresh};
use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;
use serde_json::{json, Value};

const STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
const CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";

pub async fn claim_daily_checkin(
    store: &AccountStore,
    account_id: &str,
) -> Result<Value, GatewayError> {
    let (id, _, _) = refresh::snapshot(store, account_id)?;
    let credentials = refresh::ensure_fresh(store, &id, false).await?;
    let mut result = claim_daily_checkin_once(store, &id, &credentials).await;
    if result
        .as_ref()
        .err()
        .is_some_and(|error| error.status_code == 401)
        && credentials.can_refresh()
    {
        let fresh = refresh::ensure_fresh(store, &id, true).await?;
        result = claim_daily_checkin_once(store, &id, &fresh).await;
    }
    result
}

async fn claim_daily_checkin_once(
    store: &AccountStore,
    account_id: &str,
    credentials: &Credentials,
) -> Result<Value, GatewayError> {
    let (_, session, _) = refresh::snapshot(store, account_id)?;
    let proxy = auth::account_proxy(&session)?;
    let user_id = auth::real_user_id(credentials, proxy.as_ref()).await?;
    let before = auth::payload(
        auth::request_json(
            "GET",
            &format!(
                "{}{STATUS_PATH}?timezone_id=Asia%2FShanghai",
                auth::base_url()
            ),
            None,
            &credentials.access_token,
            &user_id,
            proxy.as_ref(),
        )
        .await?,
        "签到状态查询",
    )?;
    let panel = before.get("data").unwrap_or(&before);
    let today = panel
        .get("days")
        .and_then(Value::as_array)
        .and_then(|days| {
            days.iter().find(|day| {
                let flag = day.get("is_today").or_else(|| day.get("isToday"));
                flag.and_then(Value::as_bool) == Some(true)
                    || flag
                        .and_then(Value::as_str)
                        .is_some_and(|text| matches!(text.trim(), "true" | "1"))
            })
        });
    let status = today
        .and_then(|day| day.get("status"))
        .and_then(number)
        .or_else(|| panel.get("status").and_then(number))
        .unwrap_or(0);
    if status == 3 {
        let result = claim_value(&json!({"claim_result":2}), today);
        let wallet = balance::query_with_identity(credentials, &user_id, proxy.as_ref()).await;
        return Ok(with_credit_verification(result, None, wallet));
    }
    if status != 2 {
        return Ok(
            json!({"success":false,"alreadyCompleted":false,"claimable":false,
            "status":"not_claimable","msg":"今天暂不可领取"}),
        );
    }
    let wallet_before = balance::query_with_identity(credentials, &user_id, proxy.as_ref()).await;
    if wallet_before
        .as_ref()
        .err()
        .is_some_and(|error| error.status_code == 401)
    {
        return Err(wallet_before.expect_err("401"));
    }
    let claim = auth::payload(
        auth::request_json(
            "POST",
            &format!(
                "{}{CLAIM_PATH}?timezone_id=Asia%2FShanghai",
                auth::base_url()
            ),
            Some(&json!({})),
            &credentials.access_token,
            &user_id,
            proxy.as_ref(),
        )
        .await?,
        "签到领取",
    )?;
    let result = claim_value(claim.get("data").unwrap_or(&claim), today);
    if result["success"] != true && result["alreadyCompleted"] != true {
        return Ok(result);
    }
    if result["success"] == true {
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    }
    // POST 已被上游确认；余额查询错误（含 401）不再冒泡成领取重试。
    let wallet_after = balance::query_with_identity(credentials, &user_id, proxy.as_ref()).await;
    let before_amount = wallet_before
        .ok()
        .and_then(|wallet| wallet["available"].as_f64());
    let verified = with_credit_verification(result, before_amount, wallet_after);
    // 只记录类型受控的金额/状态与哈希关联，不记录原始 claim_id 或凭据。
    crate::server::logging::log(
        "[MiniMax Checkin]",
        &json!({
            "claimResult":verified["claimResult"].as_i64(),
            "points":verified.pointer("/reward/points").and_then(number),
            "expiresAt":verified.get("expiresAt").and_then(number),
            "claimReference":verified["claimReference"].as_str(),
            "creditVerification":verified["creditVerification"].as_str(),
            "balanceBefore":verified["balanceBefore"].as_f64(),
            "balanceAfter":verified["balanceAfter"].as_f64(),
        })
        .to_string(),
    );
    Ok(verified)
}

fn claim_value(data: &Value, today: Option<&Value>) -> Value {
    let result = data.get("claim_result").and_then(number).unwrap_or(0);
    let points = data
        .get("points")
        .or_else(|| today.and_then(|day| day.get("points")))
        .cloned();
    // 官方 bonus_points 已计入 points，避免重复加分；面板预告不等于到账凭据。
    json!({
        "success":result == 1, "alreadyCompleted":result == 2, "claimable":false,
        "status":match result {1 => "claimed", 2 => "already_completed", _ => "unknown"},
        "rewardKind":"daily_checkin", "claimResult":result, "reward":{"points":points},
        "expiresAt":data.get("expire_at_ms"),
        "claimReference":data.get("claim_id").and_then(Value::as_str).map(crate::server::core::providers::refresh_flight::fingerprint),
        "msg":match result {1 => "签到已确认，额度到账待核验", 2 => "今天已签到，额度到账待核验", _ => "上游未确认签到结果"},
    })
}

fn with_credit_verification(
    mut result: Value,
    before: Option<f64>,
    after: Result<Value, GatewayError>,
) -> Value {
    let after_amount = after
        .as_ref()
        .ok()
        .and_then(|wallet| wallet["available"].as_f64());
    let delta = before
        .zip(after_amount)
        .map(|(before, after)| after - before);
    let points = result.pointer("/reward/points").and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str()?.parse::<f64>().ok())
    });
    let increased = result["success"] == true
        && points.is_some_and(|points| {
            points.is_finite() && points > 0.0 && delta.is_some_and(|delta| delta + 1e-8 >= points)
        });
    result["creditVerification"] = json!(if after_amount.is_none() {
        "query_failed"
    } else if increased {
        "balance_increased"
    } else {
        "unverified"
    });
    result["balanceBefore"] = json!(before);
    result["balanceAfter"] = json!(after_amount);
    result["balanceDelta"] = json!(delta);
    if let Ok(wallet) = after {
        result["wallet"] = wallet;
    }
    if increased {
        result["msg"] = json!(format!(
            "签到已确认，余额增加 {} credits",
            delta.unwrap_or(0.0)
        ));
    }
    result
}

fn number(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn signed_checkin_queries_personal_balance_and_never_reclaims_after_confirmation() {
        use axum::{
            body::Bytes,
            extract::State,
            http::{HeaderMap, Method, Uri},
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        async fn handle(
            State(claims): State<Arc<AtomicUsize>>,
            method: Method,
            uri: Uri,
            headers: HeaderMap,
            body: Bytes,
        ) -> Json<Value> {
            assert!(headers.contains_key("yy") && headers.contains_key("x-signature"));
            assert_eq!(headers["authorization"], "Bearer test-token");
            let url = url::Url::parse(&format!("http://localhost{uri}")).unwrap();
            let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(query["client"], "mcode");
            if uri.path() == "/v1/api/user/info" {
                return Json(
                    json!({"statusInfo":{"code":0},"data":{"userInfo":{"realUserID":"real-user"}}}),
                );
            }
            assert_eq!(query["user_id"], "real-user");
            let data = match uri.path() {
                "/matrix/api/v1/user/get_user_extra_info" => {
                    json!({"workspaces":[{"workspace_type":0,"workspace_id":"personal"}]})
                }
                "/matrix/api/v1/commerce/get_membership_info" => {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&body).unwrap(),
                        json!({"workspace_id":"personal"})
                    );
                    json!({"is_migrated_to_op":true,"op_credit_summary":{"total_remaining_amount":"0","free_remaining_amount":"0","purchased_remaining_amount":"0"}})
                }
                STATUS_PATH => {
                    json!({"days":[{"is_today":true,"status":if claims.load(Ordering::SeqCst)==0 {2} else {3},"points":800,"bonus_points":400}]})
                }
                CLAIM_PATH => {
                    assert_eq!(method, Method::POST);
                    assert_eq!(body.as_ref(), b"{}");
                    assert_eq!(claims.fetch_add(1, Ordering::SeqCst), 0, "duplicate claim");
                    json!({"claim_result":1,"points":800,"claim_id":"fixture-receipt","expire_at_ms":1800000000000i64})
                }
                _ => panic!("unexpected management endpoint"),
            };
            Json(json!({"base_resp":{"status_code":0},"data":data}))
        }
        let claims = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().fallback(handle).with_state(claims.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(old) => std::env::set_var("MINIMAX_CODE_SERVER_BASE", old),
                    None => std::env::remove_var("MINIMAX_CODE_SERVER_BASE"),
                }
            }
        }
        let _restore = Restore(std::env::var("MINIMAX_CODE_SERVER_BASE").ok());
        std::env::set_var("MINIMAX_CODE_SERVER_BASE", format!("http://{address}"));
        let (db, _guard) = crate::server::db::test_temp::TempDb::open("minimax-signed-checkin");
        let store = AccountStore::with_db(Some(db));
        let credentials = Credentials::from_payload(&json!({"accessToken":"test-token","refreshToken":"r","expiresAt":crate::server::logging::now_ms()+3600000})).unwrap();
        let account = store
            .add_minimax_code_account(&credentials, None, "fixture")
            .unwrap();
        let id = account["id"].as_str().unwrap();
        let first = claim_daily_checkin(&store, id).await.unwrap();
        assert_eq!(first["success"], true);
        assert_eq!(first["creditVerification"], "unverified");
        assert_eq!(first["balanceAfter"], 0.0);
        let second = claim_daily_checkin(&store, id).await.unwrap();
        assert_eq!(second["alreadyCompleted"], true);
        assert_eq!(claims.load(Ordering::SeqCst), 1);
        assert_eq!(
            balance::query_usage(&store, id).await.unwrap()["source"],
            "personal_workspace_membership"
        );
        assert_eq!(store.minimax_code_account_record(id).unwrap()["id"], id);
        task.abort();
    }

    #[test]
    fn claim_confirmation_does_not_assert_credit_receipt() {
        let claim = claim_value(
            &json!({"claim_result":"1","points":800,"expire_at_ms":1800000000000i64,"claim_id":"receipt"}),
            Some(&json!({"points":800,"bonus_points":400})),
        );
        assert_eq!(claim["reward"]["points"], 800);
        assert_ne!(claim["claimReference"], "receipt");
        let pending =
            with_credit_verification(claim.clone(), Some(0.0), Ok(json!({"available":0.0})));
        assert_eq!(pending["success"], true);
        assert_eq!(pending["creditVerification"], "unverified");
        let credited =
            with_credit_verification(claim.clone(), Some(0.0), Ok(json!({"available":800.0})));
        assert_eq!(credited["creditVerification"], "balance_increased");
        let failed_query = with_credit_verification(
            claim,
            Some(0.0),
            Err(GatewayError::with_status(401, "expired")),
        );
        assert_eq!(failed_query["success"], true);
        assert_eq!(failed_query["creditVerification"], "query_failed");
        assert_eq!(
            claim_value(&json!({"claim_result":2}), None)["alreadyCompleted"],
            true
        );
        assert_eq!(
            claim_value(&json!({"claim_result":0}), None)["success"],
            false
        );
    }
}
