use super::*;
use crate::server::core::auto_checkin;

#[path = "../../../../../tests/support/checkin_http.rs"]
mod http_fixture;
use http_fixture::MockUpstream;

fn task(status: &str, reward: i64) -> Value {
    json!({"errno": 0, "data": {"complete_status": status, "reward_point": reward}})
}

async fn run_tasks(replies: [(u16, Value); 2]) -> Value {
    let mut script = vec![(200, json!({"errno": 0, "data": {}}))];
    script.extend(replies);
    let mock = MockUpstream::spawn(script).await;
    let claim = claim_with_headers_at(&[("Cookie".into(), "fixture-cookie".into())], &mock.base)
        .await
        .unwrap();
    let requests = mock.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/api/genflowpro/freepoint/homenew",
            "/api/genflowpro/freepoint/taskComplete",
            "/api/genflowpro/freepoint/taskComplete",
        ]
    );
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[2].method, "POST");
    assert_eq!(requests[1].body, "task_type=LOGIN");
    assert_eq!(requests[2].body, "task_type=CHAT");
    for request in &requests {
        assert_eq!(request.headers["cookie"], "fixture-cookie");
    }
    mock.assert_drained();
    claim
}

fn summary(claim: &Value) -> Value {
    json!({"results": [{"id": "kuku-fixture", "provider": "kuku", "claim": claim, "error": null}]})
}

#[tokio::test]
async fn kuku_driver_rejects_failed_unknown_and_partial_task_results() {
    for (replies, code, reward, partial) in [
        (
            [
                (500, json!({})),
                (200, json!({"errno": -6, "errmsg": "expired"})),
            ],
            401,
            0,
            false,
        ),
        ([(500, json!({})), (503, json!({}))], 500, 0, false),
        (
            [
                (200, task("RISK_REJECTED", 50)),
                (200, json!({"errno": 0, "data": {}})),
            ],
            502,
            0,
            false,
        ),
        (
            [
                (200, task("SUCCESS", -1)),
                (
                    200,
                    json!({"errno": 0, "data": {"complete_status": "SUCCESS"}}),
                ),
            ],
            502,
            0,
            false,
        ),
        (
            [
                (200, task("SUCCESS", 50)),
                (200, json!({"errno": 42, "errmsg": "failed"})),
            ],
            502,
            50,
            true,
        ),
        (
            [(200, task("BADGE_ALREADY_RECEIVED", 0)), (401, json!({}))],
            401,
            0,
            true,
        ),
    ] {
        let claim = run_tasks(replies).await;
        assert_eq!(claim["success"], false);
        assert_eq!(claim["code"], code);
        assert_eq!(claim["partial"], partial);
        assert_eq!(claim["rewardPoints"], reward);
        assert!(claim.get("alreadyCompleted").is_none());
        assert_eq!(claim["tasks"].as_array().unwrap().len(), 2);
        assert!(auto_checkin::completed_account_ids(&summary(&claim)).is_empty());
        assert_eq!(
            auto_checkin::failed_account_labels(&summary(&claim)).len(),
            1
        );
    }
}

#[tokio::test]
async fn kuku_driver_accepts_only_explicit_success_or_already_completed() {
    for (replies, reward, already) in [
        ([task("SUCCESS", 50), task("SUCCESS", 20)], 70, false),
        (
            [task("SUCCESS", 50), task("BADGE_ALREADY_RECEIVED", 0)],
            50,
            false,
        ),
        (
            [task("SUCCESS", 0), task("BADGE_ALREADY_RECEIVED", 0)],
            0,
            true,
        ),
    ] {
        let claim = run_tasks(replies.map(|value| (200, value))).await;
        assert_eq!(claim["success"], !already);
        assert_eq!(claim["alreadyCompleted"], already);
        assert_eq!(claim["rewardPoints"], reward);
        assert_eq!(
            auto_checkin::completed_account_ids(&summary(&claim)),
            ["kuku-fixture"]
        );
        assert!(auto_checkin::failed_account_labels(&summary(&claim)).is_empty());
    }
}

#[tokio::test]
async fn kuku_driver_panel_errors_do_not_claim_or_misclassify_business_failures() {
    for (errno, status) in [(-6, 401), (42, 502)] {
        let mock = MockUpstream::spawn(vec![(
            200,
            json!({"errno": errno, "errmsg": "fixture-error"}),
        )])
        .await;
        let error = claim_with_headers_at(&[], &mock.base).await.unwrap_err();
        assert_eq!(error.status_code, status);
        assert_eq!(mock.requests().len(), 1);
        assert_eq!(mock.requests()[0].path, "/api/genflowpro/freepoint/homenew");
        mock.assert_drained();
    }
}
