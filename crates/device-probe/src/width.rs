// SPDX-License-Identifier: GPL-3.0-or-later
//! 按**显示宽度**而不是字符数对齐。
//!
//! 设备名里全是中文（扬声器、耳机式麦克风、内部 AUX 插座），Rust 的
//! `{:<20}` 数的是 char 数，中日韩字符在终端里占两格，直接用就永远对不齐。
//!
//! 这里只做终端表格够用的近似，不是完整的 Unicode 宽度实现：
//! 常见的中日韩和全角标点算 2，其余算 1。组合字符、emoji、变体选择符
//! 都不在这个项目的设备名里会出现的范围内。

/// 一个字符在等宽终端里占几格。
pub fn char_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F        // 朝鲜文字母
        | 0x2E80..=0x303E      // 中日韩部首、符号（含全角标点）
        | 0x3041..=0x33FF      // 假名、注音、兼容字符
        | 0x3400..=0x4DBF      // 中日韩扩展 A
        | 0x4E00..=0x9FFF      // 中日韩统一表意文字
        | 0xA000..=0xA4CF      // 彝文
        | 0xAC00..=0xD7A3      // 谚文音节
        | 0xF900..=0xFAFF      // 兼容表意文字
        | 0xFE30..=0xFE6F      // 中日韩兼容形式
        | 0xFF00..=0xFF60      // 全角形式
        | 0xFFE0..=0xFFE6      // 全角符号
        | 0x1F300..=0x1F9FF    // emoji
        | 0x20000..=0x3FFFD    // 中日韩扩展 B 及以后
    );
    if wide {
        2
    } else {
        1
    }
}

pub fn width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// 左对齐补到 `target` 格宽。超宽的话截断并加省略号，保证列不会被撑破 ——
/// 表格错位比名字看不全更难读。
pub fn pad(s: &str, target: usize) -> String {
    let w = width(s);
    if w <= target {
        let mut out = String::from(s);
        out.push_str(&" ".repeat(target - w));
        return out;
    }
    // 留一格给省略号
    let budget = target.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if used + cw > budget {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('~');
    out.push_str(&" ".repeat(target - used - 1));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_wide() {
        assert_eq!(width("Realtek"), 7);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn cjk_is_two_wide() {
        assert_eq!(width("扬声器"), 6);
        assert_eq!(width("耳机式麦克风"), 12);
        assert_eq!(width("内部 AUX 插座"), 4 + 1 + 3 + 1 + 4);
    }

    #[test]
    fn pad_reaches_target_width_not_char_count() {
        for s in ["扬声器", "Realtek", "内部 AUX 插座", ""] {
            assert_eq!(width(&pad(s, 20)), 20, "{s:?} 没补到 20 格");
        }
    }

    #[test]
    fn overlong_is_truncated_to_exact_width() {
        let s = "SteelSeries Sonar - Gaming 这是一个很长的设备名";
        let p = pad(s, 12);
        assert_eq!(width(&p), 12);
        assert!(p.contains('~'));
    }

    /// 截断不能把一个中文字符劈成半个格子。
    #[test]
    fn truncation_never_splits_a_wide_char() {
        // 目标宽度 6：省略号占 1，剩 5 格放得下 2 个中文（4 格），第 3 个放不下
        let p = pad("扬声器耳机", 6);
        assert_eq!(width(&p), 6);
        assert!(p.starts_with("扬声"), "{p:?}");
    }
}
