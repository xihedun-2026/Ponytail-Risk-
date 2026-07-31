//! SSH 只读探针。对应 `tools/risk_probe.py::ssh_probe`。
//!
//! 只执行只读的排查命令（id/ps/ss/stat/find），不改动目标主机。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use russh::client::{self, Handler};
use russh::keys::PublicKey;
use russh::ChannelMsg;
use serde::Serialize;

/// 单条远程命令的结果。字段名与 Python 版保持一致。
#[derive(Debug, Clone, Serialize)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 探针只做只读排查，这里接受任意主机公钥（与 Python 版的 AutoAddPolicy 行为一致）。
/// 注意：这意味着不防中间人，只应在可信内网对自有服务器使用。
struct AcceptAnyHostKey;

impl Handler for AcceptAnyHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

pub struct SshProbe {
    session: client::Handle<AcceptAnyHostKey>,
}

impl SshProbe {
    pub async fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let mut config = client::Config {
            inactivity_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        };
        config.keepalive_interval = Some(Duration::from_secs(10));

        let mut session = client::connect(Arc::new(config), (host, port), AcceptAnyHostKey).await?;
        if !session
            .authenticate_password(user, password)
            .await?
            .success()
        {
            bail!("SSH 认证失败");
        }
        Ok(Self { session })
    }

    /// 执行一条命令，收集 stdout / stderr / 退出码。
    pub async fn run(&mut self, command: &str) -> Result<CommandResult> {
        let mut channel = self.session.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = 0i32;

        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                // ext == 1 是 stderr。
                ChannelMsg::ExtendedData { data, ext: 1 } => stderr.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => exit_code = exit_status as i32,
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }

        Ok(CommandResult {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
        })
    }
}

/// Python 版执行的六组只读排查命令，原样保留。
pub fn checks() -> Vec<(&'static str, &'static str)> {
    vec![
        ("identity", "id; hostname; date -u '+%Y-%m-%dT%H:%M:%SZ'"),
        (
            "processes",
            "ps -eo pid,ppid,lstart,cmd | grep -E '[m]agic_Linux32|[m]ysqld'",
        ),
        (
            "listeners",
            "(ss -lntp || netstat -lntp) 2>/dev/null | grep -E ':(3306|6101|8101|8110|8120|8161)[[:space:]]' || true",
        ),
        (
            "gs_runtime",
            "pid=$(pgrep -f 'CONFIG=gs/gs2.ini' | head -n1); printf 'pid=%s\\n' \"$pid\"; [ -n \"$pid\" ] && { readlink -f /proc/$pid/exe; readlink -f /proc/$pid/cwd; tr '\\0' ' ' < /proc/$pid/cmdline; printf '\\n'; }",
        ),
        (
            "roots",
            "for p in /home/gs /opt/risk/gs /home/gs/dev_override /data; do [ -e \"$p\" ] && stat -c '%F|%U:%G|%a|%n' \"$p\"; done",
        ),
        (
            "top_level",
            "find /home/gs -maxdepth 2 -type d -printf '%p\\n' 2>/dev/null | sort | head -n 120",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_match_python_probe_set() {
        let checks = checks();
        assert_eq!(checks.len(), 6);
        let names: Vec<&str> = checks.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "identity",
                "processes",
                "listeners",
                "gs_runtime",
                "roots",
                "top_level"
            ]
        );
    }

    #[test]
    fn checks_are_read_only() {
        // 探针不允许出现任何写操作，误加一条就会在这里挡下。
        for (name, command) in checks() {
            for forbidden in ["rm ", "mv ", "kill", "> /", "chmod", "chown", "dd "] {
                assert!(
                    !command.contains(forbidden),
                    "命令 {name} 含疑似写操作：{forbidden}"
                );
            }
        }
    }

    #[test]
    fn command_result_serializes_python_field_names() {
        let value = serde_json::to_value(CommandResult {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        })
        .unwrap();
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["stdout"], "ok");
        assert_eq!(value["stderr"], "");
    }
}
