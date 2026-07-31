//! WDSF MySQL 只读适配层。
//!
//! 这一层只做连接、编解码和取数，**从不写游戏库**。
//! 交接报告 §3 明确：实时模式不封号、不扣款、不修改游戏数据库。

pub mod codec;
pub mod queries;

use std::collections::HashMap;
use std::env;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use mysql::prelude::Queryable;
use mysql::{Conn, OptsBuilder, Params, Value};

use codec::{decode_bytes, encode_query};

/// Python 版脚本里写死的默认测试地址，保留以兼容现有调用方式。
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_MAIN_DATABASE: &str = "dl_mdb_1";
const DEFAULT_LOG_DATABASE: &str = "dl_ldb_1";

/// 连接配置。密码只从进程环境读入，绝不落源码、日志或 API 响应。
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub main_database: String,
    pub log_database: String,
}

/// 校验数据库名，只接受 `[A-Za-z0-9_]{1,64}`（交接报告 §9 安全测试）。
/// 这些标识符会被拼进 SQL，不能走参数化，所以必须在这里挡住。
pub fn validate_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn database_identifier(env_name: &str, default: &str) -> Result<String> {
    let value = env::var(env_name).unwrap_or_else(|_| default.to_string());
    if !validate_identifier(&value) {
        bail!("{env_name} is invalid");
    }
    Ok(value)
}

impl Config {
    /// 从进程环境读取配置，与 `server.mjs::databaseEnvironment` 注入的变量一一对应。
    pub fn from_env() -> Result<Self> {
        let password = env::var("WDSF_DB_PASSWORD").unwrap_or_default();
        if password.is_empty() {
            bail!("WDSF_DB_PASSWORD is required");
        }
        Ok(Self {
            host: env::var("WDSF_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string()),
            port: env::var("WDSF_DB_PORT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(3306),
            user: env::var("WDSF_DB_USER").unwrap_or_else(|_| "root".to_string()),
            password,
            main_database: database_identifier("WDSF_MDB", DEFAULT_MAIN_DATABASE)?,
            log_database: database_identifier("WDSF_LDB", DEFAULT_LOG_DATABASE)?,
        })
    }
}

/// 一个列值。文本列在这里已经完成 GBK 解码。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
    Int(i64),
    Float(f64),
}

impl Cell {
    /// 文本视图。NULL 返回空串，对齐 Python 里 `value or ""` 的用法。
    pub fn text(&self) -> String {
        match self {
            Cell::Null => String::new(),
            Cell::Text(value) => value.clone(),
            Cell::Int(value) => value.to_string(),
            Cell::Float(value) => value.to_string(),
        }
    }

    /// 整数视图。NULL / 空串 / 非数字一律按 0，对齐 `int(value or 0)`
    /// 在本项目实际取值范围内的行为。
    pub fn int(&self) -> i64 {
        match self {
            Cell::Null => 0,
            Cell::Int(value) => *value,
            Cell::Float(value) => *value as i64,
            Cell::Text(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return 0;
                }
                trimmed
                    .parse::<i64>()
                    .or_else(|_| trimmed.parse::<f64>().map(|float| float as i64))
                    .unwrap_or(0)
            }
        }
    }

    /// Python 的真值语义：NULL、空串、0 都是 falsy。
    pub fn is_truthy(&self) -> bool {
        match self {
            Cell::Null => false,
            Cell::Text(value) => !value.is_empty(),
            Cell::Int(value) => *value != 0,
            Cell::Float(value) => *value != 0.0,
        }
    }
}

/// 一行查询结果。
#[derive(Debug, Clone, Default)]
pub struct Row {
    fields: HashMap<String, Cell>,
}

impl Row {
    pub fn get(&self, column: &str) -> Cell {
        self.fields.get(column).cloned().unwrap_or(Cell::Null)
    }

    pub fn text(&self, column: &str) -> String {
        self.get(column).text()
    }

