//! 端到端测试：真实 HTTP 往返 + 真实 AT 通道协议，只把**模组**换成假的。
//!
//! 被测链路：`HTTP 客户端 → httpd → switcher → rpc(8765 newline-JSON) → 假模组`
//!
//! 唯一被替换掉的是最末端：一个假的 `at-webserver-rust`。这样既能验到
//! `AT^SETAUTODIAL` 命令序列、轮询判据、chunked 输出，又不需要真机。
//!
//! 本机没有 `ip` / `ubus` 命令（Windows），所以 `netif` 一律拿不到地址 ——
//! 这恰好覆盖了"模组说已连接但接口没地址"这一分支，是有意义的一条路径。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use crate::httpd;
use crate::rpc::AtClient;
use crate::switcher::{AtState, Switcher};

/// 假 AT 后端。
struct FakeRpc {
    addr: SocketAddr,
    /// 依次收到的 AT 命令。
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeRpc {
    fn commands(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// 起一个假的 8765：收一行 JSON，回一行 JSON。
async fn spawn_fake_rpc<F>(plan: F) -> FakeRpc
where
    F: Fn(&str) -> (bool, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定假 RPC 端口");
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = seen.clone();
    let plan = Arc::new(plan);

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let seen = seen_srv.clone();
            let plan = plan.clone();
            tokio::spawn(async move {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(req) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    let id = req.get("id").cloned().unwrap_or(serde_json::json!(0));
                    let cmd = req
                        .get("params")
                        .and_then(|p| p.get("cmd"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().push(cmd.clone());

                    let (ok, data) = plan(&cmd);
                    let resp = serde_json::json!({
                        "id": id,
                        "result": { "success": ok, "data": data, "error": serde_json::Value::Null },
                    });
                    if write_half
                        .write_all(format!("{resp}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let _ = write_half.flush().await;
                }
            });
        }
    });

    FakeRpc { addr, seen }
}

/// 构造一个"模组状态会跟着命令变化"的假后端，行为贴近真机：
/// `AT^SETAUTODIAL=0` 后连接断开，`=1,...` 后重新连上。
async fn spawn_automaton_rpc(initial_connected: bool) -> (FakeRpc, Arc<AtomicBool>) {
    let connected = Arc::new(AtomicBool::new(initial_connected));
    let flag = connected.clone();

    let rpc = spawn_fake_rpc(move |cmd| {
        if cmd == "AT^NDISSTATQRY?" {
            let stat = if flag.load(Ordering::SeqCst) { 1 } else { 0 };
            return (true, format!("^NDISSTATQRY: {stat},0,,\"IPV4\"\r\nOK"));
        }
        if cmd == "AT^SETAUTODIAL=0" {
            flag.store(false, Ordering::SeqCst);
            return (true, "OK".to_string());
        }
        if cmd.starts_with("AT^SETAUTODIAL=1") {
            flag.store(true, Ordering::SeqCst);
            return (true, "OK".to_string());
        }
        if cmd == "AT^SETAUTODIAL?" {
            return (
                true,
                "^SETAUTODIAL:1,1,\"IP\",\"cmnet\",\"\",\"\",0\r\nOK".to_string(),
            );
        }
        (true, "OK".to_string())
    })
    .await;

    (rpc, connected)
}

fn test_config(rpc_port: u16) -> Config {
    let mut cfg = Config::default();
    cfg.listen = "127.0.0.1".into();
    cfg.port = 0;
    cfg.rpc_port = rpc_port;
    // 压到 10s：够跑完流程，又不会把用例拖成分钟级
    cfg.timeout = Duration::from_secs(10);
    cfg
}

/// 起服务并返回它的实际地址。
async fn start_server(cfg: Config) -> SocketAddr {
    start_server_with_handle(cfg).await.0
}

/// 起服务，同时返回它的 task 句柄（用例可以等它自己收场）。
async fn start_server_with_handle(cfg: Config) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let at = AtClient::new("127.0.0.1", cfg.rpc_port, "");
    let sw = Switcher::new(cfg, at);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 HTTP 端口");
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = httpd::accept_loop(listener, sw).await;
    });
    (addr, handle)
}

