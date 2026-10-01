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
//!
//! ## 三处防"被客户端拖住"的边界（都是资源上限，不动切换语义）
//!
//! 路由器内存只有百来 MB，而这个端口默认对 `0.0.0.0` 开放、且没有鉴权，
//! 所以每个连接都必须有明确的资源上限：
//!
//! | 边界 | 防的是什么 |
//! |---|---|
//! | `MAX_LINE` 在**读取期**生效 | 不发换行、一直灌字节的连接让行缓冲无界增长 |
//! | `WRITE_TIMEOUT` | 连上但不读响应的连接把本次连接的任务永久挂住 |
//! | `MAX_CONNECTIONS` | 大量并发连接（每个都是一个 task + 一份读缓冲） |
//!
//! 三条都只影响"这一条连接怎么收场"：切换跑在独立任务里，客户端怎么闹都不改变设备侧动作。

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Semaphore};

use crate::config::Config;
use crate::switcher::{AtState, SwitchReport, Switcher};

/// **单行**请求头的读取上限：防住"连上就不发数据"的连接占着资源。
///
/// ★ 它只约束一行。整体还必须有 [`HEAD_TOTAL_TIMEOUT`] 与 [`MAX_HEADER_LINES`]，
/// 否则"每 9 秒发一行、永不发结束空行"能把连接永久占住（见 `handle`）。
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// 读**整个请求头**的总时长上限（行数 × 单行上限之外的兜底）。
const HEAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// 请求头最多允许多少行（我们一个 header 都不需要，纯防御）。
const MAX_HEADER_LINES: usize = 64;

/// 请求行 / 单个 header 行的长度上限（与后端 RPC 的 8192 同量级）。
pub(crate) const MAX_LINE: usize = 8192;

/// 往一个连接里写数据的时间上限。
///
/// 客户端连上却不读响应时，socket 发送缓冲写满后 `write_all` 会一直挂着 ——
/// 没有这个上限，那条连接的任务就永久留在那里。超时后按"调用方已走"处理，
/// 与客户端主动断开走同一条路径（丢弃进度、切换照跑）。
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// 同时在处理的连接数上限。
///
/// 每个连接占一个 task 与一份读缓冲。这个端口没有鉴权，必须假设对面可能是
/// 扫描器或者配错了的客户端；超出上限就回一条 503 关掉，而不是继续吃内存。
const MAX_CONNECTIONS: usize = 32;

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
    // 连接数上限：用信号量而不是计数器，是为了"连接一结束就自动归还" ——
    // 手写计数器在每条 return 路径上都得记得减，漏一处就是永久泄漏。
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
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

        let Ok(permit) = permits.clone().try_acquire_owned() else {
            // 直接说清楚"是被上限挡住的"，而不是让对面干等 ——
            // 干等会让调用方以为服务卡了，503 是可诊断的。
            eprintln!("[ipswitch] {peer} 连接数已达上限 {MAX_CONNECTIONS}，回 503 并关闭");
            tokio::spawn(async move {
                let (_read_half, mut write_half) = stream.into_split();
                let _ = respond_plain(&mut write_half, 503, "连接数已达上限，请稍后重试").await;
            });
            continue;
        };

        let sw = sw.clone();
        tokio::spawn(async move {
            let _permit = permit; // 连接结束（含 panic 展开）时自动归还
            if let Err(e) = handle(stream, sw).await {
                eprintln!("[ipswitch] {peer} 连接异常: {e}");
            }
        });
    }
}

