#!/usr/bin/env bash
# 在本机（macOS / Linux）一条命令启动风控控制台。
#
#   ./start-local.sh
#
# 缺 Node / Rust 会先问你要不要装，装完继续；然后构建引擎、预检数据库、启动服务。
# 凭据只从 .env.local 读取，不写进源码，也不打印到终端。
set -euo pipefail

cd "$(dirname "$0")"
GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RED=$'\033[31m'; RESET=$'\033[0m'
info(){ printf '%s==>%s %s\n' "$GREEN" "$RESET" "$1"; }
warn(){ printf '%s==>%s %s\n' "$YELLOW" "$RESET" "$1"; }
fail(){ printf '%s==>%s %s\n' "$RED" "$RESET" "$1" >&2; exit 1; }
ask(){ # ask "提示" -> 0 表示同意
  if [ ! -t 0 ]; then return 1; fi
  printf '%s==>%s %s [Y/n] ' "$YELLOW" "$RESET" "$1"
  read -r reply </dev/tty || return 1
  case "${reply:-y}" in [Yy]*|"") return 0 ;; *) return 1 ;; esac
}

# ---------- 1. Node ----------
if ! command -v node >/dev/null 2>&1; then
  warn "没找到 Node.js（运行网页服务需要它）。"
  if command -v brew >/dev/null 2>&1; then
    if ask "用 Homebrew 安装 Node？"; then brew install node; fi
  fi
fi
command -v node >/dev/null 2>&1 || fail "仍然没有 Node。请手动安装后重跑：
  · 有 Homebrew：brew install node
  · 没有：去 https://nodejs.org/ 下载 LTS 安装包（一路下一步即可）"

NODE_MAJOR=$(node -p 'process.versions.node.split(".")[0]')
[ "$NODE_MAJOR" -ge 18 ] || fail "Node 版本过低（当前 $(node -v)），需要 18+。"
info "Node $(node -v)"

# ---------- 2. Rust 引擎 ----------
# Windows 的 Git Bash / MSYS 下产物带 .exe 后缀，否则下面的 -x 判断永远为假。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ENGINE="target/release/risk-live-data.exe" ;;
  *) ENGINE="target/release/risk-live-data" ;;
esac
# 只看"文件存在且可执行"不够：从别的机器同步过来的二进制架构可能不对，
# 在本机根本跑不起来。这里直接试运行一次自检来判断。
engine_works(){ [ -x "$ENGINE" ] && "$ENGINE" self-check >/dev/null 2>&1; }

if ! engine_works; then
  if [ -e "$ENGINE" ]; then
    warn "现有引擎在本机跑不起来（多半是别的系统/架构编译的），重新构建。"
    rm -f "$ENGINE"
  fi
  # rustup 装在 ~/.cargo，新开的 shell 里可能还没进 PATH
  if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi

  # macOS 上编译 Rust 需要 Xcode 命令行工具提供链接器，否则会报 linker `cc` not found
  if [ "$(uname -s)" = "Darwin" ] && ! xcode-select -p >/dev/null 2>&1; then
    warn "没装 Xcode 命令行工具（编译需要它提供链接器）。"
    if ask "现在安装？会弹出系统安装窗口，装完再重跑本脚本。"; then
      xcode-select --install || true
      fail "请等系统窗口把命令行工具装完，然后重新运行 ./start-local.sh"
    fi
    fail "缺少命令行工具。手动装：xcode-select --install"
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    warn "没找到 Rust（风控引擎用 Rust 写的，需要编译一次）。"
    if ask "现在自动安装 Rust？（官方 rustup，装到 ~/.cargo，不动系统）"; then
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
      . "$HOME/.cargo/env"
    fi
  fi
  command -v cargo >/dev/null 2>&1 || fail "仍然没有 Rust。手动装：
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  然后重跑本脚本。"

  info "正在构建 Rust 引擎（首次约 1-2 分钟，之后秒开）…"
  # 只构建引擎；risk-probe 依赖 russh/tokio，本地跑网页用不到。
  cargo build --release -p risk-engine
fi
engine_works || fail "引擎构建后仍无法运行：$ENGINE"
info "引擎就绪"

# ---------- 3. 配置 ----------
# 数据库连接在网页「规则与设置」页填写并加密保存，所以这里只需要一个登录卡密。
# .env.local 是可选的：有就用，没有就用默认卡密，绝不因此卡住。
if [ -f .env.local ]; then
  # shellcheck disable=SC1091
  set -a; . ./.env.local; set +a
  info "已载入 .env.local"
fi
export RISK_PORTAL_KEY="${RISK_PORTAL_KEY:-PONYTAIL-LOCAL-2026}"
export GAME_DB_LIVE="${GAME_DB_LIVE:-0}"
export RISK_PORT="${RISK_PORT:-4173}"

# ---------- 4. 数据源 ----------
# 数据库连接正常是在网页「规则与设置」页填写并加密保存的（data/ 目录下）。
# 只有在 .env.local 里显式配了实时模式时，才在启动前预检一次。
if [ "$GAME_DB_LIVE" = "1" ] && [ -n "${GAME_DB_PASSWORD:-}" ]; then
  info "检查数据库连通性…"
  CONN_ERR=$(mktemp)
  trap 'rm -f "$CONN_ERR"' EXIT
  if ! "$ENGINE" connection-test >/dev/null 2>"$CONN_ERR"; then
    warn "数据库连不上，先以演示数据启动。错误："
    sed 's/^/    /' "$CONN_ERR" >&2 || true
    export GAME_DB_LIVE=0
  else
    info "数据库连接正常，核心表可读。"
  fi
elif [ -f data/database-connection.enc.json ]; then
  info "已存在加密的数据库配置，服务会自动载入。"
else
  info "还没配数据库。登录后进「规则与设置」页填连接信息，点「测试并保存」即可切到真实数据。"
fi

# ---------- 5. 启动 ----------
URL="http://127.0.0.1:${RISK_PORT}/"
info "控制台地址： ${URL}"
info "登录卡密：   ${RISK_PORTAL_KEY}"
info "按 Ctrl+C 停止。"

# 服务起来后自动开浏览器（失败也不影响）
( set +e
  sleep 2
  for _ in $(seq 1 30); do
    if curl -sf -o /dev/null "$URL"; then
      if command -v open >/dev/null 2>&1; then open "$URL"; fi
      if command -v xdg-open >/dev/null 2>&1; then xdg-open "$URL"; fi
      break
    fi
    sleep 1
  done ) >/dev/null 2>&1 &

exec node server.mjs
