# 参与贡献

感谢参与 Ponytail Risk。提交变更前，请先确认它能缩短真实的风控接入、分析或复核链路。

## 开发环境

- Rust 1.82+
- Node.js 18+
- Python 3.10+（仅用于 Rust/Python 差分测试）

```bash
git clone https://github.com/xihedun-2026/Ponytail-Risk-.git
cd Ponytail-Risk-
cargo test --workspace --locked
node self_check.mjs
node engine_bridge_check.mjs
node plugin_contract_check.mjs
```

## 提交要求

- 保持改动小而完整，不引入与问题无关的抽象或依赖。
- 非平凡逻辑必须附带最小可运行检查或测试。
- 保持源码和文档为 UTF-8，不提交数据库、日志、构建产物或真实业务数据。
- 新增规则时说明证据来源、误报边界、默认阈值和 shadow 验证方法。
- 不把 AI 结论直接接到封停、扣除、冻结或销毁动作。
- Pull Request 中写明变更、验证命令和仍未验证的部分。

安全问题请按 [SECURITY.md](SECURITY.md) 私密报告。
