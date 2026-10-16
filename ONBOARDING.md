# 开发与部署

## 新产品开发入口

| 目录                   | 职责                                   | 工具                                   |
| ---------------------- | -------------------------------------- | -------------------------------------- |
| `server/`              | 邮箱同步、SQLite、HTTP / MCP、邮件操作 | Rust stable                            |
| `android/`             | 原生搜索与简单回复 App                 | JDK 17、Android SDK 36、Gradle wrapper |
| `macos/`               | SwiftUI 邮箱管理、搜索与简单回复 App | macOS 13+、Swift Command Line Tools    |
| `scripts/mcp_stdio.py` | stdio 到远程 MCP 桥接                  | Python 3 标准库                        |
| `deploy/`              | systemd 和 HTTPS 反向代理配置示例      | Linux、Caddy                           |

产品范围和配置字段见 [README.md](README.md)。服务器独立管理邮箱凭据和缓存；客户端只保存服务连接信息，不迁移旧缓存。

```bash
cargo test --manifest-path server/Cargo.toml
cargo clippy --manifest-path server/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path server/Cargo.toml --check
cargo build --manifest-path server/Cargo.toml
python3 -m unittest discover -s scripts -p 'test_*.py'
(cd android && ./gradlew testDebugUnitTest lintDebug assembleDebug)
swift run --package-path macos MailSearchChecks
bash scripts/build-macos-search.sh
```

设置 `ANDROID_HOME` 或在忽略的 `android/local.properties` 中配置 `sdk.dir`。原生 App 不需要 Rust Android target 或 NDK。可使用 Android Studio 打开 `android/`。

本机调试服务可绑定 `127.0.0.1:8080`。安卓模拟器通过 `10.0.2.2` 访问宿主机；只有 Debug 构建且显式开启开发选项才允许这个地址使用 HTTP，安卓真机使用 HTTPS。macOS Release 可使用私有 IPv4 或 IPv6 ULA 地址的 HTTP；公网和域名使用 HTTPS。stdio 桥接仅在设置 `AI_EMAIL_ALLOW_HTTP=1` 后允许 localhost / loopback HTTP。

## Linux 单机部署

以下命令是部署操作说明，不会由测试自动执行。先在目标 Linux 架构构建 `server/target/release/ai-email-server`，配置域名 DNS，安装 Caddy。不要把 macOS 构建的二进制复制到 Linux。

创建专用用户和目录：

```bash
sudo useradd --system --home /var/lib/ai-email --shell /usr/sbin/nologin ai-email
sudo install -d -m 0755 /opt/ai-email
sudo install -d -m 0750 -o root -g ai-email /etc/ai-email
sudo install -d -m 0700 -o ai-email -g ai-email /var/lib/ai-email
sudo install -m 0755 server/target/release/ai-email-server /opt/ai-email/ai-email-server
sudo install -m 0640 -o root -g ai-email server/config.example.json /etc/ai-email/config.json
```

生成两份独立令牌，避免在日志中打印：

```bash
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/ai-email/read-token; openssl rand -hex 32 > /etc/ai-email/write-token'
sudo chown root:ai-email /etc/ai-email/read-token /etc/ai-email/write-token
sudo chmod 0640 /etc/ai-email/read-token /etc/ai-email/write-token
```

首次部署保留配置的 `accounts: []`，无需先准备邮箱授权码。启动后在 macOS App 用可写令牌连接，再通过“软件设置 → 邮箱账号”添加账户；最多 10 个，不允许重复邮箱，服务自动生成稳定且不可复用的账户 id。授权码只保存在服务端数据库。

旧版配置中的账户和 SecretRef 在首次启用时导入数据库，之后 `config.accounts` 不再控制运行时账户。App 编辑时授权码留空保持原值；更新授权码后不再依赖旧密钥文件。邮箱地址和 IMAP 主机/端口固定，更换邮箱需新增账户。

示例 `allowed_hosts` 中的 `mail.example.com` 应替换成实际域名；服务只监听 loopback。`allowed_origins: []` 适合不带 Origin 的原生客户端；若 AI 客户端发送 Origin，填入其完整且明确的来源，不使用通配符。

```bash
sudo install -m 0644 deploy/ai-email.service /etc/systemd/system/ai-email.service
sudo systemctl daemon-reload
sudo systemctl enable --now ai-email
sudo systemctl status ai-email
sudo journalctl -u ai-email -n 50
```

公网部署将 `deploy/Caddyfile` 的站点配置合并进服务器 Caddy 配置，替换实际域名，验证配置后重载。不要覆盖服务器上的其他站点；后端仅监听 loopback。

仅供 macOS 在可信内网使用时，可采用如下 HTTP 反代配置（替换为实际内网 IP）。`bind` 只监听该网卡地址，后端继续绑定 `127.0.0.1:8080`：

```caddy
http://192.168.1.10:8080 {
    bind 192.168.1.10
    reverse_proxy 127.0.0.1:8080 {
        flush_interval -1
    }
}
```

将该 IP 加入 `allowed_hosts`。内网 HTTP 不加密访问令牌和邮箱配置，仅用于可信网络，不向公网转发该端口。

Android 和 macOS 各自只保存一个访问令牌：只读令牌仅能搜索，可写令牌可简单回复，macOS 可写令牌还可管理邮箱。服务器仍配置两份不同的令牌；AI 需要写操作时使用可写令牌。App 修改账户立即生效，不需重启；改变访问令牌、监听地址或全局同步配置后重启服务。

## 数据与故障处理

