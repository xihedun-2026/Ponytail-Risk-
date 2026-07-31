#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import paramiko
import pymysql


SIGNALS = (
    "char",
    "item",
    "ecard",
    "money",
    "cash",
    "trade",
    "deal",
    "mail",
    "award",
    "reward",
    "task",
    "shop",
    "consign",
    "log",
)


def db_probe(args: argparse.Namespace) -> dict:
    connection = pymysql.connect(
        host=args.host,
        port=args.db_port,
        user=args.db_user,
        password=args.db_password,
        charset="latin1",
        autocommit=True,
        cursorclass=pymysql.cursors.DictCursor,
        read_timeout=12,
        write_timeout=12,
    )
    try:
        with connection.cursor() as cursor:
            cursor.execute("show databases")
            databases = [row["Database"] for row in cursor.fetchall()]
            cursor.execute(
                """
                select table_schema,table_name,table_rows
                from information_schema.tables
                where table_schema not in ('information_schema','mysql','performance_schema','sys')
                order by table_schema,table_name
                """
            )
            tables = cursor.fetchall()
            relevant = [
                row
                for row in tables
                if any(signal in row["table_name"].lower() for signal in SIGNALS)
            ]
            selected_names = {(row["table_schema"], row["table_name"]) for row in relevant}
            columns = {}
            for schema, table in sorted(selected_names):
                cursor.execute(
                    """
                    select column_name,column_type,column_key
                    from information_schema.columns
                    where table_schema=%s and table_name=%s
                    order by ordinal_position
                    """,
                    (schema, table),
                )
                columns[f"{schema}.{table}"] = cursor.fetchall()
            return {
                "databases": databases,
                "table_count": len(tables),
                "relevant_tables": relevant,
                "columns": columns,
            }
    finally:
        connection.close()


def remote_command(client: paramiko.SSHClient, command: str) -> dict:
    _stdin, stdout, stderr = client.exec_command(command, timeout=20)
    output = stdout.read().decode("utf-8", "replace").strip()
    error = stderr.read().decode("utf-8", "replace").strip()
    return {
        "exit_code": stdout.channel.recv_exit_status(),
        "stdout": output,
        "stderr": error,
    }


def ssh_probe(args: argparse.Namespace) -> dict:
    client = paramiko.SSHClient()
    client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    client.connect(
        args.host,
        port=args.ssh_port,
        username=args.ssh_user,
        password=args.ssh_password,
        timeout=10,
        banner_timeout=10,
        auth_timeout=10,
    )
    try:
        checks = {
            "identity": "id; hostname; date -u '+%Y-%m-%dT%H:%M:%SZ'",
            "processes": "ps -eo pid,ppid,lstart,cmd | grep -E '[m]agic_Linux32|[m]ysqld'",
            "listeners": "(ss -lntp || netstat -lntp) 2>/dev/null | grep -E ':(3306|6101|8101|8110|8120|8161)[[:space:]]' || true",
            "gs_runtime": "pid=$(pgrep -f 'CONFIG=gs/gs2.ini' | head -n1); printf 'pid=%s\\n' \"$pid\"; [ -n \"$pid\" ] && { readlink -f /proc/$pid/exe; readlink -f /proc/$pid/cwd; tr '\\0' ' ' < /proc/$pid/cmdline; printf '\\n'; }",
            "roots": "for p in /home/gs /opt/risk/gs /home/gs/dev_override /data; do [ -e \"$p\" ] && stat -c '%F|%U:%G|%a|%n' \"$p\"; done",
            "top_level": "find /home/gs -maxdepth 2 -type d -printf '%p\\n' 2>/dev/null | sort | head -n 120",
        }
        return {name: remote_command(client, command) for name, command in checks.items()}
    finally:
        client.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default=os.environ.get("GAME_DB_HOST", "127.0.0.1"))
    parser.add_argument("--db-port", type=int, default=int(os.environ.get("GAME_DB_PORT", "3306")))
    parser.add_argument("--db-user", default=os.environ.get("GAME_DB_USER", "root"))
    parser.add_argument("--db-password", default=os.environ.get("GAME_DB_PASSWORD", ""))
    parser.add_argument("--ssh-port", type=int, default=int(os.environ.get("GAME_SSH_PORT", "22")))
    parser.add_argument("--ssh-user", default=os.environ.get("GAME_SSH_USER", "root"))
    parser.add_argument("--ssh-password", default=os.environ.get("GAME_SSH_PASSWORD", ""))
    args = parser.parse_args()
    if not args.db_password or not args.ssh_password:
        parser.error("set GAME_DB_PASSWORD and GAME_SSH_PASSWORD")

    result = {"host": args.host, "database": db_probe(args), "server": ssh_probe(args)}
    print(json.dumps(result, ensure_ascii=False, indent=2, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
