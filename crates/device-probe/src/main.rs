//! M2 探针：WASAPI 到底吃掉多少延迟。
//!
//! M1 已经把协议链路测干净了（同城 p95 46.5 ms）。80 ms 红线里剩下的那部分
//! 全归设备，而 M1 里给它留的 30 ms 是**拍脑袋的假设**。这个程序负责把它换掉。
//!
//! 两步：
//! 1. 把机器上所有活动端点的能力摊开。缓冲周期是延迟的主项，而它完全由设备和
//!    驱动决定 —— 不先看清楚这个，后面测出来的数字不知道该归因给谁。
//! 2. 真开流实测。不出声：渲染灌静音，采集只读 WASAPI 自带的 QPC 时间戳。

mod apm;
mod measure;
mod width;

use std::process::ExitCode;

use voice_core::redline;
use voice_core::wasapi::{self, DeviceInfo, Direction, ShareMode};
use width::pad;

const NAME_W: usize = 34;

/// M1 实测的协议链路嘴到耳 p95（city 档 / 10 ms 帧 / 缓冲 2 帧）。
/// 拿它跟设备实测值相加才是端到端。见 docs/m1-baseline.txt。
const M1_PROTOCOL_P95_MS: f64 = 46.5;

struct Args {
    seconds: f64,
    skip_apm: bool,
    exclusive: bool,
    all: bool,
    list_only: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        seconds: 3.0,
        skip_apm: false,
        exclusive: false,
        all: false,
        list_only: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => {
                let v = it.next().ok_or("--seconds 后面少了值")?;
                a.seconds = v.parse().map_err(|_| "--seconds 要是数字")?;
            }
            "--exclusive" => a.exclusive = true,
            "--no-apm" => a.skip_apm = true,
            "--all" => a.all = true,
            "--list" => a.list_only = true,
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            other => return Err(format!("不认识的参数 {other}")),
        }
    }
    if a.seconds < 0.5 {
        return Err("--seconds 太短，测不出分位数".into());
    }
    Ok(Some(a))
}

fn print_help() {
    println!("M2 设备探针 —— 测 WASAPI 采集/渲染吃掉多少延迟");
    println!();
    println!("  --list        只列设备能力，不打开任何流");
    println!("  --seconds F   每一路测多久，默认 3");
    println!("  --all         测所有真硬件端点，不只是默认端点");
    println!("  --no-apm      跳过 APM 那一段（它不碰设备，纯信号处理）");
    println!("  --exclusive   顺便测独占模式");
    println!("                注意：独占期间别的程序发不出声，测几秒就恢复");
    println!();
    println!("默认只测共享模式。全程灌静音、不保留任何音频，但会短暂点亮");
    println!("「麦克风正在使用」指示灯 —— 测采集延迟必须打开采集流。");
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("参数错误：{e}（用 --help 看用法）");
            return ExitCode::from(2);
        }
    };

    let _com = wasapi::ComGuard::new();

    let devices = match wasapi::enumerate() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("枚举音频端点失败：{e}");
            return ExitCode::FAILURE;
        }
    };

    if devices.is_empty() {
        eprintln!("没有找到任何活动的音频端点。这台机器上测不了 M2。");
        return ExitCode::FAILURE;
    }

    for direction in [Direction::Render, Direction::Capture] {
        let list: Vec<&DeviceInfo> = devices
            .iter()
            .filter(|d| d.direction == direction)
            .collect();
        println!();
        let label = match direction {
            Direction::Render => "渲染 / 放音",
            Direction::Capture => "采集 / 录音",
        };
        println!("== {} ==（{} 个活动端点）", label, list.len());
        table(&list);
    }

    inventory_notes(&devices);

    if args.list_only {
        println!();
        println!("（--list：只列能力，没有打开任何流）");
        return ExitCode::SUCCESS;
    }

    let targets = pick_targets(&devices, args.all);
    if targets.is_empty() {
        eprintln!("没有可测的端点。");
        return ExitCode::FAILURE;
    }
    measure_all(&targets, &args);
    if !args.skip_apm {
        apm_section(&args);
    }
    ExitCode::SUCCESS
}

