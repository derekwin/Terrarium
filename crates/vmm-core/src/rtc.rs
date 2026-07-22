//! 最小 mc146818 CMOS RTC 仿真（PIO 0x70 索引 / 0x71 数据）。
//!
//! M0 动机：x86 原生 persistent clock（`arch/x86/kernel/rtc.c`）直接读 CMOS，
//! 与 `CONFIG_RTC_CLASS` 无关。没有仿真时寄存器 A 的 UIP 位恒 1（悬空端口读回
//! 0xff），内核每次读时钟都等满 ~1.26s 超时，启动实测出现两次共 ~2.5s。
//! 仿真后读时钟立即返回，guest 还能拿到正确的墙上时间（UTC）。
//!
//! 只实现内核读时钟所需的最小集合：时间寄存器（BCD）、寄存器 A/B/C/D，
//! 不写闹钟、不产生中断。

use std::time::{SystemTime, UNIX_EPOCH};

/// CMOS 索引端口（写索引，bit7 = NMI 屏蔽，本实现忽略）。
pub const RTC_PORT_INDEX: u16 = 0x70;
/// CMOS 数据端口。
pub const RTC_PORT_DATA: u16 = 0x71;

// 寄存器索引（mc146818）。
const REG_SEC: u8 = 0x00;
const REG_MIN: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0a;
const REG_B: u8 = 0x0b;
const REG_C: u8 = 0x0c;
const REG_D: u8 = 0x0d;

/// 二进制转 BCD（mc146818 默认 BCD 模式，寄存器 B 的 DM 位本报 0）。
fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

/// 天数（自 1970-01-01）转 (年, 月, 日)，Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (u32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y } as u32, m, d)
}

/// 最小 mc146818 RTC。时间读数实时取自 host（UTC，内核默认按 UTC 解释 CMOS）。
#[derive(Default)]
pub struct Rtc {
    index: u8,
}

impl Rtc {
    pub fn new() -> Self {
        Rtc::default()
    }

    /// 处理 PIO 写（`offset`：0 = 索引端口，1 = 数据端口）。
    pub fn write(&mut self, offset: u16, data: &[u8]) {
        for &byte in data {
            if offset == 0 {
                // bit7 是 NMI 屏蔽位，索引取低 7 位
                self.index = byte & 0x7f;
            }
            // 数据端口写入忽略（时间来自 host，闹钟/中断不实现）
        }
    }

    /// 处理 PIO 读（`offset`：0 = 索引端口，1 = 数据端口）。
    pub fn read(&self, offset: u16) -> u8 {
        if offset == 0 {
            return self.index;
        }
        match self.index {
            REG_SEC | REG_MIN | REG_HOUR | REG_DAY | REG_MONTH | REG_YEAR => self.read_time(),
            // A：UIP=0（无更新进行中，读时钟的关键）+ DV 32.768kHz 正常分频
            REG_A => 0x20,
            // B：24h 制、BCD 模式、无中断使能
            REG_B => 0x00,
            // C：无中断标志（读 C 同时是中断确认）
            REG_C => 0x00,
            // D：VRT=1（CMOS 内容有效）
            REG_D => 0x80,
            _ => 0,
        }
    }

    fn read_time(&self) -> u8 {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = (secs / 86_400) as i64;
        let day_secs = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        to_bcd(match self.index {
            REG_SEC => (day_secs % 60) as u8,
            REG_MIN => ((day_secs / 60) % 60) as u8,
            REG_HOUR => (day_secs / 3600) as u8,
            REG_DAY => day as u8,
            REG_MONTH => month as u8,
            REG_YEAR => (year % 100) as u8,
            _ => return 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_bcd() {
        assert_eq!(0x00, to_bcd(0));
        assert_eq!(0x09, to_bcd(9));
        assert_eq!(0x10, to_bcd(10));
        assert_eq!(0x59, to_bcd(59));
    }

    #[test]
    fn test_civil_from_days() {
        assert_eq!((1970, 1, 1), civil_from_days(0));
        assert_eq!((2000, 2, 29), civil_from_days(11_016)); // 闰日
        assert_eq!((2026, 7, 22), civil_from_days(20_656));
    }

    #[test]
    fn test_index_and_status_registers() {
        let mut rtc = Rtc::new();
        // 寄存器 A：UIP 必须为 0，否则内核读时钟要等满超时
        rtc.write(0, &[REG_A]);
        assert_eq!(0x20, rtc.read(1));
        assert_eq!(0, rtc.read(1) & 0x80);
        // 寄存器 D：VRT=1
        rtc.write(0, &[REG_D]);
        assert_eq!(0x80, rtc.read(1));
        // 索引可读回（低 7 位）
        rtc.write(0, &[0x80 | REG_B]);
        assert_eq!(REG_B, rtc.read(0));
        assert_eq!(0x00, rtc.read(1));
    }

    #[test]
    fn test_time_regs_are_bcd_and_sane() {
        let mut rtc = Rtc::new();
        for (idx, max) in [
            (REG_SEC, 0x59u8),
            (REG_MIN, 0x59),
            (REG_HOUR, 0x23),
            (REG_DAY, 0x31),
            (REG_MONTH, 0x12),
        ] {
            rtc.write(0, &[idx]);
            let v = rtc.read(1);
            assert!(v <= max, "reg {idx:#x} = {v:#x} 超出 BCD 合理范围");
            assert!(v & 0x0f <= 9, "reg {idx:#x} 个位不是合法 BCD: {v:#x}");
        }
        rtc.write(0, &[REG_YEAR]);
        assert!(rtc.read(1) >= 0x26, "年份不应早于 2026");
    }
}