    pub fn int(&self, column: &str) -> i64 {
        self.get(column).int()
    }

    pub fn truthy(&self, column: &str) -> bool {
        self.get(column).is_truthy()
    }

    pub fn columns(&self) -> impl Iterator<Item = (&String, &Cell)> {
        self.fields.iter()
    }
}

/// 查询参数。全部走参数化绑定，不做字符串拼接。
#[derive(Debug, Clone)]
pub enum Param {
    /// 文本参数，按 GBK 编码上行（ASCII 与 GBK 一致，中文角色名也能正确匹配）。
    Str(String),
    Int(i64),
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Param::Str(value.to_string())
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Param::Str(value)
    }
}

impl From<i64> for Param {
    fn from(value: i64) -> Self {
        Param::Int(value)
    }
}

fn to_params(values: &[Param]) -> Params {
    if values.is_empty() {
        return Params::Empty;
    }
    Params::Positional(
        values
            .iter()
            .map(|param| match param {
                Param::Str(value) => Value::Bytes(encode_query(value)),
                Param::Int(value) => Value::Int(*value),
            })
            .collect(),
    )
}

/// 一个 WDSF 只读连接。
pub struct Wdsf {
    conn: Conn,
    main_database: String,
    log_database: String,
}

impl Wdsf {
    /// 建立连接。`SET NAMES latin1` 让服务端不做转码，
    /// 由本地按 GBK 解码，保证与 Python 版取到完全相同的字节。
    pub fn connect(config: &Config) -> Result<Self> {
        let opts = OptsBuilder::new()
            .ip_or_hostname(Some(config.host.clone()))
            .tcp_port(config.port)
            .user(Some(config.user.clone()))
            .pass(Some(config.password.clone()))
            .prefer_socket(false)
            .tcp_connect_timeout(Some(Duration::from_secs(5)))
            .read_timeout(Some(Duration::from_secs(8)))
            .write_timeout(Some(Duration::from_secs(8)))
            .init(vec!["SET NAMES latin1".to_string()]);
        let conn = Conn::new(opts).context("连接 WDSF 数据库失败")?;
        Ok(Self {
            conn,
            main_database: config.main_database.clone(),
            log_database: config.log_database.clone(),
        })
    }

    pub fn main_database(&self) -> &str {
        &self.main_database
    }

    pub fn log_database(&self) -> &str {
        &self.log_database
    }

    /// 把 SQL 里的占位库名换成实际库名。库名已经过 `validate_identifier` 校验。
    fn bind_databases(&self, sql: &str) -> String {
        sql.replace(DEFAULT_MAIN_DATABASE, &format!("`{}`", self.main_database))
            .replace(DEFAULT_LOG_DATABASE, &format!("`{}`", self.log_database))
    }