/// APM 那一段。不碰设备、不出声 —— 纯信号处理，喂合成信号就行。
fn apm_section(args: &Args) {
    println!();
    println!("==== APM（AEC3 / 降噪 / AGC2 / 高通）====");
    println!("不碰设备、不出声。每块单独计价，最后是实际要发布的全开配置。");
    println!();
    println!(
        "{:<22} {:>10} {:>10} {:>9} {:>10} {:>8} {:>9}",
        "config", "p50-us", "p95-us", "cpu%", "delay-ms", "peak", "level"
    );
    println!("{}", "-".repeat(84));

    let seconds = (args.seconds * 2.0).max(4.0);
    let rows = apm::measure_all(seconds);
    let mut shipping_delay = None;
    let mut shipping_cpu = None;

    for row in &rows {
        match row {
            Ok(r) => {
                let delay = match r.delay_ms {
                    Some(d) => format!("{d:.2}"),
                    None => "对不上".to_string(),
                };
                let peak = match r.delay_peak {
                    Some(p) => format!("{p:.2}"),
                    None => "-".to_string(),
                };
                println!(
                    "{:<22} {:>10.1} {:>10.1} {:>8.2}% {:>10} {:>8} {:>9.3}",
                    r.label, r.cpu_us_p50, r.cpu_us_p95, r.cpu_pct, delay, peak, r.level_ratio
                );
                if r.label.starts_with("全开") {
                    shipping_delay = r.delay_ms;
                    shipping_cpu = Some(r.cpu_pct);
                }
            }
            Err(e) => println!("{:<22} {e}", "(失败)"),
        }
    }

    println!("{}", "-".repeat(84));
    println!("  p50/p95-us  每 10 ms 帧的处理耗时（近端 + 远端两次调用）");
    println!(
        "  cpu%        折算成单核占比。红线 {:.0}%，但那是**整个通话**的预算，",
        redline::CPU_PCT
    );
    println!("              APM 要跟 Opus 编解码共享（M1 实测编解码 2.18%）");
    println!("  delay-ms    互相关量到的波形延迟。APM 不会自报这个数，只能测");
    println!("  peak        相关峰。低于 0.5 就判定认不出来了，宁可不报数");
    println!("  level       输出/输入幅度比。AGC 会明显改它，接近 0 说明信号被吃掉了");

    apm_verdict(shipping_delay, shipping_cpu);
}

fn apm_verdict(delay_ms: Option<f64>, cpu_pct: Option<f64>) {
    const DEVICE_MS: f64 = 30.4; // M2 实测，见 m2-baseline.txt
    const CODEC_CPU_PCT: f64 = 2.18; // M1 实测，complexity 5 / 10 ms 帧

    println!();
    println!("{}", "=".repeat(84));
    println!("M2 完整结论：协议 + 设备 + APM");
    println!("{}", "=".repeat(84));

    match delay_ms {
        Some(d) => {
            let total = M1_PROTOCOL_P95_MS + DEVICE_MS + d;
            println!(
                "  延迟：协议 {:.1}（M1）+ 设备 {:.1}（M2）+ APM {:.2} = {:.1} ms，红线 {:.0}",
                M1_PROTOCOL_P95_MS,
                DEVICE_MS,
                d,
                total,
                redline::E2E_MS
            );
            if total <= redline::E2E_MS {
                println!("    >>> 进线，余量 {:.1} ms", redline::E2E_MS - total);
            } else {
                println!("    >>> 破线 {:.1} ms", total - redline::E2E_MS);
            }
        }
        None => println!("  延迟：APM 那一段没测出可信数字（相关峰太低），端到端算不全"),
    }

    match cpu_pct {
        Some(c) => {
            let total = c + CODEC_CPU_PCT;
            println!(
                "  CPU：APM {:.2}% + 编解码 {:.2}%（M1）= {:.2}%，红线 {:.0}%",
                c,
                CODEC_CPU_PCT,
                total,
                redline::CPU_PCT
            );
            if total <= redline::CPU_PCT {
                println!(
                    "    >>> 进线，余量 {:.2} 个百分点",
                    redline::CPU_PCT - total
                );
            } else {
                println!("    >>> 破线 {:.2} 个百分点", total - redline::CPU_PCT);
            }
            println!("    注意这是单路。频道里 K 个人同时说话，解码要乘 K，APM 不用。");
        }
        None => println!("  CPU：没测出来"),
    }

    println!();
    println!("  仍然没测的：");
    println!("    - 真回声下 AEC3 的效果（这里远端喂的是静音，没有回声可消）");
    println!("    - 声学往返（音箱 → 空气 → 麦克风）");
    println!("    - 虚拟声卡那一层、蓝牙耳机");
    println!("    - 内存和安装包红线（要等 voice-core 独立进程和 Tauri 打包）");
}

