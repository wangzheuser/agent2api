#!/usr/bin/env bash
# us2 部署后基础验收；真实模型、余额、签到和活动领取须单独进行语义验证。

set -euo pipefail

PROJECT_DIR="${AGENT2API_PROJECT_DIR:-/opt/docker_projects/agent2api}"
EXPECTED_COMMIT="${AGENT2API_EXPECTED_COMMIT:-}"
BASE_URL="${AGENT2API_BASE_URL:-http://127.0.0.1:3065}"
CONTAINER="${AGENT2API_CONTAINER:-agent2api}"

fail() { echo "FAIL: $1" >&2; exit 1; }
pass() { echo "PASS: $1"; }

command -v curl >/dev/null || fail "缺少 curl"
command -v docker >/dev/null || fail "缺少 docker"
[[ -d "$PROJECT_DIR" ]] || fail "项目目录不存在：$PROJECT_DIR"

actual_commit="$(git -C "$PROJECT_DIR" rev-parse HEAD)" || fail "无法读取服务器提交"
if [[ -n "$EXPECTED_COMMIT" && "$actual_commit" != "$EXPECTED_COMMIT" ]]; then
  fail "提交不匹配：expected=$EXPECTED_COMMIT actual=$actual_commit"
fi
pass "commit=$actual_commit"

docker compose -f "$PROJECT_DIR/docker-compose.yml" config --quiet || fail "Compose 配置无效"
pass "compose config"

running="$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)"
[[ "$running" == true ]] || fail "容器未运行：$CONTAINER"
health="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$CONTAINER")"
[[ "$health" == healthy || "$health" == none ]] || fail "容器健康状态异常：$health"
restart_count="$(docker inspect -f '{{.RestartCount}}' "$CONTAINER")"
pass "container=$CONTAINER health=$health restart_count=$restart_count"

health_code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/health")" || fail "/health 请求失败"
[[ "$health_code" == 200 ]] || fail "/health HTTP $health_code"
pass "/health HTTP 200"

models_code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/v1/models")" || fail "/v1/models 请求失败"
[[ "$models_code" == 200 || "$models_code" == 401 ]] || fail "/v1/models HTTP $models_code"
pass "/v1/models HTTP $models_code"

panel_code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/")" || fail "管理面板请求失败"
[[ "$panel_code" == 200 || "$panel_code" == 302 || "$panel_code" == 401 ]] || fail "管理面板 HTTP $panel_code"
pass "panel HTTP $panel_code"

fatal_count="$(docker logs --since "${AGENT2API_LOG_SINCE:-15m}" "$CONTAINER" 2>&1 | grep -Eic 'fatal|panic' || true)"
fatal_count="${fatal_count:-0}"
[[ "$fatal_count" == 0 ]] || fail "最近日志包含 fatal/panic：$fatal_count"
pass "recent logs fatal/panic=0"
