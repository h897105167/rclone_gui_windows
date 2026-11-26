; RCGUI 安装包脚本（Inno Setup 6）。
;
; 由 scripts\build-release.ps1 调用，四个定义从命令行传进来：
;   /DAppVersion=<version>           版本号，只来自 Cargo.toml 的 [workspace.package]
;   /DAppVersionNumeric=<x.y.z.0>    同一版本的四段纯数字形式，只给 VersionInfoVersion 用
;   /DSourceDir=<dir>                portable 目录（rclone-gui.exe + core\ + assets\plugins\ + licenses\）
;   /DOutputDir=<dir>                产物目录，通常是 dist
;
; 三条硬约束：
;   1. 默认装到用户目录，不要管理员权限；用户自己改到需要提权的位置时，Inno 才会弹 UAC。
;   2. 安装包只装程序，不装任何用户数据。数据在 %APPDATA%\RCGUI，卸载也不删——
;      卸载删掉用户配好的 remote、代理、插件设置是不可接受的。
;   3. 不装 portable 标记。带这个空文件的目录被当成便携版，数据会写到 exe 旁边的 data\。
;      安装版要的是 %APPDATA%\RCGUI，所以 SourceDir 里的 portable 文件故意不列进 [Files]。

#define AppName "RCGUI"
#define AppPublisher "h897105167"
#define AppUrl "https://github.com/h897105167/rclone_gui_windows"

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
; VersionInfoVersion 只接受最多四段数字，遇到 "0.3.0-rc.1" 这类预发布后缀会直接编译失败。
; 所以版本号拆成两份：AppVersion 给人看（可以带后缀），AppVersionNumeric 给 PE 版本资源。
; build-release.ps1 会把 AppVersionNumeric 一起传进来；这里留兜底只是为了手工编译脚本本身。
#ifndef AppVersionNumeric
  #define AppVersionNumeric "0.0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\RCGUI-0.0.0-dev-windows-x64-portable"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
; AppId 是升级与卸载的识别键，发布后不要再改，否则新版本会被当成另一个程序并存。
AppId={{9C1F2A54-5B4E-4C2E-9E1D-3F7A6D2B8C41}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
VersionInfoVersion={#AppVersionNumeric}
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} setup
DefaultDirName={localappdata}\Programs\RCGUI
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; lowest = 不要求管理员；用户把安装目录改到 Program Files 时 Inno 会按需提权（OverridesAllowed=dialog）。
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
; x64compatible 需要 Inno Setup 6.3 以上：它允许在 ARM64 上用 x64 模拟运行，比写死 x64 兼容面大。
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=RCGUI-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\rclone-gui.exe
; 装之前请用户先关掉正在运行的 RCGUI，否则 exe 被占用会装不进去。
CloseApplications=yes
RestartApplications=no
AllowNoIcons=yes

[Languages]
; 中文向导是可选的。Inno Setup 6 的默认安装不带 Languages\ChineseSimplified.isl，
; 写死这一行会让整份脚本编译失败，安装包直接出不来。
; scripts\build-release.ps1 找到语言文件时才传 /DChineseMessagesFile=<路径>：
; 先看编译器目录 Languages\ChineseSimplified.isl，再看仓库里 packaging\languages\。
; 都没找到就只有英文向导——程序自己的界面语言与此无关。
#ifdef ChineseMessagesFile
Name: "chinesesimplified"; MessagesFile: "{#ChineseMessagesFile}"
#endif
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\rclone-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\core\rclone.exe"; DestDir: "{app}\core"; Flags: ignoreversion
; rclone 的许可证全文，MIT 要求随二进制分发。
Source: "{#SourceDir}\core\rclone-COPYING.txt"; DestDir: "{app}\core"; Flags: ignoreversion
Source: "{#SourceDir}\licenses\*"; DestDir: "{app}\licenses"; Flags: ignoreversion recursesubdirs
; 插件必须落在 {app}\assets\plugins：程序启动时只从 exe 同级的 assets\plugins 读随包插件
; （crates\app\src\plugins.rs::builtin_root），装到 {app}\plugins 就等于没装。
Source: "{#SourceDir}\assets\plugins\*"; DestDir: "{app}\assets\plugins"; Flags: ignoreversion recursesubdirs createallsubdirs skipifsourcedoesntexist

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\rclone-gui.exe"
Name: "{group}\{#AppName} 卸载"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\rclone-gui.exe"; Tasks: desktopicon

[Run]
; 装完可以让用户直接打开；静默安装时不弹。
Filename: "{app}\rclone-gui.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

; 故意没有 [UninstallDelete]。Inno 只删它自己装进去的文件，所以卸载不会碰用户数据；
; 反过来如果这里写 Type: filesandordirs; Name: "{app}"，用户放进安装目录的任何东西
; （包括手动丢进 core\ 的 rclone、data\ 里的配置）都会被一起删掉。