fn table(list: &[&DeviceInfo]) {
    println!(
        "{} {:<3} {:<8} {:<18} {:>9} {:>9} {:>9} {:>9}  excl-48k",
        pad("name", NAME_W),
        "def",
        "bus",
        "mix-format",
        "cur-ms",
        "engmin-ms",
        "devdef-ms",
        "devmin-ms"
    );
    println!("{}", "-".repeat(NAME_W + 78));

    // 真硬件排前面：虚拟声卡的数字不能用来定红线。
    let mut sorted: Vec<&&DeviceInfo> = list.iter().collect();
    sorted.sort_by_key(|d| (!d.is_hardware(), !d.is_default));

    for d in sorted {
        let mix = d
            .mix_format
            .map(|f| f.to_string())
            .unwrap_or_else(|| "-".into());
        let engmin = match d.engine {
            Some(e) => fmt_ms(e.min_ms()),
            None => "-".to_string(),
        };
        let excl48 = match (d.exclusive_48k_mono_i16, d.exclusive_48k_stereo_i16) {
            (true, true) => "mono+st",
            (true, false) => "mono",
            (false, true) => "stereo",
            (false, false) => "no",
        };
        println!(
            "{} {:<3} {:<8} {:<18} {:>9} {:>9} {:>9} {:>9}  {}",
            pad(&d.name, NAME_W),
            if d.is_default { "*" } else { "" },
            pad(&d.bus, 8).trim_end(),
            mix,
            fmt_ms(d.shared_period_ms()),
            engmin,
            fmt_ms(d.default_period_ms),
            fmt_ms(d.min_period_ms),
            excl48
        );
    }
}

fn fmt_ms(v: f64) -> String {
    if v.is_nan() {
        "-".to_string()
    } else {
        format!("{v:.2}")
    }
}

