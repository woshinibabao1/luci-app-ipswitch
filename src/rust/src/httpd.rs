//! 极简 HTTP/1.1 服务端（只用 tokio 手写，不引 Web 框架）。
//!
//! 只有三个 GET 路由：
//!
//! | 路由 | 作用 |
//! |---|---|
//! | `GET /switch` | 触发一次换 IP，**流式**返回进度，完成时输出标记 |
//! | `GET /status` | 只读探针，返回 JSON |
//! | `GET /`、`GET /health` | 存活检查 |
//!
//! ## 两个必须守住的点
//!
//! 1. **`/switch` 必须流式**：调用方（zgyd 的 `-ip-switch`）是「见到标记就收手」
//!    的语义，不是「读完整个响应」。所以用 chunked 逐条推，标记放在最后一块。
//! 2. **失败时绝不能输出标记**：标记是"切换成功"的唯一凭据。失败还输出标记，
//!    等于告诉调用方"放心继续跑"，而实际上 IP 根本没换 —— 那会把一次可诊断的
//!    失败变成一次静默的数据丢失。
//!
//! 写失败（调用方见到标记后主动断开）不视为异常：连接断了，但**切换动作继续跑完**，
//! 因为换 IP 是有副作用的操作，半途停下比跑完更糟。

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::switcher::Switcher;

/// 读请求头的时间上限：防住"连上就不发数据"的连接占着资源。
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// 请求行 / 单个 header 行的长度上限（与后端 RPC 的 8192 同量级）。
const MAX_LINE: usize = 8192;

/// 启动监听并进入 accept 循环。正常情况下永不返回。
pub async fn serve(cfg: Config, sw: Arc<Switcher>) -> io::Result<()> {
    let addr = format!("{}:{}", cfg.listen, cfg.port);
    let listener = TcpListener::bind(&addr).await?;
    eprintln!("[ipswitch] HTTP 已监听 http://{addr}/switch");
    accept_loop(listener, sw).await
}

/// 在**已经绑定好的** listener 上跑 accept 循环。
///
/// 拆出来是为了让测试能绑 `127.0.0.1:0` 拿系统分配的空闲端口，
/// 避免测试之间、以及测试与本机其它服务之间抢固定端口。
pub async fn accept_loop(listener: TcpListener, sw: Arc<Switcher>) -> io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // 单次 accept 失败（瞬时 fd 耗尽等）不该让整个服务退出
                eprintln!("[ipswitch] accept 失败: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let sw = sw.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, sw).await {
                eprintln!("[ipswitch] {peer} 连接异常: {e}");
            }
        });
    }
}

async fn handle(stream: TcpStream, sw: Arc<Switcher>) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let request_line = match read_line(&mut reader).await? {
        Some(l) if !l.trim().is_empty() => l,
        // 空连接（探活/端口扫描）：直接关掉，不作为错误
        _ => return Ok(()),
    };

    // 读完并丢弃请求头：我们不需要任何 header，但必须读完，
    // 否则残留字节会污染下一次（keep-alive）解析。
    loop {
        match read_line(&mut reader).await? {
            Some(l) if l.trim().is_empty() => break,
            Some(_) => continue,
            None => break,
        }
    }

    let Some((method, path)) = parse_request_line(&request_line) else {
        return respond_plain(&mut write_half, 400, "Bad Request").await;
    };

    if method != "GET" {
        return respond_plain(&mut write_half, 405, "只支持 GET").await;
    }

    match path.as_str() {
        "/switch" => handle_switch(&mut write_half, &sw).await,
        "/status" => handle_status(&mut write_half, &sw).await,
        "/" | "/health" => respond_plain(&mut write_half, 200, "ipswitchd ok").await,
        _ => respond_plain(&mut write_half, 404, "Not Found").await,
    }
}

/// `GET /switch`：触发切换并把进度流式推给调用方。
async fn handle_switch<W>(w: &mut W, sw: &Arc<Switcher>) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    // 先发响应头：chunked，让调用方可以边收边看
    // （`Connection: close` 是刻意的 —— 每个切换一个连接，语义最清晰）
    w.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/plain; charset=utf-8\r\n\
          Transfer-Encoding: chunked\r\n\
          Cache-Control: no-store\r\n\
          Connection: close\r\n\r\n",
    )
    .await?;
    w.flush().await?;

    // 切换在独立任务里跑，进度经 channel 汇到本协程写出。
    // 这样即使调用方断开，切换本身也不会被取消。
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let sw2 = sw.clone();
    let worker = tokio::spawn(async move { sw2.switch_ip(&tx).await });

    let mut broken = false;
    while let Some(msg) = rx.recv().await {
        if broken {
            continue; // 调用方已走：继续把 channel 排空，等 worker 收尾
        }
        if write_chunk(w, &fmt_progress(&msg)).await.is_err() {
            broken = true;
            eprintln!("[ipswitch] 调用方提前断开，切换仍会继续执行完");
        }
    }

    let outcome = match worker.await {
        Ok(r) => r,
        Err(e) => Err(format!("切换任务异常结束: {e}")),
    };

    // 收尾块：成功才带标记
    let tail = match &outcome {
        Ok(report) => format!(
            "{}\n切换成功：{} → {}（{}，用时 {:.1}s）{}\n",
            sw.config().marker,
            fmt_ip(report.before),
            fmt_ip(report.after),
            report.method.as_str(),
            report.elapsed.as_secs_f64(),
            if report.saw_link_down {
                ""
            } else {
                "；注意：未观察到链路中断"
            }
        ),
        // 失败**不带标记**：调用方会据此判为未完成并自行重试/上报
        Err(e) => format!("切换失败：{e}\n"),
    };

    if !broken {
        let _ = write_chunk(w, &tail).await;
    }
    let _ = w.write_all(b"0\r\n\r\n").await;
    let _ = w.flush().await;
    Ok(())
}

