# ipswitchd HTTP 接口

本服务只有一个 HTTP 服务端、三个 GET 路由，外加三个错误响应。
所有内容都以源码为准（`src/rust/src/httpd.rs`），不是"设计稿"。

```
调用方（zgyd -ip-switch / curl / 浏览器）
        │  GET http://<路由器>:8790/switch
        ▼
   ipswitchd（本服务，Rust + tokio 手写 HTTP/1.1）
        │  newline-JSON over TCP，127.0.0.1:8765
        ▼
   at-webserver-rust（MT5700 Console 后端，独占 /dev/ttyUSB1）
        │  AT 命令
        ▼
   MT5700M-CN 模组
```

**本服务不开串口**。AT 全部借道 MT5700 Console，原因见 [`adr/0001`](adr/0001-reuse-at-webserver-rust-channel.md)。

---

## 1. 通用约定

| 项 | 值 |
|---|---|
| 监听 | `listen`（默认 `0.0.0.0`）+ `port`（默认 `8790`），改完要 `reload` |
| 协议 | HTTP/1.1，**只实现了 GET**（`httpd.rs:95`） |
| 请求头 | 全部读掉并丢弃，不做任何 header 处理；但必须读完，否则残留字节污染下一次解析（`httpd.rs:83`） |
| Query 串 | **被丢弃**：`/switch?a=1` 与 `/switch` 等价（`httpd.rs:295`） |
| 字符编码 | 响应体一律 UTF-8；`Content-Type` 都带 `charset=utf-8` |
| 连接模型 | 每个请求独立连接、`Connection: close`、**不做 keep-alive / 连接池** |
| 请求上限 | 读请求头 10s 超时（`HEADER_TIMEOUT`）；单行 8192 字节（`MAX_LINE`，`httpd.rs:34,37`） |
| 空连接 | 连上不发数据就断开 → **静默关闭，不算错误**（探活/端口扫描会这样） |

---

## 2. 端点总表

| 方法 | 路径 | 状态码 | `Content-Type` | 用途 |
|---|---|---|---|---|
| GET | `/switch` | 200 | `text/plain; charset=utf-8` | **换一次出口 IP**，chunked 流式返回进度 |
| GET | `/status` | 200 | `application/json; charset=utf-8` | 只读状态探针，**不下发任何写命令** |
| GET | `/` | 200 | `text/plain; charset=utf-8` | 极简存活探测，body = `ipswitchd ok` |
| GET | `/health` | 200 / **503** | `text/plain; charset=utf-8` | 存活 + AT 通道可达性；AT 探活失败给 503 |
| 其它 | 任意 | 404 | `text/plain; charset=utf-8` | body = `Not Found` |
| 非 GET | 任意 | 405 | `text/plain; charset=utf-8` | body = `只支持 GET` |
| 请求行解析失败 | — | 400 | `text/plain; charset=utf-8` | body = `Bad Request` |
| 连接数超上限 | 任意 | **503** | `text/plain; charset=utf-8` | body = `连接数已达上限，请稍后重试` |

`reason` 短语表：`200 OK` / `400 Bad Request` / `404 Not Found` / `405 Method Not Allowed` / `503 Service Unavailable`。

### 客户端资源上限（面向不可信来源）

这个端口没有鉴权，默认依赖 OpenWrt WAN 侧 input 策略拦截外部来源，所以每条连接都有硬上限
（取舍见 [adr/0006](adr/0006-resource-bounds-for-untrusted-clients.md)）：

| 上限 | 值 | 表现 |
|---|---|---|
| 单行长度（**读取期**生效） | 8192 字节 | 超长 / 不发换行的请求：直接关连接 |
| 单次写出 | 10s | 超时按"调用方已走"处理：丢弃进度，**切换照跑** |
| 并发连接数 | 32 | 超出时新连接得到 503 并关闭 |

---

## 3. `GET /switch` —— 换一次出口 IP

### 响应头

```
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8
Transfer-Encoding: chunked
Cache-Control: no-store
Connection: close
```

**没有 `Content-Length`**（chunked），**没有区分成功/失败的 HTTP 状态码** ——
两者都是 200，判定靠响应体里有没有标记文本。

### 响应体契约

逐行推进度，每帧一个 chunk，**边跑边发**：

