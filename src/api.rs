//! High level controller queries (the read-only half of the vendor `raidapi`).

use crate::io::SectorIo;
use crate::model::*;
use crate::proto::Command;
use crate::session::Session;
use anyhow::{bail, Result};
use serde::Serialize;

// ---- command builders (kind, sub) as used by raidapi.o ------------------------

/// `_GetChipInfo`
pub fn cmd_chip_info() -> Command {
    Command::new(1, 1)
}
/// `_GetAllSataInfo`
pub fn cmd_all_sata() -> Command {
    Command::new(2, 1)
}
/// `_GetRaidPortInfo`
pub fn cmd_raid_port(port: u8) -> Command {
    Command::new(3, 2).args(&[port]).payload(&[port])
}
/// `_GetSataPortInfo`
pub fn cmd_sata_port(port: u8) -> Command {
    Command::new(2, 2).args(&[port]).payload(&[port])
}
/// `_SentFisCmd`: pass a register FIS to the disk on `port`.
pub fn cmd_fis(port: u8, fis: &[u8; 0x1e0]) -> Command {
    Command::new(2, 3).args(&[port, 2, 0, 0xe0]).payload(fis)
}
/// `CheckAlarmStatus`
pub fn cmd_alarm_status() -> Command {
    Command::new(1, 8).args(&[0, 0, 0, 0])
}
/// `SetAlarmMute`
pub fn cmd_alarm_mute() -> Command {
    Command::new(1, 6).args(&[1])
}

/// FIS used by `GetDiskSmartInfo`: SMART READ DATA (0xD0) / READ THRESHOLDS (0xD1).
pub fn smart_fis(feature: u8) -> [u8; 0x1e0] {
    let mut f = [0u8; 0x1e0];
    f[2] = feature;
    f[8] = 0x4f;
    f[10] = 0xc2;
    f[12] = 0xa0;
    f[14] = 0xb0;
    f
}

// ---- queries -------------------------------------------------------------------

pub fn chip_info<IO: SectorIo>(s: &mut Session<IO>) -> Result<ChipInfo> {
    let r = s.command(&cmd_chip_info())?;
    Ok(parse_chip_info(r.pid, &r.data))
}

/// Returns `None` when no alarm is sounding.
pub fn alarm_active<IO: SectorIo>(s: &mut Session<IO>) -> Result<bool> {
    let r = s.command(&cmd_alarm_status())?;
    Ok(r.data[0] == 0 && r.data[2] == 1)
}

pub fn mute_alarm<IO: SectorIo>(s: &mut Session<IO>) -> Result<()> {
    s.command(&cmd_alarm_mute())?;
    Ok(())
}

pub fn smart<IO: SectorIo>(s: &mut Session<IO>, port: u8) -> Result<Vec<SmartAttr>> {
    if port as usize >= MAX_PORTS {
        bail!("port must be 0..{}", MAX_PORTS - 1);
    }
    let v = s.command(&cmd_fis(port, &smart_fis(0xd0)))?;
    let t = s.command(&cmd_fis(port, &smart_fis(0xd1)))?;
    // the vendor code copies the page from offset 0x10 of the returned FIS buffer
    const OFF: usize = 0x10;
    if v.data.len() < OFF + 0x16a || t.data.len() < OFF + 0x16a {
        bail!("short SMART response");
    }
    Ok(parse_smart(&v.data[OFF..], &t.data[OFF..]))
}

// ---- snapshot --------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Health {
    /// 0 OK, 1 warning, 2 critical
    pub level: u8,
    pub issues: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub device: String,
    pub mailbox_lba: u64,
    pub controller: ChipInfo,
    pub disks: Vec<SataDisk>,
    pub raids: Vec<RaidPort>,
    pub alarm_active: Option<bool>,
    pub os_device_bytes: Option<u64>,
    pub errors: Vec<String>,
    pub health: Health,
}

pub fn collect<IO: SectorIo>(s: &mut Session<IO>, os_bytes: Option<u64>) -> Result<Snapshot> {
    let mut errors = vec![];
    let chip = chip_info(s)?;
    let all = s.command(&cmd_all_sata())?;
    let mut disks = vec![];
    let mut raids = vec![];
    for p in 0..MAX_PORTS {
        match s.command(&cmd_sata_port(p as u8)) {
            Ok(r) => disks.push(parse_sata_disk(p, &all.data, &r.data)),
            Err(e) => errors.push(format!("sata port {p}: {e}")),
        }
        match s.command(&cmd_raid_port(p as u8)) {
            Ok(r) => raids.push(parse_raid_port(p, &r.data)),
            Err(e) => errors.push(format!("raid port {p}: {e}")),
        }
    }
    // the controller mirrors volume 0 again at higher port indexes; keep the first of each
    let mut seen: Vec<(String, String)> = vec![];
    raids.retain(|r: &RaidPort| {
        if !r.valid {
            return true;
        }
        let key = (r.name.clone(), r.serial.clone());
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
    let alarm = match alarm_active(s) {
        Ok(a) => Some(a),
        Err(e) => {
            errors.push(format!("alarm status: {e}"));
            None
        }
    };
    let health = assess(&chip, &disks, &raids, alarm);
    Ok(Snapshot {
        device: s.io.name(),
        mailbox_lba: s.lba,
        controller: chip,
        disks,
        raids,
        alarm_active: alarm,
        os_device_bytes: os_bytes,
        errors,
        health,
    })
}

pub fn assess(chip: &ChipInfo, disks: &[SataDisk], raids: &[RaidPort], alarm: Option<bool>) -> Health {
    let mut level = 0u8;
    let mut issues = vec![];
    let mut bump = |l: u8, msg: String| {
        level = level.max(l);
        issues.push(msg);
    };
    for r in raids.iter().filter(|r| r.valid) {
        match r.state_code {
            3 => {}
            2 | 5 => bump(1, format!("RAID {} ({}) is {}{}", r.index, r.name, r.state, r.rebuild_percent.map(|p| format!(" ({p}%)")).unwrap_or_default())),
            1 => bump(2, format!("RAID {} ({}) is DEGRADED", r.index, r.name)),
            0 => bump(2, format!("RAID {} ({}) is BROKEN", r.index, r.name)),
            _ => bump(1, format!("RAID {} ({}) state: {}", r.index, r.name, r.state)),
        }
    }
    if let Some(h) = &chip.hw_monitor {
        if h.fan_alarm {
            bump(2, "fan speed abnormal".into());
        }
        if h.temperature_alarm {
            bump(2, "temperature abnormal".into());
        }
        if h.voltage_monitor && h.voltages.iter().any(|v| !v.in_range) {
            bump(2, "voltage out of range".into());
        }
    }
    if alarm == Some(true) {
        bump(1, "controller alarm is sounding".into());
    }
    if !raids.iter().any(|r| r.valid) && disks.iter().any(|d| d.present && d.membership_a.kind == 2) {
        bump(1, "disks report RAID membership but no volume was decoded (run `raw` and report it)".into());
    }
    Health { level, issues }
}