async fn handle(stream: TcpStream, sw: Arc<Switcher>) -> io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // ★ 读请求头也必须有**总时长**预算。
    //
    // `read_line` 的单行超时挡不住"每 9 秒发一行、永不发结束空行"——那样一条连接
    // 会被永久占住；占满 MAX_CONNECTIONS 之后所有新连接（含合法的 /switch）
    // 都会拿到 503。没有鉴权的端口上这是一条现实的 DoS 路径。
    let head_deadline = Instant::now() + HEAD_TOTAL_TIMEOUT;

    // 统一的读头封装：总时长用尽 → 回 408 而不是把它当"连接异常"上报。
    macro_rules! head_line {
        () => {
            match read_line(&mut reader, head_deadline).await {
                Ok(v) => v,
                Err(e) if e.get_ref().is_some_and(|r| r.is::<HeadDeadlineExceeded>()) => {
                    return respond_plain(&mut write_half, 408, "请求头读取超时").await;
                }
                Err(e) => return Err(e),
            }
        };
    }

    let request_line = match head_line!() {
        Some(l) if !l.trim().is_empty() => l,
        // 空连接（探活/端口扫描）：直接关掉，不作为错误
        _ => return Ok(()),
    };

    // 读完并丢弃请求头：我们不需要任何 header，但必须读完（避免把请求体的字节
    // 当成下一行）。本服务每个响应都 Connection: close，没有 keep-alive 复用。
    let mut head_done = false;
    for _ in 0..MAX_HEADER_LINES {
        match head_line!() {
            Some(l) if l.trim().is_empty() => {
                head_done = true;
                break;
            }
            Some(_) => continue,
            // ★ 对端在读头期间就关了：这是**不完整的请求**，不能当成"头读完了"
            //   去执行换 IP —— 一个 TCP 半途断开的连接不该有副作用。
            None => return Ok(()),
        }
    }
    if !head_done {
        // 行数超限：明确拒绝，别让它继续占着连接额度。
        return respond_plain(&mut write_half, 431, "请求头行数过多").await;
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
        // `/` 保持极简存活语义（也兼容用 curl 手敲的习惯）；`/health` 多说一句
        // "AT 通道通不通"。
        "/" => respond_plain(&mut write_half, 200, "ipswitchd ok").await,
        "/health" => handle_health(&mut write_half, &sw).await,
        _ => respond_plain(&mut write_half, 404, "Not Found").await,
    }
}

