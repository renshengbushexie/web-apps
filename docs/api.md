# 个人网页登录与客服应用接入

客服默认使用飞书官方扫码打开原表。业务后端使用店铺密钥访问 `/v1/*` 已发布的本机索引。接口字段见 [OpenAPI 3.1](./openapi.json)。

## 飞书原生扫码

团队成员打开内部书签 `https://kb.example.com/?base=<原表标识>`，由服务转到固定租户的原表。飞书在未登录时显示官方登录页，已登录时按本人权限打开原表。只在团队内分发这个书签。公开首页另有云文档入口，可从飞书的「与我共享」查找资料。

服务在 `/etc/rust-knowledge/feishu-web.env` 使用 `FEISHU_BASE_ORIGIN` 和 `FEISHU_BASE_TOKEN` 校验书签。此模式无需创建飞书应用，登录会话由飞书管理。访问原表能看到原库最新内容，腾讯云本地索引的更新取决于单独的数据导入或同步。

## 可选的站内查询授权

1. 管理员准备飞书网页应用，配置应用的**用户身份**权限 `bitable:app:readonly`，发布并将应用开放给需要登录的客服。客服本人还需要有目标知识表的访问权限。要编辑某条记录，需要飞书授予本人相应的编辑权限。
2. 在飞书开放平台将 HTTPS 回调地址登记为 `https://kb.example.com/auth/feishu/callback`，替换为实际站点域名。反向代理将首页、`/auth/*` 与 `/me/*` 转发给 API。OAuth 回调的查询参数含一次性授权码，代理日志应省略该路由的查询串。
3. 在 API 服务的受保护环境设置下列变量。原生安装使用 `/etc/rust-knowledge/feishu-web.env`，由 root 管理，权限设为 `0600`。重启 API 后在浏览器访问同一 HTTPS 域名。应用密钥由管理员在服务端维护。

| API 服务变量 | 内容 |
| --- | --- |
| `FEISHU_WEB_APP_ID` | 飞书网页应用的 App ID |
| `FEISHU_WEB_APP_SECRET` | 同一应用的 App Secret |
| `KB_PUBLIC_ORIGIN` | 网站的 HTTPS 源，例如 `https://kb.example.com`；仅包含协议、域名及可选端口 |
| `FEISHU_BASE_TOKEN` | 实际运营知识库的 Base app_token，指向允许浏览的主库 |
| `FEISHU_BASE_ORIGIN` | 可选，知识库所属租户的 HTTPS 源，如 `https://your-tenant.feishu.cn`。配置正确后，记录会附上该租户的飞书原表链接。 |

