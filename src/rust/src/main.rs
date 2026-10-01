//! `ipswitchd` —— H5000M / MT5700M-CN 出口 IP 切换服务。
//!
//! 对外提供一个 HTTP 端点（默认 `http://<路由器>:8790/switch`），
//! 收到请求就按配置的策略换一次出口 IP，并把进度流式返回，
//! 最后输出一行完成标记（默认 `IP切换完成`）。
//!
//! 它面向的调用方是 `zgyd` 的 `-ip-switch` 参数 —— 抓取过程中每发 N 个请求
//! 就换一次出口 IP，用来绕开上游对单个 IP 的配额限制。
//!
//! 设计要点见 `docs/adr/`，这里只强调一条：
//! **绝不自己打开模组串口**。AT 通道统一走 MT5700 Console 后端
//! （`at-webserver-rust`，监听 `127.0.0.1:8765`），本服务只是它的客户端。

mod config;
mod httpd;
mod netif;
mod rpc;
mod switcher;

#[cfg(test)]
mod e2e_tests;

use std::process::exit;

use config::Config;
use rpc::AtClient;
use switcher::{AtState, Switcher};

fn main() {
    // 刻意不用 #[tokio::main]：路由器上只需要很小的运行时就够
    // （只有 accept 循环 + 每次切换一个任务），
    // 把 worker 固定成 2 个可以避免和 AT 后端抢 CPU。
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[ipswitch] 创建异步运行时失败: {e}");
            exit(1);
        }
    };

    if let Err(e) = rt.block_on(run()) {
        // 启动期失败一律非零退出：procd 会按 respawn 策略重试，
        // 而配置不改就永远起不来 —— 这正是我们想要的"错配置必须显式暴露"。
        eprintln!("[ipswitch] 启动失败: {e}");
        exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = Config::from_uci();
    cfg.validate()?;

    let auth_key = config::read_at_auth_key();
    eprintln!(
        "[ipswitch] 配置: 策略={} 接口={} 超时={}s 监听={}:{} AT通道={}:{}",
        cfg.effective_method().as_str(),
        cfg.wan_iface,
        cfg.timeout.as_secs(),
        cfg.listen,
        cfg.port,
        cfg.rpc_host,
        cfg.rpc_port
    );
    if cfg.effective_method() != cfg.method {
        eprintln!(
            "[ipswitch] 注意: 配置的策略是 {}，但 APN 池为空，实际按 redial 执行",
            cfg.method.as_str()
        );
    }

    let at = AtClient::new(cfg.rpc_host.clone(), cfg.rpc_port, auth_key);

    // Switcher 先建：启动探活的结果要记在它身上，才能被 /health、/status 看到。
    let sw = Switcher::new(cfg.clone(), at.clone());

    // 启动期探一次 AT 通道：把"后端没跑 / 密钥不对 / 串口没起来"
    // 这类问题暴露在这里，而不是等第一次切换时才失败。
    // 探活失败**不阻止启动** —— 后端可能稍后才就绪，服务先挂着比反复重启好；
    // 但结果会记进 Switcher，让 /health 能区分"服务活着"与"服务能干活"。
    match at.send("AT").await {
        Ok(r) if r.success => {
            eprintln!("[ipswitch] AT 通道连通");
            sw.set_at_state(AtState::Ok);
        }
        Ok(r) => {
            eprintln!(
                "[ipswitch] 警告: AT 通道有应答但非成功: {}（首次切换可能失败）",
                r.text()
            );
            sw.set_at_state(AtState::Failed);
        }
        Err(e) => {
            eprintln!("[ipswitch] 警告: AT 通道不可用: {e}（首次切换可能失败）");
            sw.set_at_state(AtState::Failed);
        }
    }

    if cfg.listen == "0.0.0.0" || cfg.listen == "::" {
        eprintln!(
            "[ipswitch] 注意: 监听 {0}。已依赖 OpenWrt 默认的 WAN 侧 input 策略拦截；\
             若防火墙被改成放行 WAN 入站，请自行加规则限制来源。",
            cfg.listen
        );
    }

    httpd::serve(cfg, sw)
        .await
        .map_err(|e| format!("HTTP 服务异常退出: {e}"))
}
