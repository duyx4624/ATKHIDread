//! ATK 2.4G 鼠标电量协议（ATK1：`0xFF02` 接口，固定 17 字节帧）。
//!
//! 帧结构与校验规则来自实测，而非文档（厂商未公开）：
//!
//! 请求：`08 04 00×14 49`
//! 应答：`08 04 00 00 00 02 <level> <wired> …  <checksum>`
//!
//! 校验：`(sum(byte[0..15]) + byte[16]) & 0xFF == 0x55`

/// HID Report ID，两种模式（无线 `11D9` / 有线 `11E7`）均为 0x08。
pub const REPORT_ID: u8 = 0x08;

/// 读电量命令码。
pub const CMD_BATTERY: u8 = 0x04;

/// 应答帧长度（含 Report ID）。
pub const FRAME_LEN: usize = 17;

/// 校验基准值。
const CHECKSUM_BASE: u16 = 0x55;

/// 电量在应答帧中的偏移。
const LEVEL_INDEX: usize = 6;

/// 有线标志在应答帧中的偏移，非 0 表示已插线。
const WIRED_INDEX: usize = 7;

/// 一次成功解析的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// 电量百分比，已 clamp 到 0..=100。
    pub level: u8,
    /// 是否处于有线模式（插着 USB 线）。
    pub wired: bool,
}

/// 构造 17 字节的读电量请求帧。
pub fn request() -> [u8; FRAME_LEN] {
    let mut cmd = [0u8; 16];
    cmd[0] = CMD_BATTERY;
    cmd[15] = checksum(REPORT_ID, &cmd);

    let mut frame = [0u8; FRAME_LEN];
    frame[0] = REPORT_ID;
    frame[1..].copy_from_slice(&cmd);
    frame
}

/// `(0x55 - (report_id + sum(cmd[0..14]))) & 0xFF`
pub fn checksum(report_id: u8, cmd: &[u8]) -> u8 {
    let sum: u16 = report_id as u16 + cmd.iter().take(15).map(|b| *b as u16).sum::<u16>();
    (CHECKSUM_BASE.wrapping_sub(sum & 0xFF) & 0xFF) as u8
}

/// 校验应答帧：长度、Report ID、命令回显、校验和。
pub fn is_valid(reply: &[u8]) -> bool {
    if reply.len() < FRAME_LEN || reply[0] != REPORT_ID || reply[1] != CMD_BATTERY {
        return false;
    }
    let sum: u16 = reply[..16].iter().map(|b| *b as u16).sum::<u16>() + reply[16] as u16;
    sum & 0xFF == CHECKSUM_BASE
}

/// 解析应答帧；校验失败返回 `None`。
pub fn parse(reply: &[u8]) -> Option<Battery> {
    if !is_valid(reply) {
        return None;
    }
    Some(Battery {
        level: reply[LEVEL_INDEX].min(100),
        wired: reply[WIRED_INDEX] != 0,
    })
}

/// 把字节转成便于日志排障的十六进制串（小写、空格分隔）。
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // 写 String 不会失败
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无线模式实测帧（2026-11-18，PID 11D9，电量 15%，未插线）。
    const WIRELESS_15: &str = "08 04 00 00 00 02 0f 00 0e 8a 00 00 00 00 00 00 a0";

    /// 有线模式实测帧（2026-11-18，PID 11E7，电量 55%，已插线）。
    const WIRED_55: &str = "08 04 00 00 00 02 37 01 0f 5b 00 00 00 00 00 00 a5";

    fn bytes(hex: &str) -> Vec<u8> {
        hex.split_whitespace()
            .map(|s| u8::from_str_radix(s, 16).unwrap())
            .collect()
    }

    #[test]
    fn request_matches_measured_format() {
        let frame = request();
        assert_eq!(frame.len(), FRAME_LEN);
        assert_eq!(frame[0], 0x08);
        assert_eq!(frame[1], 0x04);
        assert!(frame[2..16].iter().all(|b| *b == 0));
        assert_eq!(frame[16], 0x49);
        assert_eq!(
            hex(&frame),
            "08 04 00 00 00 00 00 00 00 00 00 00 00 00 00 00 49"
        );
    }

    #[test]
    fn parses_wireless_frame() {
        let b = bytes(WIRELESS_15);
        assert!(is_valid(&b));
        let info = parse(&b).expect("should parse");
        assert_eq!(info.level, 15);
        assert!(!info.wired);
    }

    #[test]
    fn parses_wired_frame() {
        let b = bytes(WIRED_55);
        assert!(is_valid(&b));
        let info = parse(&b).expect("should parse");
        assert_eq!(info.level, 55);
        assert!(info.wired);
    }

    #[test]
    fn rejects_tampered_checksum() {
        let mut b = bytes(WIRELESS_15);
        b[16] = 0xA1;
        assert!(!is_valid(&b));
        assert!(parse(&b).is_none());
    }

    #[test]
    fn rejects_tampered_payload() {
        let mut b = bytes(WIRED_55);
        b[6] = 0x63; // 改成 99，但校验不动
        assert!(!is_valid(&b));
    }

    #[test]
    fn rejects_short_frame() {
        let b = bytes(WIRELESS_15);
        assert!(!is_valid(&b[..16]));
        assert!(parse(&b[..16]).is_none());
    }

    #[test]
    fn rejects_wrong_report_id() {
        let mut b = bytes(WIRELESS_15);
        b[0] = 0x09;
        assert!(!is_valid(&b));
    }

    #[test]
    fn clamps_out_of_range_level() {
        let mut b = bytes(WIRELESS_15);
        b[6] = 0xFF;
        // 重算校验，让帧本身合法，只验证 clamp 行为
        b[16] = 0;
        let sum: u16 = b[..16].iter().map(|x| *x as u16).sum();
        b[16] = ((CHECKSUM_BASE.wrapping_sub(sum & 0xFF)) & 0xFF) as u8;
        assert!(is_valid(&b));
        assert_eq!(parse(&b).unwrap().level, 100);
    }

    #[test]
    fn checksum_formula_is_stable() {
        let cmd = [0x04u8; 16];
        // 手算：0x08 + 0x04*15 = 0x08 + 0x3C = 0x44 → 0x55 - 0x44 = 0x11
        assert_eq!(checksum(0x08, &cmd), 0x11);
    }
}