fn inventory_notes(devices: &[DeviceInfo]) {
    println!();
    println!("  def         带 * 的是 Windows 当前的默认端点");
    println!("  bus         总线枚举器。HDAUDIO/USB/PCI/BTH* 是真硬件，其余是软件设备（虚拟声卡）");
    println!("  mix-format  共享模式引擎的格式。不是 48 kHz 的话我们每一帧都要过重采样");
    println!("  cur-ms      当前生效的共享模式引擎周期 —— 这是共享模式延迟的主项");
    println!("  engmin-ms   IAudioClient3 允许申请到的最小引擎周期（Win10+ 的低延迟共享模式）");
    println!("  devdef/min  GetDevicePeriod 的 default 和 minimum，后者是独占模式的周期下限");
    println!("  excl-48k    独占模式接不接受 48 kHz 16 位");

    println!();
    println!("==== 这台机器的可用余地 ====");

    for direction in [Direction::Render, Direction::Capture] {
        let hw: Vec<&DeviceInfo> = devices
            .iter()
            .filter(|d| d.direction == direction && d.is_hardware())
            .collect();
        let label = match direction {
            Direction::Render => "渲染",
            Direction::Capture => "采集",
        };
        if hw.is_empty() {
            println!("  {label}：没有真硬件端点，只有虚拟声卡 —— 这个方向测不出可信数字");
            continue;
        }
        let best_shared = hw
            .iter()
            .filter_map(|d| d.engine.map(|e| e.min_ms()))
            .fold(f64::INFINITY, f64::min);
        let best_excl = hw
            .iter()
            .map(|d| d.min_period_ms)
            .fold(f64::INFINITY, f64::min);
        let cur = hw
            .iter()
            .find(|d| d.is_default)
            .or(hw.first())
            .map(|d| d.shared_period_ms())
            .unwrap_or(f64::NAN);
        println!(
            "  {label}：真硬件 {} 个｜当前周期 {} ms｜低延迟共享最小 {} ms｜独占最小 {} ms",
            hw.len(),
            fmt_ms(cur),
            fmt_ms(best_shared),
            fmt_ms(best_excl)
        );
    }

    println!();
    for direction in [Direction::Render, Direction::Capture] {
        let label = match direction {
            Direction::Render => "默认渲染",
            Direction::Capture => "默认采集",
        };
        match devices
            .iter()
            .find(|d| d.direction == direction && d.is_default)
        {
            Some(d) if !d.is_hardware() => {
                println!(
                    "  注意：{label}端点「{}」是软件设备（bus={}）。",
                    d.name, d.bus
                );
                println!("        它在真硬件之上又叠了一层用户态路由，延迟会比硬件本身高。");
                println!("        定红线要用真硬件端点的数字，但用户实际经历的是这个。");
            }
            Some(d) => println!("  {label}端点「{}」是真硬件（bus={}）", d.name, d.bus),
            None => println!("  {label}：没有默认端点"),
        }
    }
}

