#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""检查每个 .rs 文件头的 SPDX 标识跟它所属 crate 的许可一致。

为什么要有这个检查：仓库是**混合许可**的 —— protocol 宽松、引擎和服务端 MPL、
客户端 GPL。而 MPL 是**文件级** copyleft，边界就是靠每个文件头那一行划的。
一个标着 GPL 的文件躺在 MPL 的 crate 里，不是不整洁，是边界真的模糊了。

这事会自己复发：新文件基本都是照抄同目录下的旧文件，而旧文件可能带着旧许可。
服务端改 MPL 那次就抓到一个 —— wasapi/live.rs 是前一个 commit 新增的，
头还写着 GPL。靠人眼盯不住。

期望值从 `cargo metadata` 读 crate 的 license 字段，**不在这里重复写一份
crate → 许可的映射**。两处各写一份迟早会漂移，而漂移的那天 CI 是绿的。

本地跑：python scripts/check-spdx.py
"""

import json
import pathlib
import subprocess
import sys

PREFIX = "// SPDX-License-Identifier: "


def main() -> int:
    repo = pathlib.Path(__file__).resolve().parent.parent

    # --no-deps：只读工作区自己的清单，不解析依赖树，所以不碰网络也不需要
    # 先把依赖拉下来 —— 这个 job 跑在干净的 ubuntu runner 上。
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            # encoding 要写死 utf-8：cargo 输出的是 UTF-8，但 Windows 上
            # text=True 默认拿系统编码（GBK）去解，撞上中文 description 就炸。
            cwd=repo, capture_output=True, text=True,
            encoding="utf-8", check=True,
        ).stdout
    except subprocess.CalledProcessError as e:
        print(f"cargo metadata 跑失败：\n{e.stderr}", file=sys.stderr)
        return 2

    problems: list[str] = []
    checked = 0

    for pkg in sorted(json.loads(out)["packages"], key=lambda p: p["name"]):
        want = pkg.get("license")
        if not want:
            problems.append(
                f"{pkg['name']}: Cargo.toml 没有 license 字段。"
                f"混合许可的仓库里每个 crate 都要显式声明，见 LICENSING.md"
            )
            continue

        crate_dir = pathlib.Path(pkg["manifest_path"]).parent
        expected = PREFIX + want

        for f in sorted(crate_dir.rglob("*.rs")):
            # 构建产物不算源码：OUT_DIR 里的生成代码、本地 target/。
            if "target" in f.relative_to(crate_dir).parts:
                continue
            checked += 1

            first = f.read_text(encoding="utf-8").split("\n", 1)[0].strip()
            rel = f.relative_to(repo).as_posix()

            if not first.startswith(PREFIX):
                problems.append(f"{rel}: 没有 SPDX 头，应该是 {expected!r}")
            elif first != expected:
                found = first[len(PREFIX):]
                problems.append(
                    f"{rel}: 标的是 {found}，但所属 crate `{pkg['name']}` 是 {want}"
                )

    if problems:
        print(f"SPDX 头有 {len(problems)} 处对不上：\n", file=sys.stderr)
        for p in problems:
            print(f"  {p}", file=sys.stderr)
        print(
            "\n新文件请照抄同目录下已有文件的头。"
            "哪个 crate 是什么许可见 LICENSING.md。",
            file=sys.stderr,
        )
        return 1

    print(f"{checked} 个 .rs 文件的 SPDX 头都跟所属 crate 的许可一致。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