```
开始切换出口 IP（策略 redial）
切换前 MT5700M 地址: 117.136.12.34
等待连接断开…
  等待连接断开完成（拨号状态=Some(false)）
重新拨号（APN 保持原配置）
等待拨号完成…
  等待拨号完成完成（拨号状态=Some(true)）
  接口续约方式: ubus
切换结束：117.136.12.34 → 117.136.45.67，用时 22.7s
IP切换完成
切换成功：117.136.12.34 → 117.136.45.67（redial，用时 22.7s）
```

> 每条 AT 下发还会多一行 `  AT^SETAUTODIAL=… → OK` 形态的回显。**原文不固定**
> —— 取决于 `AT^SETAUTODIAL?` 回读到的模组当前配置（协议 / APN / 账号 / 鉴权类型），
> 所以这里不写死样例。上面列的是与模组配置无关的那几行。

最后两块是**收尾块**，格式固定（`httpd.rs:147-163`）：

| 结果 | 收尾块文本 |
|---|---|
| 成功 | `<marker>\n` + `切换成功：<before> → <after>（<method>，用时 <n.n>s）` + 可选 `；注意：未观察到链路中断` |
| 失败 | `切换失败：<原因>\n` —— **不含标记文本** |

★★ **标记文本只在成功时输出**，这是调用方判定"切换完成"的**唯一凭据**
（见 [`adr/0004`](adr/0004-marker-only-on-success.md)）。失败还输出标记，
等于把一次可诊断的失败变成一次静默的数据流失。

`<marker>` 取自配置 `ipswitch.config.marker`，默认 `IP切换完成`，**必须与调用方一致**。
若中间状态有变化会插入 `  拨号状态=Some(true)` 这类行 —— 只在**状态变化时**打一行，
不是定时刷（`switcher.rs:340-403`）。

### 行为特性（都有测试钉死）

| 特性 | 说明 | 测试 |
|---|---|---|
| 调用方中途断开，**切换仍跑完** | 换 IP 有副作用，半途停下比跑完更糟 | `switch_survives_client_disconnect_and_finishes_the_job` |
| 多个请求**串行执行、不交错** | 内部互斥锁，不会两条 AT 命令交叉下发 | `concurrent_switches_are_serialized_not_interleaved` |
| 失败**绝不吐标记** | — | `switch_failure_never_emits_marker` |
| 拨号一直不回 → 超时判定 | 上限为配置 `timeout`（默认 45s） | `switch_reports_failure_when_dial_never_comes_back` |
| 完整 AT 序列 → 带标记 | — | `switch_outputs_marker_and_runs_full_at_sequence` |
| `method=apn` 轮换 APN | `apn_list` 为空时**退化为 redial** | `apn_strategy_switches_apn_and_reports_it` |

### 请求示例

```sh
curl -N http://192.168.10.1:8790/switch
# -N 关掉 curl 的输出缓冲，否则看不到流式进度
```

```sh
# 只判成败（见到标记即成功）
curl -sN http://192.168.10.1:8790/switch | grep -q 'IP切换完成' && echo OK || echo FAIL
```

---

## 4. `GET /status` —— 只读状态

只读，**不下发任何写命令**（`httpd.rs:173`）。响应头 `Content-Length` 精确、`Connection: close`，
**不带** `Cache-Control`。

```json
{
  "wan_iface": "MT5700M",
  "wan_ip": "10.76.139.149",
  "dial_active": true,
  "dial_raw": "^NDISSTATQRY: 1,...",
  "at_channel": "ok",
  "method": "redial",
  "marker": "IP切换完成",
  "apn_pool_size": 0,
  "timeout_secs": 45,
  "last_switch": {
    "method": "redial",
    "apn": null,
    "before": "117.136.12.34",
    "after": "117.136.45.67",
    "saw_link_down": true,
    "renewed_by": "ubus",
    "elapsed_secs": 22.7
  }
}
```

### 字段

