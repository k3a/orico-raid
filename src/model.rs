//! Decoding of controller responses into typed state.
//!
//! Layouts are derived from `GetControllerInfo`, `GetRaidPortInfo`,
//! `GetSataPortInfo`, `GetHwMonitorInfo`, `GetDiskSmartInfo`, `CheckAlarmStatus`
//! (raidapi.o) and from the enum tables / display logic in the Mac GUI
//! (`st_szRaidState`, `st_szRaidLevel`, `RaidDiskPage`).
//! Anything that could not be tied to a GUI meaning is exposed as a raw value.

use serde::Serialize;

pub const MAX_PORTS: usize = 5;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Vendor `_Reserved`: ATA-style strings arrive with every byte pair swapped.
/// Returns the cleaned string.
pub fn swapstr(src: &[u8]) -> String {
    let mut out = Vec::with_capacity(src.len());
    for p in src.as_chunks::<2>().0 {
        out.push(p[1]);
        out.push(p[0]);
    }
    let end = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    String::from_utf8_lossy(&out[..end]).trim().to_string()
}

fn ascii(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s: String = b[..end].iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
    s.trim().to_string()
}

// ------------------------------------------------------------- enums ---------

pub const RAID_STATE_NAMES: [&str; 6] = ["Broken", "Degraded", "Rebuilding", "Normal", "Expansion", "Backup"];
pub const RAID_LEVEL_NAMES: [&str; 8] = ["RAID 0", "RAID 1", "LARGE", "RAID 3", "CLONE", "RAID 5", "RAID 10", "RAID 0 Opt"];

pub fn raid_state_name(s: u8) -> String {
    RAID_STATE_NAMES.get(s as usize).map(|s| s.to_string()).unwrap_or_else(|| format!("Unknown({s})"))
}

/// A mirror (level 1) built from more than three members is shown as RAID 10 by the GUI.
pub fn raid_level_name(level: u8, members: u8) -> String {
    if level == 1 && members > 3 {
        return "RAID 10".into();
    }
    RAID_LEVEL_NAMES.get(level as usize).map(|s| s.to_string()).unwrap_or_else(|| format!("Unknown({level})"))
}

/// Capacity fields are reported in units of 32 MiB (the GUI divides them by 32 to print GB).
pub const CAPACITY_UNIT_BYTES: u64 = 32 * 1024 * 1024;
pub fn units_to_gib(units: u32) -> f64 {
    units as f64 / 32.0
}

// -------------------------------------------------------- chip / controller --

