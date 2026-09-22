# 许可说明

一句话：**协议宽松，引擎能嵌入，成品是 copyleft 的。**

分三层，因为这三层面对的威胁根本不是一回事：

| 层 | crate | 许可 |
|---|---|---|
| **协议** | `protocol` | **MIT OR Apache-2.0** |
| **引擎** | `voice-core`、`transport` | **MPL-2.0** |
| **成品** | `client`、`client-core`、`server`、探针 | **GPL-3.0-or-later** |
| **第三方** | `third_party/webrtc-audio-processing-sys` | BSD-3-Clause（Google / WebRTC，原样保留） |

许可全文：[`LICENSE`](LICENSE)（GPL-3.0）、[`LICENSES/MPL-2.0.txt`](LICENSES/MPL-2.0.txt)、
[`LICENSES/MIT.txt`](LICENSES/MIT.txt)、[`LICENSES/Apache-2.0.txt`](LICENSES/Apache-2.0.txt)。

每个 `.rs` 文件头部的 `SPDX-License-Identifier` 是**权威来源**，manifest 里的
`license` 字段跟它一致。MPL 是文件级 copyleft，边界就是靠每个文件的这行划的。

---

## 为什么 `protocol` 是宽松的

`protocol` 是客户端和服务端共享的**唯一一份**协议定义：包格式、序列化、加密。
把它放开，是为了让别人能自由地：

- 用别的语言写第三方客户端
- 写机器人、录制工具、桥接
- 在自己的项目里直接实现这个协议，不受任何回馈义务约束

**照着协议写客户端的人越多，这个协议的生态越好** —— 这是收益不是损失。
而且它本来就是刻意抽出来的一份定义，让它自由流通符合它存在的理由。

技术上也没有冲突：`protocol` 不依赖任何 copyleft 的东西。

## 为什么引擎是 MPL

`voice-core` 和 `transport` 要能**链进别人的闭源产品**。GPL 做不到这件事 ——
一个闭源游戏没法链接 GPL 库，这不是商量的余地，是许可本身的效力。

MPL-2.0 是这里唯一合适的档：

- **文件级 copyleft**。改了这两个 crate 里的文件，那些文件的改动要公开；
  但把它们链接进任何许可的产品（包括闭源商业产品）都没有问题，
  静态链接动态链接都一样。这正好是「别人能用，但改了引擎要交回来」
- **跟 GPL 兼容**。所以 GPL 的 `client` 依赖 MPL 的 `voice-core` 完全合法，
  依赖图一行都不用改。这也是**没有**采用 Exhibit B
  （"Incompatible With Secondary Licenses"）的原因 —— 加了那条就不兼容了
- **依赖全吃得下**：libopus (BSD-3)、libwebrtc APM (BSD-3)、
  所有 Rust crate (MIT / Apache-2.0)

### 为什么不是 LGPL

LGPL 看着是「中间路线」，在 Rust 里是陷阱。LGPL 要求接收方能替换库**重新链接**；
Rust 默认静态链接，iOS 和主机平台（PS / Switch / Xbox）基本都是静态链接 + NDA，
那个义务根本没法满足。法务看到 LGPL 会直接否掉。

### 为什么不是 MIT / Apache

那两个等于放弃「改了引擎要交回来」。MPL 的成本对使用者几乎为零
（不改就没有任何义务），却保住了这一条，没理由不要。

## 为什么成品是 GPL

**不想被套壳。** 开麦这个成品是给玩家做的开源工具，不是给人拿去加广告、加后门、
改成收费版再分发的素材。GPL 要求分发修改版时带上源码，这条正好挡住那种做法。

注意这条只约束**成品**。引擎放开了，成品不放 —— 想拿引擎去做自己的东西可以，
想拿开麦本身套个壳卖不行。

顺带解决一个实际问题：**UI 框架的选择自由。** 一些很适合这个场景的 Rust GUI
框架（比如 Slint）提供 GPL 档。`client` 是 GPL 的话，那一档直接可用，
没有署名条件和使用限制。

## 为什么不是 AGPL

AGPL 多管的是「改了代码只拿去跑服务、不分发二进制」。对这个项目：

- **收益很小**：真想拿去做商业语音服务的，瓶颈是带宽和运维不是代码；
  而且他们更可能直接拿 Mumble（BSD，随便用）
- **代价很实在**：很多公司的法务政策直接禁止 AGPL，而这个产品的核心卖点
  就是自部署 —— 不想在公会、战队、公司内部开黑群的 IT 那里被卡住

GPL-3.0 够了，AGPL 是过度防御。

## 插件不受影响

插件走 **gRPC + 独立进程**（Discord bot 模型）。独立进程通过定义好的协议通信，
插件**不构成衍生作品** —— 任何许可的插件都能接，包括闭源商业插件。

这不是钻空子，是 copyleft 的标准边界。而插件用独立进程本来就是设计决定
（崩了不影响主服务、任何语言都能写），恰好也落在边界的正确一侧。

如果当初选的是「动态库 in-process 加载插件」，GPL 会卡死整个插件生态。

## 对普通用户没有任何影响

GPL 的义务只在**分发**时产生。

- 下载、安装、使用 —— 没有任何义务
- 自己架服务器给朋友用 —— 没有任何义务
- 改了代码自己用 —— 没有任何义务
- **把修改过的版本发给别人** —— 这时才要带上源码

所以 GPL 不会让「好用」打折。

## 贡献：用 DCO，不用 CLA

见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

简单说：提交时加一行 `Signed-off-by`，声明「这段代码我有权按本项目的许可提交」。
不需要把版权转让给任何人 —— 贡献者保留自己的版权。

代价是**许可从此基本锁死**：没有版权集中，就没法单方面改许可，也没法做
开源 + 商业双授权。这是刻意的取舍 —— 这个项目的定位是给玩家做的开源工具，
不是拿开源引流的商业软件。社区气质比商业期权重要。

## 第三方代码

`third_party/webrtc-audio-processing-sys/` 是 `webrtc-audio-processing-sys` 2.1.0
打过 Windows 补丁的副本，**原样保留上游的 BSD-3-Clause 许可**
（见该目录下的 `COPYING`）。补丁本身很小、全部用 `kaimai patch:` 标出，
理应回馈上游；合并之后这个目录就可以删掉。

为什么要 vendor：见 [`docs/m2-apm-windows.md`](docs/m2-apm-windows.md)。