个人登录使用飞书官方授权页、一次性 `state` 与 PKCE S256。浏览器仅持有 `Secure`、`HttpOnly` 的站点会话 Cookie。服务把短期用户令牌保存在内存中，会话有效期最多两小时，且提前于该令牌过期。服务重启、会话到期或飞书撤权后，客服需重新授权。应用注册及协议详见[飞书授权说明](https://open.feishu.cn/document/uAjLw4CM/uYjL24iN/mobile-app)和[获取用户令牌](https://open.feishu.cn/document/uAjLw4CM/ukTMukTMukTM/authentication-management/access-token/get-user-access-token-v3)。

| 用户操作 | 浏览器接口 |
| --- | --- |
| 登录或退出 | `GET /auth/feishu/login` 转到飞书，飞书回调 `GET /auth/feishu/callback`。在本站通过 `POST /auth/logout` 退出。 |
| 查看表 | `GET /me/tables` 返回当前用户可见的 `SKU明细`、`商品QA`、`晓多商品知识` 表；以返回的 `table_id` 为准。 |
| 选择筛选字段 | `GET /me/fields?table_id=...` 返回可展示的字段名称、类型和支持的筛选运算符。 |
| 查看或筛选记录 | `GET /me/records?table_id=...`；按需传字段名 `field`、对应 `value`、`op`。默认每页 10，最多 25，可用返回的 `next_page_token` 发起下一页。 |

用本人账号打开记录的 `source` 链接即可在飞书原表查看、编辑。租户源缺失或无效时，`source` 为 `null`，仍可浏览记录。编辑权限由飞书原表及行、列级授权决定。业务系统使用本机索引时，新结果还要等待飞书同步发布。个人入口返回 401 时重新登录，返回 403 时由知识库管理员核对本人可见范围和应用的用户身份权限。

## 后端应用的地址与鉴权

完整资料目录使用服务器上的只读 SQLite 快照，由 `KB_CATALOG_DB` 指定。管理员可在本机请求 `GET /v1/admin/catalog/tables` 查看各表记录数、列名和顺序，再用 `GET /v1/admin/catalog/rows?table=...&q=...&limit=10` 读取原始行。筛选条件至少给出表名或关键词之一；每页最多 25 条，下一页传回 `next_after` 作为 `after`。记录值按表头顺序排列，保留来源行号和停用标识。完整目录沿用既有内部管理鉴权。店铺业务查询使用已按范围发布的 SKU 和话术。

业务后端先验证客服的登录身份、店铺和商品访问权限，再使用对应店铺密钥。查询商品 ID 使用字符串，保留前导零。

开发机默认地址为 `http://127.0.0.1:8080`。跨机器调用使用实际部署的 HTTPS 地址。每个业务请求都携带 `Authorization: Bearer <店铺密钥原值>`。同一店铺密钥可读取该店铺授权发布的所有商品和通用话术，应留在业务后端。接入该接口的业务系统负责其客服的登录、权限、审计和离职撤权。

`GET /healthz` 允许匿名，用于读取 `{"status":"ok","version":0}` 等健康结果。后台预部署时只配置独立的 `KB_ADMIN_TOKEN` 即可启动；店铺密钥为空时，店铺查询会收到 HTTP 401。管理员密钥仅用于内部发布接口。

**运维可只保存店铺密钥的 SHA-256 摘要。** 在可信终端用以下 Bash 命令输入自行指定的密钥并计算摘要。使用随机生成的高强度密钥有助于保护生产环境。

```bash
read -rsp '输入店铺密钥: ' SHOP_KEY
printf '\n'
printf '%s' "$SHOP_KEY" | sha256sum
unset SHOP_KEY
```

将输出开头的 64 个十六进制字符配置到服务端的 `KB_SHOP_TOKEN_HASHES`，例如 `{"store-a":"<64位十六进制摘要>"}`。业务后端保存密钥原值，调用时发送原值。也支持用 `KB_SHOP_TOKENS` 配置 JSON 对象，将店铺 ID 映射到密钥原值。同一店铺只选一种配置；管理员和各店铺使用不同密钥。可接受的密钥长度为 1–1024 个可见 ASCII 字节（不含空格）。

**轮换时**，先准备新密钥和摘要，再协调更新业务后端密钥与服务端配置，并重启知识库服务以加载新值。同一店铺同一时刻只接受一把密钥。切换后检查新密钥查询成功、旧密钥返回 401。

## 调用示例

以下示例在业务后端执行。`KB_URL` 为 API 基地址，`SHOP_KEY` 为通过环境或密钥管理服务注入的店铺密钥原值。

```bash
curl --fail-with-body --get "$KB_URL/v1/sku" \
  -H "Authorization: Bearer $SHOP_KEY" \
  --data-urlencode 'product_id=000123' \
  --data-urlencode 'model=iPhone 17 Pro Max'

curl --fail-with-body --get "$KB_URL/v1/search" \
  -H "Authorization: Bearer $SHOP_KEY" \
  --data-urlencode 'product_id=000123' \
  --data-urlencode 'q=售后怎么申请' \
  --data-urlencode 'limit=3'
```

查询共用以下返回格式。示例中的商品资料和版本均为演示值。

```json
{
  "version": 7,
  "items": [
    {
      "id": "qa:42",
      "sku": "",
      "model": "",
      "question": "售后怎么申请？",
      "answer": "请在订单详情页提交售后申请。",
      "source": "飞书售后知识表 · 记录 42"
    }
  ]
}
```

- **SKU** 必填 `product_id`，可加 `sku`、`model`。两者都传时须同时匹配；只给商品 ID 时最多返回 100 条已启用的卡片；有多个匹配项时由客服选定。工作台可在切换商品会话时预取。
- **话术** 必填 `product_id`、`q`。商品专用话术和本店通用话术共同参加相关性排序。`limit` 默认为 5，可取 1–20。查询文本和商品 ID 各自最多 256 个 UTF-8 字节。两种查询未命中都返回 HTTP 200 和空的 `items`。
- **展示与复制** 使用 `answer` 原文，并保留 `source` 和 `version`。来源既可能是文字，也可能是运营维护的链接。将文字按纯文本显示，只有符合应用链接策略的 HTTPS 链接才渲染为可点击项。服务返回 `Cache-Control: no-store`；如工作台已预取卡片，在重要话术复制前可重新查询以确认最新启用状态。

Rust 业务后端可以复用一份 `reqwest::Client` 的连接池。下例使用 `reqwest` 的 `json` 功能和 `serde_json`，由调用方传入已核验属于当前会话的商品 ID。

```rust
async fn lookup(
    client: &reqwest::Client,
    api: &str,
    shop_key: &str,
    product_id: &str,
    model: &str,
) -> Result<serde_json::Value, reqwest::Error> {
    client
        .get(format!("{}/v1/sku", api.trim_end_matches('/')))
        .bearer_auth(shop_key)
        .query(&[("product_id", product_id), ("model", model)])
        .send().await?
        .error_for_status()?
        .json().await
}
```

## AI 商品比较

配置外部提供方后，可从同一店铺比较两个不同商品。服务先发送已授权的资料和版本，再流式发送解释。每个商品最多取五条 SKU 卡片和三条检索命中的话术，`more_cards: true` 表示该商品还有其他 SKU。服务只把这两个商品的选定资料、相关店铺通用话术以及问题交给已配置的模型。请在启用 AI 前确认提供方适合处理这些业务资料。

```bash
curl --no-buffer --fail-with-body "$KB_URL/v1/compare" \
  -H "Authorization: Bearer $SHOP_KEY" \
  -H 'Content-Type: application/json' \
  -H 'Accept: text/event-stream' \
  --data '{"left_product_id":"000123","right_product_id":"000124","question":"这两款适配机型有什么区别？"}'
```

业务后端解析 SSE，并把所需内容转发给已登录的客服端。`sources` 包含 `version` 和两个商品的 `records`；`delta` 包含 `text`；`done` 表示完整结束。收到 `error` 或连接提前断开时，将解释标记为未完成；如错误为 `knowledge_changed`，重新查询商品。流开始后 HTTP 状态已是 200，完成状态以事件为准。模型未启用时返回 503 `ai_disabled`，两个并发生成名额均满时返回 429 `ai_busy`，普通查询仍可调用。

## 错误与发布

| HTTP 状态 | 应用处理 |
| --- | --- |
| 400、413、415、422 | 检查字段、JSON、响应体提示或请求大小；返回体可能是纯文本。 |
| 401 | 密钥缺失、错误或用于错误的端点。返回 `{"error":"unauthorized"}`。检查业务后端配置。 |
| 404、409 | 比较时商品缺少可访问的 SKU，或资料版本已切换。409 时重新请求。 |
| 429、503 | 容量暂满时做有限退避；`ai_disabled` 时直接展示参数卡。 |

内部同步通过 `POST /v1/admin/changes` 发布。请在反向代理上将该路由限制在内部网络。每批最多 50,000 项、32 MiB，使用独立管理员 Bearer 密钥和 `application/json`，响应为 `{"version":7}`。更新时提交完整 `item`，删除时显式提交 `"item":null`。重试同一批、且内容已一致时版本保持不变。字段和范围规则见 [OpenAPI](./openapi.json) 的 `Change` 与 `Item`。
