//! GBK ↔ UTF-8 编解码。
//!
//! RISK 的文本列以 GBK 字节存放在 latin1 列里。Python 版通过
//! `charset="latin1"` 拿到「按 latin1 解码的字符串」，再 `encode("latin1").decode("gbk")`
//! 还原。Rust 版直接连接设为 latin1（单字节、字节透明），
//! 从驱动取原始字节后一次性 GBK 解码，少一次往返转换。

use encoding_rs::GBK;

/// 原始列字节 -> UTF-8 字符串。无法按 GBK 解码时回退到 latin1 原样保留，
/// 对应 Python 里解码失败就返回原值的分支。
pub fn decode_bytes(raw: &[u8]) -> String {
    // 必须用 decode_without_bom_handling：`decode` 会做 BOM 嗅探，
    // 列值恰好以 FF FE / EF BB BF 开头时会被当成 BOM 吞掉甚至改用别的编码。
    let (decoded, had_errors) = GBK.decode_without_bom_handling(raw);
    if had_errors {
        // 回退：按 latin1 逐字节映射，保证不丢数据、不 panic。
        return raw.iter().map(|byte| *byte as char).collect();
    }
    decoded.into_owned()
}

/// UTF-8 查询参数 -> GBK 字节，用于按角色名等中文字段查询。
/// 无法编码时回退为 UTF-8 字节，对应 Python `database_value` 的 except 分支。
pub fn encode_query(value: &str) -> Vec<u8> {
    let (encoded, _, had_errors) = GBK.encode(value);
    if had_errors {
        return value.as_bytes().to_vec();
    }
    encoded.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_gbk_currency_labels() {
        // 「金元宝」的 GBK 字节。
        assert_eq!(
            decode_bytes(&[0xBD, 0xF0, 0xD4, 0xAA, 0xB1, 0xA6]),
            "金元宝"
        );
        // 「银元宝」的 GBK 字节。
        assert_eq!(
            decode_bytes(&[0xD2, 0xF8, 0xD4, 0xAA, 0xB1, 0xA6]),
            "银元宝"
        );
    }

    #[test]
    fn ascii_passes_through_unchanged() {
        assert_eq!(decode_bytes(b"1003281"), "1003281");
        assert_eq!(decode_bytes(b""), "");
    }

    #[test]
    fn round_trips_chinese_player_names() {
        for name in ["北境长歌", "山海一梦", "青竹小号07", "玄天令"] {
            assert_eq!(decode_bytes(&encode_query(name)), name);
        }
    }

    #[test]
    fn invalid_gbk_falls_back_without_panicking() {
        // 0xFF 在 GBK 中非法，必须回退而不是 panic 或丢字节。
        let decoded = decode_bytes(&[0xFF, 0xFE]);
        assert_eq!(decoded.chars().count(), 2);
    }

    #[test]
    fn bom_like_prefixes_are_not_swallowed() {
        // FF FE 是 UTF-16LE BOM、EF BB BF 是 UTF-8 BOM。
        // 列值恰好这样开头时必须原样保留，不能被 BOM 嗅探吃掉。
        assert_eq!(decode_bytes(&[0xFF, 0xFE]).chars().count(), 2);
        assert!(!decode_bytes(&[0xEF, 0xBB, 0xBF]).is_empty());

        // BOM 字节后面跟正常内容时，内容不能丢。
        let mut raw = vec![0xEF, 0xBB, 0xBF];
        raw.extend_from_slice(b"1003281");
        assert!(decode_bytes(&raw).ends_with("1003281"));
    }

    #[test]
    fn encode_query_keeps_ascii_identifiers_intact() {
        assert_eq!(encode_query("1003281"), b"1003281".to_vec());
    }
}
