//! UCI 配置读取。
//!
//! 与 MT5700 Console 的 `config.rs` 同口径：**配置只在进程启动阶段读一次**，
//! 此后视为只读常量，因此多任务并发读取无需加锁。改配置后由 procd `reload`
//! 重启本服务来生效。
//!
//! 为什么不每次请求都重读：`uci` 是外部进程，一次切换要读十几个键，
//! 每次请求都读会在切换路径上平白多出十几次 fork+exec；更要紧的是
//! **同一次切换中途配置发生变化**会让前半段和后半段按不同参数执行，
//! 这种问题极难排查。启动定型 + reload 生效是更好的取舍。

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use crate::netif;

/// 换 IP 的策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// 重拨：关闭自动拨号再打开，触发重新协商。**不改 APN**。
    Redial,
    /// 切 APN：在 APN 池里轮换下一个 APN 后重新拨号。
    /// 池为空时自动退化为 [`Method::Redial`]（见 [`Config::effective_method`]）。
    Apn,
}

impl Method {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "redial" | "reconnect" => Some(Method::Redial),
            "apn" | "switch-apn" => Some(Method::Apn),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Redial => "redial",
            Method::Apn => "apn",
        }
    }
}

/// 运行期配置。
#[derive(Debug, Clone)]
pub struct Config {
    /// HTTP 监听地址。
    pub listen: String,
    /// HTTP 监听端口。
    pub port: u16,
    /// 写进响应流、供调用方判定"切换完成"的标记文本。
    /// 默认与 zgyd 的 `-ip-switch-marker` 默认值一致。
    pub marker: String,
    /// 策略：`redial` / `apn`。
    pub method: Method,
    /// APN 池（轮换使用）。空 = 不做 APN 轮换。
    pub apn_list: Vec<String>,
    /// APN 协议类型，写进 `AT^SETAUTODIAL` 第 3 个参数。`IP` / `IPV6` / `IPV4V6`。
    pub apn_protocol: String,
    /// 拨号方式，写进 `AT^SETAUTODIAL` 第 2 个参数。
    /// 手册 16.18：`0`=模组内部拨号，`1`=上位机拨号(USB)，`2`=上位机拨号(网口)。
    pub dial_mode: u8,
    /// 单次切换的最长等待时间。
    pub timeout: Duration,
    /// WAN 接口名（用于判断"是否已拿到地址"）。
    pub wan_iface: String,
    /// MT5700 Console 后端 RPC 地址。
    pub rpc_host: String,
    pub rpc_port: u16,
}

/// 默认值。与 `root/etc/config/ipswitch` 里的默认配置保持一致 ——
/// 两处不一致会让"删掉配置文件后的行为"和"首次安装后的行为"不一样。
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0".into(),
            port: 8790,
            marker: "IP切换完成".into(),
            method: Method::Redial,
            apn_list: Vec::new(),
            apn_protocol: "IP".into(),
            dial_mode: 1,
            timeout: Duration::from_secs(45),
            wan_iface: "MT5700M".into(),
            rpc_host: "127.0.0.1".into(),
            rpc_port: 8765,
        }
    }
}

impl Config {
    /// 从 UCI 读配置。读不到或读不通时退回默认值并记一条日志 ——
    /// 本服务不该因为少一个配置项就起不来。
    pub fn from_uci() -> Self {
        let mut cfg = Config::default();
        let map = match uci_show("ipswitch") {
            Some(m) => m,
            None => {
                eprintln!("[ipswitch] 读取 UCI 失败，使用默认配置");
                return cfg;
            }
        };

        if let Some(v) = map.get("listen") {
            if !v.trim().is_empty() {
                cfg.listen = v.trim().to_string();
            }
        }
        if let Some(v) = map.get("port") {
            match v.trim().parse::<u16>() {
                Ok(p) if p > 0 => cfg.port = p,
                _ => eprintln!("[ipswitch] port 配置无效（{v}），沿用默认 {}", cfg.port),
            }
        }
        if let Some(v) = map.get("marker") {
            if !v.is_empty() {
                cfg.marker = v.clone();
            }
        }
        if let Some(v) = map.get("method") {
            match Method::parse(v) {
                Some(m) => cfg.method = m,
                None => eprintln!("[ipswitch] method 配置无效（{v}），沿用默认 redial"),
            }
        }
        if let Some(v) = map.get("apn_list") {
            cfg.apn_list = parse_apn_list(v);
        }
        if let Some(v) = map.get("apn_protocol") {
            match v.trim().to_ascii_uppercase().as_str() {
                "IP" | "IPV6" | "IPV4V6" => cfg.apn_protocol = v.trim().to_ascii_uppercase(),
                other => eprintln!("[ipswitch] apn_protocol 配置无效（{other}），沿用默认 IP"),
            }
        }
        if let Some(v) = map.get("dial_mode") {
            match v.trim().parse::<u8>() {
                Ok(m) if m <= 2 => cfg.dial_mode = m,
                _ => eprintln!("[ipswitch] dial_mode 只支持 0/1/2（收到 {v}），沿用默认 1"),
            }
        }
        if let Some(v) = map.get("timeout") {
            match v.trim().parse::<u64>() {
                Ok(sec) if sec > 0 => cfg.timeout = Duration::from_secs(sec),
                _ => eprintln!("[ipswitch] timeout 配置无效（{v}），沿用默认 45"),
            }
        }
        if let Some(v) = map.get("wan_iface") {
            if !v.trim().is_empty() {
                cfg.wan_iface = v.trim().to_string();
            }
        }
        if let Some(v) = map.get("rpc_host") {
            if !v.trim().is_empty() {
                cfg.rpc_host = v.trim().to_string();
            }
        }
        if let Some(v) = map.get("rpc_port") {
            match v.trim().parse::<u16>() {
                Ok(p) if p > 0 => cfg.rpc_port = p,
                _ => eprintln!("[ipswitch] rpc_port 配置无效（{v}），沿用默认 8765"),
            }
        }

        cfg
    }

