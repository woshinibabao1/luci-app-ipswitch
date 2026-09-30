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
use crate::switcher::Switcher;

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

    fn count_of(&self, prefix: &str) -> usize {
        self.commands()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
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
    let at = AtClient::new("127.0.0.1", cfg.rpc_port, "");
    let sw = Switcher::new(cfg, at);
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