| 字段 | 类型 | 含义 |
|---|---|---|
| `wan_iface` | string | WAN 接口名（配置 `wan_iface`） |
| `wan_ip` | string \| null | 该接口的 IPv4 地址；`null` = 没读到 |
| `dial_active` | bool \| null | 模组拨号状态。**`null` = 本次没读出来**，与 `false`（读出来了、未连接）严格区分 |
| `dial_raw` | string \| null | `AT^NDISSTATQRY?` 原始回显 |
| `at_channel` | string | **启动时** AT 通道探活结果：`ok` / `failed` / `unknown`（还没探过）。`failed` 说明"服务活着但干不了活" |
| `method` | string | **生效的**策略：`redial` 或 `apn`（`apn` + 空池会显示 `redial`） |
| `marker` | string | 当前标记文本 |
| `apn_pool_size` | number | `apn_list` 条目数 |
| `timeout_secs` | number | 单次切换上限 |
| `last_switch` | object \| null | **本进程内**上一次切换报告；从未切过则是 `null` |
| `last_switch.method` | string | 该次实际用的策略 |
| `last_switch.apn` | string \| null | 该次切到的 APN（`redial` 时为 `null`） |
| `last_switch.before` / `after` | string \| null | 切换前 / 后的接口地址 |
| `last_switch.saw_link_down` | bool | 是否观察到链路中断。`false` 不代表失败（见下） |
| `last_switch.renewed_by` | string \| null | 接口续约方式；`null` = 续约没成功（**不等于切换失败**） |
| `last_switch.elapsed_secs` | number | 耗时（秒，一位小数） |

两个容易误读的点：

