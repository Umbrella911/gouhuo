// SPDX-License-Identifier: MIT OR Apache-2.0

//! Crockford base32。邀请链接就是用它编码的。
//!
//! # 为什么手写
//!
//! 这个 crate 是「协议定义」，要能被任何人用任何语言照着实现。依赖越少越好 ——
//! 一个 40 行的编解码不值得拉一个 crate 进来，还让别的语言的实现者多一件事要查。
//!
//! # 为什么是 Crockford 而不是 RFC 4648
//!
//! 邀请链接主要靠复制粘贴，但总会有人在电话里念、在纸上抄、或者从截图里认。
//! Crockford 的字母表刻意去掉了 `I` `L` `O` `U`：
//! - 去掉 `I` `L` `O` 是因为它们跟 `1` `1` `0` 长得太像
//! - 去掉 `U` 是为了降低意外拼出脏字的概率
//!
//! 解码时反过来宽容：`I` `i` `L` `l` 都当 `1`，`O` `o` 当 `0`，大小写不敏感，
//! 连字符直接忽略（有人会自己加分隔符）。
//!
//! 输出统一小写 —— 小写在聊天软件里不容易被自动首字母大写搞坏。

/// Crockford 的字母表，小写。刻意没有 i l o u。
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u16 = 0;
    let mut bits: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u16;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1F) as usize;
            out.push(ALPHABET[idx] as char);
        }
    }
    // 收尾：不足 5 位的补零。不加 padding —— base32 的 `=` 在 URL 里碍事，
    // 而我们的负载长度是自描述的，不需要靠 padding 还原。
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1F) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidChar(pub char);

impl core::fmt::Display for InvalidChar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "邀请码里有不认识的字符：{:?}", self.0)
    }
}

impl std::error::Error for InvalidChar {}

fn decode_char(c: char) -> Result<u8, InvalidChar> {
    let v = match c.to_ascii_lowercase() {
        '0' => 0,
        // 宽容：手抄和 OCR 最容易把这三个认错
        '1' | 'i' | 'l' => 1,
        '2' => 2,
        '3' => 3,
        '4' => 4,
        '5' => 5,
        '6' => 6,
        '7' => 7,
        '8' => 8,
        '9' => 9,
        'a' => 10,
        'b' => 11,
        'c' => 12,
        'd' => 13,
        'e' => 14,
        'f' => 15,
        'g' => 16,
        'h' => 17,
        'j' => 18,
        'k' => 19,
        'm' => 20,
        'n' => 21,
        'o' => 0, // 同上，o 当 0
        'p' => 22,
        'q' => 23,
        'r' => 24,
        's' => 25,
        't' => 26,
        'v' => 27,
        'w' => 28,
        'x' => 29,
        'y' => 30,
        'z' => 31,
        other => return Err(InvalidChar(other)),
    };
    Ok(v)
}

/// 解码。连字符和空白一律忽略 —— 用户会自己加分隔符，也会从聊天记录里带进换行。
pub fn decode(text: &str) -> Result<Vec<u8>, InvalidChar> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8 + 1);
    let mut buffer: u16 = 0;
    let mut bits: u32 = 0;

    for c in text.chars() {
        if c == '-' || c.is_whitespace() {
            continue;
        }
        buffer = (buffer << 5) | decode_char(c)? as u16;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // 剩下的不足 8 位是编码时补的零，丢掉。
    // 这里**不**校验补位是否真的为零：多一位少一位由上层的校验和兜住，
    // 而对用户来说「校验和不对」比「base32 补位非零」好懂得多。
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_various_lengths() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len)
                .map(|i| (i as u8).wrapping_mul(37).wrapping_add(11))
                .collect();
            let encoded = encode(&data);
            assert_eq!(decode(&encoded).unwrap(), data, "长度 {len} 没往返回来");
        }
    }

    #[test]
    fn output_avoids_confusable_letters() {
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&data);
        for bad in ['i', 'l', 'o', 'u'] {
            assert!(!encoded.contains(bad), "输出里不该出现 {bad:?}");
        }
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn decoding_is_forgiving() {
        let data = b"gouhuo";
        let canonical = encode(data);
        // 大写、混入连字符和空白、把 0/1 抄成 O/l —— 都该还原成同一个东西
        let mangled = canonical
            .to_uppercase()
            .chars()
            .map(|c| match c {
                '0' => 'O',
                '1' => 'l',
                other => other,
            })
            .collect::<String>();
        let spaced = format!(" {}-{} \n", &mangled[..4], &mangled[4..]);
        assert_eq!(decode(&spaced).unwrap(), data);
    }

    #[test]
    fn rejects_unknown_characters() {
        assert_eq!(decode("abc!"), Err(InvalidChar('!')));
        // 中文标点也要报得明白，别 panic
        assert!(decode("abc，def").is_err());
    }

    #[test]
    fn empty_roundtrips() {
        assert_eq!(encode(&[]), "");
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }

    /// 编码长度要可预测：每 5 字节 8 个字符，不足的向上取整。
    #[test]
    fn encoded_length_is_ceil_of_eight_fifths() {
        for len in 0..40usize {
            let encoded = encode(&vec![0u8; len]);
            assert_eq!(encoded.len(), (len * 8).div_ceil(5), "长度 {len}");
        }
    }
}