    /// 这次实际要用的策略。
    ///
    /// APN 池为空却要求切 APN，是**配置错误**而不是"切到一个空 APN"——
    /// 后者会把设备的 APN 清掉、直接断网。这里退化为重拨并在日志里说明，
    /// 与 [`validate`](Self::validate) 启动期的硬失败形成双层保护。
    pub fn effective_method(&self) -> Method {
        match self.method {
            Method::Apn if self.apn_list.is_empty() => Method::Redial,
            m => m,
        }
    }

    /// 启动期校验：**能在启动阶段发现的配置错误，就不要等切换时才发现**。
    /// 失败返回可读原因，由 main 打日志后退出（procd 会按 respawn 策略重试，
    /// 但配置不改就永远起不来 —— 这正是我们要的：错配置必须显式暴露）。
    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("port 不能为 0".into());
        }
        if self.rpc_port == 0 {
            return Err("rpc_port 不能为 0".into());
        }
        if self.marker.is_empty() {
            return Err("marker 不能为空：调用方要靠它判定切换完成".into());
        }
        if self.timeout.is_zero() {
            return Err("timeout 必须为正".into());
        }
        if self.dial_mode > 2 {
            return Err(format!("dial_mode 只支持 0/1/2，收到 {}", self.dial_mode));
        }
        if self.method == Method::Apn && self.apn_list.is_empty() {
            return Err("method=apn 但 apn_list 为空：请在 UCI 里配置 APN 池，\
                 或改用 method=redial（切到空 APN 会直接断网）"
                .into());
        }
        // 接口名要能安全地拼进命令行参数：只允许字母数字下划线点减号。
        // 这里挡的不是"我们自己的配置"，而是"配置被注入后"的影响面。
        if !self
            .wan_iface
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(format!("wan_iface 含非法字符: {}", self.wan_iface));
        }
        // 启动时就确认一次依赖可用，并把接口名暴露出去 ——
        // 否则要等到第一次切换失败才发现名字写错了。
        if netif::ipv4_of(&self.wan_iface).is_none() {
            eprintln!(
                "[ipswitch] 提示: 接口 {} 当前没有 IPv4 地址（可能还没拨上号，或名字写错了）",
                self.wan_iface
            );
        }
        Ok(())
    }
}

