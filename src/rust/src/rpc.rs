//! 复用 MT5700 Console 后端（`at-webserver-rust`）的 AT 通道。
//!
//! ★ 为什么不自己打开 `/dev/ttyUSB1`：
//!   串口同一时刻只能有一个持有者。后端不仅独占串口，还负责 URC 分发、
//!   命令排队以及短信/扫频等长任务的互斥。再起一个进程去抢，会把既有功能
//!   一起弄坏 —— 真机上的表现是 LuCI 整页卡住、短信发不出去，而根因在
//!   另一个进程里，极难定位。因此本服务只做 `127.0.0.1:8765` 的
//!   newline-JSON 客户端。
//!
//! 协议（对应 MT5700 Console `src/rust/src/rpcserver.rs`）：
//! ```text
//! 请求一行 JSON：
//!   {"id":N,"method":"at","params":{"cmd":"AT+CSQ","auth_key":"..."}}
//! 应答一行 JSON：
//!   {"id":N,"result":{"success":true,"data":"...","error":null}}
//! ```
//! 约束：行长上限 8192 字节；服务端在串口重连时**会主动断开**所有连接。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// 读一遍应答的时间上限。
///
/// 服务端单条 AT 的最坏耗时 ≈ `QUEUE_WAIT_TIMEOUT(8s) + COMMAND_TIMEOUT(2s) + 3s`
/// ≈ 13s（见 `rpcserver::run_command` 的超时叠加），这里留约一倍余量。
const RPC_TIMEOUT: Duration = Duration::from_secs(25);

/// 一条 AT 应答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtReply {
    pub success: bool,
    pub data: String,
    pub error: String,
}

impl AtReply {
    /// 展示用文本：成功看 `data`，失败看 `error`。
    pub fn text(&self) -> &str {
        if self.data.is_empty() {
            &self.error
        } else {
            &self.data
        }
    }
}

/// AT 通道客户端。
///
/// 刻意**不做连接池**：一次切换只发几条命令，回环建连不到 1ms；而服务端会在
/// 串口重连时主动断开连接，复用连接反而要写一整套失效检测与重连逻辑。
/// 「连接生命周期 = 一次调用」让出错范围天然收敛到单条命令上。
#[derive(Clone)]
pub struct AtClient {
    host: String,
    port: u16,
    auth_key: String,
    seq: Arc<AtomicU64>,
}

impl AtClient {
    pub fn new(host: impl Into<String>, port: u16, auth_key: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port,
            auth_key: auth_key.into(),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 发一条 AT 命令并读回应答。
    ///
    /// 返回 `Err` 表示**这次通信本身失败**（连不上/超时/应答不是合法 JSON），
    /// 而**不是**"模组回了 ERROR" —— 后者是 `Ok(AtReply { success: false, .. })`。
    /// 两者必须分开：前者要重试或报基础设施故障，后者是业务结果。
    pub async fn send(&self, cmd: &str) -> Result<AtReply, String> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let payload = json!({
            "id": id,
            "method": "at",
            "params": { "cmd": cmd, "auth_key": self.auth_key },
        });
        let mut line =
            serde_json::to_string(&payload).map_err(|e| format!("AT 请求编码失败: {e}"))?;
        line.push('\n');

        let io = async {
            let stream = TcpStream::connect((self.host.as_str(), self.port))
                .await
                .map_err(|e| {
                    format!(
                        "连接 AT 通道 {}:{} 失败: {e}（MT5700 Console 后端是否在运行？）",
                        self.host, self.port
                    )
                })?;
            let (read_half, mut write_half) = stream.into_split();

            write_half
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("发送 AT 请求失败: {e}"))?;
            write_half
                .flush()
                .await
                .map_err(|e| format!("刷新 AT 请求失败: {e}"))?;

            let mut reader = BufReader::new(read_half);
            let mut buf = String::new();
            let n = reader
                .read_line(&mut buf)
                .await
                .map_err(|e| format!("读取 AT 应答失败: {e}"))?;
            if n == 0 {
                return Err("AT 通道未返回应答就关闭了连接（后端可能在重连串口）".to_string());
            }
            Ok(buf)
        };

        let raw = tokio::time::timeout(RPC_TIMEOUT, io)
            .await
            .map_err(|_| format!("AT 通道无应答（超过 {}s）", RPC_TIMEOUT.as_secs()))??;

        parse_reply(raw.trim())
    }
}

/// 解析一行应答 JSON。
///
/// 拆出来是为了可单测：这里处理的是"对面给的东西不符合预期"的全部情况，
/// 是整条链路上最容易被真实环境打脸的地方。
fn parse_reply(line: &str) -> Result<AtReply, String> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| format!("AT 应答不是合法 JSON: {e}（原文: {}）", truncate(line, 160)))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("AT 通道返回错误: {msg}"));
    }

    let result = v
        .get("result")
        .ok_or_else(|| format!("AT 应答缺少 result 字段: {}", truncate(line, 160)))?;

    Ok(AtReply {
        success: result
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // ★ data / error 在 JSON 里可能是 null：`.as_str()` 对 null 返回 None，
        //   正好落到空串，不需要额外判空。
        data: result
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        error: result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_reply() {
        let r = parse_reply(r#"{"id":1,"result":{"success":true,"data":"OK","error":null}}"#)
            .expect("应解析成功");
        assert!(r.success);
        assert_eq!(r.data, "OK");
        assert_eq!(r.error, "");
        assert_eq!(r.text(), "OK");
    }

    #[test]
    fn parses_modem_error_reply_as_ok_with_success_false() {
        // 模组回 ERROR 属于业务结果，不是通信失败 —— 必须区分开
        let r = parse_reply(r#"{"id":2,"result":{"success":false,"data":null,"error":"ERROR"}}"#)
            .expect("应解析成功");
        assert!(!r.success);
        assert_eq!(r.data, "");
        assert_eq!(r.error, "ERROR");
        assert_eq!(r.text(), "ERROR", "data 为空时应回落到 error");
    }

    #[test]
    fn rpc_level_error_becomes_err() {
        let e = parse_reply(r#"{"id":3,"error":{"code":-32001,"message":"认证失败"}}"#)
            .expect_err("RPC 层错误应视为通信失败");
        assert!(e.contains("认证失败"), "错误信息应带上原文: {e}");
    }

    #[test]
    fn malformed_json_is_reported_with_excerpt() {
        let e = parse_reply("not json at all").expect_err("非法 JSON 应报错");
        assert!(e.contains("不是合法 JSON"));
    }

    #[test]
    fn missing_result_field_is_reported() {
        let e = parse_reply(r#"{"id":4}"#).expect_err("缺 result 应报错");
        assert!(e.contains("缺少 result"));
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(truncate("abc", 10), "abc");
        assert_eq!(truncate("abcdefghij", 3), "abc…");
    }
}
