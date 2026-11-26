# 发布流程

一句话：**发布 = 打 tag**。tag 推上去之后，`.github/workflows/release.yml` 负责构建、
自检、上传资产并创建 Release；人只负责打 tag 之前的合并，以及打 tag 之后的验收。

本地能构建出同样的东西（`scripts/build-release.ps1`），但本地构建**不产生 Release**，
产物也不该手传。手传的东西没有 CI 的自检记录，出问题时无法复现。

## 产物

| 文件 | 内容 |
| --- | --- |
| `RCGUI-<version>-windows-x64-setup.exe` | Inno Setup 安装包 |
| `RCGUI-<version>-windows-x64-portable.zip` | 便携版压缩包，解压即用 |
| `SHA256SUMS` | 上面两个文件的 SHA-256 |

`<version>` 只来自 `Cargo.toml` 的 `[workspace.package] version`，不带 `v`。
tag 是 `v<version>`（例如 `v0.3.0`）。两者不一致时 release workflow 直接失败：
"改完版本号再打 tag，不要给旧代码贴新版本。"

历史上 v0.2.1 的资产叫 `rclone-gui-v0.2.1-windows-amd64.zip`，那是旧命名，
不改历史资产名；新命名从 0.3.0 开始。

## 便携版结构

```
RCGUI-<version>-windows-x64-portable\
├─ rclone-gui.exe
├─ portable                      ← 空文件，仅仅"存在"
├─ core\
│  ├─ rclone.exe
│  ├─ rclone-COPYING.txt
│  └─ .fetched.json              ← 内核版本与 zip sha256，供诊断与汇总读取
├─ assets\
│  └─ plugins\
│     ├─ imagehost\
│     └─ teldrive\
└─ licenses\
   └─ NOTICE.txt
```

- `portable` 的存在就是便携模式的开关：数据写进 exe 旁边的 `data\`，不碰 `%APPDATA%`。
  安装包**不带**这个文件，所以安装版的数据在 `%APPDATA%\RCGUI`。
- **插件必须在 `assets\plugins\`**，不是根下的 `plugins\`：程序只认 `<exe>\assets\plugins`
  （`crates\app\src\plugins.rs::builtin_root`）。放到 `plugins\` 等于没装，所以打包脚本
  把"根下出现 `plugins\`"直接判成产物有问题。
- **内核固定在 `core\rclone.exe`**（`crates\app\src\core.rs` 先找这里）。缺 `rclone.exe`
  或缺它的 `COPYING` 都直接中止打包：MIT 要求随二进制一起分发许可证，缺了就是一份不合规的包。
- **不含 onerclone**（一键 rclone）。它从清单里删掉即不再参与构建；清单里没有的目录一律不进包，
  构建时还会顺手清掉 `crates\app\assets\plugins\` 下遗留的旧目录——只动构建资产，不碰用户目录。
- 插件的目录只有 `config.json`、`main.wasm`、`check.sha256`，没有源码；打包时会对清单里每一项
  重新按 `check.sha256` 逐文件核对一遍。

## 安装包行为

- 装到 `%LOCALAPPDATA%\Programs\RCGUI`，`PrivilegesRequired=lowest`，默认不请求管理员权限。
  用户手动改成需要提权的目录时，Inno 才会弹 UAC。
- `AppId` 固定，升级是覆盖安装而不是并列装两个。**发布后不要再改 AppId。**
- 不创建、不删除、不迁移 `%APPDATA%\RCGUI` 里的任何数据。没有 `[UninstallDelete]` 项：
  卸载只删它自己装进去的文件，用户手放的 `core\rclone.exe`、`assets\plugins\` 里的额外插件、`data\` 都留着。

## rclone 内核

`packaging/core-manifest.json` 固定一个版本：

```json
{
  "core": {
    "version": "v1.76.0",
    "asset_name": "rclone-v1.76.0-windows-amd64.zip",
    "asset_url": "https://github.com/rclone/rclone/releases/download/v1.76.0/rclone-v1.76.0-windows-amd64.zip",
    "asset_sha256": "8b4fd2737ade4e70c057ef4a49deb7cffe94b0fb9d7587c189d149823f21a1dc",
    "asset_bytes": 32193987,
    "checksums_url": "https://github.com/rclone/rclone/releases/download/v1.76.0/SHA256SUMS",
    "checksums_format": "pgp-clearsigned"
  }
}
```

`scripts/fetch_core.ps1` 做四件事：核对 `asset_sha256`；核对字节数；下载官方
`SHA256SUMS` 并**只解析 `<64位哈希>  <文件名>` 这样的行**（这个文件是 PGP 明文签名的，
签名头和 `-----BEGIN PGP SIGNED MESSAGE-----` 之类的内容不是数据，不能当哈希行读，
可参考 <https://rclone.org/release_signing/>）；最后运行 `rclone.exe version` 确认能跑且版本对得上。
任何一步对不上就失败，不会打包。

换内核版本 = 改这三个字段（version / asset_url / asset_sha256，字节数顺手改）+ 让 CI 重新验证。
**不要**把新内核 exe 手塞进产物里。

`-LocalZip` 允许用本地已下载的 zip，但同样要过哈希校验；用它的构建不算可重现构建，
不要用它出正式包。

## 插件

`plugins/manifest.json` 固定每个插件的 `tag` + `asset` + `sha256`，指向插件自己仓库的 Release。
`scripts/fetch_plugins.ps1` 下载、校验 zip 哈希、解开后再校验包内 `check.sha256` 列出的每个文件。
release workflow 还会核对解出来的 `.fetched.json` 与清单一致，不一致就不发。

清单里的 sha256 必须是 **Release 上那个 zip** 的哈希。本地重新打包出来的 zip 哈希不同
（时间戳会变），拿本地哈希填清单，别人拉下来必然校验失败。

## SHA256SUMS

格式是 `sha256sum` 的老格式，两个空格分隔，无 BOM：

```
c0e97cea438b363ecf749b3731ba4cc3fefd7098dcbacb3c724747042e58b92b  RCGUI-0.3.0-windows-x64-portable.zip
<另一个哈希>  RCGUI-0.3.0-windows-x64-setup.exe
```

用户核对的命令：

```powershell
Get-FileHash .\RCGUI-0.3.0-windows-x64-setup.exe -Algorithm SHA256
# 或者
certutil -hashfile .\RCGUI-0.3.0-windows-x64-setup.exe SHA256
```

**每次构建的哈希都不同**，即使代码完全一样：zip 里带文件时间戳，安装包带编译时间。
所以 `SHA256SUMS` 是"这一次构建的两个文件"的记录，不是版本号的指纹。
它能证明"你下到的就是我上传的"，不能证明"这个包和上次那个一样"。
要判断"这次构建是否只改了版本号"，看 git diff，不要看哈希。

## 本地试跑

```powershell
# 只走打包+校验和，不构建、不拉插件、不拉内核、不做安装包（CI 的冒烟测试就是这么跑的）
.\scripts\build-release.ps1 -SkipBuild -SkipPlugins -SkipCore -SkipInstaller

