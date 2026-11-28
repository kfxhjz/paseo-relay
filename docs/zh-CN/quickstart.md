# 五分钟 VPS 快速部署

[English](../quickstart.md) | **简体中文**

这是 **大多数自托管用户** 的路径：一台公网 VPS、一个 relay 节点、
`relay.example.com` 自动 HTTPS。不需要负载均衡，也不需要集群。

## 你需要什么

- 有公网 IPv4 的 VPS（自动 TLS 需要域名）。
- 为 `relay.example.com` 之类域名配置 DNS `A` 记录（用 IPv6 再加 `AAAA`），
  指向 VPS。
- Docker + Compose v2，并在防火墙/安全组开放入站 TCP `80`/`443`
  （HTTP/3 可选 UDP `443`）。
- **relay 容器约 512 MB 内存**（Compose 默认 `PASEO_RELAY_MEM_LIMIT=512m`、
  129 MiB ingress 预算，连接上限按 512 MB 设置），另给 Caddy 和连接峰值留余量。
  一个 idle 已接受 socket 仍占约 23 KiB，规划时要算进去。调高
  `PASEO_RELAY_MEM_LIMIT` 时连接上限要一起调高。如果把
  `PASEO_RELAY_INGRESS_BUDGET_BYTES` 提到内置默认 512 MiB，建议至少 ~1 GB。
  从源码构建需要至少 ~1 GB 和几分钟。

## 部署已发布镜像

直接使用已发布镜像，不需要 Rust 工具链。

```sh
git clone https://github.com/kfxhjz/paseo-relay.git
cd paseo-relay

RELAY_DOMAIN=relay.example.com \
docker compose -f examples/docker-compose.caddy.yml up -d
```

Compose 会拉取当前 release `ghcr.io/kfxhjz/paseo-relay:v0.1.4`；`latest` 跟随
最新 **stable** release（预发布 tag 不会移动它）。release workflow 会先按 digest
对镜像做 smoke test、签名/证明，再提升版本 tag 或 `latest`，所以跟随 `latest` 不会
拿到未验证的构建。可用 `PASEO_RELAY_IMAGE` 覆盖版本；如果需要固定不可变的 digest，
见[部署指南](deployment.md)。示例还把
`PASEO_RELAY_MAX_CONNECTIONS`（4,000）、`PASEO_RELAY_MAX_SESSIONS`（4,000）、
`PASEO_RELAY_MAX_TCP_CONNECTIONS`（5,000）限制到适合 512 MB 内存上限的值；调高
`PASEO_RELAY_MEM_LIMIT` 时请一起调高这些上限。

示例固定当前稳定版 Caddy 镜像 `caddy:2.11.9-alpine`（多架构 digest）。升级前请查看
Caddy release notes，再手动更新 pin，不要直接浮动到 `latest`。Caddy 会自动申请证书。
验证：

```sh
curl https://relay.example.com/health   # {"status":"ok"}
```

relay 本身不映射到宿主机端口，只有 Caddy 对外；relay 以 read-only、非 root、
drop 所有 capability 运行。`/metrics` 和 `/ready` 被 Caddy 屏蔽；如果要 Prometheus，
请在 Compose 内网抓取。

各组件运行位置：

```mermaid
flowchart LR
  WEB["互联网或手机客户端"] --> CADDY["Caddy 443 自动 TLS"]
  DAEMON["Paseo daemon"] -->|"主动出站 wss"| CADDY
  CADDY --> RELAY["relay 容器 4000 仅内网"]
```

## VPS 上已有 Caddy 或其他域名

如果这台 VPS 已经为其他域名提供服务，80/443 端口只能由一个进程监听。此时
**不要启动示例里的 `caddy` 服务**，而是复用已经在运行的反向代理。

**已有 Caddy。** 把 relay 的 site block 合并进现有 Caddyfile（Caddy 按 TLS
SNI/Host 选择站点，并为每个域名分别申请证书）：

```caddyfile
relay.example.com {
    encode zstd gzip
    @operations path /metrics /ready /version
    handle @operations { respond 404 }
    handle { reverse_proxy relay:4000 }
}
```

- 如果现有 Caddy 在 Docker 中、且能访问本项目的网络，保留
  `reverse_proxy relay:4000`。
- 如果 Caddy 直接运行在宿主机上，只把 relay 发布到 loopback（见下方 override），
  然后使用 `reverse_proxy 127.0.0.1:4000`。
