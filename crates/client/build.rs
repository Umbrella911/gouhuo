// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    // fluent-dark 只影响 std-widgets 里那几个控件（这里用到的是 ScrollView）。
    // 别的都是自己画的，见 ui/theme.slint。
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("编译 .slint 失败");
}