/// `GET /status`：只读，不改任何设备状态。
async fn handle_status<W>(w: &mut W, sw: &Arc<Switcher>) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let probe = sw.probe().await;
    let last = sw.last_report().await;
    let cfg = sw.config();

    let body = serde_json::json!({
        "wan_iface": cfg.wan_iface,
        "wan_ip": probe.wan_ip.map(|a| a.to_string()),
        "dial_active": probe.dial_active,        // null = 本次没读出来
        "dial_raw": probe.dial_raw,
        "method": cfg.effective_method().as_str(),
        "marker": cfg.marker,
        "apn_pool_size": cfg.apn_list.len(),
        "timeout_secs": cfg.timeout.as_secs(),
        "last_switch": last.as_ref().map(|r| serde_json::json!({
            "method": r.method.as_str(),
            "apn": r.apn,
            "before": r.before.map(|a| a.to_string()),
            "after": r.after.map(|a| a.to_string()),
            "saw_link_down": r.saw_link_down,
            "renewed_by": r.renewed_by,
            "elapsed_secs": r.elapsed.as_secs_f64(),
        })),
    })
    .to_string();

    respond_json(w, 200, &body).await
}

// ─────────────────────────── HTTP 原语 ───────────────────────────

/// 写一个 chunked 分块。
///
/// 长度必须按**字节**算 —— `String::len()` 正是字节数，含中文的进度文本
/// 用字符数当长度会让分块边界错位、调用方解析失败。
async fn write_chunk<W>(w: &mut W, data: &str) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }
    w.write_all(format!("{:x}\r\n", data.len()).as_bytes())
        .await?;
    w.write_all(data.as_bytes()).await?;
    w.write_all(b"\r\n").await?;
    w.flush().await
}

async fn respond_plain<W>(w: &mut W, code: u16, body: &str) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let head = format!(
        "HTTP/1.1 {code} {}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body.as_bytes()).await?;
    w.flush().await
}

async fn respond_json<W>(w: &mut W, code: u16, body: &str) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let head = format!(
        "HTTP/1.1 {code} {}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    w.write_all(head.as_bytes()).await?;
    w.write_all(body.as_bytes()).await?;
    w.flush().await
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Unknown",
    }
}

/// 读一行；返回 `None` 表示对端关闭。超长行按"读到上限即截断"处理。
async fn read_line<R>(reader: &mut R) -> io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut buf = Vec::new();
    let read = tokio::time::timeout(HEADER_TIMEOUT, reader.read_until(b'\n', &mut buf)).await;
    match read {
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "读请求超时")),
        Ok(Err(e)) => Err(e),
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(_)) => {
            if buf.len() > MAX_LINE {
                buf.truncate(MAX_LINE);
            }
            Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
        }
    }
}

/// 从请求行解析出方法与路径（丢弃 query）。
fn parse_request_line(line: &str) -> Option<(String, String)> {
    let mut it = line.split_whitespace();
    let method = it.next()?.to_string();
    let target = it.next()?;
    let path = target.split('?').next().unwrap_or("").to_string();
    Some((method, path))
}

/// 进度文本按行加前缀，便于调用方直接落日志。
fn fmt_progress(msg: &str) -> String {
    format!("{msg}\n")
}

fn fmt_ip(ip: Option<std::net::Ipv4Addr>) -> String {
    ip.map(|a| a.to_string()).unwrap_or_else(|| "无地址".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line_and_drops_query() {
        assert_eq!(
            parse_request_line("GET /switch?x=1 HTTP/1.1\r\n"),
            Some(("GET".into(), "/switch".into()))
        );
        assert_eq!(
            parse_request_line("GET / HTTP/1.0"),
            Some(("GET".into(), "/".into()))
        );
    }

    #[test]
    fn rejects_malformed_request_line() {
        assert_eq!(parse_request_line("GET"), None);
        assert_eq!(parse_request_line(""), None);
    }

    #[tokio::test]
    async fn chunk_length_is_byte_count_not_char_count() {
        let mut out: Vec<u8> = Vec::new();
        // 中文按 UTF-8 是 3 字节：长度必须是字节数，否则调用方会解错帧
        write_chunk(&mut out, "中文").await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "6\r\n中文\r\n", "实际输出: {text:?}");
    }

    #[tokio::test]
    async fn empty_chunk_is_skipped() {
        let mut out: Vec<u8> = Vec::new();
        write_chunk(&mut out, "").await.unwrap();
        assert!(out.is_empty(), "空块不应产生任何字节");
    }

    #[tokio::test]
    async fn plain_response_has_correct_content_length() {
        let mut out: Vec<u8> = Vec::new();
        respond_plain(&mut out, 404, "Not Found").await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        // "Not Found" 是 9 字节
        assert!(text.contains("Content-Length: 9\r\n"), "实际: {text}");
        assert!(text.ends_with("\r\n\r\nNot Found"));
    }

    #[test]
    fn reason_maps_known_codes() {
        assert_eq!(reason(200), "OK");
        assert_eq!(reason(405), "Method Not Allowed");
        assert_eq!(reason(999), "Unknown");
    }
}
