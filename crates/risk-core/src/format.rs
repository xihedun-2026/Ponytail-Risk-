//! 展示层格式化。行为与 `tools/wdsf_live_data.py` 的 `number` / `stamp_label` 逐字对齐。

/// 等价于 Python `f"{int(value or 0):,}"`。
pub fn number(value: i64) -> String {
    let negative = value < 0;
    // i64::MIN 取负会溢出，先转 i128 再取绝对值。
    let mut digits = (value as i128).unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    let bytes = digits.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 && (bytes.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(*byte as char);
    }
    digits.clear();
    if negative {
        digits.push('-');
        digits.push_str(&out);
        digits
    } else {
        out
    }
}

/// Python 里 `number()` 常常收到字符串或 None。空/非数字一律按 0 处理，
/// 与 `int(value or 0)` 在本项目实际取值范围内的行为一致。
pub fn number_loose(value: &str) -> String {
    number(value.trim().parse::<i64>().unwrap_or(0))
}

/// Python `str.isdigit()` 在本项目的 ASCII 取值范围内的等价实现。
pub fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// `YYYYMMDDHHMMSS` -> `MM-DD HH:MM:SS`；其余原样返回，空值返回「未知时间」。
pub fn stamp_label(value: &str) -> String {
    if value.len() == 14 && is_ascii_digits(value) {
        format!(
            "{}-{} {}:{}:{}",
            &value[4..6],
            &value[6..8],
            &value[8..10],
            &value[10..12],
            &value[12..14],
        )
    } else if value.is_empty() {
        "未知时间".to_string()
    } else {
        value.to_string()
    }
}

/// 等价于 Python `value.strip().strip(":").upper()[:96]`。
pub fn normalized_iid(value: &str) -> String {
    value
        .trim()
        .trim_matches(':')
        .to_uppercase()
        .chars()
        .take(96)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_matches_python_thousands_separator() {
        assert_eq!(number(0), "0");
        assert_eq!(number(1), "1");
        assert_eq!(number(999), "999");
        assert_eq!(number(1_000), "1,000");
        assert_eq!(number(9_582_200), "9,582,200");
        assert_eq!(number(1_000_000_000), "1,000,000,000");
        assert_eq!(number(-1_234), "-1,234");
        assert_eq!(number(i64::MIN), "-9,223,372,036,854,775,808");
    }

    #[test]
    fn number_loose_treats_blank_as_zero() {
        assert_eq!(number_loose(""), "0");
        assert_eq!(number_loose("12345"), "12,345");
    }

    #[test]
    fn stamp_label_formats_wdsf_timestamps() {
        assert_eq!(stamp_label("20260101000000"), "01-01 00:00:00");
        assert_eq!(stamp_label("20260730142108"), "07-30 14:21:08");
        assert_eq!(stamp_label(""), "未知时间");
        assert_eq!(stamp_label("not-a-stamp"), "not-a-stamp");
        // 14 位但非全数字，走原样分支。
        assert_eq!(stamp_label("2026010100000x"), "2026010100000x");
    }

    #[test]
    fn normalized_iid_strips_colons_and_uppercases() {
        // 对应 Python self_check 断言。
        assert_eq!(
            normalized_iid(":6a617f69000102542fd9:"),
            "6A617F69000102542FD9"
        );
        assert_eq!(normalized_iid("  :a1:  "), "A1");
        assert_eq!(normalized_iid(""), "");
    }
}
