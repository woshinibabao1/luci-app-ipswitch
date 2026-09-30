# ADR 0004：标记文本只在成功时输出

- 状态：**已采纳**
- 日期：定案于项目初版（v1.0.0）

## 背景

zgyd 侧的契约（`pkg/ipswitch.go`）是这样的：

- 发 `GET`，要求 `HTTP 200`
- **边收边找**标记文本 —— 用 `waitForMarker` **流式**匹配，不等 EOF
- 匹配时保留 `len(marker)-1` 字节的尾窗，防止标记被切在两个分块之间
- **看到标记 = 成功；一直看不到 = 失败**（由 zgyd 自己超时或收流结束）

也就是说，「标记文本」是整个协议里**唯一的成功信号**，承载的语义很重。
它一旦出现得不该出现，调用方就会在一个没换 IP 的出口上继续发请求
（对 zgyd 来说这意味着继续被限流），且**调用方没有任何办法察觉**。

## 选项

**A. 无论成败都输出标记，用别的文本表示失败**
- 优点：调用方总能「看到」标记，可能更「稳定」
- 缺点：直接违反契约 —— 调用方会把失败也当成功。不可接受

**B. 先输出标记，再跑切换**
- 优点：调用方立刻返回，体感快
- 缺点：又违反契约 —— 标记的语义变成了「已开始」而不是「已完成」

**C. 只在成功时输出标记，失败时只输出原因** ← 本方案（也是唯一正确的）

## 决策

选 **C**，并用测试把它**钉死**：

- `httpd.rs::handle_switch` 的收尾块（tail）：
  ```rust
  let tail = match &outcome {
      Ok(report)  => format!("{}\n切换成功：…", sw.config().marker),  // 带标记
      Err(e)      => format!("切换失败：{e}\n"),                      // ★ 不带标记
  };
  ```
  失败分支里**根本没有拼接 `marker` 这个变量** —— 不是「拼了再删」，是结构上不可能带上
- 进度信息（`[ipswitch] …`）**不包含标记文本**，并且 `marker` 在配置校验里
  要求非空且被当作不可信输入看待（`validate()` 检查非空）
- 端到端用例 `switch_failure_never_emits_marker`：构造一个「拨号永远不回来」的假模组，
  断言整个响应体**不包含** `IP切换完成`
- 配套用例 `switch_outputs_marker_and_runs_full_at_sequence`：正常路径必须包含标记

「写了守卫 ≠ 有了守卫」—— 这两条用例是**对照**的：一个必须出现、一个必须不出现。
只有正向用例，反向路径漏了也没人知道。

## 附带决定：进度文本走 chunked，标记走收尾块

响应头是 `Transfer-Encoding: chunked`，进度边跑边发，标记放在最后一帧。
这样：

- 调用方（zgyd）能**边收边匹配**，不需要等整个响应
- 但标记的**位置在最后** —— 只有整轮切换真的跑完了才发得出来

分块长度**按字节**算（`write_chunk` 用 `data.len()`，即字节数），
因为进度文本含中文；用字符数当长度会让分块边界错位、调用方解析失败。
对应用例 `chunk_length_is_byte_count_not_char_count`。

**另一个附带决定**：调用方中途断开时，**切换继续执行完**
（`switch_ip` 跑在 `tokio::spawn` 出来的独立任务里，进度经 `mpsc` channel 汇出；
写出失败时只置 `broken = true`，不再中断切换，但继续排空 channel 直到任务收尾）。
理由：调用方断开通常意味着它自己超时了 —— 此时「半途放弃」会让模组停在
`AT^SETAUTODIAL=0` 之后的断开态，**直接把设备搞断网**；跑完才是安全的选择。
对应用例 `switch_survives_client_disconnect_and_finishes_the_job`。

## 后果

- ✅ 成功信号不可伪造：任何非成功路径都不可能吐出标记
- ✅ 调用方断开 = 设备不会停在半路（最坏情况被消除）
- ⚠️ 失败时调用方要等自己的超时（不知道服务端已经放弃）——
  但这是 zgyd 契约决定的（它只匹配标记、不解析失败文本）
- ⚠️ marker 可配置 → 用户改 marker 时**必须两边同改**，否则本服务永远「成功不了」
  （README 第 7 节给了对照说明）

## 相关

- `src/rust/src/httpd.rs`：`handle_switch()`、`write_chunk()`
- `src/rust/src/config.rs`：`validate()`（marker 非空）
- 测试：`switch_failure_never_emits_marker`、
  `switch_outputs_marker_and_runs_full_at_sequence`、
  `switch_survives_client_disconnect_and_finishes_the_job`、
  `chunk_length_is_byte_count_not_char_count`
- 契约来源：zgyd `pkg/ipswitch.go` 的 `waitForMarker`