# 完整走一遍（需要网络、需要本机装了 Inno Setup 6）
.\scripts\build-release.ps1
```

产物在 `dist\`。`-SkipBuild` 复用已有二进制；`-SkipCore` / `-SkipPlugins` 跳过下载，仍逐项核对已有文件与锁定清单，缺失或 hash 不符会失败。`-SkipInstaller` 只生成便携版。发布流水线不使用这些跳过开关，防止遗漏构建或资产。

workflow 的手动触发（`workflow_dispatch`）只构建并上传到 Actions，**不创建 Release**，
方便先看一眼产物再决定要不要打 tag。

## 发布步骤

1. 把 `[workspace.package] version` 改成要发的版本，在 `docs/CHANGELOG.md` 里把
   `## [Unreleased]` 的内容整理成 `## [x.y.z] - YYYY-MM-DD`。
2. 走 PR 合并到 main/master。CI 会跑格式、编译、测试、打包冒烟。
3. 在合并后的提交上打 tag 并推上去：`git tag v0.3.0 && git push origin v0.3.0`。
4. 看 release workflow 跑完：产物自检、插件哈希核对、SHA256SUMS 重算都会打印在日志里。
5. 人工验收（这一步 CI 代替不了）：在干净的用户账户里跑一遍安装包，确认
   - 装到 `%LOCALAPPDATA%\Programs\RCGUI`，没弹 UAC
   - 首次启动能在设置里选中受管内核并连上
   - 卸载后 `%APPDATA%\RCGUI` 还在
   便携版解压到 U 盘或别的目录，确认数据写在 `data\` 里。
6. 验收通过后再动商店索引：新插件或升版本按 `docs/store/README.md` 提 PR。

## 不要做的事

- 不要把同一个 tag 的资产换成不同内容（重传）。已经有人下过了，哈希就对不上了。
- 不要手工往 `dist\` 里塞文件再上传。`dist\` 是脚本的输出目录，不是编辑区。
- 不要用 `-SkipCore` 的产物发布，也不要拿掉 `SHA256SUMS`。
- 不要在发布前改 `AppId` 或安装目录，那会让老用户的升级变成两个并存的程序。
- 不要在仓库里放构建日志、`target/`、`dist/`——它们会带出本机路径。

## 撤回

发现严重问题时，优先**发一个补丁版本**，而不是删资产或改 tag：
用户手里已经有那个文件，删除只会让他们的哈希核对失败、也查不到发生了什么。
需要立刻降低影响时，把 Release 标成 pre-release（页面默认不展示），
在 `docs/CHANGELOG.md` 里写清问题和受影响版本。

## 许可证

仓库目前**没有声明项目许可证**（GitHub 的 license 接口返回 404）。
所以发布件里只有第三方材料：rclone 的 `COPYING`（MIT），插件各自 `config.json` 里写明的信息，
以及 `licenses\NOTICE.txt` 里对这两者的说明。给项目本体加许可证是仓库所有者的决定，
不是发布流程能顺手做的事。
