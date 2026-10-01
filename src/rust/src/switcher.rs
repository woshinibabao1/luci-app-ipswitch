//! 换 IP 的编排：断开 → 重新拨号（或切 APN）→ 等设备侧就绪 → 通知 netifd 续约。
//!
//! ## 为什么判据是 `AT^NDISSTATQRY?` 而不是只看接口地址
//!
//! 路由器侧 `eth2` 的地址来自模组内部 DHCP，租约可以很长（真机实测 6 天）。
//! PDP 断掉后 netifd 并不会立刻发现 —— `99-mt5700-renew` 的注释里记着这个坑：
//! 「netifd 只看到 link up，认为接口正常，不会重跑 DHCP」。
//! 所以**「接口上有地址」不能证明拨号是好的**。真正的设备侧状态要看模组自己：
//! `AT^NDISSTATQRY?` 回 `^NDISSTATQRY: 1,...` 才算已连接
//! （MT5700 Console 前端 `dial.js` 用的也是这一条判据）。
//! 本模块两个条件都要求：模组说已连接，**且**接口确实拿到了地址。
//!
//! ## 一次切换的命令序列
//!
//! ```text
//! AT^SETAUTODIAL=0                                   ← 断开
//! AT^SETAUTODIAL=1,<mode>[,"<proto>","<apn>",...]    ← 重新拨号 / 切 APN
//! ubus -S call network.interface.<iface> renew       ← 让 netifd 重新取地址
//! ```

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex};

use crate::config::{Config, Method};
use crate::netif;
use crate::rpc::{AtClient, AtReply};

/// AT 通道可达性：`Unknown` = 还没探过。
///
/// ★ 为什么需要它：`/health` 只说 "ipswitchd ok" 时，**进程活着**与
/// **能真的换 IP** 是两回事。最常见的坏状态恰恰是"服务在跑、但 AT 后端挂了" ——
/// 那时候 `/health` 依旧 200，调用方却每次切换都失败。把这个状态暴露出去，
/// 让"服务活着但干不了活"可被一条探针看出来。
///
/// 用 `AtomicU8` 而不是 `Mutex<bool>`：它是一个只在启动时写一次的三态，
/// 而读它的 `/health` 要能被高频调用、不该有任何阻塞。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtState {
    Unknown,
    Ok,
    Failed,
}

impl AtState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => AtState::Ok,
            2 => AtState::Failed,
            _ => AtState::Unknown,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            AtState::Unknown => 0,
            AtState::Ok => 1,
            AtState::Failed => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AtState::Unknown => "unknown",
            AtState::Ok => "ok",
            AtState::Failed => "failed",
        }
    }
}

/// 轮询间隔。700ms 是"足够快"与"别把 AT 通道占满"之间的折中
/// —— MT5700 Console 的硬性要求是稳态 AT 请求 ≤ 2 次/秒。
const POLL_INTERVAL: Duration = Duration::from_millis(700);

/// 等"连接断开"的预算。断开是个快速动作，给太久只会拖长整次切换。
const LINK_DOWN_BUDGET: Duration = Duration::from_secs(8);

/// 重新拨号后等设备侧就绪的预算上限。
const DIAL_UP_BUDGET: Duration = Duration::from_secs(30);

/// renew 之后等地址稳定的预算。
const SETTLE_BUDGET: Duration = Duration::from_secs(6);

/// AT 通道连续失败多少次就认定是通道本身坏了。
///
/// 不能因为一次读失败就下结论：串口上偶发一帧丢失是常态，
/// 但如果连续多次连不上，那就是后端进程/串口的问题，必须报出来
/// —— 而不是继续"再等等"，让调用方以为只是拨号慢。
const AT_ERROR_TOLERANCE: u32 = 3;

