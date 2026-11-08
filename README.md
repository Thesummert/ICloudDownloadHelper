# iCloud Download Helper

Rust 编写的跨平台 iCloud 照片命令行下载工具，独立登录 Apple 账号，不依赖 Python、系统“照片”App 或其他下载程序。

## 功能

- 国际区 / 中国大陆区账号，SRP-6a 登录、受信任设备六位验证码、会话复用。
- 列出个人照片库相册；按名称或 ID 选择，支持多个相册和交互选择。
- 自定义目标目录；默认 8 路并发，可设置 1–64。
- 下载原始照片、视频；可选 Live Photo 的 MOV 伴随视频。
- 分页读取、按页下载，避免预先装载整个照片库。
- 网络失败重试、HTTP Range 续传、进度统计、Ctrl+C 停止。
- 文件名带资源标识，避免不同照片的同名文件互相覆盖。
- 完成记录包含本地 SHA-256；重复运行会检查大小和哈希，校验通过才跳过。
- 仅下载，不删除云端照片。

## 编译

需要 Rust 1.92 或更高版本。支持 macOS、Windows 和 Linux，HTTPS 使用 rustls。

```sh
cargo build --release --locked
```

产物为 `target/release/icloud-dl`（Windows 为 `icloud-dl.exe`）。也可在项目目录中安装到 Cargo bin 目录：

```sh
cargo install --path . --locked --bin icloud-dl
```

## 打包

在目标系统上安装 Rust 1.92 或更高版本，并在仓库根目录运行对应脚本：

```sh
# macOS：生成包含图形 .app 和命令行程序的 zip
./scripts/package-macos.sh

# Linux：生成包含图形界面和命令行程序的 tar.gz
./scripts/package-linux.sh
```

```powershell
# Windows PowerShell：需要 MSVC Rust 工具链和 Visual Studio C++ 构建工具
powershell -ExecutionPolicy Bypass -File .\scripts\package-windows.ps1
```

压缩包输出到 `dist/`，文件名包含版本和构建机器的 Rust 目标架构。每个包包含 `README.md`、`LICENSE` 和 `THIRD_PARTY_NOTICES.md`。脚本仅构建当前主机架构，不生成跨架构或通用二进制。macOS 的 `.app` 不包含开发者签名或公证；Windows 程序也未签名。

Linux 上构建图形界面需要系统开发库。Debian/Ubuntu 可安装 `pkg-config libfontconfig-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libx11-xcb-dev libxcursor-dev libxkbcommon-x11-dev libx11-dev`。运行时需要兼容构建环境的系统库，以及 X11 或 Wayland 桌面；压缩包不是包含所有系统依赖的 AppImage。图形程序的 Slint 许可适用于分发包，请在分发前确认下方许可说明。

## 图形界面

使用 Slint 图形界面登录、选择个人相册、分页预览缩略图，并下载所选相册的原始文件：

```sh
cargo run --release --locked --features gui --bin icloud-dl-gui
```

输入 Apple 账号、选择 `com` 或 `cn` 并连接；需要时在窗口中输入密码和受信任设备验证码。选择相册后加载一页预览，点击“下一页”继续，预览加载期间可以直接切换到其他相册；底部可填写下载目录并下载当前相册。可选填拍摄日期起止和文件数量上限，日期按 UTC 自然日计算，包含起止日。数量按文件计数，已存在或下载失败的文件也占用额度；留空表示不限。缺少拍摄日期时，按日期筛选会报错并停止。图形下载沿用命令行的会话、文件锁、续传和校验规则，当前按文件顺序下载，不包含 Live Photo 的伴随视频。预览使用 Apple 提供的缩略图，缺少预览资源时只显示文件名；视频不支持播放。界面不保存密码。