#[derive(Debug, Clone, Serialize)]
pub struct HwMonitor {
    /// voltage monitoring valid (`b[0x55] & 0x10`)
    pub voltage_monitor: bool,
    pub fan_alarm: bool,
    pub temperature_alarm: bool,
    /// three monitored voltage channels: computed value, allowed range, in-range flag.
    pub voltages: Vec<VoltageChannel>,
    /// 0 = stopped or unknown
    pub fan_rpm: u32,
    pub fan_stopped: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VoltageChannel {
    pub value: u32,
    pub min: u32,
    pub max: u32,
    pub in_range: bool,
}

impl EventBits {
    /// Vendor `GetNotificationFlag` classification: 0 none, 1 = word_1e8 bits 8..12,
    /// 2 = rebuild-port bits (word_1e4 bits 16..21), 3 = controller changed (word_1ec bit 14),
    /// 4 = disk-port bits (word_1e4 bits 0..5 or word_1e8 bits 0..4), 5 = 4 + 2.
    pub fn notification_group(&self) -> u8 {
        if self.controller_changed {
            return 3;
        }
        let mut g = 0;
        if self.rebuild_ports_mask != 0 {
            g = 2;
        }
        if self.disk_ports_mask != 0 || self.word_1e8 & 0x1f != 0 {
            g = 4;
        }
        if g == 0 && self.word_1e8 & 0x1f00 != 0 {
            g = 1;
        }
        if g == 4 && self.rebuild_ports_mask != 0 {
            g = 5;
        }
        g
    }
    pub fn group_name(&self) -> &'static str {
        match self.notification_group() {
            0 => "none",
            1 => "controller notice (word_1e8 bits 8..12)",
            2 => "RAID/rebuild activity",
            3 => "controller configuration changed",
            4 => "disk status change",
            _ => "disk status change + RAID/rebuild activity",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EventBits {
    pub word_1e4: u32,
    pub word_1e8: u32,
    pub word_1ec: u16,
    /// "controller changed" flag (`0x4000` in word_1ec)
    pub controller_changed: bool,
    /// per-port bit masks of pending disk/RAID notifications
    pub rebuild_ports_mask: u8,
    pub disk_ports_mask: u8,
    pub any_pending: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChipInfo {
    pub pid: u8,
    pub flags: u8,
    pub chip_code: u16,
    pub capabilities: u32,
    pub firmware_blob_hex: String,
    pub firmware_blob_ascii: String,
    pub ic_signature: String,
    pub assigned_id: u16,
    pub events: EventBits,
    pub hw_monitor: Option<HwMonitor>,
    pub raw_hex: String,
}

fn parse_hwmon(d: &[u8]) -> Option<HwMonitor> {
    if d[1] & 0x80 == 0 || d[0x54] == 0 {
        return None;
    }
    let f = d[0x55];
    let m = le32(d, 0x56);
    let chan = |adc: u8, scale: u8, lo: u8, hi: u8| -> VoltageChannel {
        let (h, l) = ((scale >> 4) as u32, (scale & 0xf) as u32);
        let conv = |x: u8| -> u32 {
            let x = x as u32;
            (x.wrapping_mul(l).wrapping_mul(m) / 10).wrapping_add(x.wrapping_mul(h).wrapping_mul(m))
        };
        let (value, min, max) = (conv(adc), conv(lo), conv(hi));
        VoltageChannel { value, min, max, in_range: !(value < min || value > max) }
    };
    let voltages = vec![
        chan(d[0x6a], d[0x6b], d[0x68], d[0x69]),
        chan(d[0x6e], d[0x6f], d[0x6c], d[0x6d]),
        chan(d[0x72], d[0x73], d[0x70], d[0x71]),
    ];
    let (div_code, count) = (d[0x75], d[0x76]);
    let (fan_rpm, fan_stopped) = if count == 0xff {
        (0, true)
    } else if count == 0 || div_code == 0 {
        (0, false)
    } else {
        (1_350_000 / (((2u32) << (div_code - 1)) * count as u32), false)
    };
    Some(HwMonitor {
        voltage_monitor: f & 0x10 != 0,
        fan_alarm: f & 0x20 != 0,
        temperature_alarm: f & 0x40 != 0,
        voltages,
        fan_rpm,
        fan_stopped,
    })
}

pub fn parse_chip_info(pid: u8, d: &[u8]) -> ChipInfo {
    assert!(d.len() >= 0x1f4);
    let w1e4 = le32(d, 0x1e4);
    let w1e8 = le32(d, 0x1e8);
    let w1ec = le16(d, 0x1ec);
    let sig: Vec<u8> = d[0x94..0x9c].iter().map(|b| !b).collect();
    let events = EventBits {
        word_1e4: w1e4,
        word_1e8: w1e8,
        word_1ec: w1ec,
        controller_changed: w1ec & 0x4000 != 0,
        rebuild_ports_mask: ((w1e4 >> 16) & 0x3f) as u8,
        disk_ports_mask: (w1e4 & 0x3f) as u8,
        any_pending: w1ec & 0x4000 != 0 || w1e4 & 0x3f_003f != 0 || w1e8 & 0x1f1f != 0,
    };
    ChipInfo {
        pid,
        flags: d[1],
        chip_code: le16(d, 2),
        capabilities: le32(d, 4),
        firmware_blob_hex: hex(&d[0x14..0x54]),
        firmware_blob_ascii: ascii(&d[0x14..0x54]),
        ic_signature: ascii(&sig),
        assigned_id: le16(d, 0x1ee),
        events,
        hw_monitor: parse_hwmon(d),
        raw_hex: hex(&d[..0x1f4]),
    }
}

// ------------------------------------------------------------------- disks ---

#[derive(Debug, Clone, Serialize)]
pub struct Membership {
    /// 0 none, 2 RAID member, 3 spare, 4 clone/backup (GUI free-disk logic)
    pub kind: u8,
    pub raid_index: u8,
    pub member_slot: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskIdentity {
    pub model: String,
    pub serial: String,
    pub flag: bool,
    pub wwn_like: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SataDisk {
    pub port: usize,
    pub present: bool,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub capacity_units: u32,
    pub capacity_gib: f64,
    /// 1 = free / stand-alone disk, 2 = in a RAID (GUI `_CONTROLLER` disk type)
    pub usage: u8,
    pub membership_a: Membership,
    pub membership_b: Membership,
    pub status_flags: u16,
    pub recorded_member_a: DiskIdentity,
    pub recorded_member_b: DiskIdentity,
    pub raw_all_sata_hex: String,
    pub raw_port_hex: String,
}

pub fn parse_sata_disk(port: usize, all: &[u8], port_info: &[u8]) -> SataDisk {
    let a = &all[port * 0x50..port * 0x50 + 0x50];
    let s = port_info;
    let cap = le32(a, 0x3c);
    let ident = |m: usize, ser: usize, flag: usize, w: usize| DiskIdentity {
        model: swapstr(&s[m..m + 0x29]),
        serial: swapstr(&s[ser..ser + 0x15]),
        flag: s[flag] & 1 != 0,
        wwn_like: format!("{:08x}{:08x}", le32(s, w + 4), le32(s, w)),
    };
    SataDisk {
        port,
        present: a[0x4c] & 1 != 0,
        model: swapstr(&a[0..0x29]),
        serial: swapstr(&a[0x28..0x3d]),
        firmware: swapstr(&s[0x44..0x4d]),
        capacity_units: cap,
        capacity_gib: units_to_gib(cap),
        usage: a[0x48],
        membership_a: Membership { kind: a[0x41], raid_index: a[0x42], member_slot: a[0x43] },
        membership_b: Membership { kind: a[0x45], raid_index: a[0x46], member_slot: a[0x47] },
        status_flags: (a[0x4b] & 0x3f) as u16 | ((a[0x4d] as u16) << 6),
        recorded_member_a: ident(0x84, 0xac, 0xc6, 0xcc),
        recorded_member_b: ident(0x104, 0x12c, 0x146, 0x14c),
        raw_all_sata_hex: hex(a),
        raw_port_hex: hex(s),
    }
}

// -------------------------------------------------------------------- RAID ---

#[derive(Debug, Clone, Serialize)]
pub struct RaidMember {
    pub slot: usize,
    pub valid: bool,
    /// 0 = first group, 1 = second group (a disk can host two RAID volumes)
    pub group: u8,
    pub disk_port: u8,
    pub used_units: u32,
    pub raw: [u8; 4],
}

#[derive(Debug, Clone, Serialize)]
pub struct RaidPort {
    pub index: usize,
    /// slot holds a configured RAID volume (`flags & 1`)
    pub valid: bool,
    pub name: String,
    pub serial: String,
    pub level_code: u8,
    pub level: String,
    pub state_code: u8,
    pub state: String,
    pub member_count: u8,
    pub members: Vec<RaidMember>,
    pub capacity_units: u32,
    pub capacity_gib: f64,
    pub rebuild_progress_units: u32,
    /// only meaningful while rebuilding
    pub rebuild_percent: Option<u32>,
    /// 1 (highest) .. 5; 0 = not reported
    pub rebuild_priority: u8,
    pub standby_minutes: u16,
    pub has_password: bool,
    pub flags: u16,
    pub raw_hex: String,
}

pub fn parse_raid_port(index: usize, r: &[u8]) -> RaidPort {
    assert!(r.len() >= 0x1a4);
    let flags = le16(r, 0x5a);
    let level_code = r[0x54];
    let members_n = r[0x55];
    let mut state = r[0x46];
    if state == 2 && flags & 0x80 != 0 && flags & 0x20 != 0 {
        state = 5;
    }
    let size = le32(r, 0x40);
    let progress = le32(r, 0x60);
    let pr = le16(r, 0x64);
    let prio = match () {
        _ if pr & 0x4000 != 0 => 1,
        _ if pr & 0x2000 != 0 => 2,
        _ if pr & 0x1000 != 0 => 3,
        _ if pr & 0x0800 != 0 => 4,
        _ if pr & 0x0400 != 0 => 5,
        _ => 0,
    };
    let mut members = vec![];
    for k in 0..MAX_PORTS {
        let m = &r[0xa0 + 0x20 * k..0xa0 + 0x20 * (k + 1)];
        members.push(RaidMember {
            slot: k,
            valid: m[4] != 0,
            group: m[0xa],
            disk_port: m[0xb],
            used_units: le32(m, 0x10),
            raw: [m[4], m[8], m[0xa], m[0xb]],
        });
    }
    let eff_size = if level_code == 1 && members_n == 4 { size >> 1 } else { size };
    let rebuild_percent = if (state == 2 || state == 5) && eff_size > 0 {
        Some((progress as u64 * 100 / eff_size as u64).min(100) as u32)
    } else {
        None
    };
    RaidPort {
        index,
        // bit0 is the documented "volume exists" flag; also accept a populated member table
        valid: flags & 1 != 0 || (members_n > 0 && members.iter().any(|m| m.valid)),
        name: swapstr(&r[4..0x2d]),
        serial: swapstr(&r[0x2c..0x41]),
        level_code,
        level: raid_level_name(level_code, members_n),
        state_code: state,
        state: raid_state_name(state),
        member_count: members_n,
        members,
        capacity_units: size,
        capacity_gib: units_to_gib(size),
        rebuild_progress_units: progress,
        rebuild_percent,
        rebuild_priority: prio,
        standby_minutes: le16(r, 0x66),
        has_password: r[0x7c..0x84].iter().any(|&b| b != 0),
        flags,
        raw_hex: hex(&r[..0x1a4]),
    }
}

// ------------------------------------------------------------------- SMART ---

#[derive(Debug, Clone, Serialize)]
pub struct SmartAttr {
    pub id: u8,
    pub name: &'static str,
    pub flags: u16,
    pub current: u8,
    pub worst: u8,
    pub threshold: u8,
    pub raw: u64,
    pub raw_hex: String,
    /// current <= threshold (attribute is failing now)
    pub failing: bool,
}

pub fn smart_attr_name(id: u8) -> &'static str {
    match id {
        1 => "Raw_Read_Error_Rate",
        2 => "Throughput_Performance",
        3 => "Spin_Up_Time",
        4 => "Start_Stop_Count",
        5 => "Reallocated_Sector_Ct",
        7 => "Seek_Error_Rate",
        8 => "Seek_Time_Performance",
        9 => "Power_On_Hours",
        10 => "Spin_Retry_Count",
        11 => "Calibration_Retry_Count",
        12 => "Power_Cycle_Count",
        171 => "Program_Fail_Count",
        172 => "Erase_Fail_Count",
        177 => "Wear_Leveling_Count",
        181 => "Program_Fail_Cnt_Total",
        182 => "Erase_Fail_Count_Total",
        183 => "Runtime_Bad_Block",
        184 => "End-to-End_Error",
        187 => "Reported_Uncorrect",
        188 => "Command_Timeout",
        189 => "High_Fly_Writes",
        190 => "Airflow_Temperature_Cel",
        191 => "G-Sense_Error_Rate",
        192 => "Power-Off_Retract_Count",
        193 => "Load_Cycle_Count",
        194 => "Temperature_Celsius",
        195 => "Hardware_ECC_Recovered",
        196 => "Reallocated_Event_Count",
        197 => "Current_Pending_Sector",
        198 => "Offline_Uncorrectable",
        199 => "UDMA_CRC_Error_Count",
        200 => "Multi_Zone_Error_Rate",
        201 => "Soft_Read_Error_Rate",
        202 => "Data_Address_Mark_Errs",
        220 => "Disk_Shift",
        222 => "Loaded_Hours",
        223 => "Load_Retry_Count",
        224 => "Load_Friction",
        226 => "Load-in_Time",
        231 => "SSD_Life_Left",
        233 => "Media_Wearout_Indicator",
        240 => "Head_Flying_Hours",
        241 => "Total_LBAs_Written",
        242 => "Total_LBAs_Read",
        _ => "Unknown_Attribute",
    }
}

/// `values` = SMART READ DATA sector (first 0x16a bytes used), `thresholds` = READ THRESHOLDS.
pub fn parse_smart(values: &[u8], thresholds: &[u8]) -> Vec<SmartAttr> {
    let mut out = vec![];
    for k in 0..30 {
        let b = 2 + 12 * k;
        let id = values[b];
        if id == 0 {
            continue;
        }
        let mut raw = 0u64;
        for i in (0..6).rev() {
            raw = raw << 8 | values[b + 5 + i] as u64;
        }
        let thr = (0..30).map(|t| 2 + 12 * t).find(|&t| thresholds[t] == id).map(|t| thresholds[t + 1]).unwrap_or(0);
        let cur = values[b + 3];
        out.push(SmartAttr {
            id,
            name: smart_attr_name(id),
            flags: le16(values, b + 1),
            current: cur,
            worst: values[b + 4],
            threshold: thr,
            raw,
            raw_hex: hex(&values[b + 5..b + 11]),
            failing: thr != 0 && cur <= thr,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_strings() {
        // "ST4000" stored pair-swapped
        assert_eq!(swapstr(b"TS0400  \0\0"), "ST4000");
    }

    #[test]
    fn raid10_naming() {
        assert_eq!(raid_level_name(1, 2), "RAID 1");
        assert_eq!(raid_level_name(1, 4), "RAID 10");
        assert_eq!(raid_level_name(5, 4), "RAID 5");
    }

    #[test]
    fn rebuild_percent_and_state() {
        let mut r = vec![0u8; 0x1a4];
        r[0x46] = 2; // rebuilding
        r[0x54] = 5;
        r[0x55] = 4;
        r[0x40..0x44].copy_from_slice(&1000u32.to_le_bytes());
        r[0x60..0x64].copy_from_slice(&250u32.to_le_bytes());
        r[0x5a] = 1;
        let p = parse_raid_port(0, &r);
        assert_eq!(p.state, "Rebuilding");
        assert_eq!(p.rebuild_percent, Some(25));
        assert!(p.valid);
        r[0x5a] = 1 | 0x80 | 0x20;
        assert_eq!(parse_raid_port(0, &r).state, "Backup");
    }

    #[test]
    fn hwmon_fan() {
        let mut d = vec![0u8; 0x200];
        d[1] = 0x80;
        d[0x54] = 1;
        d[0x55] = 0x10 | 0x20;
        d[0x75] = 1;
        d[0x76] = 100; // 1350000 / (2*100)
        let c = parse_chip_info(0, &d);
        let h = c.hw_monitor.unwrap();
        assert_eq!(h.fan_rpm, 6750);
        assert!(h.fan_alarm && h.voltage_monitor && !h.temperature_alarm);
    }

    #[test]
    fn smart_thresholds() {
        let mut v = vec![0u8; 512];
        let mut t = vec![0u8; 512];
        v[2] = 5;
        v[2 + 3] = 90; // current
        v[2 + 5] = 7; // raw
        t[2] = 5;
        t[3] = 95; // threshold above current -> failing
        let a = parse_smart(&v, &t);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].raw, 7);
        assert!(a[0].failing);
    }
}