/// 一次切换的结果。用于日志与 `/status`。
#[derive(Debug, Clone)]
pub struct SwitchReport {
    pub method: Method,
    /// 本次实际切到的 APN（`redial` 策略下为 `None`）。
    pub apn: Option<String>,
    pub before: Option<Ipv4Addr>,
    pub after: Option<Ipv4Addr>,
    /// 是否**观察到**连接真的断开过。
    ///
    /// `false` 不足以判失败（模组可能断开得很快、采样没赶上），但它是一个
    /// 重要事实：调用方与被调用方都该看到，而不是被一句"切换完成"盖过去。
    pub saw_link_down: bool,
    /// 实际生效的续约方式（`ubus renew` / `ifdown/ifup`）。
    pub renewed_by: Option<String>,
    pub elapsed: Duration,
}

/// `/status` 用的只读探针结果。
#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub wan_ip: Option<Ipv4Addr>,
    /// `None` = 这一次没读出来（**不等于**"没连上"）。
    pub dial_active: Option<bool>,
    pub dial_raw: String,
}

/// `AT^SETAUTODIAL?` 解析出来的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutodialCfg {
    pub enable: u8,
    pub mode: u8,
    pub protocol: String,
    pub apn: String,
    pub username: String,
    pub password: String,
    pub auth_type: u8,
}

/// 换 IP 的执行体。
pub struct Switcher {
    cfg: Config,
    at: AtClient,
    /// 同一时刻只允许一次切换。
    ///
    /// 这是**必须的**，不是优化：并发切换会让两条命令序列在唯一的串口上交错，
    /// 模组的拨号状态会卡在中间态，而日志里两条流水各自看起来都正常 ——
    /// 那是最难查的一类故障。锁在这里，让后来的请求排队而不是并行。
    lock: Mutex<()>,
    /// APN 池轮换游标。
    apn_cursor: AtomicUsize,
    /// 最近一次切换结果，供 `/status` 查阅。
    last: Mutex<Option<SwitchReport>>,
    /// AT 通道可达性（启动探活结果），供 `/health` 与 `/status` 查阅。
    at_state: AtomicU8,
}

impl Switcher {
    pub fn new(cfg: Config, at: AtClient) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            at,
            lock: Mutex::new(()),
            apn_cursor: AtomicUsize::new(0),
            last: Mutex::new(None),
            at_state: AtomicU8::new(AtState::Unknown.as_u8()),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// 记录一次 AT 通道探活结果（启动时调用一次）。
    pub fn set_at_state(&self, st: AtState) {
        self.at_state.store(st.as_u8(), Ordering::Relaxed);
    }

    /// 读取 AT 通道探活结果。
    pub fn at_state(&self) -> AtState {
        AtState::from_u8(self.at_state.load(Ordering::Relaxed))
    }

    pub async fn last_report(&self) -> Option<SwitchReport> {
        self.last.lock().await.clone()
    }

    /// 只读探针：不改任何设备状态，供 `/status` 使用。
    pub async fn probe(&self) -> ProbeReport {
        let (dial_active, dial_raw) = match self.at.send("AT^NDISSTATQRY?").await {
            Ok(r) => (parse_ndis_active(r.text()), first_line(r.text())),
            Err(e) => (None, format!("(读取失败: {e})")),
        };
        ProbeReport {
            wan_ip: netif::ipv4_of(&self.cfg.wan_iface),
            dial_active,
            dial_raw,
        }
    }

