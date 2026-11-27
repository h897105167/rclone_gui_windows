# RCGUI 版本与变更记录约定

这份文档回答四件事：版本号从哪来、什么时候改、文档里怎么描述尚未发布的状态、以及变更记录怎么写。

## 1. 版本的唯一来源

```
Cargo.toml → [workspace.package] version = "0.3.2"
```

成员 crate 全部用 `version.workspace = true` 继承，不写死自己的版本。二进制的版本通过 `env!("CARGO_PKG_VERSION")` 读取（见 `crates/app/src/caps.rs`），插件拿到的是同一个值，商店的 `min_gui_version` 也和它比较。所以**只有改这一行**才叫改版本，其他地方的版本字面量都是 bug。

## 2. 状态词

文档、README、issue 里描述一个功能时，只用这三个词：

| 词 | 含义 | 可以怎么写 |
|---|---|---|
| **released** | 已经出现在某个 tag 的 Release 产物里 | 「0.2.1 支持 X」并附 Release 链接 |
| **developing** | 代码在树里（或部分在树里），没有对外发布的产物 | 「0.3.0 开发中：X 正在实现」 |
| **planned** | 只有设计，还没有实现 | 「规划：X」，不得配截图或勾选状态 |

另外两个只在发布流程内部使用的词：

- **release candidate**：功能冻结、等验收，可能还会修 bug 但不加功能。对外写作「0.3.0 候选版本」，仍然不是 released。
- **blocked**：等外部依赖或决策，写明等的是什么。

规则：

- 当前树里的版本是 **0.3.2，状态 developing**；对外可用的最新版本是 **v0.2.1**。任何"0.3.2 已发布 / 全新版本已上线"的表述都是错的。
- 没有 `v0.3.0`，也没有 `v0.3.1`。这一批工作曾经在树里标成 0.3.0、又改成 0.3.1，但两次都没打 tag、都没发布；它随 0.3.2 一起发布，所以文中出现的「0.3.0 开发中」都指同一批内容，只是版本号从 0.3.2 起算。之所以一直是补丁位而不是 minor 位：对已发布的 0.2.1 而言，这批内容没有改变既有行为；插件用 `min_gui_version` 声明自己真正需要的门槛——携带新插件协议的版本是 0.3.1，而 0.3.2 才在用户打字时重新向插件要状态，所以靠这一点工作的插件（K-Vault 0.3.1）写的是 `"0.3.2"`。
- 没进过任何 tag 的功能，不能出现在「功能表」里打勾的位置，只能写在「开发中」段。
- 截图、badge、性能数字同样受这条约束：不使用无法复现的素材。

## 3. 语义化版本

采用 SemVer 2.0，配合 0.x 阶段的宽松解释：

- **主版本 0.x**：`0.(minor).(patch)`。minor 用于功能批次，patch 用于只修 bug 的补丁。0.x 期间允许在 minor 里做不兼容的接口调整，但必须在 CHANGELOG 的 `Changed` 里写清楚怎么迁移。
- **1.0.0 之后**：接口（含 `rcgui.conf` schema、插件协议、CLI 参数）破坏性变更只能进主版本。
- **预发布**：`0.3.0-rc.1` 这类后缀只用于 tag 和产物，不进 `Cargo.toml` 的日常开发版本（上游 crate 版本带 `-rc` 会污染依赖解析和 `min_gui_version` 比较）。

什么时候 bump：

| 改动 | 版本 |
|---|---|
| 只修 bug、文档、翻译 | patch |
| 新功能、新视图、新 CLI 参数、插件协议兼容性扩展 | minor |
| 插件协议破坏性变更、`rcgui.conf` schema 不兼容、CLI 参数语义变更 | 主版本（0.x 期间是 minor，并显著标注） |
| 只改构建/CI/脚本，用户看不到 | 不 bump，记在 CHANGELOG `Unreleased` 的 `Changed` 下 |

## 4. Tag 与 Release

- tag 名 `vX.Y.Z`（预发布 `vX.Y.Z-rc.N`），打在 `main` 上，指向通过 CI 的提交。
- 打 tag 触发 `.github/workflows/release.yml`，构建并上传产物。产物命名见 [docs/RELEASE.md](RELEASE.md)：`RCGUI-<version>-windows-x64-setup.exe`、`RCGUI-<version>-windows-x64-portable.zip`、`SHA256SUMS`。
- **同一个 tag 不允许重新上传不同内容的产物**。发现问题就删 Release、保留 tag 记录在 CHANGELOG 里说明，然后发新的 patch 版本。
- 已发布的 tag 不移动、不删除、不重写。历史上 `v0.1.0`、`v0.2.0`、`v0.2.1` 存在，其中 `v0.2.1` 的产物用的是旧命名 `rclone-gui-v0.2.1-windows-amd64.zip`，不要为了命名统一去改历史。
- 手动触发（`workflow_dispatch`）只用于验证产物能否构建；它产生的构件**不是发布**，除非维护者明确决定按上面的规则发 tag。

## 5. 变更记录

`docs/CHANGELOG.md` 按 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 组织，条目中文书写，面向"用这个软件的人"而不是"看 diff 的人"。

- 顶部永远是 `## [Unreleased]`，写已经合并、还没发布的东西。
- 发布时把 `Unreleased` 段整体挪到 `## [0.3.0] - YYYY-MM-DD`，并在文件末尾更新比较链接。
- 分类只用这几组：`Added` / `Changed` / `Deprecated` / `Removed` / `Fixed` / `Security`。
- 每条写**用户能观察到的结果**，不写内部实现名：「挂载失败时区分端口被占用与内核启动失败」，而不是「重构 mount 错误枚举」。
- 破坏性改动在条目前加 **`BREAKING`**，并写清楚用户要做什么。
- 不写"优化了代码结构""提升了稳定性"这类没有可验证内容的条目。

`0.3.0` 这一段区分两件事：已经合进主干的实现，和仍在设计里的部分。仍在设计的不进 CHANGELOG，写进 [docs/ARCHITECTURE.md](ARCHITECTURE.md) 的 `规划` 行。

## 6. 插件版本与 min_gui_version

- 插件自己的版本由插件仓库决定，格式 semver，写在它的 `config.json` 里，并和 `plugins/manifest.json` 的 `tag` 对应。
- 商店索引里的 `min_gui_version` 是**声明值得信**：提交者声明"这个版本要求 GUI 至少是 X"，审核者核对它确实存在于那个 GUI 版本。不能声明一个从未包含该插件的发布版本——例如某个插件是 0.3.0 才第一次随包分发的，`min_gui_version` 就不能写 `0.2.1`。
- 主程序升级插件的规则见 [docs/PLUGIN.md §7](PLUGIN.md)：按 `config.json.version` 比较，随包版本更新时覆盖用户目录里的旧副本，用户配置（kv）保留。
