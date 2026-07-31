//! `risk-probe`：RISK 环境只读探针，`tools/risk_probe.py` 的 drop-in 替代。
//!
//! 用途是接入新服前摸清库表结构与进程/端口现状。全部操作只读，
//! 凭据只从命令行或进程环境传入，不写入源码，也不回显到输出里。

mod ssh;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// 表名里出现这些片段就认为与资产/行为相关，需要拉取列结构。
const SIGNALS: [&str; 14] = [
    "char", "item", "ecard", "money", "cash", "trade", "deal", "mail", "award", "reward", "task",
    "shop", "consign", "log",
];

#[derive(Parser, Debug)]
#[command(name = "risk-probe", about = "RISK 环境只读探针")]
struct Cli {
    #[arg(long, env = "GAME_DB_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long = "db-port", env = "GAME_DB_PORT", default_value_t = 3306)]
    db_port: u16,
    #[arg(long = "db-user", env = "GAME_DB_USER", default_value = "root")]
    db_user: String,
    #[arg(long = "db-password", env = "GAME_DB_PASSWORD", default_value = "")]
    db_password: String,
    #[arg(long = "ssh-port", env = "GAME_SSH_PORT", default_value_t = 22)]
    ssh_port: u16,
    #[arg(long = "ssh-user", env = "GAME_SSH_USER", default_value = "root")]
    ssh_user: String,
    #[arg(long = "ssh-password", env = "GAME_SSH_PASSWORD", default_value = "")]
    ssh_password: String,
}

#[derive(Debug, Serialize)]
struct TableRow {
    table_schema: String,
    table_name: String,
    table_rows: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ColumnRow {
    column_name: String,
    column_type: String,
    column_key: String,
}

fn table_is_relevant(table_name: &str) -> bool {
    let lowered = table_name.to_lowercase();
    SIGNALS.iter().any(|signal| lowered.contains(signal))
}

fn db_probe(cli: &Cli) -> Result<Value> {
    let opts = OptsBuilder::new()
        .ip_or_hostname(Some(cli.host.clone()))
        .tcp_port(cli.db_port)
        .user(Some(cli.db_user.clone()))
        .pass(Some(cli.db_password.clone()))
        .prefer_socket(false)
        .tcp_connect_timeout(Some(Duration::from_secs(10)))
        .read_timeout(Some(Duration::from_secs(12)))
        .write_timeout(Some(Duration::from_secs(12)))
        .init(vec!["SET NAMES latin1".to_string()]);
    let mut conn = Conn::new(opts).context("连接 MySQL 失败")?;

    let databases: Vec<String> = conn.query("show databases")?;

    let tables: Vec<(String, String, Option<i64>)> = conn.query(
        "select table_schema,table_name,table_rows
         from information_schema.tables
         where table_schema not in ('information_schema','mysql','performance_schema','sys')
         order by table_schema,table_name",
    )?;
    let table_count = tables.len();

    let relevant: Vec<TableRow> = tables
        .into_iter()
        .filter(|(_, table_name, _)| table_is_relevant(table_name))
        .map(|(table_schema, table_name, table_rows)| TableRow {
            table_schema,
            table_name,
            table_rows,
        })
        .collect();

    // 按 (schema, table) 排序去重，与 Python 版 `sorted(selected_names)` 对齐。
    let mut selected: Vec<(String, String)> = relevant
        .iter()
        .map(|row| (row.table_schema.clone(), row.table_name.clone()))
        .collect();
    selected.sort();
    selected.dedup();

    let mut columns = Map::new();
    for (schema, table) in selected {
        let rows: Vec<(String, String, String)> = conn.exec(
            "select column_name,column_type,column_key
             from information_schema.columns
             where table_schema=? and table_name=?
             order by ordinal_position",
            (&schema, &table),
        )?;
        let mapped: Vec<ColumnRow> = rows
            .into_iter()
            .map(|(column_name, column_type, column_key)| ColumnRow {
                column_name,
                column_type,
                column_key,
            })
            .collect();
        columns.insert(format!("{schema}.{table}"), serde_json::to_value(mapped)?);
    }

    Ok(json!({
        "databases": databases,
        "table_count": table_count,
        "relevant_tables": relevant,
        "columns": columns,
    }))
}

async fn ssh_probe(cli: &Cli) -> Result<Value> {
    let mut probe =
        ssh::SshProbe::connect(&cli.host, cli.ssh_port, &cli.ssh_user, &cli.ssh_password)
            .await
            .context("SSH 连接失败")?;

    let mut results = Map::new();
    for (name, command) in ssh::checks() {
        results.insert(
            name.to_string(),
            serde_json::to_value(probe.run(command).await?)?,
        );
    }
    Ok(Value::Object(results))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.db_password.is_empty() || cli.ssh_password.is_empty() {
        bail!("set GAME_DB_PASSWORD and GAME_SSH_PASSWORD");
    }

    let database = db_probe(&cli)?;
    let server = ssh_probe(&cli).await?;

    let result = json!({
        "host": cli.host,
        "database": database,
        "server": server,
    });
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_list_matches_python() {
        assert_eq!(SIGNALS.len(), 14);
        assert!(SIGNALS.contains(&"money"));
        assert!(SIGNALS.contains(&"consign"));
    }

    #[test]
    fn relevance_filter_is_case_insensitive_substring() {
        assert!(table_is_relevant("money_log"));
        assert!(table_is_relevant("MONEY_LOG"));
        assert!(table_is_relevant("char_info"));
        assert!(table_is_relevant("item_transfer_log"));
        // 只要包含 log 就算相关。
        assert!(table_is_relevant("login_log"));

        assert!(!table_is_relevant("pet_info"));
        assert!(!table_is_relevant("guild"));
        assert!(!table_is_relevant(""));
    }

    #[test]
    fn cli_requires_both_passwords() {
        let cli = Cli::try_parse_from(["risk-probe"]).unwrap();
        // 默认不带密码，main 会据此报错退出。
        assert!(cli.db_password.is_empty());
        assert!(cli.ssh_password.is_empty());
        assert_eq!(cli.db_port, 3306);
        assert_eq!(cli.ssh_port, 22);
        assert_eq!(cli.db_user, "root");
    }

    #[test]
    fn cli_accepts_explicit_flags() {
        let cli = Cli::try_parse_from([
            "risk-probe",
            "--host",
            "10.0.0.5",
            "--db-port",
            "3307",
            "--ssh-port",
            "2222",
            "--db-password",
            "x",
            "--ssh-password",
            "y",
        ])
        .unwrap();
        assert_eq!(cli.host, "10.0.0.5");
        assert_eq!(cli.db_port, 3307);
        assert_eq!(cli.ssh_port, 2222);
    }
}