- `dial_active: null` ≠ `false`。前者是"读不出来"，后者是"读出来了、未连接"。
  这是刻意的：把"读不出来"伪装成"没有"会误导排障（见 [`adr` 相关约定](../README.md#5-完成判定为什么是设备侧状态)）。
- `saw_link_down: false` 只表示**没采到**断开瞬间（可能断开很快，也可能本就未建立），
  不代表这次切换失败。成功响应里会附一句 `；注意：未观察到链路中断`。

---

## 5. `GET /` 与 `GET /health` —— 存活探测

### `GET /`

```
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8
Content-Length: 12
Connection: close

ipswitchd ok
```

**不碰模组、不碰 AT 通道**，可放心当高频心跳用。永远是 200。

### `GET /health`

在上面那份 body 后面**追加一行 JSON**，并按 AT 通道探活结果给状态码：

```
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8

ipswitchd ok
{"at_channel":"ok","status":"ok"}
```

AT 通道**明确探活失败**时（`main.rs` 启动探活失败并记录下来）：

```
HTTP/1.1 503 Service Unavailable
Content-Type: text/plain; charset=utf-8

ipswitchd ok
{"at_channel":"failed","status":"degraded"}
```

| 字段 | 类型 | 含义 |
|---|---|---|
| `at_channel` | string | `ok` / `failed` / `unknown`（还没探过） |
| `status` | string | `failed` 时是 `degraded`，否则 `ok` |

两个要点：

- **`ipswitchd ok` 前缀保留**：既有脚本是拿 `grep "ipswitchd ok"` 判活的，
  直接换成纯 JSON 会让它们静默失效 —— 宁可多一行 JSON。
- **`unknown` 不算失败**（给 200）：启动探活失败并不阻止服务启动（后端可能稍后就绪），
  所以"没探过"≠"干不了活"。
- 这一条**也不碰设备**：只读内存里的一个状态量，可以高频调用。
  （对比：`/status` 会真的下发一条只读的 `AT^NDISSTATQRY?`，通道忙时可能等上十几秒。）

---

## 6. 上游依赖接口（本服务 → MT5700 Console）

本服务消费的**不是** HTTP，而是 **newline-JSON over 裸 TCP**（`rpc.rs:13`）：

```
请求  {"id":N,"method":"at","params":{"cmd":"AT+CSQ","auth_key":"<key>"}}\n
响应  {"id":N,"result":{"success":true,"data":"...","error":null}}\n
错误  {"id":3,"error":{"code":-32001,"message":"认证失败"}}\n
```

| 项 | 值 |
|---|---|
| 地址 | 配置 `rpc_host` / `rpc_port`，默认 `127.0.0.1:8765` |
| 报文上限 | 8192 字节（与后端一致） |
| 成败判定 | **只看 `result.success`**，不看 `data` 文本里像不像 OK |
| `id` | 每连接自增，用于请求响应配对 |
| `auth_key` | **不来自本包配置** —— 从 `/etc/config/at-webserver` 的 `auth_key` 读（`config.rs:254`）。读不到就用空串，与"后端没配密钥"这一正常形态一致 |

本服务用到的 AT（全部经这条通道下发）：

| 命令 | 用途 |
|---|---|
| `AT^SETAUTODIAL=0` | 断开 |
| `AT^SETAUTODIAL=1,<mode>,"<协议>","<APN>","<账号>","<密码>",<鉴权>` | 重新拨号。`redial` 用回读到的当前配置原样写回；`apn` 用配置的 `apn_protocol` + 池里的下一个 APN（`switcher.rs:267-294`） |
| `AT^SETAUTODIAL=1,<mode>` | **降级形态**：回读不到当前配置时才用（省略 APN 有被清空风险，见 [`adr/0003`](adr/0003-redial-writes-back-current-apn.md)） |
| `AT^SETAUTODIAL?` | 回读当前自动拨号配置（切换前后各一次，确认 APN 没被清掉） |
| `AT^NDISSTATQRY?` | 查拨号状态（`^NDISSTATQRY: 1,…` = 已连接） |

---

## 7. 调用方契约（zgyd → 本服务）

zgyd 侧实现在 `pkg/ipswitch.go`：

1. 发 `GET`，要求 **HTTP 200**
2. **边收边找**标记文本，保留 `len(marker)-1` 字节的尾窗（防标记被分块切在中间）
3. **看到标记即成功，不等 EOF**

第 3 条决定了本服务必须把标记放在**收尾块**、而进度先流出去 —— 所以
`/switch` 用 chunked 而不是先攒完再一次发。

```sh
zgyd -ip-switch http://192.168.10.1:8790/switch
```

标记两边都要改：

```sh
uci set ipswitch.config.marker='换好了'
uci commit ipswitch && /etc/init.d/ipswitch reload

zgyd ... -ip-switch-marker '换好了'
```

| zgyd 参数 | 默认 | 与本服务的关系 |
|---|---|---|
| `-ip-switch-every` | `50` | 每 50 个请求换一次 → 决定本服务被调用频率 |
| `-ip-switch-timeout` | `60`（秒） | **必须 > `ipswitch.timeout`**（默认 45） |
| `-ip-switch-retries` | `2` | 重试会**再打一次** `/switch`；本服务内部串行化，不会交叉下发 AT |

★ **超时必须配对**：zgyd 先超时收工，而本服务还在跑，就会留下一次"没人认领"的切换；
下次请求又撞上锁。默认 45 < 60 是刻意留的余量。

---

## 8. 配置项 → 接口行为映射

| 配置（`/etc/config/ipswitch`） | 默认 | 影响的接口行为 |
|---|---|---|
| `listen` / `port` | `0.0.0.0` / `8790` | 监听地址。`0.0.0.0` 的安全性依赖 OpenWrt 默认 WAN 侧 input 策略 |
| `marker` | `IP切换完成` | `/switch` 成功收尾块的标记文本（空值被 `validate()` 拒绝） |
| `method` | `redial` | `/switch` 策略、`/status.method` |
| `apn_list` | 空 | `method=apn` 时的 APN 池；空 → 退化为 redial |
| `apn_protocol` | `IP` | `method=apn` 切 APN 时写进 `AT^SETAUTODIAL` 第 3 参数 |
| `dial_mode` | `1` | 写进 `AT^SETAUTODIAL` 第 2 参数（H5000M 是 1） |
| `timeout` | `45` | `/switch` 单次上限、`/status.timeout_secs` |
| `wan_iface` | `MT5700M` | `/status.wan_iface`、地址读取目标 |
| `rpc_host` / `rpc_port` | `127.0.0.1` / `8765` | 上游通道地址，**不要改成外网地址** |

配置**只在进程启动时读一次**（避免同一次切换中途配置变化、前后半段按不同参数执行），
改完必须 `/etc/init.d/ipswitch reload`。

---

## 9. 这些契约谁在守

`src/rust/src/e2e_tests.rs` 起**真的 TCP 假后端 + 真的 HTTP 假客户端**，
把 `HTTP → httpd → switcher → rpc → 假模组` 整条链路跑通，覆盖上表里每条行为特性。
共 9 个端到端用例（另有 39 个单元用例）。

本机没有交叉编译工具链，所以 **Rust 侧改动必须在 CI 或 OpenWrt SDK 里验证**
（见 [`adr/0005`](adr/0005-build-time-hard-constraints.md)）。

---

## 10. 真机验证状态

- **换 IP 本身：已确认可用。** 2026-09-30 在真机（Hiveton H5000M + MT5700M-CN）上实测
  **能切换出口 IP**。
- **未单独记录**：用的哪种策略（`redial` / `apn`）、是直接打 `/switch` 还是经 zgyd 调用、
  标记文本的匹配情况、`/status` 各字段的真机取值。
  需要的话按第 3、4 节的契约逐项补测即可 —— 接口行为本身已由 9 个端到端用例钉死。