- SQLite 存在 `database_path`；目录必须存在且仅服务用户可读写（`0700`）。数据库包含邮件正文及 App 添加的邮箱授权码，WAL/SHM 和备份需同等保护。
- 保留期按 IMAP INTERNALDATE 计算为滚动 365 天。首次接入默认跳过现存邮件；服务器停机期间的新邮件会在恢复后按游标补取，只取保留期内。
- `folders` 是邮箱侧 IMAP 文件夹名称，不是 AI 内容分类名称。先在邮箱侧配置规则把需要的邮件放入对应文件夹，再指定它们。Gmail 多标签邮件可能在不同文件夹分别出现。
- 源邮箱 UIDVALIDITY 变化时，旧引用会失效并重新建立同步起点；默认不回导历史。写操作会核验 UIDVALIDITY，避免误操作另一封邮件。
- 列表展示同步状态；同步出错不代表没有邮件，已有缓存仍可搜索。原始邮件超过 25 MiB 不进入缓存。
- 搜索空关键词返回当前缓存的最新邮件；多词按 AND 匹配，`%`、`_` 按普通字符检索。
- 服务不会自动清空垃圾箱或永久删除远端邮件。移动只在服务商支持 IMAP MOVE 时执行；移动出同步范围后不再显示。
- SMTP 超时或连接中断可能发生在服务商已接受之后。`unknown` 结果不会自动重发；应人工核对邮箱。准备操作超过 365 天变为 `expired`，不能再发送；终态及过期准备会清除正文，只保留编号、请求摘要、Message-ID 和状态用于去重核对。`sent` 只表示 SMTP 接受，不保证收件人最终投递。
- 邮箱侧的已读/星标变化不会通过此版本持续刷新全部缓存；通过 MCP 完成的操作立即更新本地状态。
- 备份数据库时先停止服务，再复制数据库文件，完成后恢复服务；不要在运行中仅复制 `.db` 而漏掉 WAL。备份密钥应另行加密保护。

## 客户端权限与回复接口

客户端连接地址填服务根 URL，不带 `/api` 或 `/mcp`。`GET /api/accounts` 返回 `can_write`，表示当前访问令牌是否允许写操作。不要仅依赖客户端隐藏按钮，服务端始终执行权限校验。

邮箱管理接口均要求可写令牌，响应不含授权码或密钥引用：

- `GET /api/account-settings`：返回 `{accounts: [...]}`，包含账户 id、邮箱、显示名、IMAP/SMTP 主机及端口、同步文件夹。
- `POST /api/account-settings`：提交 `email`、`display_name`、`imap_host`、`imap_port`、`smtp_host`、`smtp_port`、`folders`、`password`，创建并返回账户；新账户密码必填。
- `PUT /api/account-settings/{id}`：相同结构，`password` 缺省、null 或空字符串保留旧值；邮箱地址与 IMAP 主机/端口不可变。
- `DELETE /api/account-settings/{id}`：返回 `{deleted: true}`，原子清理服务器账户、缓存、游标及未执行的发送内容；远端邮件不变。未执行发送置为 `cancelled`，保留操作编号防止重放。

- `POST /api/messages/{id}/reply/prepare`：可写令牌提交 `operation_id` 和 `body_text`，只准备回复；返回发件账户、收件人、主题、正文及状态供用户确认。
- `POST /api/send/{operation_id}/send`：用户确认后执行一次发送。
- `GET /api/send/{operation_id}`：查询 `prepared / sending / sent / failed / unknown / expired / cancelled` 状态及 Message-ID；不会重新发送。

账号配置及管理接口支持 `aliases` 数组，最多 100 个，不得重复或包含主地址。更新时缺省保留原别名，空数组清空；只读账户列表也返回别名。macOS 在“软件设置 → 邮箱账号”中管理。

回复准备和通用 `prepare_send` / MCP 支持可选 `from_address`，仅允许所属账号主地址或已配置别名。省略时回复按原信收件身份匹配，普通发信使用主地址。回复预览的 `from_address` 和兼容字段 `account_email` 均返回实际发件地址，`account_id` 标识登录账号。SMTP 认证仍用主账号，邮件 From 与信封发件人使用预览地址。

旧配置与缓存缺少新字段时仍可读取。新邮件解析 `X-Original-To`、`Delivered-To`、`Envelope-To`，作为 To/Cc 无匹配时的补充；旧缓存不会为补齐投递头自动重抓，已有 To/Cc 中的别名在配置后即可匹配。

发送失败、响应丢失或客户端重启后，先查询原编号，不自动生成新编号重发。`unknown` 需要核对邮箱；`sent` 仅代表 SMTP 接受。

## 本机检查与 CI

macOS 使用 `swift run --package-path macos MailSearchChecks` 执行原生可执行检查，不依赖完整 Xcode 的 XCTest 环境。`scripts/build-macos-search.sh` 生成 `macos/build/AI Email.app`，CI 用 tar.gz 保留可执行权限；当前构建不承担正式签名、公证或发布。

macOS 构建按图标内容生成资源文件名，避免替换图标后系统继续使用同名缓存。修改 `macos/AppIcon.icns` 后重新构建并重新打开 App。

Git 检查使用独立安装的 `lefthook` 和 `gitleaks`，不依赖 Node。macOS 可通过 `brew install lefthook gitleaks` 安装，再运行 `lefthook install`。现有 Git hooks 会优先调用 PATH 中的 lefthook；不得用 `--no-verify` 跳过失败检查。提交前检查 Rust 格式、clippy、凭据及标题格式；推送前检查服务端测试与 Python 桥接/冒烟。Android 和 macOS 检查按改动范围执行，CI 分别构建。

网络传输回归测试只监听本机临时端口，受限执行环境可能需要允许 localhost 监听；这不需要真实邮箱凭据。
