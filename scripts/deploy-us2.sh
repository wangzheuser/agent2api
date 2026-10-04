#!/usr/bin/env bash
# us2 日常部署包装器。
# 服务器上的 update_version.sh 仍是唯一发布入口；本脚本只负责异步提交、状态记录和轮询。

set -euo pipefail

PROJECT_DIR="${AGENT2API_PROJECT_DIR:-/opt/docker_projects/agent2api}"
BRANCH="${AGENT2API_DEPLOY_BRANCH:-dev/pr-integration}"
STATE_DIR="$PROJECT_DIR/.deploy/status"
LOCK_FILE="$PROJECT_DIR/.deploy/deploy.lock"
UPDATE_SCRIPT="$PROJECT_DIR/update_version.sh"

usage() {
  cat <<'EOF'
用法：
  scripts/deploy-us2.sh start [分支]
  scripts/deploy-us2.sh status [operation-id]
  scripts/deploy-us2.sh wait [operation-id] [超时秒数]
  scripts/deploy-us2.sh verify [预期提交]

环境变量：
  AGENT2API_PROJECT_DIR   项目目录，默认 /opt/docker_projects/agent2api
  AGENT2API_DEPLOY_BRANCH 部署分支，默认 dev/pr-integration
EOF
}

require_server_layout() {
  [[ -d "$PROJECT_DIR" ]] || { echo "错误：项目目录不存在：$PROJECT_DIR" >&2; exit 1; }
  [[ -x "$UPDATE_SCRIPT" ]] || { echo "错误：部署入口不可执行：$UPDATE_SCRIPT" >&2; exit 1; }
  command -v flock >/dev/null || { echo "错误：需要 flock 以防止并发部署" >&2; exit 1; }
  command -v setsid >/dev/null || { echo "错误：需要 setsid 以脱离 SSH 会话" >&2; exit 1; }
}

validate_branch() {
  [[ "$BRANCH" =~ ^[A-Za-z0-9._/-]+$ ]] || { echo "错误：部署分支包含非法字符" >&2; exit 1; }
}

write_status() {
  local status_file="$1" operation_id="$2" phase="$3" pid="$4" exit_code="$5" started_at="$6" finished_at="$7" log_file="$8"
  local tmp_file="${status_file}.tmp.$$"
  umask 077
  printf '{"operation_id":"%s","phase":"%s","pid":%s,"branch":"%s","started_at":"%s","finished_at":"%s","exit_code":%s,"log":"%s"}\n' \
    "$operation_id" "$phase" "$pid" "$BRANCH" "$started_at" "$finished_at" "$exit_code" "$log_file" > "$tmp_file"
  chmod 600 "$tmp_file"
  mv -f "$tmp_file" "$status_file"
}

start_worker() {
  local operation_id="$1" status_file="$2" log_file="$3" started_at="$4"
  local exit_code=0 finished_at

  write_status "$status_file" "$operation_id" running "$BASHPID" 0 "$started_at" "" "$log_file"
  set +e
  (cd "$PROJECT_DIR" && exec "$UPDATE_SCRIPT") > "$log_file" 2>&1
  exit_code=$?
  set -e
  finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if (( exit_code == 0 )); then
    write_status "$status_file" "$operation_id" completed "$BASHPID" "$exit_code" "$started_at" "$finished_at" "$log_file"
  else
    write_status "$status_file" "$operation_id" failed "$BASHPID" "$exit_code" "$started_at" "$finished_at" "$log_file"
  fi
}

start() {
  require_server_layout
  validate_branch
  mkdir -p "$STATE_DIR"
  local lock_fd operation_id started_at status_file log_file
  exec {lock_fd}>"$LOCK_FILE"
  flock -n "$lock_fd" || { echo "错误：已有部署任务运行中" >&2; exit 2; }

  operation_id="$(date -u +%Y%m%dT%H%M%SZ)-$$"
  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  status_file="$STATE_DIR/$operation_id.json"
  log_file="$STATE_DIR/$operation_id.log"
  write_status "$status_file" "$operation_id" queued 0 0 "$started_at" "" "$log_file"
  printf '%s\n' "$operation_id" > "$STATE_DIR/latest"
  chmod 600 "$STATE_DIR/latest"

  # setsid -f 让 worker 脱离 SSH 会话；文件描述符 9 继承锁，直到 worker 结束。
  setsid -f bash "$0" worker "$operation_id" "$status_file" "$log_file" "$started_at" "$BRANCH" 9>&"$lock_fd" \
    </dev/null >/dev/null 2>&1
  printf 'operation_id=%s\nstatus=%s\nlog=%s\n' "$operation_id" "$status_file" "$log_file"
}

worker() {
  local operation_id="$1" status_file="$2" log_file="$3" started_at="$4"
  BRANCH="${5:-$BRANCH}"
  start_worker "$operation_id" "$status_file" "$log_file" "$started_at"
}

resolve_operation() {
  local operation_id="${1:-}"
  if [[ -z "$operation_id" ]]; then
    [[ -s "$STATE_DIR/latest" ]] || { echo "错误：没有部署记录" >&2; exit 1; }
    operation_id="$(<"$STATE_DIR/latest")"
  fi
  printf '%s/%s.json\n' "$STATE_DIR" "$operation_id"
}

status() {
  local status_file
  status_file="$(resolve_operation "${1:-}")"
  [[ -f "$status_file" ]] || { echo "错误：部署记录不存在：$status_file" >&2; exit 1; }
  cat "$status_file"
}

wait_for_completion() {
  local status_file operation_id="${1:-}" timeout="${2:-1200}" deadline phase elapsed
  status_file="$(resolve_operation "$operation_id")"
  operation_id="$(basename "$status_file" .json)"
  deadline=$(( $(date +%s) + timeout ))
  while :; do
    phase="$(sed -n 's/.*"phase":"\([^"]*\)".*/\1/p' "$status_file" 2>/dev/null || true)"
    case "$phase" in
      completed) cat "$status_file"; return 0 ;;
      failed) cat "$status_file"; return 1 ;;
    esac
    if (( $(date +%s) >= deadline )); then
      echo "错误：等待部署任务超时：$operation_id" >&2
      cat "$status_file"
      return 2
    fi
    elapsed=$(( timeout - (deadline - $(date +%s)) ))
    if (( elapsed == 0 || elapsed % 30 == 0 )); then cat "$status_file"; fi
    sleep 5
  done
}

verify() {
  local expected_commit="${1:-}"
  [[ -x "$PROJECT_DIR/scripts/verify-us2.sh" ]] || { echo "错误：验收脚本不存在：$PROJECT_DIR/scripts/verify-us2.sh" >&2; exit 1; }
  if [[ -n "$expected_commit" ]]; then
    AGENT2API_EXPECTED_COMMIT="$expected_commit" "$PROJECT_DIR/scripts/verify-us2.sh"
  else
    "$PROJECT_DIR/scripts/verify-us2.sh"
  fi
}

command_name="${1:-}"
case "$command_name" in
  start) BRANCH="${2:-$BRANCH}"; start ;;
  worker) shift; worker "$@" ;;
  status) shift; status "${1:-}" ;;
  wait) shift; wait_for_completion "${1:-}" "${2:-1200}" ;;
  verify) shift; verify "${1:-}" ;;
  *) usage; exit 2 ;;
esac