/// 起服务并把 AT 通道探活结果设成 `state`（供 /health、/status 用例）。
async fn start_server_with_at_state(cfg: Config, state: AtState) -> SocketAddr {
    let at = AtClient::new("127.0.0.1", cfg.rpc_port, "");
    let sw = Switcher::new(cfg, at);
    sw.set_at_state(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定 HTTP 端口");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = httpd::accept_loop(listener, sw).await;
    });
    addr
}

/// 发一个 GET 并把整个响应（含 chunked 原始帧）读回来。
async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).await.expect("连接 HTTP 服务");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    s.flush().await.unwrap();

    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).into_owned()
}

// ─────────────────────────── 用例 ───────────────────────────

#[tokio::test]
async fn switch_outputs_marker_and_runs_full_at_sequence() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;

    let body = http_get(addr, "/switch").await;

    assert!(
        body.contains("IP切换完成"),
        "成功路径必须输出标记，实际响应: {body}"
    );
    assert!(body.starts_with("HTTP/1.1 200 OK"), "响应头不对: {body}");
    assert!(
        body.contains("Transfer-Encoding: chunked"),
        "必须是流式响应: {body}"
    );

    let cmds = rpc.commands();
    assert!(
        cmds.iter().any(|c| c == "AT^SETAUTODIAL=0"),
        "必须先断开: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("AT^SETAUTODIAL=1")),
        "必须重新拨号: {cmds:?}"
    );
    // 断开必须排在拨号之前，否则等于没换
    let down_at = cmds.iter().position(|c| c == "AT^SETAUTODIAL=0").unwrap();
    let up_at = cmds
        .iter()
        .position(|c| c.starts_with("AT^SETAUTODIAL=1"))
        .unwrap();
    assert!(down_at < up_at, "命令顺序反了: {cmds:?}");

    // APN 必须被原样写回（不能退化成省略形态）
    let up_cmd = &cmds[up_at];
    assert!(
        up_cmd.contains("\"cmnet\""),
        "重拨必须带上原 APN，否则可能清空 APN 导致断网: {up_cmd}"
    );
}

#[tokio::test]
async fn switch_failure_never_emits_marker() {
    // 假模组：拒绝重新拨号
    let rpc = spawn_fake_rpc(|cmd| {
        if cmd == "AT^NDISSTATQRY?" {
            return (true, "^NDISSTATQRY: 0,0,,\"IPV4\"\r\nOK".to_string());
        }
        if cmd.starts_with("AT^SETAUTODIAL=1") {
            return (false, "ERROR".to_string());
        }
        (true, "OK".to_string())
    })
    .await;

    let addr = start_server(test_config(rpc.addr.port())).await;
    let body = http_get(addr, "/switch").await;

    assert!(
        !body.contains("IP切换完成"),
        "★ 切换失败时绝不能输出标记，否则调用方会以为 IP 已经换好了。响应: {body}"
    );
    assert!(body.contains("切换失败"), "应说明失败原因: {body}");
}

#[tokio::test]
async fn switch_reports_failure_when_dial_never_comes_back() {
    // 假模组：断开后永远连不上
    let rpc = spawn_fake_rpc(|cmd| {
        if cmd == "AT^NDISSTATQRY?" {
            return (true, "^NDISSTATQRY: 0,0,,\"IPV4\"\r\nOK".to_string());
        }
        (true, "OK".to_string())
    })
    .await;

    let mut cfg = test_config(rpc.addr.port());
    cfg.timeout = Duration::from_secs(4); // 缩短等待，别让用例跑太久
    let addr = start_server(cfg).await;

    let body = http_get(addr, "/switch").await;
    assert!(!body.contains("IP切换完成"), "没拨上号就不能报完成: {body}");
    assert!(body.contains("切换失败"), "应报失败: {body}");
}