/// 解析逗号分隔的 APN 池，顺带做**参数净化**。
///
/// ★ 净化不是洁癖：这些字符串会被直接拼进 AT 命令的引号内，
/// 一个 `"` 就能提前闭合引号、把后面的内容变成第二条 AT 指令。
/// 与 MT5700 Console 前端 `Parse.sanitizeAtParam` 同一目的，
/// 但**必须在这里再做一遍** —— 前端净化挡不住手写配置文件的人。
fn parse_apn_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| sanitize_at_param(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 把字符串净化到可以安全放进 AT 命令引号内。
///
/// 白名单而非黑名单：APN 合法字符是字母、数字、点、横线、下划线
/// （如 `cmnet` / `3gnet` / `ctnet` / `cmmtm`），其余一律剔除。
pub fn sanitize_at_param(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .take(99) // 手册：APN 最长 99 字节
        .collect()
}

/// 读 MT5700 Console 后端 RPC 的 `auth_key`。
///
/// 后端在「配置了密钥」时要求每个请求都带；没配则空串放行 ——
/// 两种都是正常形态，所以这里读不到就返回空串，不报错。
pub fn read_at_auth_key() -> String {
    uci_show("at-webserver")
        .and_then(|m| m.get("auth_key").cloned())
        .unwrap_or_default()
}

/// `uci -q show <pkg>` → `{ option: value }`。
///
/// 只取 `config` 这一段（本包与 `at-webserver` 都只有一个 section）。
/// `uci show` 的输出形如 `ipswitch.config.port='8790'`，值用单引号包裹。
/// 我们不接受包含换行的值，因此按行解析是安全的。
fn uci_show(pkg: &str) -> Option<HashMap<String, String>> {
    let out = Command::new("uci")
        .args(["-q", "show", pkg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(parse_uci_show(&text, pkg))
}

/// 纯函数形式的解析，便于单测。
///
/// `pkg` 参与前缀匹配：否则读 `at-webserver` 时会被 `ipswitch.` 前缀
/// 一刀切掉、一个键都取不到。
fn parse_uci_show(text: &str, pkg: &str) -> HashMap<String, String> {
    let prefix = format!("{pkg}.");
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        // 形如 <pkg>.<section>.<option>='<value>'
        let Some(rest) = line.strip_prefix(prefix.as_str()) else {
            continue;
        };
        let Some((sec, kv)) = rest.split_once('.') else {
            continue;
        };
        if sec != "config" {
            continue;
        }
        let Some((key, val)) = kv.split_once('=') else {
            continue;
        };
        let val = val.trim().trim_matches('\'');
        map.insert(key.trim().to_string(), val.to_string());
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uci_show_output() {
        let text = "ipswitch.config=ipswitch\n\
                    ipswitch.config.port='8790'\n\
                    ipswitch.config.method='apn'\n\
                    ipswitch.config.marker='IP切换完成'\n";
        let m = parse_uci_show(text, "ipswitch");
        assert_eq!(m.get("port").map(String::as_str), Some("8790"));
        assert_eq!(m.get("method").map(String::as_str), Some("apn"));
        assert_eq!(m.get("marker").map(String::as_str), Some("IP切换完成"));
    }

    #[test]
    fn ignores_other_sections_and_other_packages() {
        let text = "ipswitch.config.port='8790'\n\
                    ipswitch.other.port='1'\n\
                    at-webserver.config.websocket_port='8765'\n";
        let m = parse_uci_show(text, "ipswitch");
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("port").map(String::as_str), Some("8790"));
    }

    #[test]
    fn reads_another_package_with_its_own_prefix() {
        // ★ 回归守卫：前缀必须跟着 pkg 走。写死 "ipswitch." 时，
        //   读 at-webserver 会一个键都取不到，auth_key 永远为空。
        let text = "at-webserver.config=at-webserver\n\
                    at-webserver.config.websocket_port='8765'\n\
                    at-webserver.config.auth_key='secret'\n";
        let m = parse_uci_show(text, "at-webserver");
        assert_eq!(m.get("auth_key").map(String::as_str), Some("secret"));
        assert_eq!(m.get("websocket_port").map(String::as_str), Some("8765"));
    }

    #[test]
    fn sanitize_strips_quotes_and_control_chars() {
        // 注入尝试：提前闭合引号再拼一条 AT 命令
        assert_eq!(sanitize_at_param("cmnet\",0\r\nAT+CFUN=0"), "cmnet0ATCFUN0");
        assert_eq!(sanitize_at_param("3gnet"), "3gnet");
        assert_eq!(sanitize_at_param(" cm net "), "cmnet");
        assert_eq!(sanitize_at_param(""), "");
    }

    #[test]
    fn apn_list_is_split_cleaned_and_deduped_of_blanks() {
        assert_eq!(
            parse_apn_list("cmnet, 3gnet ,,ctnet"),
            vec!["cmnet", "3gnet", "ctnet"]
        );
        assert_eq!(parse_apn_list("  "), Vec::<String>::new());
    }

    #[test]
    fn apn_method_without_pool_degrades_to_redial() {
        let mut cfg = Config::default();
        cfg.method = Method::Apn;
        assert_eq!(cfg.effective_method(), Method::Redial);
        cfg.apn_list = vec!["cmnet".into()];
        assert_eq!(cfg.effective_method(), Method::Apn);
    }

    #[test]
    fn validate_rejects_bad_values() {
        let mut cfg = Config::default();
        cfg.marker = String::new();
        assert!(cfg.validate().is_err(), "空 marker 必须被拒绝");

        let mut cfg = Config::default();
        cfg.wan_iface = "eth2; reboot".into();
        assert!(cfg.validate().is_err(), "接口名含非法字符必须被拒绝");

        let mut cfg = Config::default();
        cfg.method = Method::Apn;
        cfg.apn_list.clear();
        assert!(cfg.validate().is_err(), "method=apn 但池为空必须被拒绝");
    }

    #[test]
    fn default_config_is_valid() {
        assert!(Config::default().validate().is_ok());
    }
}
