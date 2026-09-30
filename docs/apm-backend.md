# APM 用哪个后端：C++ 的 libwebrtc，还是纯 Rust 的 sonora

> 音质修复后显式启用了 adaptive_digital（最大增益 20 dB、初始 0 dB、
> 每秒最大变化 3 dB）。本文历史 A/B 中的两端都没有启用自适应增益；
> CPU、延迟及声学数字不能作为新配置的验收结果。见 [音质验收](audio-quality.md)。

> **状态：已经换了。** C++ 那份、`third_party/` 和 `[patch.crates-io]` 都删掉了，
> APM 现在默认开着 —— `git clone` 完 `cargo run` 跑出来的客户端带回声消除和降噪，
> 不需要装任何东西。
>
> **仍然欠一次可信的声学往返测量。** 这台机器上的麦克风都自带噪声门，
> 量不出可信的数（见下面「噪声门」那一节）。也就是说「外放开黑会不会啸叫」
> 目前只有合成回路的证据。**这是拍板时就知道的风险，不是遗漏。**
>
> 调查和实施：2026-09-22。机器：Windows 11 Pro / rustc 1.96 MSVC。
> 前情见 [`m2-apm-windows.md`](m2-apm-windows.md) —— 那篇描述的 C++ 构建流程
> 已经不再适用，留着是历史。

## 为什么会有这个问题

[`m2-apm-windows.md`](m2-apm-windows.md) 里写清楚了：`webrtc-audio-processing`
开箱在 Windows/MSVC 上编不过，我们打了七个补丁才跑起来。代价是：

- 仓库里躺着 5.3 MB 的第三方 C++ 源码
- 贡献者要装 meson、ninja、llvm-tools、libclang
- 构建根目录必须极短（MAX_PATH），所以有了 `CARGO_TARGET_DIR=C:\t`
- 为此专门写了 [`scripts/win-buildenv.ps1`](../scripts/win-buildenv.ps1)

**最贵的代价不是这些，是 APM 因此默认关着。** `crates/client/Cargo.toml` 里
`apm` 不在默认 feature 里 —— 也就是说 `git clone` 完 `cargo run` 跑出来的客户端
**没有回声消除、没有降噪**。对一个「外放开黑不啸叫」是核心卖点的产品，
这是个不小的窟窿。

## sonora 是什么