- 如果现有 Caddy 属于另一个 Compose 项目，把该 Caddy 容器接入本项目网络，或使用
  loopback 端口。

宿主机 Caddy 的最小 override 和只启动 relay 的命令：

```yaml
# docker-compose.override.yml
services:
  relay:
    ports: ["127.0.0.1:4000:4000"]
```

```sh
RELAY_DOMAIN=relay.example.com \
docker compose \
  -f examples/docker-compose.caddy.yml \
  -f docker-compose.override.yml \
  up -d relay
```

**把其他域名接到本 Caddy。** 保持只有一个 Caddy 进程，为每个域名添加独立的 site
block（也可以把多个名字写在一起：`relay.example.com, relay2.example.com { ... }`）。
每个公网域名都需要自己的 DNS 记录；Caddy 会自动申请证书。不要同时运行两个都发布
80/443 的 Caddy/nginx 容器。

**已有 Nginx/Traefik/HAProxy。** 增加一个虚拟主机，把
`relay.example.com/ws` 反代到 relay，并保留 WebSocket upgrade；边缘要求和多节点示例见
[部署指南](deployment.md)。

同一个 relay 可以有多个公网域名，但 Paseo 必须配置用户实际访问的那个 hostname。
证书签发按域名有速率限制；反复测试时请使用 staging CA。

## 从源码构建（可选）

只在你要改 relay 本身时使用；正常部署直接用已发布镜像。需要至少 ~1 GB 内存和
支持 Rust 的 Docker builder：

```sh
RELAY_DOMAIN=relay.example.com \
docker compose \
  -f examples/docker-compose.caddy.yml \
  -f examples/docker-compose.caddy.build.yml \
  up -d --build
```

## 让 Paseo 接进来

在运行 Paseo daemon 的机器上：

```sh
export PASEO_RELAY_ENDPOINT=relay.example.com:443
export PASEO_RELAY_PUBLIC_ENDPOINT=relay.example.com:443
export PASEO_RELAY_USE_TLS=true
export PASEO_RELAY_PUBLIC_USE_TLS=true
export PASEO_RELAY_ENABLED=true
paseo daemon pair --relay
```

请在 **运行中的 daemon** 环境（或 `config.json`）设置这些变量，而不是只在执行
`pair` 的 shell 里设置；已经在运行的 daemon 可能不会读取一次性环境变量。

用 Paseo App 扫描/打开配对链接。daemon 会 **主动出站** 连接 relay，不需要为
daemon 做端口映射；手机客户端必须能访问 `relay.example.com:443`。

## 升级

```sh
git pull
docker compose -f examples/docker-compose.caddy.yml up -d
```

relay 状态是临时的：Paseo 客户端会重连，E2EE 配对是端到端的，不存储在 relay。
不要 `SIGKILL`；Compose 会发 `SIGTERM`，relay 会在 grace window 内 drain。

## 排障

| 现象 | 检查 |
|---|---|
| Caddy 申请证书失败 | DNS 是否指向 VPS；80/443 是否可达；看 Caddy 日志 |
| `/health` 正常但配对失败 | 使用 `wss`/443；手机是否能访问同一公网 endpoint |
| Caddy 返回 502/504 | relay 容器是否 healthy；`docker compose logs relay` |
| 所有连接被拒 | 检查 `PASEO_RELAY_MAX_CONNECTIONS`、`/ready` 和 relay 日志 |
| `healthcheck` 报 "not loopback" | 配置了原生 TLS 但 `PASEO_RELAY_TLS_CERT` 不可读：探针只信任 loopback。修复证书或改用 loopback 目标。明文非 loopback 探测允许但无法认证 |
| 看不到 metrics | `/metrics` 和 `/ready` 被 Caddy 屏蔽；从内网抓 relay |
| 要下线节点 | 先从边缘摘除，再发 SIGTERM，等待 grace window |
| OOM 或构建失败 | 使用预构建镜像；同时调高 `PASEO_RELAY_MEM_LIMIT` 和连接上限（每个 idle socket 约 23 KiB），或给构建至少 ~1 GB 内存 |

## 什么情况下不要用这条路径

- **多 relay 节点**：需要边缘按 `serverId` query 做一致性哈希
  （Nginx/HAProxy/Envoy）；见 [deployment.md](deployment.md)。
- **需要 relay 级认证**：本项目按设计没有；请放在 VPN/mTLS 或带认证的边缘后。
- **无法开放 80/443**：使用 tunnel/VPN 或在别处终止 TLS；见
  [deployment.md](deployment.md)。