/// 挑测哪些设备。
///
/// 两个都要：**真硬件**的端点（这台机器的能力上限，用来定红线），
/// 以及 **Windows 当前的默认端点**（用户实际经历的延迟，哪怕它是虚拟声卡）。
/// 只测前者会得出一个真实用户达不到的乐观值，只测后者又没法归因给硬件。
fn pick_targets(devices: &[DeviceInfo], all: bool) -> Vec<&DeviceInfo> {
    let mut out: Vec<&DeviceInfo> = Vec::new();
    for direction in [Direction::Capture, Direction::Render] {
        if all {
            out.extend(
                devices
                    .iter()
                    .filter(|d| d.direction == direction && d.is_hardware()),
            );
        }
        if let Some(d) = devices
            .iter()
            .find(|d| d.direction == direction && d.is_default)
        {
            out.push(d);
        }
        if let Some(d) = devices
            .iter()
            .find(|d| d.direction == direction && d.is_hardware())
        {
            out.push(d);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|d| seen.insert(d.id.clone()));
    out
}

fn measure_all(targets: &[&DeviceInfo], args: &Args) {
    println!();
    println!("==== 实测 ====");
    println!("全程灌静音，不保留任何音频。测采集会短暂点亮「麦克风正在使用」指示灯。");
    if args.exclusive {
        println!("独占模式已开：测独占的那几秒里，别的程序发不出声。");
    }
    println!();

    let mut modes = vec![ShareMode::Shared];
    if args.exclusive {
        modes.push(ShareMode::Exclusive);
    }

    println!(
        "{} {:<8} {:<14} {:<17} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>6}",
        pad("device", NAME_W),
        "dir",
        "mode@queue",
        "format",
        "buf-ms",
        "rep-ms",
        "p50",
        "p95",
        "max",
        "wake95",
        "glitch"
    );
    println!("{}", "-".repeat(NAME_W + 98));

    let mut results = Vec::new();
    for d in targets {
        for &mode in &modes {
            let batch = match d.direction {
                Direction::Capture => vec![measure::run_capture(d, mode, args.seconds)],
                // 渲染不是测一个数，是扫出撑得住的最浅队列深度 —— 见 measure.rs
                Direction::Render => measure::sweep_render(d, mode, args.seconds),
            };
            for m in batch {
                let suspect = !m.capture_timestamp_trustworthy();
                match &m.outcome {
                    Ok(r) => println!(
                    "{} {:<8} {:<14} {:<17} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>7.2} {:>6}",
                    pad(&m.device, NAME_W),
                    m.direction.to_string(),
                    m.label,
                    r.format,
                    r.buffer_ms,
                    r.reported_latency_ms,
                    r.measured.p50,
                    r.measured.p95,
                    r.measured.max,
                    r.wake.p95,
                    r.glitches
                ),
                    Err(e) => println!(
                        "{} {:<8} {:<14} {}",
                        pad(&m.device, NAME_W),
                        m.direction.to_string(),
                        m.label,
                        e
                    ),
                }
                if suspect && m.outcome.is_ok() {
                    println!(
                        "{}   ^ 这一行的采集延迟不可信：{:.2} ms 小于四分之一个周期（{:.1} ms），",
                        " ".repeat(NAME_W),
                        m.outcome
                            .as_ref()
                            .map(|r| r.measured.p50)
                            .unwrap_or(f64::NAN),
                        m.period_ms
                    );
                    println!(
                    "{}     音频物理上至少要攒够一个周期才可能交付 —— 这个时间戳不是设备记录时刻",
                    " ".repeat(NAME_W)
                );
                }
                results.push((d.is_hardware(), m));
            }
        }
    }

    println!("{}", "-".repeat(NAME_W + 98));
    println!("               从一个周期起往上扫，撑住一个就停 —— 再深只是更安全、延迟更差");
    println!("  mode@queue   渲染的 @N 是**实际维持的队列深度**，也就是渲染吃掉的延迟");
    println!("  p50/p95/max  采集：设备记录到我们拿到手的实测延迟");
    println!("               渲染：欠载余量（补数据前队列还剩多少）。**不是延迟**，");
    println!("               离咔哒声还有多远。余量 = 队列深度 - 一个周期");
    println!("  buf-ms       Initialize 实际给到的缓冲时长");
    println!("  rep-ms       驱动自报的 GetStreamLatency。0 说明驱动压根没填，别当回事");
    println!("  wake95       事件唤醒间隔的 p95。跟周期差太多说明线程醒不准，低延迟是假的");
    println!("  glitch       采集的数据不连续 / 渲染的欠载。不是 0 说明这个配置扛不住");

    budget(&results);
}

/// 把测出来的采集 + 渲染加起来，跟 M1 里那个 30 ms 的假设对一对。
///
/// **只用真硬件端点的数字。** 虚拟声卡（SteelSeries Sonar 这类）两边都不可信：
/// 采集侧它报的 QPC 时间戳是自己造的，只覆盖「路由把数据交给我们」那一段，
/// 不含上游真声卡把声音数字化的那一段；渲染侧同理，它的队列深度也只是它自己
/// 那一层。所以虚拟声卡会测出一个**比硬件还低**的数 —— 那不是更快，那是没测全。
fn budget(results: &[(bool, measure::Measurement)]) {
    // 采集：取真硬件里最低的 p95。
    let capture = |mode: ShareMode| -> Option<f64> {
        results
            .iter()
            .filter(|(hw, m)| *hw && m.direction == Direction::Capture && m.share_mode == mode)
            .filter_map(|(_, m)| m.latency_ms())
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a: f64| a.min(v)))
            })
    };
    // 渲染：两个数分开报。
    //   floor —— 最浅的零欠载深度，物理下限，但余量可能只有一帧
    //   safe  —— 还留得住半个周期余量的最浅深度，这才是能发布的工作点
    let render = |mode: ShareMode, need_headroom: bool| -> Option<(String, f64)> {
        results
            .iter()
            .filter(|(hw, m)| *hw && m.direction == Direction::Render && m.share_mode == mode)
            .filter(|(_, m)| {
                if need_headroom {
                    m.has_headroom()
                } else {
                    m.clean()
                }
            })
            .filter_map(|(_, m)| m.latency_ms().map(|v| (m.label.clone(), v)))
            .fold(None, |acc: Option<(String, f64)>, v| {
                Some(match acc {
                    Some(a) if a.1 <= v.1 => a,
                    _ => v,
                })
            })
    };

    println!();
    println!("{}", "=".repeat(84));
    println!("M2 结论：设备到底吃掉多少");
    println!("{}", "=".repeat(84));

    let mut shared_total = None;
    for mode in [ShareMode::Shared, ShareMode::Exclusive] {
        let cap = capture(mode);
        let floor = render(mode, false);
        let safe = render(mode, true);
        if cap.is_none() && floor.is_none() && safe.is_none() {
            continue;
        }
        println!();
        println!("  {mode} 模式（只算真硬件端点）");
        match cap {
            Some(c) => println!("    采集实测 p95        {c:.2} ms"),
            None => {
                println!("    采集                测不出可信数字（时间戳不可信，见上面的标注）")
            }
        }
        if let Some((label, r)) = &floor {
            println!("    渲染理论下限        {r:.2} ms（{label}）");
            println!("                        零欠载，但余量可能只有一帧 —— 漏醒一次就咔哒");
        }
        match &safe {
            Some((label, r)) => {
                println!("    渲染可用工作点      {r:.2} ms（{label}，余量 >= 一个周期）");
                if let Some(c) = cap {
                    let total = c + r;
                    println!("    合计（按可用工作点）{total:.2} ms");
                    if mode == ShareMode::Shared {
                        shared_total = Some(total);
                    }
                }
            }
            None => println!("    渲染                没有一个深度既不欠载又留得住一个周期的余量"),
        }
        if mode == ShareMode::Exclusive {
            println!("    独占只是标定上限 —— 独占期间游戏本身发不出声，开黑软件不能用这个模式。");
        }
    }

    let Some(total) = shared_total else {
        println!();
        println!("  共享模式没测全，算不出预算。");
        return;
    };

    let e2e = M1_PROTOCOL_P95_MS + total;
    println!();
    println!("  对账 M1 的假设：");
    println!(
        "    协议 {:.1}（M1 实测）+ 设备 {:.1}（本次实测）= 端到端 {:.1} ms，红线 {:.0}",
        M1_PROTOCOL_P95_MS,
        total,
        e2e,
        redline::E2E_MS
    );
    if e2e <= redline::E2E_MS {
        println!("    >>> 进线，余量 {:.1} ms", redline::E2E_MS - e2e);
    } else {
        println!("    >>> 破线 {:.1} ms", e2e - redline::E2E_MS);
    }
    let drift = total - redline::DEVICE_BUDGET_MS;
    println!(
        "    redline::DEVICE_BUDGET_MS = {:.1} ms，本次实测 {:.1} ms，差 {:+.1} ms",
        redline::DEVICE_BUDGET_MS,
        total,
        drift
    );
    if drift.abs() > 2.0 {
        println!("    >>> 差得有点多，该把 DEVICE_BUDGET_MS 改成本次实测值了");
    }

    println!();
    println!("  这个数字**不**代表所有用户：");
    println!("    - 虚拟声卡（SteelSeries Sonar / 雷蛇 / Voicemeeter / 直播虚拟线）没算进来。");
    println!("      它们在真硬件之上再叠一层，用户实际延迟只会更高，不会更低。");
    println!("      上面表里虚拟设备测出来比硬件还低，那不是更快，是这种量法测不到它的上游。");
    println!("    - 蓝牙耳机没算进来，那是另一个量级（编解码器本身就几十毫秒）。");

    println!();
    println!("  还没测的：");
    println!("    - 声学往返（音箱 → 空气 → 麦克风）。要出声，得单独做");
    println!("    - APM（AEC3/降噪/AGC）的延迟与 CPU");
    println!("    - 采集和渲染两个时钟不同步时的重采样代价");
}
