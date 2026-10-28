#!/usr/bin/env bash
# 华为云（huaweiyun）部署包装器：镜像替换 + 基础验收。
# 服务器项目目录不是 git 仓库、无 update_version.sh；镜像由本地/构建机构建后
# docker save/load 传入，本脚本负责替换前的备份与 compose 替换，以及验收。

set -euo pipefail

PROJECT_DIR="${AGENT2API_PROJECT_DIR:-/opt/docker_projects/agent2api}"
COMPOSE_FILE="${AGENT2API_HW_COMPOSE:-$PROJECT_DIR/.deploy/docker-compose.huawei.yml}"
ENV_FILE="${AGENT2API_HW_ENV_FILE:-$PROJECT_DIR/.env}"
CONTAINER="${AGENT2API_CONTAINER:-agent2api}"
BASE_URL="${AGENT2API_BASE_URL:-http://127.0.0.1:19050}"

usage() {
  cat <<'EOF'
用法（在华为云服务器上执行）：
  scripts/deploy-hw.sh backup <tag>            # 版本化备份 data/ 与 compose
  scripts/deploy-hw.sh switch <tag>            # 改 compose image 并 up -d（自动先 backup）
  scripts/deploy-hw.sh verify [预期tag]        # 基础验收（镜像/容器/健康/面板/日志）
  scripts/deploy-hw.sh rollback <tag>          # 把 compose image 改回 <tag> 并 up -d

环境变量：
  AGENT2API_PROJECT_DIR   项目目录，默认 /opt/docker_projects/agent2api
  AGENT2API_HW_COMPOSE    compose 文件，默认 .deploy/docker-compose.huawei.yml
  AGENT2API_HW_ENV_FILE   变量来源文件，默认项目根目录的 .env（不存在则不传）
  AGENT2API_CONTAINER     容器名，默认 agent2api
  AGENT2API_BASE_URL      健康检查基址，默认 http://127.0.0.1:19050
EOF
}

require_layout() {
  [[ -d "$PROJECT_DIR" ]] || { echo "错误：项目目录不存在：$PROJECT_DIR" >&2; exit 1; }
  [[ -f "$COMPOSE_FILE" ]] || { echo "错误：compose 文件不存在：$COMPOSE_FILE" >&2; exit 1; }
  command -v docker >/dev/null || { echo "错误：缺少 docker" >&2; exit 1; }
  docker compose version >/dev/null 2>&1 || { echo "错误：缺少 docker compose" >&2; exit 1; }
}

compose_image() {
  sed -n 's/^[[:space:]]*image:[[:space:]]*agent2api:/agent2api:/p' "$COMPOSE_FILE" | head -1
}

set_image_tag() {
  local tag="$1"
  python3 - "$COMPOSE_FILE" "$tag" <<'PY'
import re, sys
path, tag = sys.argv[1], sys.argv[2]
text = open(path, encoding="utf-8").read()
new, n = re.subn(r'(image:\s*agent2api:)[^\s]+', r'\g<1>' + tag, text, count=1)
if n != 1:
    sys.exit("compose 中未找到唯一的 agent2api image 行")
open(path, "w", encoding="utf-8").write(new)
PY
}

backup() {
  local tag="$1" stamp dest
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  dest="$PROJECT_DIR/backups/agent2api-pre-${tag}-${stamp}"
  mkdir -p "$dest"
  if [[ -d "$PROJECT_DIR/data" ]]; then
    tar -C "$PROJECT_DIR" -czf "$dest/data.tar.gz" data
  fi
  cp -a "$COMPOSE_FILE" "$dest/docker-compose.huawei.yml"
  printf 'backup=%s\n' "$dest"
}

up() {
  # docker compose 默认只读「compose 文件所在目录」的 .env，而项目的 .env 与
  # backups/ 一样放在项目根目录 —— 不显式传 --env-file，compose 里的
  # ${AGENT2API_ADMIN_USER:-} 之类会全部渲染成空串（面板管理员、验证码开关、
  # 自更新开关都跟着失效）。这里把根 .env 明确指定为变量来源。
  local args=(-f "$COMPOSE_FILE")
  [[ -f "$ENV_FILE" ]] && args=(--env-file "$ENV_FILE" "${args[@]}")
  docker compose "${args[@]}" up -d
}

switch_tag() {
  local tag="$1"
  docker image inspect "agent2api:$tag" >/dev/null 2>&1 \
    || { echo "错误：镜像不存在：agent2api:$tag" >&2; exit 1; }
  backup "$tag"
  set_image_tag "$tag"
  up
  verify "$tag"
}

verify() {
  local expected_tag="${1:-}" current
  fail() { echo "FAIL: $1" >&2; exit 1; }
  pass() { echo "PASS: $1"; }

  current="$(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null || true)"
  [[ -n "$current" ]] || fail "容器不存在：$CONTAINER"
  if [[ -n "$expected_tag" && "$current" != "agent2api:$expected_tag" ]]; then
    fail "镜像标签不匹配：expected=agent2api:$expected_tag actual=$current"
  fi
  docker image inspect "$current" >/dev/null 2>&1 || fail "镜像不存在：$current"
  pass "image=$current"

  running="$(docker inspect -f '{{.State.Running}}' "$CONTAINER")"
  [[ "$running" == true ]] || fail "容器未运行"
  health="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$CONTAINER")"
  [[ "$health" == healthy || "$health" == none ]] || fail "容器健康状态异常：$health"
  pass "health=$health restarts=$(docker inspect -f '{{.RestartCount}}' "$CONTAINER")"

  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/health")" || fail "/health 请求失败"
  [[ "$code" == 200 ]] || fail "/health HTTP $code"
  pass "/health HTTP 200"

  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/v1/models")" || fail "/v1/models 请求失败"
  [[ "$code" == 200 || "$code" == 401 ]] || fail "/v1/models HTTP $code"
  pass "/v1/models HTTP $code"

  code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "$BASE_URL/")" || fail "管理面板请求失败"
  [[ "$code" == 200 || "$code" == 302 || "$code" == 401 ]] || fail "管理面板 HTTP $code"
  pass "panel HTTP $code"

  fatal="$(docker logs --since "${AGENT2API_LOG_SINCE:-15m}" "$CONTAINER" 2>&1 | grep -Eic 'fatal|panic' || true)"
  fatal="${fatal:-0}"
  [[ "$fatal" == 0 ]] || fail "最近日志包含 fatal/panic：$fatal"
  pass "recent logs fatal/panic=0"
}

command_name="${1:-}"
case "$command_name" in
  backup)   shift; require_layout; backup "${1:?缺少 tag}" ;;
  switch)   shift; require_layout; switch_tag "${1:?缺少 tag}" ;;
  verify)   shift; require_layout; verify "${1:-}" ;;
  rollback) shift; require_layout; set_image_tag "${1:?缺少 tag}"; up; verify "${1:?缺少 tag}" ;;
  *) usage; exit 2 ;;
esac