/// `GET /health`：存活 + AT 通道可达性。
///
/// ★ 为什么不是无脑 200：最常见的坏状态是"服务在跑、AT 后端挂了" ——
/// 那时 `/health` 200、而每次切换都失败，调用方与人都看不出来。
/// 所以 AT 通道**明确探活失败**时返回 503。`Unknown`（还没探过）不算失败：
/// 后端可能稍后才就绪，启动探活失败并不阻止本服务启动（见 `main.rs`）。
///
/// ★ 兼容性：响应体仍以既有契约里的 `ipswitchd ok` 开头 —— 历史上调用方是拿
/// `grep "ipswitchd ok"` 判活的（`docs/api.md` 记的就是这个契约）。改成纯 JSON
/// 会让既有脚本静默失效，所以保留前缀、只在后面补一行 JSON 摘要。
///
/// 这一条**不碰设备**（只读一个内存里的原子量），可以高频调用。
async fn handle_health<W>(w: &mut W, sw: &Arc<Switcher>) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let at = sw.at_state();
    let body = format!(
        "ipswitchd ok\n{}",
        serde_json::json!({
            "status": if at == AtState::Failed { "degraded" } else { "ok" },
            "at_channel": at.as_str(),
        })
    );
    let code = if at == AtState::Failed { 503 } else { 200 };
    respond_plain(w, code, &body).await
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
        // ★ 写也带超时：客户端连上但不读，socket 缓冲写满后 `write_all` 会一直挂着，
        // 没有这一层那条连接就永久留在任务里。超时按"调用方已走"处理 ——
        // 与它主动断开同一条路径（丢弃进度、切换照跑）。
        match tokio::time::timeout(WRITE_TIMEOUT, write_chunk(w, &fmt_progress(&msg))).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                broken = true;
                eprintln!("[ipswitch] 调用方提前断开（{e}），切换仍会继续执行完");
            }
            Err(_) => {
                broken = true;
                eprintln!(
                    "[ipswitch] 调用方 {WRITE_TIMEOUT:?} 内没有读走进度，按已断开处理，切换仍会继续执行完"
                );
            }
        }
    }

    let outcome = match worker.await {
        Ok(r) => r,
        Err(e) => Err(format!("切换任务异常结束: {e}")),
    };

    // 收尾块：成功才带标记；提示语由 `switch_notes` 拼（纯函数，可单测）。
    //
    // 这两条提示只加在文本里，**不改成功/失败的判定** —— 标记仍然是"设备侧已就绪"
    // 的唯一凭据，调用方按老规矩解析即可。
    let tail = match &outcome {
        Ok(report) => format!(
            "{}\n切换成功：{} → {}（{}，用时 {:.1}s）{}\n",
            sw.config().marker,
            fmt_ip(report.before),
            fmt_ip(report.after),
            report.method.as_str(),
            report.elapsed.as_secs_f64(),
            switch_notes(report)
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
    let attempt = sw.last_attempt().await;
    let cfg = sw.config();

    let body = serde_json::json!({
        "wan_iface": cfg.wan_iface,
        "wan_ip": probe.wan_ip.map(|a| a.to_string()),
        "dial_active": probe.dial_active,        // null = 本次没读出来
        "dial_raw": probe.dial_raw,
        "at_channel": sw.at_state().as_str(),    // 启动探活结果：ok/failed/unknown
        "method": cfg.effective_method().as_str(),
        "marker": cfg.marker,
        "apn_pool_size": cfg.apn_list.len(),
        "timeout_secs": cfg.timeout.as_secs(),
        // ★ 最近一次**尝试**（成功失败都记）。失败时 last_switch 会停留在上一次成功，
        //   只看它会把"一直失败"误读成"一直成功" —— 这两个字段要一起看。
        "last_attempt": attempt.as_ref().map(|a| serde_json::json!({
            "at_unix": a.at_unix,
            "ok": a.ok,
            "err": a.err,
            "note": a.note,
        })),
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
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// 读一行；返回 `None` 表示对端关闭。
///
/// ★ 上限必须在**读取期**生效，不能在读完之后截断。
///
/// `read_until` 会一直往 `buf` 里追加，直到遇到 `\n`；`HEADER_TIMEOUT` 约束的是
/// **整次调用**的耗时，不是字节数。所以"客户端每 9 秒发 1 字节、始终不发换行"
/// 可以让 `buf` 一直长下去 —— 读完之后再 `truncate` 已经晚了，内存已经吃掉了。
/// 这里用 `take(MAX_LINE)` 把可读字节数本身就卡死：到上限后 `read_until` 看到
/// EOF 返回，下面的 `n == 0` 分支会把它当"对端给了个不完整的行"关掉连接。
///
/// 判红用例见 `e2e_tests::read_line_is_bounded_by_max_line`：它直接把一个
/// **永不结束**的输入喂进来，量"到底读掉了多少字节"——这是唯一能直接观测
/// 上限的位置（在 socket 上量会被内核缓冲掩盖）。
pub(crate) async fn read_line<R>(reader: &mut R, deadline: Instant) -> io::Result<Option<String>>
where
    R: AsyncBufReadExt + Unpin,
{
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(deadline_exceeded());
    }
    // 本行只允许用 HEADER_TIMEOUT，但同时不超过总预算。
    let cap = left.min(HEADER_TIMEOUT);
    let mut buf = Vec::new();
    let mut limited = reader.take(MAX_LINE as u64); // 读取期硬上限
    let read = tokio::time::timeout(cap, limited.read_until(b'\n', &mut buf)).await;
    match read {
        Err(_) => {
            if Instant::now() >= deadline {
                Err(deadline_exceeded())
            } else {
                Err(io::Error::new(io::ErrorKind::TimedOut, "读请求超时"))
            }
        }
        Ok(Err(e)) => Err(e),
        // 0 字节：对端关闭，或已经读到 MAX_LINE 上限（行超长/不发换行）
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(_)) => Ok(Some(String::from_utf8_lossy(&buf).into_owned())),
    }
}

/// 读请求头超过**总时长**预算。
///
/// 用一个自定义错误类型而不是 `io::ErrorKind::TimedOut`：调用方要能把它与
/// "真的读出错"区分开 —— 前者回 408 就收工（客户端自己磨蹭导致的），
/// 后者才算连接异常、值得打日志。
#[derive(Debug)]
pub(crate) struct HeadDeadlineExceeded;

impl std::fmt::Display for HeadDeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "读请求头超时（总时长 {}s 用尽）",
            HEAD_TOTAL_TIMEOUT.as_secs()
        )
    }
}

