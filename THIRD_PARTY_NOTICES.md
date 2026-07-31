# 第三方组件说明

Ponytail Risk 自身源码采用 MIT License，见 [LICENSE](LICENSE)。

仓库包含固定版本的 Lucide 浏览器图标脚本：

- `public/vendor/lucide-0.468.0.min.js`
- 项目主页：https://lucide.dev/
- 上游许可证：ISC

Rust 依赖及精确版本记录在 `Cargo.lock`。每个依赖仍适用其各自许可证；再发布二进制前，发布者应对当前锁定版本执行许可证清单检查，并随发布物保留所需声明。Node 控制层仅使用 Node.js 标准库，没有 npm 运行时依赖。
