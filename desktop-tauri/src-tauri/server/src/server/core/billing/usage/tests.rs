use super::*;
use crate::server::core::{account_store::AccountStore, auth::AuthService};
use axum::{
    extract::Json,
    http::{HeaderMap, StatusCode},
    routing::post,
    Router,
};
use std::sync::{Arc, Mutex};

fn service() -> BillingService {
    BillingService::new(AuthService::for_store(AccountStore::with_db(None)))
}

#[test]
fn absent_accounts_and_business_errors_are_not_empty_balance() {
    assert!(resource_accounts(&json!({})).is_err());
    assert!(resource_accounts(&json!({"Response":{"Data":{"Accounts":null}}})).is_err());
    assert!(resource_accounts(
        &json!({"Response":{"Error":{"Code":"failure"},"Data":{"Accounts":[]}}})
    )
    .is_err());
    for data in [
        json!({"Response":{"Data":{"Accounts":[]}}}),
        json!({"data":{"Response":{"Data":{"Accounts":[]}}}}),
        json!({"accounts":[]}),
        json!({"data":{"accounts":[]}}),
    ] {
        assert!(resource_accounts(&data).unwrap().is_empty());
    }
}

#[tokio::test]
async fn enterprise_type_without_id_reports_missing_information() {
    for kind in ["enterprise", "ultimate", "exclusive"] {
        let error = service()
            .query_usage(Some(&json!({"account":{"type":kind}})), None)
            .await
            .unwrap_err();
        assert_eq!(error.status_code, 502);
        assert!(error.message.contains("企业信息"));
    }
}

#[tokio::test]
async fn pagination_preserves_session_filters_precision_and_auth_error_channel() {
    let queries = Arc::new(Mutex::new(Vec::<(String, u64, String)>::new()));
    let recorded = queries.clone();
    let app = Router::new().route(BILLING_USER_RESOURCE.path, post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let recorded = recorded.clone();
        async move {
            assert_eq!(body["OnlyValidPeriod"], true);
            assert_eq!(body["PageSize"], 100);
            assert_eq!(body["ProductCode"], "p_tcaca");
            assert_eq!(body["Status"], json!([0,3]));
            assert!(body.get("PackageCodes").is_none());
            let uid = headers["x-user-id"].to_str().unwrap().to_string();
            let page = body["PageNumber"].as_u64().unwrap();
            let token = headers["authorization"].to_str().unwrap().to_string();
            recorded.lock().unwrap().push((uid.clone(), page, token));
            if uid == "first401" || (uid == "later401" && page == 2) {
                return (StatusCode::UNAUTHORIZED, Json(json!({"code":401})));
            }
            if uid == "failure" && page == 2 { return (StatusCode::BAD_GATEWAY, Json(json!({"code":1}))); }
            if uid == "business" { return (StatusCode::OK, Json(json!({"code":8,"data":{"Response":{"Data":{"Accounts":[]}}}}))); }
            if uid == "missing" { return (StatusCode::OK, Json(json!({"code":0,"data":{}}))); }
            let count: usize = uid.parse().unwrap_or(101);
            let offset = if uid == "overlap" && page == 2 { 99 } else if uid == "repeat" { 0 } else { (page as usize - 1) * 100 };
            let accounts: Vec<Value> = (offset..(offset + 100).min(count)).map(|index| json!({
                "ResourceId":format!("{uid}-{index}"), "CycleCapacityRemainPrecise":0.75,
                "CapacityRemainPrecise":500, "CycleEndTime":"2030-01-01T00:00:00Z"
            })).collect();
            (StatusCode::OK, Json(json!({"code":0,"data":{"Response":{"Data":{"Accounts":accounts,"TotalDosage":1}}}})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let billing = service();
    for (uid, count, pages) in [
        ("0", 0, 1),
        ("100", 100, 2),
        ("101", 101, 2),
        ("200", 200, 3),
        ("2000", 2000, 20),
    ] {
        let session = json!({"endpoint":endpoint,"edition":"intl","auth":{"accessToken":format!("token-{uid}")},"account":{"uid":uid}});
        let usage = billing
            .query_credits_summary(Some(&session), Some("en"))
            .await
            .unwrap();
        // 新精确余额不逐包取整；旧整数简报仍逐包截断保持兼容。
        assert_eq!(usage["totalLeft"], 0);
        assert_eq!(
            usage["creditDetails"]["segments"].as_array().unwrap().len(),
            count
        );
        if uid == "2000" {
            assert_eq!(usage["creditDetails"]["remaining"], Value::Null);
            assert_eq!(usage["creditDetails"]["complete"], false);
        } else {
            assert_eq!(
                usage["creditDetails"]["remaining"].as_f64().unwrap(),
                count as f64 * 0.75
            );
        }
        let entries = queries.lock().unwrap();
        let entries: Vec<_> = entries
            .iter()
            .filter(|(account, _, _)| account == uid)
            .collect();
        assert_eq!(entries.len(), pages);
        for (index, (_, page, token)) in entries.iter().enumerate() {
            assert_eq!(*page as usize, index + 1);
            assert_eq!(token, &format!("Bearer token-{uid}"));
        }
    }
    for uid in ["repeat", "overlap", "failure"] {
        let session =
            json!({"endpoint":endpoint,"auth":{"accessToken":"fixture"},"account":{"uid":uid}});
        let usage = billing.get_personal_usage(&session, None).await.unwrap();
        assert_eq!(usage["resources"].as_array().unwrap().len(), 100);
        assert_eq!(usage["creditDetails"]["remaining"], Value::Null);
        assert_eq!(usage["creditDetails"]["issues"], json!(["truncated"]));
    }
    for uid in ["first401", "later401", "business", "missing"] {
        let session =
            json!({"endpoint":endpoint,"auth":{"accessToken":"fixture"},"account":{"uid":uid}});
        let error = billing
            .get_personal_usage(&session, None)
            .await
            .unwrap_err();
        if uid.contains("401") {
            assert_eq!(error.status_code, 401);
        } else if uid == "business" {
            assert_eq!(error.upstream_code, Some(8));
        } else {
            assert_eq!(error.status_code, 502);
        }
    }
    server.abort();
}