    pub fn fetch_all(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Row>> {
        let bound = self.bind_databases(sql);
        let started_at = Instant::now();
        let rows: Vec<mysql::Row> = self
            .conn
            .exec(&bound, to_params(params))
            .with_context(|| "查询 WDSF 数据失败".to_string())?;
        if env::var("WDSF_QUERY_TRACE").as_deref() == Ok("1") {
            let label = sql.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
            eprintln!(
                "wdsf-query {}ms rows={} sql={label}",
                started_at.elapsed().as_millis(),
                rows.len()
            );
        }
        Ok(rows.into_iter().map(convert_row).collect())
    }

    pub fn fetch_one(&mut self, sql: &str, params: &[Param]) -> Result<Option<Row>> {
        Ok(self.fetch_all(sql, params)?.into_iter().next())
    }

    /// 取单个聚合计数。查不到行时返回 0，对应 Python 里 `[...]["count"]` 的用法。
    pub fn fetch_count(&mut self, sql: &str, params: &[Param], column: &str) -> Result<i64> {
        Ok(self
            .fetch_one(sql, params)?
            .map(|row| row.int(column))
            .unwrap_or(0))
    }
}

fn convert_row(row: mysql::Row) -> Row {
    let columns = row.columns();
    let mut fields = HashMap::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let name = column.name_str().to_string();
        let cell = match row.as_ref(index) {
            Some(Value::NULL) | None => Cell::Null,
            Some(Value::Bytes(raw)) => Cell::Text(decode_bytes(raw)),
            Some(Value::Int(value)) => Cell::Int(*value),
            Some(Value::UInt(value)) => Cell::Int(*value as i64),
            Some(Value::Float(value)) => Cell::Float(*value as f64),
            Some(Value::Double(value)) => Cell::Float(*value),
            Some(other) => Cell::Text(other.as_sql(false).trim_matches('\'').to_string()),
        };
        fields.insert(name, cell);
    }
    Row { fields }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_validation_matches_security_requirement() {
        // 交接报告 §9：数据库名称只接受 [A-Za-z0-9_]。
        assert!(validate_identifier("dl_mdb_1"));
        assert!(validate_identifier("A"));
        assert!(validate_identifier(&"a".repeat(64)));

        assert!(!validate_identifier(""));
        assert!(!validate_identifier(&"a".repeat(65)));
        assert!(!validate_identifier("bad-name"));
        assert!(!validate_identifier("bad name"));
        assert!(!validate_identifier("db`;drop"));
        assert!(!validate_identifier("库名"));
        assert!(!validate_identifier("a.b"));
    }

    #[test]
    fn cell_int_handles_python_falsy_values() {
        assert_eq!(Cell::Null.int(), 0);
        assert_eq!(Cell::Text(String::new()).int(), 0);
        assert_eq!(Cell::Text("  ".to_string()).int(), 0);
        assert_eq!(Cell::Text("42".to_string()).int(), 42);
        assert_eq!(Cell::Text("-7".to_string()).int(), -7);
        assert_eq!(Cell::Text("3.9".to_string()).int(), 3);
        assert_eq!(Cell::Text("abc".to_string()).int(), 0);
        assert_eq!(Cell::Int(5).int(), 5);
    }

    #[test]
    fn cell_text_never_returns_null_literal() {
        assert_eq!(Cell::Null.text(), "");
        assert_eq!(Cell::Int(7).text(), "7");
        assert_eq!(Cell::Text("北境长歌".to_string()).text(), "北境长歌");
    }

    #[test]
    fn cell_truthiness_matches_python() {
        assert!(!Cell::Null.is_truthy());
        assert!(!Cell::Text(String::new()).is_truthy());
        assert!(!Cell::Int(0).is_truthy());
        assert!(Cell::Text("0".to_string()).is_truthy()); // 非空字符串在 Python 里是 truthy
        assert!(Cell::Int(1).is_truthy());
    }

    #[test]
    fn missing_column_reads_as_null() {
        let row = Row::default();
        assert_eq!(row.text("nope"), "");
        assert_eq!(row.int("nope"), 0);
        assert!(!row.truthy("nope"));
    }

    #[test]
    fn params_encode_chinese_as_gbk() {
        let params = to_params(&[Param::Str("北境长歌".to_string()), Param::Int(3)]);
        match params {
            Params::Positional(values) => {
                assert_eq!(values.len(), 2);
                match &values[0] {
                    Value::Bytes(raw) => {
                        // GBK 下每个汉字两字节。
                        assert_eq!(raw.len(), 8);
                        assert_eq!(decode_bytes(raw), "北境长歌");
                    }
                    other => panic!("期望字节参数，实际 {other:?}"),
                }
                assert_eq!(values[1], Value::Int(3));
            }
            other => panic!("期望位置参数，实际 {other:?}"),
        }
    }

    #[test]
    fn empty_params_map_to_empty() {
        assert!(matches!(to_params(&[]), Params::Empty));
    }
}
