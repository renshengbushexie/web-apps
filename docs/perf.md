# 本地 HTTP 容量验证

负载程序通过 HTTP 访问单个 API 容器。它先确认数据库版本为 0，再写入 20,000 条合成记录，覆盖 10 家店铺、2,000 个商品。每个商品有 4 条 SKU 和 6 条中文问答。每家店铺使用独立的测试令牌。

完成 100 次预热读取后，程序以固定到达时间先后发起每秒 100 和 300 次读取，每档默认持续 60 秒。SKU 精确查询占 60%，中文全文检索占 40%。每档中途发布一次更新，修改 2,000 条现存记录，读取持续进行。程序对每次成功响应核对目标记录、商品范围和与版本对应的内容标记。

计划到达至响应检查完成的延迟包含负载机调度、网络往返、服务处理、正文读取和客户端 JSON 校验。程序另报负载机启动延迟和从实际派发请求到响应校验完成的延迟。返回字节数只计算 HTTP 响应正文。发布期间的统计选取在发布调用开始到结束之间完成的读取。超时、非成功 HTTP 状态、内容校验失败、负载机过载各自计数。

## 2026 年 11 月 24 日实测

服务使用 Docker Desktop 的 Linux amd64 release 镜像 `sha256:f2cc45ccded0d99dd5abaf2b9ad289eb73186a5b2be12a08da644713a3aaf33f`，负载由 Windows 主机发出。容器 CPU 配额为 2 核，内存限额 2 GiB，禁用额外交换空间。初次发布请求正文为 7,972,153 字节，20,000 条记录在 0.360 秒内完成发布。

| 固定到达速率 | 校验成功 / 计划请求 | P50 | P95 | P99 | 查询响应正文总字节 | 发布 2,000 行用时 |
|---|---:|---:|---:|---:|---:|---:|
| 100 次/秒，60 秒 | 6,000 / 6,000 | 1.589 ms | 2.124 ms | 2.447 ms | 1,658,468 | 104.8 ms |
| 300 次/秒，60 秒 | 18,000 / 18,000 | 1.185 ms | 1.900 ms | 2.339 ms | 4,960,915 | 103.6 ms |

表中分位数采用计划到达时间。100 次/秒时，SKU 与全文检索的 P99 分别为 2.356 ms、2.528 ms。300 次/秒时，两者分别为 2.252 ms、2.461 ms。负载生成器自己的启动延迟 P99 分别为 0.729 ms 和 0.681 ms。

两次发布调用期间分别完成 11 和 32 次读取，均通过内容及版本校验，这两个短窗口观察到的最大延迟分别为 2.793 ms 和 3.987 ms。容器 cgroup 记录的内存峰值为 126,681,088 字节（120.81 MiB），CPU 配额节流和 OOM 均为 0 次。内存统计覆盖 API 进程及其归属的文件缓存等容器开销。宿主操作系统、负载生成器以及其他服务使用的资源另计。

每档只运行了一次，两档的细微延迟差异不能用于推断负载增加会缩短响应时间。2,000 代表本次合成数据的商品数，压测未模拟 2,000 个独立客服客户端或各自的长连接。

## 复现

以下命令可在已安装 Rust、Docker、Python 3、curl 和 OpenSSL 的本机 Bash 环境运行。每次执行启动一个新的临时容器，API 只映射到回环地址，测试结束后移除容器。负载生成器运行在容器外。容器的可用 CPU 为 2 核，内存和内存加交换空间的上限均为 2 GiB。

```bash
set -euo pipefail
docker build -t rust-knowledge:v1-local .
cargo build --locked --example kb-bench
export KB_BENCH_ADMIN="$(openssl rand -hex 32)"
export KB_ADMIN_TOKEN="$KB_BENCH_ADMIN"
export KB_BENCH_SHOP_PREFIX="$(openssl rand -hex 32)"
export KB_SHOP_TOKENS="$(python3 -c 'import json,os; p=os.environ["KB_BENCH_SHOP_PREFIX"]; print(json.dumps({f"smoke-{i:02d}":f"{p}-{i}" for i in range(10)}))')"
cid="$(docker run --pull never -d --rm --cpus=2 --memory=2g --memory-swap=2g \
  -p 127.0.0.1::8080 -e KB_ADMIN_TOKEN -e KB_SHOP_TOKENS \
  -e KB_BIND=0.0.0.0:8080 -e KB_DB=/data/knowledge-bench.sqlite3 rust-knowledge:v1-local)"
trap 'docker stop "$cid" >/dev/null' EXIT
export KB_BENCH_URL="http://$(docker port "$cid" 8080/tcp)"
for _ in $(seq 1 40); do
  if curl -fsS "$KB_BENCH_URL/healthz" >/dev/null; then break; fi
  sleep 0.25
done
target/debug/examples/kb-bench
docker exec "$cid" cat /sys/fs/cgroup/memory.peak /sys/fs/cgroup/cpu.stat /sys/fs/cgroup/memory.events
```

驱动程序在标准输出中逐行写入 JSON。可使用 `KB_BENCH_SECONDS` 调整每档时长，默认 60 秒。实际容量还取决于商品数、话术长度、热门店铺的流量分布和数据发布频率。本机测试不包含公网 TLS、浏览器长连接、第三方平台限流、AI 调用或真实客服会话。
