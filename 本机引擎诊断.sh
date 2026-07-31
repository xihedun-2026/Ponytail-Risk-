#!/usr/bin/env bash
# 在你自己的电脑上跑：bash 本机引擎诊断.sh
#
# 用来分清「总览一直转圈 / 实时数据源不可用」到底是哪一种：
#   A. 引擎二进制不是本机架构（从别的机器同步过来的） → 重新编译
#   B. 引擎被系统杀掉（信号 9/Killed）              → 安全策略拦截
#   C. 引擎能跑，但 dashboard 太慢撞上 180 秒超时    → 需要批量查询优化
# 只做只读查询，不改数据库、不打印密码。
set -uo pipefail
cd "$(dirname "$0")"

# Windows 的 Git Bash / MSYS 下产物带 .exe 后缀。
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) ENGINE="target/release/risk-live-data.exe" ;;
  *) ENGINE="target/release/risk-live-data" ;;
esac
echo "=== 1. 引擎二进制 ==="
if [ ! -e "$ENGINE" ]; then
  echo "没有 $ENGINE —— 还没在本机编译过。跑 ./start-local.sh 会自动编译。"
  exit 1
fi
ls -l "$ENGINE"
file "$ENGINE" 2>/dev/null || true
echo "本机架构：$(uname -s) $(uname -m)"
echo "隔离属性(quarantine)：$(xattr "$ENGINE" 2>/dev/null | tr '\n' ' ' || echo '(无)')"

echo
echo "=== 2. 能不能起来（self-check，不连数据库）==="
if "$ENGINE" self-check >/dev/null 2>&1; then
  echo "OK：引擎可以在本机执行"
else
  rc=$?
  echo "失败：退出码 $rc"
  [ "$rc" -gt 128 ] && echo "→ 被信号 $((rc - 128)) 杀掉（架构不符或系统安全策略拦截）。解决：rm -f $ENGINE && ./start-local.sh 重新编译。"
  exit 1
fi

echo
echo "=== 3. 载入配置 ==="
if [ -f .env.local ]; then
  set -a; . ./.env.local; set +a; echo "已载入 .env.local"
elif [ -f 本地配置-复制成.env.local.txt ]; then
  set -a; . ./本地配置-复制成.env.local.txt; set +a
  echo "已载入 本地配置-复制成.env.local.txt（建议复制成 .env.local）"
else
  echo "没找到配置文件，下面两步会走演示数据。"
fi

run_timed() { # run_timed <说明> <操作>
  local label="$1" op="$2" start end rc
  start=$(date +%s)
  "$ENGINE" "$op" >/tmp/risk_diag_out.json 2>/tmp/risk_diag_err.txt
  rc=$?
  end=$(date +%s)
  echo "$label：退出码 $rc，耗时 $((end - start)) 秒"
  if [ "$rc" -ne 0 ]; then
    echo "  stderr: $(head -c 300 /tmp/risk_diag_err.txt)"
  else
    echo "  输出前 160 字节: $(head -c 160 /tmp/risk_diag_out.json)"
  fi
}

echo
echo "=== 4. 连接测试（应该 1 秒内）==="
run_timed "connection-test" connection-test

echo
echo "=== 5. 总览取数（云端同一条命令是 47 秒；本机若 >180 秒就是超时的原因）==="
echo "别中断，慢慢等，最长可能几分钟…"
run_timed "dashboard" dashboard

echo
echo "=== 结论参考 ==="
echo "· 第 5 步成功但耗时接近或超过 180 秒 → 就是超时。临时办法：启动前 export RISK_ENGINE_TIMEOUT_MS=600000；根治办法是把逐角色查询改成批量查询。"
echo "· 第 5 步很快成功 → 问题不在引擎，把 node server.mjs 那个终端窗口的 live data dashboard: 那行贴出来。"
echo "· 第 2 步就失败 → 二进制问题，按上面提示重新编译。"
