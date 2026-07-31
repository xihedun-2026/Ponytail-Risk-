//! `risk-live-data`：RISK 风控数据引擎 CLI。
//!
//! 这是 `tools/risk_live_data.py` 的 drop-in 替代：相同的子命令、相同的环境变量、
//! stdout 输出同构 JSON、查不到目标时以退出码 2 返回 `{"error": ...}`。
//! `server.mjs` 因此只需把可执行文件从 `python` 换成本二进制。

mod self_check;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use rusqlite::Connection;
use serde_json::{json, Value};

use risk_adapter::queries::{self, LookupError};
use risk_adapter::{Config, GameDatabase};
use risk_ledger::{apply_snapshot, prepare_ledger};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Operation {
    Dashboard,
    Player,
    Asset,
    #[value(name = "asset-search")]
    AssetSearch,
    Alerts,
    #[value(name = "behavior-profile")]
    BehaviorProfile,
    #[value(name = "gameplay-catalog")]
    GameplayCatalog,
    #[value(name = "collect-once")]
    CollectOnce,
    #[value(name = "connection-test")]
    ConnectionTest,
    #[value(name = "self-check")]
    SelfCheck,
}

#[derive(Parser, Debug)]
#[command(
    name = "risk-live-data",
    about = "RISK 行为风控数据引擎（只读）",
    disable_help_subcommand = true
)]
struct Cli {
    /// 要执行的操作
    #[arg(value_enum)]
    operation: Operation,
    /// 可选查询条件：角色 ID / 角色名 / 账号，或资产 IID
    query: Option<String>,
}

/// 本地快照账本路径。默认 `<当前工作目录>/data/risk.db`，
/// 与 Python 版相对脚本父目录的位置一致（`server.mjs` 以项目根目录为 cwd）。
fn ledger_path() -> Result<PathBuf> {
    let path = match std::env::var("RISK_DB_PATH") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => std::env::current_dir()?.join("data").join("risk.db"),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(path)
}

fn open_ledger() -> Result<Connection> {
    let connection = Connection::open(ledger_path()?)?;
    prepare_ledger(&connection)?;
    Ok(connection)
}

fn run(cli: Cli) -> Result<Value> {
    if cli.operation == Operation::SelfCheck {
        return Ok(json!({ "ok": true, "checks": self_check::run()? }));
    }

    let started_at = Instant::now();
    let mut db = GameDatabase::connect(&Config::from_env()?)?;
    let query = cli.query.as_deref();

    match cli.operation {
        Operation::Dashboard => queries::dashboard_result(&mut db, started_at),
        Operation::Player => queries::player_result(&mut db, query),
        Operation::Asset => {
            // 账本是可选证据源：没有本地账本时资产链路依然可查，只是少了快照节点。
            let ledger = open_ledger().ok();
            queries::asset_result(&mut db, query, ledger.as_ref())
        }
        Operation::AssetSearch => queries::asset_search_result(&mut db, query),
        Operation::Alerts => queries::alerts_result(&mut db).map(Value::from),
        Operation::BehaviorProfile => queries::behavior_profile(&mut db),
        Operation::GameplayCatalog => queries::gameplay_catalog_result(&mut db),
        Operation::CollectOnce => {
            let rows = queries::current_assets(&mut db)?;
            let ledger = open_ledger()?;
            let scanned_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
            let result = apply_snapshot(&ledger, &rows, &scanned_at)?;
            Ok(json!({
                "ok": true,
                "scanned": result.scanned,
                "changes": result.changes,
            }))
        }
        Operation::ConnectionTest => queries::connection_test(&mut db),
        Operation::SelfCheck => unreachable!("已在上面提前返回"),
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(result) => {
            println!("{result}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // 查不到目标是正常业务结果，走 stdout + 退出码 2，
            // 让 server.mjs 能区分「没找到」（404）和「数据源不可用」（503）。
            if let Some(lookup) = error.downcast_ref::<LookupError>() {
                println!("{}", json!({ "error": lookup.0 }));
                return ExitCode::from(2);
            }
            // 其余错误只写 stderr，绝不把连接串或密码带进 stdout。
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_accepts_every_python_operation_name() {
        for name in [
            "dashboard",
            "player",
            "asset",
            "asset-search",
            "alerts",
            "behavior-profile",
            "gameplay-catalog",
            "collect-once",
            "connection-test",
            "self-check",
        ] {
            let cli = Cli::try_parse_from(["risk-live-data", name])
                .unwrap_or_else(|error| panic!("操作 {name} 应可解析：{error}"));
            assert!(cli.query.is_none());
        }
    }

    #[test]
    fn cli_accepts_optional_query() {
        let cli = Cli::try_parse_from(["risk-live-data", "player", "1003281"]).unwrap();
        assert_eq!(cli.operation, Operation::Player);
        assert_eq!(cli.query.as_deref(), Some("1003281"));
    }

    #[test]
    fn cli_rejects_unknown_operation() {
        assert!(Cli::try_parse_from(["risk-live-data", "drop-database"]).is_err());
    }

    #[test]
    fn self_check_runs_without_database() {
        let cli = Cli::try_parse_from(["risk-live-data", "self-check"]).unwrap();
        let result = run(cli).expect("自检不应依赖数据库");
        assert_eq!(result["ok"], true);
        assert!(result["checks"].as_u64().unwrap() >= 20);
    }

    #[test]
    fn ledger_path_honours_env_override() {
        let temporary = std::env::temp_dir()
            .join("risk-ledger-test")
            .join("risk.db");
        std::env::set_var("RISK_DB_PATH", &temporary);
        let path = ledger_path().unwrap();
        std::env::remove_var("RISK_DB_PATH");
        assert_eq!(path, temporary);
        assert!(temporary.parent().unwrap().exists());
    }
}
