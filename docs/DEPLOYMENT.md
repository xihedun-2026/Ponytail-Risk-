# 部署指南

本文覆盖本机演示、只读实时数据库、插件 Agent 和 Linux 生产发布。首次接入建议始终从演示与 shadow 模式开始。

## 1. 环境要求

| 组件 | 最低版本 | 用途 |
|---|---:|---|
| Node.js | 18 | Web 控制层，无 npm 依赖 |
| Rust | 1.82 | 构建数据引擎、Agent 和 SDK |
| Python | 3.10 | 仅用于差分测试和旧工具基线 |
| C/C++ 构建工具 | 平台当前稳定版 | `rusqlite bundled` 和 C ABI 检查 |

Windows 启动脚本可检查并安装缺失组件。Linux 生产机器使用预编译发布包时不需要 Rust 或源码。

## 2. 本机演示

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File .\start-local.ps1
```

Linux / macOS：

```bash
chmod +x start-local.sh
./start-local.sh
```

默认地址为 `http://127.0.0.1:4173/`。未设置卡密时只为本机演示提供 `PONYTAIL-DEMO-2026`，不要把这个值带入生产环境。

## 3. 配置文件

复制示例并生成独立主密钥：

```bash
cp .env.example .env.local
openssl rand -hex 32
```

Windows 可生成同等强度的值：

```powershell
$bytes = [byte[]]::new(32)
$rng = [Security.Cryptography.RandomNumberGenerator]::Create()
$rng.GetBytes($bytes)
$rng.Dispose()
($bytes | ForEach-Object { $_.ToString("x2") }) -join ""
```

关键变量：

| 变量 | 必需 | 说明 |
|---|---|---|
| `RISK_PORTAL_KEY` | 生产必需 | 控制台登录卡密，禁止使用演示值 |
| `RISK_CONFIG_MASTER_KEY` | 生产必需 | 64 个十六进制字符，用于 AES-GCM 配置加密 |
| `RISK_HOST` | 否 | 默认 `127.0.0.1` |
| `RISK_PORT` | 否 | 默认 `4173` |
| `WDSF_ENGINE` | 否 | 数据引擎绝对路径 |
| `WDSF_LIVE` | 实时模式必需 | 设置为 `1` |
| `WDSF_HOST` | 实时模式必需 | 游戏数据库地址 |
| `WDSF_DB_PORT` | 否 | 默认 `3306` |
| `WDSF_DB_USER` | 实时模式必需 | 只读账号 |
| `WDSF_DB_PASSWORD` | 实时模式必需 | 只从环境读取 |
| `WDSF_MDB` / `WDSF_LDB` | 实时模式必需 | 主库与日志库名 |

`.env.local` 和 `data/` 已被 Git 忽略。不要把凭据放进脚本、README、命令历史或截图。

## 4. 只读实时数据库

先创建受限账号，按实际库名调整：

```sql
create user 'risk_reader'@'风控机地址' identified by '独立强密码';
grant select on game_main.* to 'risk_reader'@'风控机地址';
grant select on game_log.* to 'risk_reader'@'风控机地址';
flush privileges;
```

数据库端口只允许风控机访问。不要使用 `root@%`，也不要把 3306 暴露到公网。

运行只读探针：

```bash
cargo run --release -p wdsf-probe -- \
  --host "$WDSF_HOST" \
  --mdb "$WDSF_MDB" \
  --ldb "$WDSF_LDB"
```

探针通过后设置 `WDSF_LIVE=1` 启动 Portal。保持 shadow 模式，先检查数据覆盖率、编码、规则证据和查询耗时；数据库不可用时，API 会明确返回数据源错误，不以演示数据冒充实服结果。

## 5. 插件 Agent 与 SDK

启动本机 Agent：