    /// 执行一次切换。进度逐条写入 `tx`（调用方负责把它们流式发出去）。
    ///
    /// 返回 `Err` 表示**这次切换没有达成目标**，调用方应当重试或上报。
    pub async fn switch_ip(
        &self,
        tx: &mpsc::UnboundedSender<String>,
    ) -> Result<SwitchReport, String> {
        let started = Instant::now();
        // 排队串行化（见字段注释）。
        let _guard = self.lock.lock().await;

        let deadline = started + self.cfg.timeout;
        let iface = self.cfg.wan_iface.as_str();
        let method = self.cfg.effective_method();

        emit(tx, format!("开始切换出口 IP（策略 {}）", method.as_str()));

        let before = netif::ipv4_of(iface);
        emit(tx, format!("切换前 {iface} 地址: {}", fmt_ip(before)));

        // 记录当前 APN：切换之后要回读比对，确认没有被清掉。
        let apn_before = self.read_autodial().await;

        // ── ① 断开 ───────────────────────────────────────────────
        self.run_at(tx, "AT^SETAUTODIAL=0").await?;

        // ── ② 等断开（等不到也继续，但如实记录）─────────────────
        let budget = remaining(deadline, LINK_DOWN_BUDGET);
        let saw_link_down = !self
            .wait_dial(Some(false), budget, tx, "等待连接断开")
            .await?;
        if !saw_link_down {
            emit(
                tx,
                "  未观察到连接断开（可能断开很快未采到，也可能本就未建立）",
            );
        }

        // ── ③ 重新拨号 / 切 APN ─────────────────────────────────
        let apn = self.next_apn(method);
        let cmd = self.dial_command(&apn, apn_before.as_ref());
        if let Some(a) = &apn {
            emit(tx, format!("切换到 APN「{a}」并重新拨号"));
        } else {
            emit(tx, "重新拨号（APN 保持原配置）");
        }
        self.run_at(tx, &cmd).await?;

        // ── ④ 等设备侧就绪 ──────────────────────────────────────
        let budget = remaining(deadline, DIAL_UP_BUDGET);
        if !self
            .wait_dial(Some(true), budget, tx, "等待拨号完成")
            .await?
        {
            return Err(format!(
                "拨号未在 {}s 内恢复。可到 MT5700 Console「运行日志 → 模组拨号」看模组侧原因。",
                budget.as_secs()
            ));
        }

        // ── ⑤ 通知 netifd 重新取地址 ────────────────────────────
        let renewed_by = match netif::renew(iface).await {
            Ok(how) => {
                emit(tx, format!("  接口续约方式: {how}"));
                Some(how.to_string())
            }
            Err(e) => {
                // 续约失败不等于切换失败：模组侧确实已经重拨了，
                // 只是路由器还没把新地址接过来。如实说明，让调用方自己决定。
                emit(tx, format!("  接口续约未成功: {e}"));
                None
            }
        };

        // ── ⑥ 最终确认：模组已连接 + 接口有地址 ─────────────────
        let settle = remaining(deadline, SETTLE_BUDGET);
        let after = self.confirm_ready(settle, tx).await?;

        // ── ⑦ 回读 APN，确认没被清掉 ────────────────────────────
        if let (Some(old), Some(now)) = (apn_before.as_ref(), self.read_autodial().await) {
            if !old.apn.is_empty() && now.apn != old.apn {
                return Err(format!(
                    "APN 在切换过程中发生变化（{} → {}），已中止以免在错误的 APN 上继续。\
                     请在 MT5700 Console 的「自动拨号与 APN」里核对。",
                    old.apn, now.apn
                ));
            }
        }

        let report = SwitchReport {
            method,
            apn,
            before,
            after,
            saw_link_down,
            renewed_by,
            elapsed: started.elapsed(),
        };
        *self.last.lock().await = Some(report.clone());

        emit(
            tx,
            format!(
                "切换结束：{} → {}，用时 {:.1}s",
                fmt_ip(report.before),
                fmt_ip(report.after),
                report.elapsed.as_secs_f64()
            ),
        );
        Ok(report)
    }

    /// 取下一个要切的 APN（`redial` 策略返回 `None`）。
    fn next_apn(&self, method: Method) -> Option<String> {
        if method != Method::Apn || self.cfg.apn_list.is_empty() {
            return None;
        }
        let idx = self.apn_cursor.fetch_add(1, Ordering::Relaxed);
        Some(self.cfg.apn_list[idx % self.cfg.apn_list.len()].clone())
    }

