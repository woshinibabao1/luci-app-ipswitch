//! WAN 接口的地址查询与续约。
//!
//! 为什么走命令而不是 netlink：设备上 busybox 的 `ip` 与 `ubus` 是 netifd
//! 自身运维路径的一部分，必然存在；而一次切换最多查十几次、总耗时不到 200ms。
//! 换成 rtnetlink 要多写几百行且更难在真机上验证 —— 这里把"可读、可诊断"排在前面。

use std::net::Ipv4Addr;
use std::process::Command as StdCommand;
use std::time::Duration;

use tokio::process::Command;

/// 读接口当前的 IPv4 地址；没有地址则 `None`。
///
/// 同步实现：单次 `ip` 调用实测 < 10ms，且调用点都夹在等待间隙里，
/// 不会长时间占住 tokio 的 worker 线程（切换本身是低频操作，不值得为它
/// 引入一层 netlink 抽象）。
pub fn ipv4_of(iface: &str) -> Option<Ipv4Addr> {
    let out = StdCommand::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", iface])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_ipv4(&String::from_utf8_lossy(&out.stdout))
}

/// 从 `ip -4 -o addr` 输出里取第一个 `inet` 地址。
///
/// 真机输出形如：
/// `6: eth2    inet 10.76.139.149/8 brd 10.255.255.255 scope global eth2 ...`
///
/// 用「逐 token 找 `inet` 再取下一个」而不是正则：不引入 regex 依赖，
/// 且 `ip -o` 的字段顺序是稳定的。
fn parse_ipv4(text: &str) -> Option<Ipv4Addr> {
    for line in text.lines() {
        let mut it = line.split_whitespace();
        while let Some(tok) = it.next() {
            if tok == "inet" {
                let addr = it.next()?;
                // 去掉 /prefix 前缀长度
                return addr.split('/').next()?.parse().ok();
            }
        }
    }
    None
}

/// 触发接口续约，让 netifd 重新向模组要一次地址。
///
/// 优先 `ubus ... renew`：**不重建接口**，链路不闪断，是换 IP 后最合适的动作。
/// 只有在 renew 不可用时才回退 `ifdown/ifup` —— 那会把接口彻底拆掉再建，
/// 断网窗口明显更长，属于兜底手段（`99-mt5700-renew` 用的也是同一套优先级）。
///
/// 返回实际生效的方式，供日志说明"到底走了哪条路"。
pub async fn renew(iface: &str) -> Result<&'static str, String> {
    let target = format!("network.interface.{iface}");
    if run_ok("ubus", &["-S", "call", &target, "renew"]).await {
        return Ok("ubus renew");
    }

    let _ = run_ok("ifdown", &[iface]).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    if run_ok("ifup", &[iface]).await {
        return Ok("ifdown/ifup");
    }

    Err(format!(
        "{iface} 续约失败：ubus renew 不可用，ifdown/ifup 也没成功"
    ))
}

async fn run_ok(prog: &str, args: &[&str]) -> bool {
    match Command::new(prog).args(args).output().await {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_device_output() {
        let text = "6: eth2    inet 10.76.139.149/8 brd 10.255.255.255 scope global eth2\\       valid_lft forever preferred_lft forever\n";
        assert_eq!(parse_ipv4(text), Some("10.76.139.149".parse().unwrap()));
    }

    #[test]
    fn parses_multiple_lines_and_picks_first_inet() {
        let text = "6: eth2    inet 10.0.0.5/8 brd 10.255.255.255 scope global eth2\n\
                    9: br-lan  inet 192.168.10.1/24 brd 192.168.10.255 scope global br-lan\n";
        assert_eq!(parse_ipv4(text), Some("10.0.0.5".parse().unwrap()));
    }

    #[test]
    fn returns_none_when_no_address() {
        // 接口存在但没有 IPv4 地址时，`ip -4 -o addr` 输出为空
        assert_eq!(parse_ipv4(""), None);
        // 只有 IPv6 时同样不应误判
        assert_eq!(parse_ipv4("6: eth2    inet6 fe80::1/64 scope link\n"), None);
    }

    #[test]
    fn ignores_malformed_address() {
        assert_eq!(
            parse_ipv4("6: eth2    inet not-an-ip/8 scope global\n"),
            None
        );
    }
}