图形界面依赖 Slint，其许可条款与本项目 MIT 许可不同；分发带图形界面的程序前请确认适用的 [Slint 许可](https://slint.dev/pricing)。

## 快速开始

使用 Apple 账号的常规密码，密码通过终端隐藏输入。首次登录需在受信任的 Apple 设备上允许登录并提供验证码。

```sh
# 国际区；中国大陆云上贵州账号将 com 改成 cn
icloud-dl -u you@example.com --domain com login

# 查看相册名称和相册 ID
icloud-dl -u you@example.com albums

# 下载“旅行”相册，8 路并发
icloud-dl -u you@example.com download -a "旅行" -d "/Volumes/Backup/iCloud" -j 8

# 下载多个相册，同时保留 Live Photo 视频
icloud-dl -u you@example.com download -a "旅行" -a "家人" -d "./photos" --live

# 不提供 -a 或 --all 时，终端列出相册，输入序号选择
icloud-dl -u you@example.com download -d "./photos"

# 按 UTC 拍摄日期筛选，最多选择 100 个文件
icloud-dl -u you@example.com download -a "旅行" -d "./photos" --from 2024-01-01 --to 2024-12-31 --limit 100

# 下载整个个人照片库，不含隐藏和最近删除
icloud-dl -u you@example.com download --all -d "./photos" -j 16

# 预览文件路径和大小；仍需登录，但不下载媒体
icloud-dl -u you@example.com download -a "旅行" -d "./photos" --dry-run
```

中国大陆账号的完整示例：

```sh
icloud-dl -u you@example.com --domain cn download -a "旅行" -d "./photos" -j 8
```

两种区域均使用 `idmsa.apple.com` 完成 Apple 账号认证；`--domain cn` 选择中国大陆的 iCloud 网页和数据服务（`icloud.com.cn`）。若旧版本在 `SRP 初始化` 阶段返回 HTTP 404，请重新编译并使用新程序，不必把中国大陆账号改为 `--domain com`。端点划分参考 [PyiCloud 的实现](https://pyicloud.readthedocs.io/en/latest/api/pyicloud-service/)。

若旧版本显示 `https://www.apple.com/filenotfound`，是认证请求未声明接受 JSON 后，重定向掩盖了原始响应。新版为认证请求设置 `Accept: application/json, text/javascript` 和 `Content-Type: application/json`；遇到认证重定向会显示原始状态码及地址，不再自动跟随。认证与数据客户端共享会话 cookie。

`--from`、`--to` 和 `--limit` 均可单独使用。日期是 iCloud 的拍摄日期（UTC），包含所填日期的全天；如果某个文件缺少拍摄日期，按日期筛选时会报错停止。数量上限跨所有选定相册生效，按 iCloud 返回的日期顺序选取文件；已存在、下载失败和 `--dry-run` 中列出的文件均计入上限。启用 `--live` 时伴随视频也单独计数，达到上限时可能只选中照片。

Windows PowerShell 中可用 `-d "D:\Photos\iCloud"`。同名相册必须使用 `albums` 输出的 ID 精确选择。

## 目录与续传

相册下载到 `目标目录/相册名_相册标识/`；`--all` 下载到 `目标目录/AllPhotos/`。每个媒体文件前缀是资源标识，后面是清理过的原始文件名，例如：

```text
photos/
  旅行_<相册标识>/
    <资源标识>_IMG_1234.HEIC
    <资源标识>_IMG_1234.HEIC.done.json
    <另一资源标识>_IMG_1234_LIVE.MOV
```

下载期间使用 `.part` 文件，完成后才改为正式文件名。中断后运行相同命令会继续下载。若服务器忽略 Range，自动从头下载该文件。请保留 `.done.json`，它用于判断正式文件是否完整；校验失败或完成记录缺失时，程序报告错误并保留现有文件，移走该文件后可重新下载。这里的 SHA-256 是本地完整性记录，不是对 Apple 服务端校验和的验证。

同一目标目录只能运行一个下载任务，使用操作系统文件锁；进程退出后锁自动释放。不同相册、`--all` 使用独立子目录，因此同一照片在这些不同任务之间可能保存多份。目录身份和文件名前缀用于避免覆盖，不能手动去掉后继续沿用增量下载。

失败默认重试 3 次（共最多 4 次尝试），可使用 `--retries 5` 调整。部分文件失败时退出码为 1；Ctrl+C 退出码为 130。重新运行会重新获取下载地址并重试，已经完成且通过校验的文件会跳过。

## 会话与自动化

会话默认位于系统配置目录下的 `icloud-download-helper/sessions`，按账号和区域隔离。可自定义：

```sh
icloud-dl -u you@example.com --session-dir "./.icloud-session" login
icloud-dl -u you@example.com --session-dir "./.icloud-session" download --all -d "./photos"
```

会话仅保存令牌和客户端标识，不保存密码。Unix 上会话文件权限为 `0600`；Windows 使用目录继承的 ACL，请放在当前用户的私有目录。会话令牌能访问账号数据，请勿提交到 Git 或分享。媒体下载使用独立 HTTP 客户端，不携带账号 cookie。

`--fresh-login` 可重新登录。支持环境变量 `ICLOUD_USERNAME`、`ICLOUD_PASSWORD`、`ICLOUD_2FA_CODE`；无人值守任务请先在终端完成登录并保存会话，随后使用 `--all` 或 `--album` 明确范围。环境变量中的密码不会写入配置；优先使用隐藏输入。会话到期后需要重新认证，程序不会自动绕过双重认证。

## 当前范围与限制

- 当前支持个人照片库的用户相册及整个个人照片库。尚未实现 iCloud 共享照片图库、公开共享相册链接、硬件安全密钥和短信验证码流程。
- 下载原始资源，不导出编辑后的渲染版本、相册文件夹层级或额外的 XMP 元数据。
- 需要开启 iCloud 照片，并允许在网页上访问 iCloud 数据。高级数据保护可能阻止此类网页接口；本工具不会改变你的账号安全设置。
- Apple 未提供个人照片库的公开下载 API。本项目依据公开客户端实现对接网页私有接口，Apple 变更接口可能导致登录或下载失败。遇到 HTTP 401/403、索引未完成或账号条款更新，请先在 iCloud 网页检查状态。
- 并发数是同时传输的文件数，由 Tokio 多线程运行时处理；单个大视频使用一条传输连接，并不分段多线程下载。速度也受 Apple 限速和本地带宽影响。
- 已完成本地编译、SRP 协议及本地 HTTP 测试，并使用不含账号和密码的空请求验证 Apple SRP 初始化路由的 JSON 响应；当前尚未使用真实 Apple 账号验证完整登录与下载流程。

协议实现参考 [kei 的认证模块](https://github.com/rhoopr/kei/tree/main/src/auth) 和 [照片模块](https://github.com/rhoopr/kei/tree/main/src/icloud/photos)，相关 MIT 许可保留在 `THIRD_PARTY_NOTICES.md`。

## 开发检查

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

测试使用本地 HTTP 服务验证续传、服务器忽略 Range、错误 Content-Range、文件完整性及同名路径隔离，不访问 Apple，也不需要真实账号。

可选的 Apple 初始化路由检查（仅发送空 JSON，不发送账号或密码，不尝试账号登录；默认不运行）：

```sh
cargo test apple_srp_route_returns_json_without_credentials -- --ignored
```