[dignifiedquire/sonora](https://github.com/dignifiedquire/sonora)：libwebrtc
AudioProcessing 模块的**纯 Rust 移植**，AEC3 + 降噪 + AGC2 + 高通全都有。
BSD-3，跟之前那份 C++ 同一个许可，所以 LICENSING.md 一个字都不用改。

值得认真看的四条理由：

- **上游作者参与了。** 贡献者里有 `ford-prefect`（Arun Raghavan）—— 就是把 APM
  从 libwebrtc 里抽出来、做成 meson 独立库的那个人，也就是我们原来 vendor
  在 `third_party/` 的那份源码的上游维护者。
- **C++ 的 2400+ 条测试套件跑通了 Rust 后端**（通过 `sonora-sys` FFI 桥接反向验证）。
  移植类项目里这是最硬的证据 —— 不是"看起来差不多"，是逐条对过。
- **Windows x86_64 在 CI 里 build + test**，SIMD 有 SSE2/AVX2/NEON。
- 性能对得上：官方 benchmark 48 kHz 单声道 Rust 13.3 µs vs C++ 10.8 µs。

**风险要说清楚**：v0.2.0，两个多月，83 star，主要一个人在推，README 自己标了
移植过程 "AI assisted"。这不是 libwebrtc 那种十几年、几亿人在用的东西。
这条风险是**明知道并且接受了**的 —— 换来的是 APM 能默认打开，
而一个默认关着的回声消除等于没有。

## 最重要的一条：两边不是同一个版本

**我们的 C++ 是 WebRTC M131，sonora 移植的是 M145。差 14 个里程碑。**

依据：那份 C++ 的 `NEWS`（`third_party/` 删掉之前）里写着
"Release 2.0 — Bump to code from WebRTC M131"；sonora 的 README 写着
"Ported from the WebRTC Native Code (M145) audio processing module"。

**所以下面所有的差异都不能读成「Rust 比 C++ 强」。** 更可能的解释是
「新版 AEC3 比旧版强」，语言只是顺带的。要严格比语言，得拿 M145 的 C++ 来比 ——
而那正是我们编不出来的东西。

## 数字

三组测量，都是两个后端跑**同一份代码**，只有泛型参数不同 —— A/B 期间
`voice_core::apm` 里有一个 `ApmBackend` trait 专门为此存在（换完就删了）。
这是 A/B 的前提：数字有差别只可能来自后端本身，不可能来自"这边多做了一步"。

### 一、CPU 和延迟（`device-probe`）

| 配置 | webrtc (C++) | sonora (Rust) | Δ |
|---|---|---|---|
| **全开（实际配置）** | 0.66% / 15.00 ms | **0.79% / 14.92 ms** | **+0.13 个百分点** |
| 只开 AEC3 | 0.58% / 9.00 ms | 0.71% / 8.92 ms | +0.13 |
| 只开 降噪 | 0.21% / 7.00 ms | 0.27% / 6.92 ms | +0.06 |
| 只开 AGC2 | 0.04% / 0.00 ms | 0.03% / 0.00 ms | −0.02 |
| 只开 高通 | 0.14% / 1.00 ms | 0.21% / 0.92 ms | +0.08 |
| 全关（基线） | 0.04% | 0.01% | −0.03（少了 FFI 开销） |

**两个后端都进红线**：端到端 91.9 → 91.8 ms（产品线 120，闸 100）；
CPU 2.84% → 2.97%（产品线 6%，闸 4%）。

延迟那个 −0.08 ms 在每一行都一样，是个固定的 4 采样点偏移，没有意义。

### 二、合成回声（`apm::echo`，线性回路）

| | webrtc (C++) | sonora (Rust) |
|---|---|---|
| 回声消掉多少 | 26.0 dB | **57.9 dB** |
| 收敛曲线 | 36 → 30 → 27 → **25**（往下掉） | 36 → 31 → 52 → **58**（涨上去不掉） |
| 没回声时削掉人声 | 0.2 dB | 1.1 dB |

**这组数字不能当产品表现看。** 测试里的回声路径是纯线性的 —— 延迟加衰减，
没有音箱的非线性失真、没有削波、没有时钟漂移。而恰恰是这些非线性的东西
才真正区分 AEC 实现的好坏。线性路径上自适应滤波器能收敛到近乎完美，
所以这里量到的是「滤波器收敛得多好」。

### 三、真声学往返（`apm::acoustic`）

EDIFIER G2000 音箱 + Arctis Nova Pro 麦克风，两次独立跑：

| | 回声进去 | 残留 | 消掉 |
|---|---|---|---|
| webrtc (C++) | −45.0 dB | −52.4 dB | **7.4 dB**（另一次 7.6 / 8.1） |
| sonora (Rust) | −45.0 dB | −58.0 dB | **12.9 dB**（另一次 13.7） |

复现性很好，sonora 稳定高 5.5–6.1 dB。但**两个数都低得不能用** ——
原因见下一节，不是 AEC 的问题。

## 踩到的坑：游戏耳麦自带噪声门，量不了 AEC

**这一节是这篇里最值得留下来的东西。**

Arctis Nova Pro 安静时的本底是 **−96 dB**，正好是 16 位的最低位，也就是
纯数字静音。真实房间不可能这么安静（再静的房间加麦克风底噪也就 −70 dB 上下）。

决定性的证据：**同一个麦、同一个房间、相隔 15 秒，本底一次 −69.0 dB、
一次 −96.6 dB。** 门是时变的。

为什么这会让整次测量作废：

> 噪声门是**非线性、时变**的处理，而且它在我们的 APM **之前**。
> AEC 的线性滤波器建模的是「播出去的信号怎么变成录回来的信号」，
> 中间插一个会突然把信号整个掐掉的门，这个映射就不存在了 ——
> 滤波器刚学会就被推翻一次。

webrtc 的逐秒曲线正好印证：15.0 → 11.6 → 10.6 → … → 8.2，**一路往下掉**。
那不是在收敛，是在被反复推翻。

游戏耳麦基本都带这个（SteelSeries ClearCast、雷蛇、HyperX）。
**量 AEC 必须用不带 DSP 的麦克风**：普通 USB 麦、3.5mm 麦、摄像头麦。

`apm_acoustic.rs` 里把这条做成了守卫（`DIGITAL_SILENCE_DB`）：本底低于
−90 dB 就在**放音之前**判掉，消息直接说清是场景问题不是 AEC 问题。
这是照着 `mod echo` 的思路做的 ——「没测出来」和「测出来很差」必须报不同的话。

### 顺带修掉的一个测量 bug

本底噪声原本是在**刚打开**的采集流上量的。无线耳麦在 `Start()` 之后
要过一会儿才真的出数据，之前交付的是全零帧 —— 量出来 −96 dB，
看着像个正经数字，其实是假的。

表现是：先跑的后端本底 −96.6 dB、后跑的 −75.0 dB，同一个房间同一个麦，
差 21 dB。现在丢掉头 50 帧（`WARMUP_FRAMES`）。

## 换过去的代价

代码上很小 —— sonora 的 API 是照着 C++ `AudioProcessing::Config` 一比一映射的，
比我们现在用的那层 Rust 化封装**更贴近原版**。

三处要动脑子的：

1. **`&self` → `&mut self`。** `AudioProcessor` trait 是 `&self` + `Arc`，
   靠上游 crate 的内部可变性撑着；sonora 的方法全是 `&mut self`，所以
   `Apm` 里包了一把 `Mutex`。**语义有差别**：C++ 的 APM 是
   capture 和 render 两把独立的锁，我们这把是一把，采集会被渲染挡一下。
   最坏十几微秒 / 10 ms 预算 = 0.13%，但这是实时线程。
2. **不是原地处理。** sonora 的签名是源和目标两个 slice，我们的接口是原地改，
   所以挂了 scratch，每帧多一次 480 点的拷贝。
3. **没有「只分析不修改」的远端接口。** C++ 有 `analyze_render_frame`，
   外放场景能省一次拷贝；sonora 只有 `process_render_f32`，结果往 scratch 里丢。

还有一处**必须显式写**的配置：sonora 的 `EchoCanceller::enforce_high_pass_filtering`
默认 `true`，而上游 crate 在 FFI 那层把它**硬编码成 `false`**。用 `default()`
的话两个后端配出来的东西不一样，A/B 就成了在比两份不同的配置。

MSRV：sonora 要 1.91，工作区是 1.80。**只抬了 `voice-core` 一个 crate** ——
`protocol` 是给第三方照着实现的，`transport`/`server` 也没理由被一个客户端
音频依赖拖着升。

## 换完之后实际删掉/改掉了什么

- `third_party/`（5.3 MB C++ 和七个补丁）、根 `Cargo.toml` 的
  `[patch.crates-io]` 和 `exclude`
- `scripts/win-buildenv.ps1` 从 130 行缩到 60 行 —— meson、ninja、伪造的
  `nm.exe`、libclang、`CARGO_TARGET_DIR=C:	` 全删了。**留下的只有给 libopus
  找 cmake 那一件事**，而且多数机器上不跑它也行
- [`m2-apm-windows.md`](m2-apm-windows.md) 留着当历史，顶上标了已不适用
- `client` 的 `--features apm` 开关没了；`client-core` 和 `server` 里那些
  「不带 apm」的 `default-features = false` 也没了
- A/B 用的 `ApmBackend` trait 删了 —— 它存在的唯一理由是让两份实现跑同一份
  测量代码，只剩一份之后这个理由就不成立了

**`cargo build` 现在 18 秒编完整个 sonora（8 个 crate），一个外部工具都不用装。**
`git clone` 完 `cargo run` 跑出来的客户端带回声消除和降噪。

## 还没解决的

1. **一次可信的声学往返测量。** 需要一个不带 DSP 的麦克风。现在手上只有
   合成数字和一组被噪声门污染的真机数字 —— 也就是说
   **「外放开黑不啸叫」这条卖点还没有真机证据**。这是这次换实现留下的
   最大一个洞，优先级最高。
2. **双讲**（两个人同时说）。合成信号量不了，见 `mod echo` 末尾的说明。
3. **M145 的 C++ 做对照。** 没有它就无法把「新版 AEC3」和「Rust 移植」这两个
   因素分开。优先级不高 —— 我们要选的是"用哪个"，不是"为什么"。
4. **安装包体积。** C++ 那份 strip 后的静态库是 38 MB（实际进二进制的少得多）。
   sonora 是不是更小没测过。60 MB 红线要到 M6 打包才验。

## 怎么跑这些测量

都不需要 `win-buildenv.ps1`，也不需要任何外部工具。

```bash
# CPU 和延迟逐块计价
cargo run --release -p device-probe -- --seconds 3

# 合成回声（线性回路）
cargo test --release -p voice-core echo:: -- --nocapture --test-threads 1

# 真声学往返。**会真的出声、真的录音**，要摆好场景：
#   音箱开着、耳机摘了、人别说话、麦克风不能带 DSP
set GOUHUO_AEC_RENDER=EDIFIER
set GOUHUO_AEC_CAPTURE=Insta360
cargo test --release -p voice-core acoustic -- --ignored --nocapture --test-threads 1

# 哪个麦克风真的在出数据（排查用）
cargo test --release -p voice-core --lib device_scan -- --ignored --nocapture
```
