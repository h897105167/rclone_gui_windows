# 参与 RCGUI

RCGUI 是 Windows 上的 rclone 桌面管理器：Rust + Slint，原生窗口，不含 webview。这份文档说明怎么搭环境、怎么提交改动、什么会被拒。开始前请读 [AGENTS.md](AGENTS.md)（工作约定）和 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)（分层与契约）。

## 1. 环境

- Windows 10/11 x64。
- Rust 稳定版工具链（`rustup`），需要 `wasm32-wasip1` 目标：插件是 WASI 模块。
- 挂载功能另需 WinFsp；这是 rclone 之外的独立依赖，不装也能跑除挂载外的功能。
- 没有运行中的 rclone 也能开发和调试：RCGUI 自带受管内核（managed core），可以自己拉起一个 `rclone.exe`；也可以连接你已经启动的外部端点。

```powershell
rustup target add wasm32-wasip1
```

## 2. 构建

```powershell
.\scripts\build.ps1                    # debug
.\scripts\build.ps1 -Release           # release，产物 target\release\rclone-gui.exe
.\scripts\build.ps1 -Release -Run      # 构建后直接启动
```

插件不在本仓库，构建前必须先按 `plugins/manifest.json` 拉取并校验：

```powershell
.\scripts\fetch_plugins.ps1            # 已存在且 tag+sha256 匹配就跳过
.\scripts\fetch_plugins.ps1 -Force     # 强制重下
.\scripts\fetch_plugins.ps1 -Offline   # 不联网，缺什么就报错退出
```

`crates/app/build.rs` 只负责把 `crates/app/assets/plugins/` 复制到二进制旁边，不编译插件。所以次序是「先 fetch 再 build」。**不要把 sha256 校验改成"不匹配只警告"**，那会让校验形同虚设，这类 PR 直接关闭。

## 3. 代码约定

- `cargo fmt` 后提交；CI 跑 `cargo fmt --check`、`cargo check`、`cargo test`。
- 版本号只改 `Cargo.toml` 的 `[workspace.package] version`，成员 crate 通过 `version.workspace = true` 继承。不要手改各 crate 的版本。
- 新增依赖要说明理由和体积/许可证影响；不要为了一个小工具引入一个大 crate。
- 线程模型遵循 [docs/ARCHITECTURE.md §4.1](docs/ARCHITECTURE.md)：UI 线程只跑 Slint，IO 在 tokio 上；同一类轮询不允许重叠；旧响应不得覆盖新状态。
- 用户可见文案走 `crates/i18n`，中英都要有。硬编码中文字符串会被要求改。
- 提交信息用「祈使句 + 范围前缀」，正文写原因和破坏性影响，例如：

```
fix(mounts): 端口冲突时报出占用者，而不是"启动失败"

受管内核分配端口失败时，原来只显示笼统错误；现在区分端口被占用
与内核启动失败，方便用户判断是不是另一个 rclone 在跑。
```

## 4. 提交 PR

1. Fork → 分支（`fix/…`、`feat/…`、`docs/…`）。
2. 只做一件事。无关的重构、格式化、重命名请单独提。
3. 填 `.github/PULL_REQUEST_TEMPLATE.md`，**证据要能被别人重复**：
   - 改了什么、为什么；
   - 你实际跑过的命令和结果（编译、单元测试、手动验收步骤）；
   - 涉及界面的，给截图或说明为什么没有（视觉由维护者亲自验收）；
   - 涉及发布/打包/商店索引的，贴校验脚本的实际输出。
4. 不要提交构建产物、`target/`、日志、`%APPDATA%` 下的真实配置、任何 token。

## 5. 审查门槛

代码改动看这些：

| 维度 | 要求 |
|---|---|
| 正确性 | 错误路径有处理；失败可以恢复；不吞异常 |
| 数据安全 | 不覆盖用户配置；迁移先复制；凭据不进日志/截图/崩溃报告 |
| 进程边界 | 只结束 RCGUI 自己拉起的 `rclone.exe`；外部端点只能断开，禁止 `core/quit` 和 kill |
| 并发 | 单写者、single-flight、可取消；不阻塞 UI |
| 性能 | 不超 [docs/PERFORMANCE.md](docs/PERFORMANCE.md) 的预算；数字必须来自 `scripts/measure-perf.ps1` |
| 可访问性 | 键盘可达、有可读名称、焦点可见、对比度 ≥ 4.5:1、状态不只靠颜色 |
| 文档诚实 | 未发布的功能写"开发中"，不写"已支持"；不编造版本号、哈希、badge |

会被直接拒绝的改动：

- 削弱或跳过哈希/签名校验；
- 删除或改写测试使其通过，`assert!(true)` 式占位，用脚本跑通冒充功能验证；
- 文档或 README 里把规划当已发布；
- 往仓库里塞二进制、插件源码、真实凭据或绝对本地路径；
- 在外部端点模式下加任何结束进程的代码路径；
- 未经请求就大改文件管理、传输、内嵌 HTTP 这些存量功能。

## 6. 插件

插件源码不在本仓库。想让插件出现在商店页面里，走 PR 改索引，不要上传可执行文件：

1. 插件自己发 Release，zip 内含 `config.json`、`main.wasm`、`check.sha256`（格式见 [docs/PLUGIN.md](docs/PLUGIN.md)）。
2. 按 [docs/store/README.md](docs/store/README.md) 提 PR，往 `docs/store/index.json` 加一条，`sha256` 填 Release 里那个 zip 的哈希。
3. 审核会核验：schema、id 唯一、semver、https 且域名在允许列表、哈希与真实 Release 一致，然后才看代码/许可证/权限/恶意行为。

商店不接受占位条目、假哈希、指向自己服务器的下载地址，也不收 `oneclick`（它和主程序耦合，刻意不上架）。

## 7. 发布

只有维护者能打 tag 和发 Release。版本规则见 [docs/VERSIONING.md](docs/VERSIONING.md)，产物与校验和约定见 [docs/RELEASE.md](docs/RELEASE.md)。合并进 `main` 不等于发布，`docs/CHANGELOG.md` 里只有真的出去了才从 `Unreleased` 挪到具体版本下。

## 8. 安全与凭据

不要在 issue、PR、截图、日志里贴 RC 密码、OAuth token、`rclone.conf` 内容。报告安全问题时先只描述影响面和复现条件，凭据类细节等维护者私下对接。`rclone obscure` 是混淆不是加密，别把它当加密方案。

## 9. 许可证现状

本仓库目前**没有声明开源许可证**（GitHub 上 license 接口返回 404）。提交 PR 表示你同意该改动可由仓库所有者按届时选定的许可证分发；在许可证确定前，这只是一个工作约定，不构成正式授权文本。第三方材料（例如 rclone 本体）各自的许可证见 `licenses/`。