```bash
export PGR_TENANT_ID="tenant-demo"
export PGR_SERVER_ID="server-1"
export PGR_LOCAL_TOKEN="至少32字节的独立随机值"
export PGR_QUEUE_DB="./data/plugin-events.db"
export PGR_MODE="shadow"
cargo run --release -p risk-agent -- serve
```

Agent 默认监听 `127.0.0.1:17870`。游戏插件使用：

- Windows：`target/release/risk_sdk.dll`
- Linux：`target/release/librisk_sdk.so`
- 头文件：`crates/risk-sdk/include/ponytail_risk_sdk.h`

完整事件字段、幂等要求和处置回执见 [GAME_PLUGIN_INTEGRATION_V1.md](GAME_PLUGIN_INTEGRATION_V1.md)。接入必须位于游戏逻辑的真实提交点，不能用 UI 文案、客户端按钮或“调用成功”替代权威事件。

## 6. HTTPS 反向代理

Portal 保持监听 loopback，由反向代理终止 TLS。以 Nginx 为例：

```nginx
server {
    listen 443 ssl http2;
    server_name risk.example.com;

    ssl_certificate     /etc/letsencrypt/live/risk.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/risk.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:4173;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-For $remote_addr;
    }
}
```

同时设置：

```bash
RISK_BEHIND_TLS_PROXY=1
RISK_TRUSTED_PROXY_IPS=127.0.0.1,::1
```

只信任实际反向代理地址。Agent 端口不要暴露到公网。

## 7. Linux 签名发布包

发布机准备固定版本 Node.js 二进制和 3072-bit RSA 发布私钥。私钥只保存在发布机或 CI 密钥库：

```bash
umask 077
openssl genrsa -out /secure/ponytail-release-rsa.pem 3072
```

构建预编译包：

```bash
PGR_NODE_BIN=/opt/node/bin/node \
PGR_NODE_SHA256="已核对的Node二进制SHA256" \
PGR_RELEASE_SIGNING_KEY=/secure/ponytail-release-rsa.pem \
  bash deploy/build-linux-bundle.sh \
  --base-url https://download.example.com/releases
```

上传 `dist/linux-release/` 中的安装器、版本包、签名、公钥和 SHA-256 文件。客户侧先独立核对安装器 SHA-256，再执行；不要发布未校验的 `curl | sudo bash` 命令。

安装器会校验 HTTPS、版本包 SHA-256、RSA/SHA-256 独立签名、包内清单和 CPU 架构，然后创建低权限服务账号、Portal/Agent systemd 服务及持久目录。升级使用新的 release 目录和原子软链接切换；健康检查失败时回滚到上个版本。

常用命令：

```bash
systemctl status ponytail-risk ponytail-risk-agent
journalctl -u ponytail-risk -f
sudo cat /etc/ponytail-risk/portal.env
```

`portal.env` 包含敏感配置，只允许 root 读取，不应复制到工单或聊天。

## 8. 上线检查

```bash
cargo fmt --all -- --check
cargo test --workspace --locked
node self_check.mjs
node engine_bridge_check.mjs
node plugin_contract_check.mjs
```

上线前还应确认：

- Portal 只通过 HTTPS 访问，HTTP 和 Agent 端口未对公网开放；
- 数据库账号只有 `SELECT`，连接失败日志不包含密码；
- 所有租户、区服和环境使用独立密钥；
- AI Provider 收到的是脱敏证据，研判不能自动执行处罚；
- 处置命令按 `action.id` 幂等，页面只在收到 `applied` 回执后显示成功；
- 规则阈值已使用真实服分位数校准，并经过至少一个完整观察周期。

## 9. 故障定位

控制台报“实时数据源不可用”或长时间无结果时：

```bash
bash 本机引擎诊断.sh
```

Windows：

```powershell
powershell -ExecutionPolicy Bypass -File .\本机引擎诊断.ps1
```

依次确认引擎架构、数据库网络、只读权限、库名、GBK 字节读取和单次查询耗时。构建成功只证明代码可编译，不证明真实数据库、插件事件或处置回执已打通。