#[tokio::test]
async fn switch_survives_client_disconnect_and_finishes_the_job() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;

    // 连上就立刻断开：模拟调用方见到标记后主动 Close
    {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /switch HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let _ = s.flush().await;
        // 不发完请求就 drop，让服务端在写出时遇到 broken pipe
        drop(s);
    }

    // 给切换留出跑完的时间（主要是 confirm_ready 的 settle 窗口）
    tokio::time::sleep(Duration::from_secs(3)).await;

    let cmds = rpc.commands();
    assert!(
        cmds.iter().any(|c| c == "AT^SETAUTODIAL=0"),
        "★ 调用方断开后，切换仍必须继续执行（换 IP 有副作用，半途停下更糟）: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("AT^SETAUTODIAL=1")),
        "★ 断开后必须继续完成拨号: {cmds:?}"
    );
}

#[tokio::test]
async fn concurrent_switches_are_serialized_not_interleaved() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;

    let a = http_get(addr, "/switch");
    let b = http_get(addr, "/switch");
    let (ra, rb) = tokio::join!(a, b);

    assert!(ra.contains("IP切换完成") && rb.contains("IP切换完成"));

    // 两次切换必须各自完整：断开、拨号各 2 次，且不能交错
    let cmds = rpc.commands();
    let downs = cmds.iter().filter(|c| *c == "AT^SETAUTODIAL=0").count();
    assert_eq!(downs, 2, "两次请求应各断开一次: {cmds:?}");

    // 交替检查：把命令压成 D(断)/U(连) 序列，必须是 D..U..D..U 而不是 D D U U
    let seq: Vec<char> = cmds
        .iter()
        .filter_map(|c| {
            if c == "AT^SETAUTODIAL=0" {
                Some('D')
            } else if c.starts_with("AT^SETAUTODIAL=1") {
                Some('U')
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        seq,
        vec!['D', 'U', 'D', 'U'],
        "★ 两次切换交错会让模组卡在中间态: {seq:?}"
    );
}

#[tokio::test]
async fn status_route_is_read_only_and_returns_json() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;

    let before = rpc.commands().len();
    let body = http_get(addr, "/status").await;

    assert!(body.starts_with("HTTP/1.1 200 OK"), "{body}");
    assert!(body.contains("application/json"), "{body}");
    assert!(
        body.contains("\"dial_active\":true"),
        "应报告模组侧状态: {body}"
    );
    assert!(body.contains("\"last_switch\":null"), "还没切过: {body}");

    // ★ 只读路由不得下发任何 SETAUTODIAL 写命令
    let after = rpc.commands();
    let writes = after[before..]
        .iter()
        .filter(|c| c.starts_with("AT^SETAUTODIAL") && !c.ends_with('?'))
        .count();
    assert_eq!(writes, 0, "/status 不许改设备状态");
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;
    let body = http_get(addr, "/nope").await;
    assert!(body.contains("404"), "{body}");
}

#[tokio::test]
async fn root_and_health_report_ok() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;
    assert!(http_get(addr, "/").await.contains("ipswitchd ok"));
    assert!(http_get(addr, "/health").await.contains("200 OK"));
}

#[tokio::test]
async fn apn_strategy_switches_apn_and_reports_it() {
    let rpc = spawn_fake_rpc(|cmd| {
        if cmd == "AT^NDISSTATQRY?" {
            return (true, "^NDISSTATQRY: 1,0,,\"IPV4\"\r\nOK".to_string());
        }
        if cmd == "AT^SETAUTODIAL?" {
            return (
                true,
                "^SETAUTODIAL:1,1,\"IP\",\"cmnet\",\"\",\"\",0\r\nOK".to_string(),
            );
        }
        (true, "OK".to_string())
    })
    .await;

    let mut cfg = test_config(rpc.addr.port());
    cfg.method = crate::config::Method::Apn;
    cfg.apn_list = vec!["3gnet".into(), "ctnet".into()];
    let addr = start_server(cfg).await;

    let body = http_get(addr, "/switch").await;
    assert!(body.contains("IP切换完成"), "{body}");

    let cmds = rpc.commands();
    let up = cmds
        .iter()
        .find(|c| c.starts_with("AT^SETAUTODIAL=1"))
        .expect("应有拨号命令");
    assert!(up.contains("\"3gnet\""), "应切到池里第一个 APN: {up}");

    // 第二次应轮换到下一个
    let body2 = http_get(addr, "/switch").await;
    assert!(body2.contains("IP切换完成"), "{body2}");
    let cmds = rpc.commands();
    let ups: Vec<_> = cmds
        .iter()
        .filter(|c| c.starts_with("AT^SETAUTODIAL=1"))
        .collect();
    assert!(
        ups.last().unwrap().contains("\"ctnet\""),
        "APN 池应轮换: {ups:?}"
    );
}