    /// 组装重新拨号的命令。
    ///
    /// ★ `redial` 策略下**优先把读到的 APN 原样写回**，而不是用
    /// `AT^SETAUTODIAL=1,<mode>` 这种省略形态。原因：省略 APN 时模组是否
    /// 保留原配置，手册没有明确承诺；而 APN 一旦被清空，模组会因空 APN
    /// 被网络拒绝、进入退避窗口 —— 那是**直接断网**，代价远大于多写几十字节。
    /// 只有当读不到原配置（解析失败）时才退回省略形态。
    fn dial_command(&self, apn: &Option<String>, current: Option<&AutodialCfg>) -> String {
        let mode = self.cfg.dial_mode;
        if let Some(a) = apn {
            // 切 APN：协议用配置值，账号密码沿用当前配置（本插件不改它们）
            let (user, pass, auth) = match current {
                Some(c) => (c.username.as_str(), c.password.as_str(), c.auth_type),
                None => ("", "", 0),
            };
            return format!(
                "AT^SETAUTODIAL=1,{mode},\"{}\",\"{}\",\"{}\",\"{}\",{auth}",
                self.cfg.apn_protocol,
                sanitize(a),
                sanitize(user),
                sanitize(pass),
            );
        }

        match current {
            Some(c) if !c.apn.is_empty() => format!(
                "AT^SETAUTODIAL=1,{mode},\"{}\",\"{}\",\"{}\",\"{}\",{}",
                sanitize(&c.protocol),
                sanitize(&c.apn),
                sanitize(&c.username),
                sanitize(&c.password),
                c.auth_type
            ),
            _ => format!("AT^SETAUTODIAL=1,{mode}"),
        }
    }

    /// 读一次当前自动拨号配置；读不出来返回 `None`（调用方据此降级）。
    async fn read_autodial(&self) -> Option<AutodialCfg> {
        let reply = self.at.send("AT^SETAUTODIAL?").await.ok()?;
        if !reply.success {
            return None;
        }
        parse_setautodial(reply.text())
    }

    /// 发一条 AT，非 OK 即失败。成功时把回显第一行写进进度流。
    async fn run_at(
        &self,
        tx: &mpsc::UnboundedSender<String>,
        cmd: &str,
    ) -> Result<AtReply, String> {
        let reply = self
            .at
            .send(cmd)
            .await
            .map_err(|e| format!("AT 通道调用失败（{cmd}）: {e}"))?;
        if !reply.success {
            return Err(format!("模组拒绝 `{cmd}`: {}", first_line(reply.text())));
        }
        emit(tx, format!("  {cmd} → {}", first_line(reply.text())));
        Ok(reply)
    }

