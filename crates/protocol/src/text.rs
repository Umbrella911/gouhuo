// SPDX-License-Identifier: MIT OR Apache-2.0

//! 解析用户粘贴进来的文本时会踩的坑。
//!
//! # 为什么这么一个小函数值得单独一个模块
//!
//! 下面这个写法看着人畜无害，实际上会 panic：
//!
//! ```ignore
//! if s.len() >= PREFIX.len() && s[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) { ... }
//! ```
//!
//! `s.len()` 是**字节数**，`s[..n]` 也是按字节切。用户往输入框里粘一段中文，
//! 长度检查轻松通过（一个汉字三字节），然后切片落在字符中间 —— 直接 panic。
//!
//! 这不是假想：邀请码和身份导入这两个框，正是最容易被粘进中文的地方
//! （「你发我的那个码呢」「这是什么东西」）。客户端不能因为用户粘错了东西就崩。
//!
//! 所以这个判断只写一次，两个解析器共用。

/// 大小写不敏感地剥掉 ASCII 前缀。**对任意 UTF-8 输入都安全，不会 panic。**
///
/// `prefix` 必须是纯 ASCII（调用方保证；本项目里都是 `gouhuo://j/` 这类字面量）。
pub fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let n = prefix.len();
    // is_char_boundary 同时挡住了「太短」和「切在字符中间」两种情况。
    if !s.is_char_boundary(n) {
        return None;
    }
    let (head, rest) = s.split_at(n);
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_when_it_matches() {
        assert_eq!(
            strip_prefix_ignore_ascii_case("gouhuo://j/abc", "gouhuo://j/"),
            Some("abc")
        );
        assert_eq!(
            strip_prefix_ignore_ascii_case("GOUHUO://J/abc", "gouhuo://j/"),
            Some("abc")
        );
        assert_eq!(strip_prefix_ignore_ascii_case("abc", ""), Some("abc"));
    }

    #[test]
    fn returns_none_when_it_does_not() {
        assert_eq!(
            strip_prefix_ignore_ascii_case("nope://abc", "gouhuo://j/"),
            None
        );
        assert_eq!(strip_prefix_ignore_ascii_case("short", "gouhuo://j/"), None);
        assert_eq!(strip_prefix_ignore_ascii_case("", "gouhuo://j/"), None);
    }

    /// 这条就是当初那个 panic。
    #[test]
    fn never_panics_on_multibyte_input() {
        for s in [
            "随便一串东西", // 纯中文，字节数够长但边界对不上
            "你发我的那个码呢？",
            "🎮🎧",                  // emoji 是四字节
            "a中b文c",               // 混排
            "\u{1F600}gouhuo://j/x", // 前面挂一个四字节字符
        ] {
            // 不崩就算过；返回什么都行
            let _ = strip_prefix_ignore_ascii_case(s, "gouhuo://j/");
            let _ = strip_prefix_ignore_ascii_case(s, "gouhuo-secret-v1-");
        }
    }

    /// 前缀长度正好落在某个多字节字符中间 —— 最容易漏掉的那种情况。
    #[test]
    fn handles_boundary_landing_mid_character() {
        // "中" 是 3 字节，前缀 2 字节 -> 边界落在字符内部
        assert_eq!(strip_prefix_ignore_ascii_case("中", "ab"), None);
        // 前缀 3 字节，正好跨过一个汉字 -> 是合法边界，但内容对不上
        assert_eq!(strip_prefix_ignore_ascii_case("中x", "abc"), None);
    }
}