// ─────────────────── 资源上限：客户端不能把服务拖住 ───────────────────
//
// 这三条都是"资源上限"用例，判据是**连接必须自己收场**，而不是"内存数字" ——
// 内存数字在 CI 里不可观测，但"服务端任务在 N 秒内结束"是可观测且稳定的代理指标。

/// 超长的行（不发换行）不会让服务端无界地读下去。
///
/// 旧实现是 `read_until` 之后才 `truncate`：`HEADER_TIMEOUT` 约束的是整次调用耗时，
/// 不是字节数，所以"慢慢发、不发换行"能让行缓冲一直长。修好后上限在**读取期**生效。
///
/// ★ 这条直接在 `read_line` 上量"到底读掉了多少字节" —— 这是唯一能直接观测上限的
/// 位置。在 socket 上量会被内核收发缓冲掩盖（实测：客户端灌 4MB 也「成功」，
/// 因为对端根本不读、字节全堆在内核缓冲里），那种判据是测不出来的。
///
/// 变异验证：把 `reader.take(MAX_LINE as u64)` 去掉（回到"读完再截断"）→ 判红。
#[tokio::test]
async fn read_line_is_bounded_by_max_line() {
    /// 一个**永不结束**、也**永不给换行**的输入。
    ///
    /// `limit` 是夹具自身的安全阀：万一被测代码真的没有上限，这里会先报错，
    /// 而不是让测试进程去申请几百 GB（变异验证时亲眼见过
    /// `memory allocation of 17179869184 bytes failed` —— 那正是这个漏洞的形态）。
    struct NoNewlineForever {
        served: Arc<std::sync::atomic::AtomicUsize>,
        limit: usize,
    }
    impl tokio::io::AsyncRead for NoNewlineForever {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let already = self.served.load(std::sync::atomic::Ordering::Relaxed);
            if already >= self.limit {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("夹具安全阀：已送出 {already} 字节，被测代码没有上限"),
                )));
            }
            let n = buf.remaining();
            buf.put_slice(&vec![b'A'; n]);
            self.served
                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            std::task::Poll::Ready(Ok(()))
        }
    }

    const SAFETY_LIMIT: usize = 4 * 1024 * 1024;
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut reader = tokio::io::BufReader::new(NoNewlineForever {
        served: served.clone(),
        limit: SAFETY_LIMIT,
    });

    let got = tokio::time::timeout(Duration::from_secs(5), httpd::read_line(&mut reader))
        .await
        .expect("read_line 必须在上限处返回，而不是一直读下去");

    let n = served.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        n <= httpd::MAX_LINE * 2,
        "read_line 从一条没有换行的流里读掉了 {n} 字节（上限 {}）：读取期没有上限",
        httpd::MAX_LINE
    );
    // 到上限就当成"这不完整的一行"处理：要么给回截断内容，要么当对端关闭。
    assert!(
        matches!(got, Ok(None) | Ok(Some(_))),
        "read_line 到上限应正常返回，而不是报错: {got:?}"
    );
}