    /// 轮询 `AT^NDISSTATQRY?` 直到与 `want` 一致。
    ///
    /// 返回 `Ok(true)` = 达成；`Ok(false)` = 预算耗尽仍未达成。
    /// AT 通道连续 `AT_ERROR_TOLERANCE` 次读不到才返回 `Err` ——
    /// 一次读失败就报错会把串口上常见的单帧丢失误判成故障。
    async fn wait_dial(
        &self,
        want: Option<bool>,
        budget: Duration,
        tx: &mpsc::UnboundedSender<String>,
        label: &str,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + budget;
        let mut errs = 0u32;
        let mut last_note = String::new();

        emit(tx, format!("{label}…"));
        loop {
            match self.at.send("AT^NDISSTATQRY?").await {
                Ok(reply) => {
                    errs = 0;
                    let active = parse_ndis_active(reply.text());
                    if active == want {
                        emit(tx, format!("  {label}完成（拨号状态={:?}）", active));
                        return Ok(true);
                    }
                    // 只在状态发生变化时打一行，避免 700ms 一条把日志刷爆
                    let note = format!("拨号状态={active:?}");
                    if note != last_note {
                        emit(tx, format!("  {note}"));
                        last_note = note;
                    }
                }
                Err(e) => {
                    errs += 1;
                    if errs >= AT_ERROR_TOLERANCE {
                        return Err(format!("连续 {errs} 次无法通过 AT 通道读取拨号状态: {e}"));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// 最终确认：模组说"已连接"**且**接口上确实有地址。
    ///
    /// 两个条件缺一不可 —— 只看模组会漏掉"netifd 没把地址接过来"，
    /// 只看接口地址会漏掉"租约还没过期但 PDP 已经断了"。
    async fn confirm_ready(
        &self,
        budget: Duration,
        tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Option<Ipv4Addr>, String> {
        let deadline = Instant::now() + budget;
        let mut errs = 0u32;
        let mut last_note = String::new();
        let mut best: Option<Ipv4Addr> = None;

        loop {
            match self.at.send("AT^NDISSTATQRY?").await {
                Ok(reply) => {
                    errs = 0;
                    best = netif::ipv4_of(&self.cfg.wan_iface);
                    let active = parse_ndis_active(reply.text());
                    if active == Some(true) && best.is_some() {
                        return Ok(best);
                    }
                    let note = format!("拨号状态={active:?} 接口地址={}", fmt_ip(best));
                    if note != last_note {
                        emit(tx, format!("  {note}"));
                        last_note = note;
                    }
                }
                Err(e) => {
                    errs += 1;
                    if errs >= AT_ERROR_TOLERANCE {
                        return Err(format!("连续 {errs} 次无法读取拨号状态: {e}"));
                    }
                }
            }
            if Instant::now() >= deadline {
                // 模组已连接但接口还没地址：不判失败（renew 可能还在跑），
                // 返回已知的最佳结果，由调用方从 report 里自行判断。
                return Ok(best);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

// ─────────────────────────── 解析 ───────────────────────────

/// 判定 `AT^NDISSTATQRY?` 是否报告"已连接"。
///
/// 应答形如 `^NDISSTATQRY: 1,0,,"IPV4"`。返回值：
/// - `Some(true)` / `Some(false)`：读到了明确状态
/// - `None`：**没读到**（格式不认识 / 没有这一行）
///
/// ★ `None` 必须与 `Some(false)` 分开。「读不出来」被当成「没连上」，
/// 会让一次 AT 抖动直接升级成"切换失败"。
fn parse_ndis_active(raw: &str) -> Option<bool> {
    for line in raw.replace('\r', "\n").lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("^NDISSTATQRY:") else {
            continue;
        };
        let first = rest.split(',').next().unwrap_or("").trim();
        return match first {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
    }
    None
}

/// 解析 `AT^SETAUTODIAL?` 的应答。
///
/// 两种拼写都认：手册是 `^SETAUTODIAL`，部分固件回 `^SETAUTODAIL`
/// （MT5700 Console 前端也同时认这两种）。
fn parse_setautodial(raw: &str) -> Option<AutodialCfg> {
    for line in raw.replace('\r', "\n").lines() {
        let line = line.trim();
        // ★ 这里必须 `continue` 而不是 `?`：应答里通常还有别的行
        //   （`^HCSQ:`、URC 等），用 `?` 会在第一个不匹配的行就整体返回 None，
        //   真正的 `^SETAUTODIAL` 行永远读不到。（本用例就是为这个写的。）
        let Some(rest) = line
            .strip_prefix("^SETAUTODIAL:")
            .or_else(|| line.strip_prefix("^SETAUTODAIL:"))
        else {
            continue;
        };
        let fields = split_csv_fields(rest.trim());
        if fields.is_empty() {
            continue;
        }
        let Some(enable) = fields[0].parse::<u8>().ok() else {
            continue;
        };
        let num = |i: usize| -> Option<u8> { fields.get(i).and_then(|s| s.parse::<u8>().ok()) };
        let str_at = |i: usize| -> String { fields.get(i).cloned().unwrap_or_default() };
        return Some(AutodialCfg {
            enable,
            mode: num(1).unwrap_or(0),
            protocol: str_at(2),
            apn: str_at(3),
            username: str_at(4),
            password: str_at(5),
            auth_type: num(6).unwrap_or(0),
        });
    }
    None
}

/// 按逗号切分，尊重双引号（引号内的逗号不算分隔符），并去掉外层引号。
///
/// 不能直接 `split(',')`：`^SETAUTODIAL:1,1,"IPV4V6","","","",0` 里有连续空字段。
fn split_csv_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;

    for ch in s.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur);

    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// 净化要写进 AT 命令引号内的参数（与 `config::sanitize_at_param` 同一目的）。
fn sanitize(s: &str) -> String {
    crate::config::sanitize_at_param(s)
}

// ─────────────────────────── 小工具 ───────────────────────────

fn emit(tx: &mpsc::UnboundedSender<String>, msg: impl Into<String>) {
    // 接收端可能已经走了（调用方断开连接）：忽略发送失败，不要因此中断切换
    // —— 切换是有副作用的动作，半途停下比跑完更糟。
    let _ = tx.send(msg.into());
}

/// 取剩余预算，但不超过 `cap`。
fn remaining(deadline: Instant, cap: Duration) -> Duration {
    let left = deadline.saturating_duration_since(Instant::now());
    left.min(cap).max(Duration::from_millis(1))
}

fn fmt_ip(ip: Option<Ipv4Addr>) -> String {
    ip.map(|a| a.to_string()).unwrap_or_else(|| "无地址".into())
}

fn first_line(s: &str) -> String {
    s.replace('\r', "\n")
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ndis_active_true_on_connected() {
        assert_eq!(
            parse_ndis_active("^NDISSTATQRY: 1,0,,\"IPV4\"\r\nOK"),
            Some(true)
        );
    }

    #[test]
    fn ndis_active_false_on_disconnected() {
        assert_eq!(
            parse_ndis_active("^NDISSTATQRY: 0,0,,\"IPV4\"\r\nOK"),
            Some(false)
        );
    }

    #[test]
    fn ndis_active_none_when_unreadable() {
        // ★ 关键区分：读不出来 != 没连上
        assert_eq!(parse_ndis_active("OK"), None);
        assert_eq!(parse_ndis_active("^NDISSTATQRY: abc\r\nOK"), None);
        assert_eq!(parse_ndis_active(""), None);
    }

    #[test]
    fn parses_setautodial_full_form() {
        let cfg = parse_setautodial("^SETAUTODIAL:1,0,\"IPV4V6\",\"\",\"\",\"\",0\r\nOK")
            .expect("应解析成功");
        assert_eq!(cfg.enable, 1);
        assert_eq!(cfg.mode, 0);
        assert_eq!(cfg.protocol, "IPV4V6");
        assert_eq!(cfg.apn, "");
        assert_eq!(cfg.auth_type, 0);
    }

    #[test]
    fn parses_setautodial_with_real_apn() {
        let cfg = parse_setautodial("^SETAUTODIAL:1,1,\"IP\",\"cmnet\",\"user\",\"pw\",1\r\nOK")
            .expect("应解析成功");
        assert_eq!(cfg.mode, 1);
        assert_eq!(cfg.protocol, "IP");
        assert_eq!(cfg.apn, "cmnet");
        assert_eq!(cfg.username, "user");
        assert_eq!(cfg.password, "pw");
        assert_eq!(cfg.auth_type, 1);
    }

    #[test]
    fn parses_setautodial_accepts_misspelled_variant() {
        // 部分固件把 SETAUTODIAL 拼成 SETAUTODAIL
        let cfg = parse_setautodial("^SETAUTODAIL: 1,1\r\nOK").expect("应解析成功");
        assert_eq!(cfg.enable, 1);
        assert_eq!(cfg.mode, 1);
        assert_eq!(cfg.apn, "", "缺省字段应为空串而不是解析失败");
    }

    #[test]
    fn parses_setautodial_picks_the_right_line_from_noise() {
        let mixed = "^HCSQ: \"NR\",72,201,30\r\n^SETAUTODIAL: 1,2,\"IP\",\"3gnet\"\r\nOK";
        let cfg = parse_setautodial(mixed).expect("应解析成功");
        assert_eq!(cfg.mode, 2);
        assert_eq!(cfg.apn, "3gnet");
    }

    #[test]
    fn csv_split_respects_quotes_and_empty_fields() {
        assert_eq!(
            split_csv_fields("1,0,\"IPV4V6\",\"\",\"\",\"\",0"),
            vec!["1", "0", "IPV4V6", "", "", "", "0"]
        );
        // 引号内的逗号不是分隔符
        assert_eq!(split_csv_fields("1,\"a,b\",2"), vec!["1", "a,b", "2"]);
    }

    #[test]
    fn redial_command_writes_back_current_apn() {
        let cfg = Config::default();
        let sw = Switcher::new(cfg, AtClient::new("127.0.0.1", 8765, ""));
        let current = AutodialCfg {
            enable: 1,
            mode: 1,
            protocol: "IP".into(),
            apn: "cmnet".into(),
            username: String::new(),
            password: String::new(),
            auth_type: 0,
        };
        let cmd = sw.dial_command(&None, Some(&current));
        // ★ 必须把 APN 原样带上，不能退化成省略形态
        assert!(cmd.contains("\"cmnet\""), "重拨必须原样写回 APN: {cmd}");
        assert!(
            cmd.starts_with("AT^SETAUTODIAL=1,1,"),
            "命令形态不对: {cmd}"
        );
    }

    #[test]
    fn redial_command_falls_back_to_short_form_when_unreadable() {
        let cfg = Config::default();
        let sw = Switcher::new(cfg, AtClient::new("127.0.0.1", 8765, ""));
        assert_eq!(sw.dial_command(&None, None), "AT^SETAUTODIAL=1,1");
    }

    #[test]
    fn apn_command_carries_protocol_and_sanitized_apn() {
        let cfg = Config::default();
        let sw = Switcher::new(cfg, AtClient::new("127.0.0.1", 8765, ""));
        let cmd = sw.dial_command(&Some("cmnet\",0\r\nAT+CFUN=0".into()), None);
        assert!(cmd.contains("\"cmnet0ATCFUN0\""), "APN 必须被净化: {cmd}");
        assert_eq!(
            cmd.matches("AT+CFUN").count(),
            0,
            "不得拼出第二条命令: {cmd}"
        );
    }

    #[test]
    fn apn_rotation_cycles_through_the_pool() {
        let mut cfg = Config::default();
        cfg.method = Method::Apn;
        cfg.apn_list = vec!["a".into(), "b".into(), "c".into()];
        let sw = Switcher::new(cfg, AtClient::new("127.0.0.1", 8765, ""));
        let got: Vec<_> = (0..4).map(|_| sw.next_apn(Method::Apn).unwrap()).collect();
        assert_eq!(got, vec!["a", "b", "c", "a"], "APN 池应循环轮换");
    }

    #[test]
    fn redial_never_returns_an_apn() {
        let cfg = Config::default();
        let sw = Switcher::new(cfg, AtClient::new("127.0.0.1", 8765, ""));
        assert_eq!(sw.next_apn(Method::Redial), None);
    }

    #[test]
    fn remaining_is_capped_and_never_zero() {
        let far = Instant::now() + Duration::from_secs(300);
        assert_eq!(
            remaining(far, Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        let past = Instant::now() - Duration::from_secs(1);
        assert!(remaining(past, Duration::from_secs(10)) > Duration::ZERO);
    }

    #[test]
    fn first_line_strips_cr_and_blank_lines() {
        assert_eq!(
            first_line("\r\n^SETAUTODIAL: 1,1\r\nOK"),
            "^SETAUTODIAL: 1,1"
        );
        assert_eq!(first_line(""), "");
    }
}
