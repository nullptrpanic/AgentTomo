#!/usr/bin/env bash
set -euo pipefail

base='https://suoaugc1grnqab7h67pnj.apigateway-cn-beijing.volceapi.com'
app='harness_agent'
user_id='demo'
session_id=''
prompt=''
key=''

usage() {
  printf '用法：%s --apikey KEY --prompt TEXT [--userid ID] [--sessionid ID]\n' "${0##*/}"
  printf '用 --apikey - 可在提示时输入密钥。默认用户为 demo。省略会话 ID 时新建，指定时创建或续聊。\n'
}

argument_error() {
  printf '%s\n' "$1" >&2
  usage >&2
  exit 2
}

while (( $# )); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --apikey|--prompt|--userid|--sessionid) ;;
    *) argument_error '无法识别参数。' ;;
  esac
  [[ ${2-} == *[![:space:]]* ]] || argument_error "$1 需要非空值。"
  case "$1" in
    --apikey) key=$2 ;;
    --prompt) prompt=$2 ;;
    --userid) user_id=$2 ;;
    --sessionid) session_id=$2 ;;
  esac
  shift 2
done

[[ -n $prompt ]] || argument_error '必须指定 --prompt。'
[[ -n $key ]] || argument_error '必须指定 --apikey。'
if [[ $key == '-' ]]; then
  key=''
  IFS= read -rsp 'Runtime API Key: ' key || true
  if [[ -t 0 ]]; then printf '\n' >&2; fi
fi
[[ -n $key && $key != *[[:space:]]* ]] || argument_error '--apikey 需要有效的密钥值。'

api() {
  curl -q -sS --path-as-is --connect-timeout 10 --max-time 120 \
    -H @- -H 'Content-Type: application/json' "$@" <<<"Authorization: Bearer $key"
}

session_status() {
  api --output /dev/null --write-out '%{http_code}' "$base$1"
}

open_session() {
  local path status result body created_id
  path="/apps/$app/users/$(jq -nr --arg id "$user_id" '$id | @uri')/sessions"
  if [[ -n $session_id ]]; then
    path+="/$(jq -nr --arg id "$session_id" '$id | @uri')"
    status=$(session_status "$path")
    case "$status" in
      200) return 0 ;;
      404) ;;
      *) printf '查询会话失败（HTTP %s）。\n' "$status" >&2; return 1 ;;
    esac
  fi

  result=$(api --data-binary '{}' --write-out $'\n%{http_code}' "$base$path")
  status=${result##*$'\n'}
  body=${result%$'\n'*}
  if [[ $status != 2?? ]]; then
    # 两个调用同时指定同一新 ID 时，另一个调用可能刚创建完它。
    if [[ -n $session_id && ( $status == 400 || $status == 409 ) ]] &&
       [[ $(session_status "$path") == 200 ]]; then return 0; fi
    printf '创建会话失败（HTTP %s）：%s\n' "$status" "$body" >&2
    return 1
  fi
  created_id=$(jq -er '.id | strings | select(length > 0)' <<<"$body") || {
    echo '创建会话的响应缺少 id。' >&2
    return 1
  }
  if [[ -n $session_id && $created_id != "$session_id" ]]; then
    echo '服务端返回的会话 ID 与指定值不一致。' >&2
    return 1
  fi
  session_id=$created_id
}

open_session
printf 'session_id=%s\n' "$session_id"

message=$(jq -n --arg app "$app" --arg user "$user_id" --arg sid "$session_id" --arg text "$prompt" '{
  app_name: $app, user_id: $user, session_id: $sid,
  new_message: {role: "user", parts: [{text: $text}]}, streaming: false
}')

# /run_sse 使用 SSE 封装事件。请求完整回复，便于直接显示文字。
reply=$(api --fail-with-body --data-binary "$message" "$base/run_sse") || {
  printf '发送消息失败：%s\n' "$reply" >&2
  exit 1
}
sed -n -e '/^data: *\[DONE\][[:space:]]*$/d' -e 's/^data: *//p' <<<"$reply" |
  jq -ers '
    map(.data // .) as $events
    | [$events[] | .errorMessage // .error // .errorCode // empty] as $errors
    | if ($errors | length) > 0 then error($errors[0] | tostring)
      else
        [$events[] | select(.author != "user" and .content.role != "user")
         | (.content.parts // .parts // [])[] | select(.thought != true) | .text // empty]
        | join("") | if length > 0 then . else error("未收到 Agent 的文本回复") end
      end
  '