/// 客户端连上却不读响应：这条连接不能把服务端拖住。
///
/// ★ 判据不能等 accept 循环（它永不返回），也不能等连接任务本身（拿不到句柄）。
/// 这里用**可观测的行为**：等第一个连接结束之后，服务端必须还能正常服务 ——
/// 一个把连接任务挂死、或者把连接额度漏掉的实现，会在这一步露馅。
///
/// 变异验证：把 `/switch` 里的写超时去掉、并让连接任务永久挂住（例如把
/// `broken` 分支的 `continue` 改成 `return` 之前忘了排空 channel）→ 判红。
#[tokio::test]
async fn client_that_never_reads_does_not_hold_the_connection_task() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;

    {
        let mut s = TcpStream::connect(addr).await.expect("连接 HTTP 服务");
        s.write_all(b"GET /switch HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        s.flush().await.unwrap();
        // ★ 刻意不读任何响应；连接 drop 后服务端的写会失败 → 走"调用方已走"路径。
    }

    // 服务端必须继续正常服务（这条也顺带证明连接额度被归还了）。
    let body = tokio::time::timeout(Duration::from_secs(20), http_get(addr, "/status"))
        .await
        .expect("第一个连接结束后，服务端没有在 20s 内继续响应新请求");
    assert!(body.contains("200 OK"), "{body}");
}

// ─────────────────── /health 要能反映"能不能干活" ───────────────────

/// AT 通道明确探活失败时，`/health` 必须报 degraded 并给 503。
///
/// 最常见的坏状态是"服务在跑、AT 后端挂了"：那时每次切换都会失败，
/// 但只看"进程活着"的 /health 依旧 200，调用方与人都是瞎的。
#[tokio::test]
async fn health_reports_degraded_when_at_channel_probe_failed() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let cfg = test_config(rpc.addr.port());

    let ok_addr = start_server_with_at_state(cfg.clone(), AtState::Ok).await;
    let ok_body = http_get(ok_addr, "/health").await;
    assert!(ok_body.contains("200 OK"), "{ok_body}");
    assert!(ok_body.contains("\"at_channel\":\"ok\""), "{ok_body}");

    let bad_addr = start_server_with_at_state(cfg, AtState::Failed).await;
    let bad_body = http_get(bad_addr, "/health").await;
    assert!(
        bad_body.contains("503"),
        "AT 通道探活失败时 /health 应给 503: {bad_body}"
    );
    assert!(bad_body.contains("degraded"), "{bad_body}");
    assert!(bad_body.contains("\"at_channel\":\"failed\""), "{bad_body}");
}

/// `/status` 也要带上 AT 通道状态（它是排查时的第一站）。
#[tokio::test]
async fn status_exposes_at_channel_state() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server_with_at_state(test_config(rpc.addr.port()), AtState::Failed).await;

    let before = rpc.commands().len();
    let body = http_get(addr, "/status").await;

    assert!(body.contains("\"at_channel\":\"failed\""), "{body}");
    // ★ 仍然只读：不许下发任何写命令
    let after = rpc.commands();
    let writes = after[before..]
        .iter()
        .filter(|c| c.starts_with("AT^SETAUTODIAL") && !c.ends_with('?'))
        .count();
    assert_eq!(writes, 0, "/status 不许改设备状态");
}

/// `/` 保持极简存活语义（兼容手敲 curl 的习惯），不因为 AT 挂了就报错。
#[tokio::test]
async fn root_stays_minimal_even_when_at_is_down() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server_with_at_state(test_config(rpc.addr.port()), AtState::Failed).await;
    let body = http_get(addr, "/").await;
    assert!(body.contains("200 OK"), "{body}");
    assert!(body.contains("ipswitchd ok"), "{body}");
}

/// 未设置探活结果（`Unknown`）时不报 degraded —— 后端可能稍后才就绪，
/// 启动探活失败并不阻止服务启动，所以"没探过"不等于"干不了活"。
#[tokio::test]
async fn health_unknown_state_is_not_degraded() {
    let (rpc, _) = spawn_automaton_rpc(true).await;
    let addr = start_server(test_config(rpc.addr.port())).await;
    let body = http_get(addr, "/health").await;
    assert!(body.contains("200 OK"), "{body}");
    assert!(body.contains("\"at_channel\":\"unknown\""), "{body}");
}