impl std::error::Error for HeadDeadlineExceeded {}

fn deadline_exceeded() -> io::Error {
    io::Error::new(io::ErrorKind::Other, HeadDeadlineExceeded)
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

/// 成功响应尾部的提示语。
///
/// 抽成纯函数是为了能单测 —— 这段逻辑出错的代价是"给出误导性的成功信息"。
///
/// 两条提示的含义：
/// - **未观察到链路中断**：`false` 不足以判失败（模组可能断得很快、没采到），
///   但它是重要事实，不该被一句"切换完成"盖过去。
/// - **出口地址与切换前相同**：对"每 N 个请求换一次 IP 绕配额"这个用途是致命的 ——
///   后面几十个请求会继续从同一个 IP 发出去。★ 两个地址**都读到了**才比：
///   都读不到（例如接口尚未拿到地址）时不做结论，否则每次都会误报。
fn switch_notes(report: &SwitchReport) -> String {
    let mut notes = String::new();
    if !report.saw_link_down {
        notes.push_str("；注意：未观察到链路中断");
    }
    if let (Some(before), Some(after)) = (report.before, report.after) {
        if before == after {
            notes.push_str("；注意：出口地址与切换前相同，本次可能没有真正换到新 IP");
        }
    }
    notes
}

fn fmt_ip(ip: Option<std::net::Ipv4Addr>) -> String {
    ip.map(|a| a.to_string()).unwrap_or_else(|| "无地址".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个只关心提示语的报告。
    fn report(
        before: Option<&str>,
        after: Option<&str>,
        saw_link_down: bool,
    ) -> crate::switcher::SwitchReport {
        crate::switcher::SwitchReport {
            method: crate::config::Method::Redial,
            apn: None,
            before: before.map(|s| s.parse().unwrap()),
            after: after.map(|s| s.parse().unwrap()),
            saw_link_down,
            renewed_by: None,
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn notes_empty_on_a_clean_switch() {
        assert_eq!(
            switch_notes(&report(Some("1.1.1.1"), Some("2.2.2.2"), true)),
            ""
        );
    }

    #[test]
    fn notes_warn_when_link_down_was_not_observed() {
        let n = switch_notes(&report(Some("1.1.1.1"), Some("2.2.2.2"), false));
        assert!(n.contains("未观察到链路中断"), "{n}");
    }

    #[test]
    fn notes_warn_when_ip_did_not_change() {
        let n = switch_notes(&report(Some("1.1.1.1"), Some("1.1.1.1"), true));
        assert!(n.contains("出口地址与切换前相同"), "{n}");
    }

    #[test]
    fn notes_stay_silent_when_addresses_are_unknown() {
        // ★ 都读不到时不做结论：否则每次切换都会误报"没换到新 IP"
        //   （真机上"接口还没拿到地址"是完全正常的一步）。
        assert_eq!(switch_notes(&report(None, None, true)), "");
        assert_eq!(switch_notes(&report(Some("1.1.1.1"), None, true)), "");
        assert_eq!(switch_notes(&report(None, Some("2.2.2.2"), true)), "");
    }

    #[test]
    fn notes_can_carry_both_warnings() {
        let n = switch_notes(&report(Some("1.1.1.1"), Some("1.1.1.1"), false));
        assert!(n.contains("未观察到链路中断"), "{n}");
        assert!(n.contains("出口地址与切换前相同"), "{n}");
    }

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
