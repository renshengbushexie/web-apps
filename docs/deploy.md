# Linux x86_64 原生部署

适用于 TencentOS Server 3.3 等使用 systemd 的 Linux x86_64 主机。发布包内含静态编译的 `kb-api`、`kb-sync`、两个 service 文件、`install.sh` 和记录完整 Git 提交号的 `VERSION`。安装脚本依赖系统的 Bash、systemd、curl、openssl、iproute（`ss`）、util-linux（`flock`）及账户管理工具。

## 安装并检查

核对发行方提供的 SHA-256，把已验证的包解压到管理员专用目录。在含 `install.sh` 的解包目录执行：

```bash
sudo bash ./install.sh
sudo systemctl is-active kb-api.service
curl --fail --silent --show-error http://127.0.0.1:8022/healthz
curl --silent --output /dev/null --write-out '%{http_code}\n' \
  'http://127.0.0.1:8022/v1/sku?product_id=demo'
```

健康接口返回 `status=ok` 和知识版本，最后一个匿名请求应返回 `401`。初次安装会生成随机管理员密钥，只启动并开机启用 `kb-api.service`。店铺密钥留空时，所有业务查询都返回 401。日志中不展示管理员密钥。

| 位置 | 内容 |
| --- | --- |
| `/opt/rust-knowledge/releases/<完整提交号>/` | 只读发行文件，运行账户为无登录权限的 `rust-knowledge` 系统用户 |
| `/opt/rust-knowledge/current` | 指向当前发行目录的符号链接 |
| `/etc/rust-knowledge/api.env` | 仅 root 可读的 API 配置；首次安装设置 `KB_BIND=127.0.0.1:8022` |
| `/etc/rust-knowledge/feishu-web.env` | 个人飞书登录的应用凭据、站点源和原表标识，由 root 保存为 0600，详见[网页授权配置](api.md)。 |
| `/etc/rust-knowledge/sync.env` | 开通同步时单独创建，只供后台同步服务使用 |
| `/var/lib/rust-knowledge/knowledge.sqlite3` | 知识正文、商品关联和发布版本 |
| `/var/lib/rust-knowledge/sync.sqlite3` | 建议的同步检查点路径，需在 `sync.env` 中设置 |

## HTTPS 与店铺接入

发布包含个人工作台的版本时，在同一 HTTPS 虚拟主机中加载随包的 `nginx-locations.conf`，检查 `nginx -t` 并重载配置。首页、样式、登录回调和个人查询需要同时转发。匿名打开首页应返回 200，读取 `/me/tables` 应返回 401。

将现有 HTTPS 入口的 API 路由转发到 `http://127.0.0.1:8022`，保持原有证书管理方式。只将客服应用需要的 `/v1/sku`、`/v1/search`、可选 `/v1/compare` 提供给受信任的业务后端。让 `/v1/admin/changes` 仅走本机或受控管理网络。AI 比较使用 SSE，上游代理需要支持长连接并关闭该路由的响应缓冲。

通过 `sudoedit /etc/rust-knowledge/api.env` 将店铺 ID 与密钥摘要加入现有配置。例如：

```text
KB_SHOP_TOKEN_HASHES='{"store-a":"<客户自定密钥的64位十六进制SHA-256摘要>"}'
```

保存后执行 `sudo systemctl restart kb-api.service`，用健康接口及真实店铺凭据检查对应的业务请求。密钥原值保存在调用方业务后端；服务端可仅保存摘要。字段格式、轮换步骤、请求示例及 SSE 事件见 [API 指南](api.md)。

可选 AI 使用 `KB_AI_BASE_URL`、`KB_AI_TOKEN`、`KB_AI_MODEL`。基地址是带 HTTPS 的 API 前缀（例如提供方的 `/v1` 前缀）；服务追加 `/chat/completions`。三个变量均留空时 SKU 和话术接口可照常工作。

## 开通飞书同步

先取得实际主库的表及字段映射，以及能读取目标知识的飞书应用授权。参考 [同步配置说明](sync.md) 准备映射，在服务端创建独立的受保护配置：

```bash
sudo sh -c 'umask 077; touch /etc/rust-knowledge/sync.env'
sudo chmod 0600 /etc/rust-knowledge/sync.env
sudoedit /etc/rust-knowledge/sync.env
```

```text
KB_API_URL=http://127.0.0.1:8022
KB_ADMIN_TOKEN=<与api.env一致的管理员密钥>
KB_SYNC_DB=/var/lib/rust-knowledge/sync.sqlite3
KB_POLL_SECONDS=60
FEISHU_APP_ID=<应用ID>
FEISHU_APP_SECRET=<应用凭据>
FEISHU_BASE_TOKEN=<实际运营主库的app_token>
KB_TABLES_JSON='<已核对的单行JSON数组>'
```

`KB_TABLES_JSON` 的值是字段映射数组的内容。配置完成后用 `sudo systemctl start kb-sync.service` 启动，并运行 `sudo journalctl -u kb-sync.service -n 30 --no-pager` 查看每个表的发布结果。在客服端查验已知商品、停用话术和跨店隔离后，执行 `sudo systemctl enable kb-sync.service` 开通自启。暂停同步使用 `sudo systemctl disable --now kb-sync.service`。

## 升级、回滚和备份

在新发行包中重新执行 `sudo bash ./install.sh`。安装程序保留现有配置、数据库及历史发行目录，用原子重命名切换 `current`。它核对 systemd 主进程实际运行的文件、8022 监听进程，以及 `/healthz` 的 JSON 响应。原本正在运行的同步服务会在 API 验证完成后恢复。

激活失败时，安装程序会重新指向上一发行目录、恢复服务单元、重启 API 并重新验活，命令仍返回非零；首装失败会停止 API，便于排查。查看 `sudo journalctl -u kb-api.service -n 50 --no-pager`。主动回退时，用对应版本保存的已校验发布包重复执行安装。软件回退只切换程序版本；引入数据库格式变更的发行应另外验证向后兼容性。

备份应包含两份 SQLite 数据库及受保护配置。可在维护窗口暂停已启用的同步与 API，再备份整个 `/var/lib/rust-knowledge` 和 `/etc/rust-knowledge`，以包含 SQLite WAL 相关文件。完成后按原状态恢复服务，并在隔离环境验证备份可恢复。
