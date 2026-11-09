//! Human readable output.

use crate::api::Snapshot;
use crate::model::*;
use std::fmt::Write;

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

pub fn render_snapshot(s: &Snapshot) -> String {
    let mut o = String::new();
    let c = &s.controller;
    let _ = writeln!(o, "Controller on {}", s.device);
    let _ = writeln!(
        o,
        "  IC signature : {}   chip code {:#06x}   pid {:#04x}   capabilities {:#010x}",
        dash(&c.ic_signature),
        c.chip_code,
        c.pid,
        c.capabilities
    );
    if !c.firmware_blob_ascii.is_empty() {
        let _ = writeln!(o, "  Firmware info: {}", c.firmware_blob_ascii);
    }
    let _ = writeln!(o, "  Mailbox sector: LBA {}", s.mailbox_lba);
    if let Some(a) = s.alarm_active {
        let _ = writeln!(o, "  Alarm        : {}", if a { "SOUNDING" } else { "off" });
    }
    if let Some(h) = &c.hw_monitor {
        let _ = writeln!(
            o,
            "  Hardware monitor: fan {} | temperature {} | voltages {}",
            if h.fan_stopped {
                "STOPPED".to_string()
            } else if h.fan_rpm > 0 {
                format!(
                    "{} rpm{}",
                    h.fan_rpm,
                    if h.fan_alarm { " (ALARM)" } else { "" }
                )
            } else {
                "n/a".into()
            },
            if h.temperature_alarm { "ALARM" } else { "ok" },
            if !h.voltage_monitor {
                "n/a".to_string()
            } else if h.voltages.iter().all(|v| v.in_range) {
                "ok".into()
            } else {
                "OUT OF RANGE".into()
            },
        );
    }
    if c.events.any_pending {
        let _ = writeln!(
            o,
            "  Pending controller events: yes (see `orico-raid events`)"
        );
    }

    let _ = writeln!(o, "\nRAID volumes");
    let mut any = false;
    for r in s.raids.iter().filter(|r| r.valid) {
        any = true;
        let members: Vec<String> = r
            .members
            .iter()
            .filter(|m| m.valid)
            .map(|m| format!("port{}", m.disk_port))
            .collect();
        let _ = writeln!(
            o,
            "  RAID {}: {:<10} {:<10} {:>9.1} GiB  members: {}{}{}",
            r.index,
            r.level,
            r.state,
            r.capacity_gib,
            if members.is_empty() {
                "-".to_string()
            } else {
                members.join(",")
            },
            r.rebuild_percent
                .map(|p| format!("  rebuild {p}%"))
                .unwrap_or_default(),
            if r.name.is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", r.name)
            },
        );
    }
    if !any {
        let _ = writeln!(o, "  (none configured)");
    }

    let _ = writeln!(o, "\nDisks");
    for d in &s.disks {
        if !d.present {
            let _ = writeln!(
                o,
                "  port {}: not present (the controller does not report this port)",
                d.port
            );
            continue;
        }
        if d.model.trim().is_empty() && d.serial.trim().is_empty() {
            let _ = writeln!(o, "  port {}: no disk", d.port);
            continue;
        }
        let role = match (d.membership_a.kind, d.usage) {
            (3, _) => "spare".to_string(),
            (2, _) => format!("member of RAID {}", d.membership_a.raid_index),
            (0, 1) => "free".to_string(),
            (k, u) => format!("kind {k}/{u}"),
        };
        let _ = writeln!(
            o,
            "  port {}: {:<28} {:<20} fw {:<9} {:>8.1} GiB  {}",
            d.port,
            dash(&d.model),
            dash(&d.serial),
            dash(&d.firmware),
            d.capacity_gib,
            role
        );
    }
    let _ = writeln!(o);
    if s.health.issues.is_empty() {
        let _ = writeln!(o, "Health: OK");
    } else {
        let _ = writeln!(
            o,
            "Health: {}",
            ["OK", "WARNING", "CRITICAL"][s.health.level.min(2) as usize]
        );
        for i in &s.health.issues {
            let _ = writeln!(o, "  - {i}");
        }
    }
    for e in &s.errors {
        let _ = writeln!(o, "  ! {e}");
    }
    o
}

pub fn render_raid_detail(r: &RaidPort) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "RAID {}  (slot {})", dash(&r.name), r.index);
    let _ = writeln!(o, "  state     : {} (code {})", r.state, r.state_code);
    let _ = writeln!(o, "  level     : {} (code {})", r.level, r.level_code);
    let _ = writeln!(
        o,
        "  capacity  : {:.2} GiB ({} x 32 MiB)",
        r.capacity_gib, r.capacity_units
    );
    if let Some(p) = r.rebuild_percent {
        let _ = writeln!(o, "  rebuild   : {p}%");
    }
    let _ = writeln!(o, "  serial    : {}", dash(&r.serial));
    let _ = writeln!(o, "  members   : {} reported", r.member_count);
    for m in r.members.iter().filter(|m| m.valid) {
        let _ = writeln!(
            o,
            "    slot {} -> port {} (group {}, {:.2} GiB used)",
            m.slot,
            m.disk_port,
            m.group,
            units_to_gib(m.used_units)
        );
    }
    let _ = writeln!(
        o,
        "  rebuild priority {}, standby {} min, password {}",
        r.rebuild_priority,
        r.standby_minutes,
        if r.has_password { "set" } else { "none" }
    );
    o
}

pub fn render_smart(port: u8, attrs: &[SmartAttr]) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "SMART attributes, port {port}");
    let _ = writeln!(
        o,
        "  ID  NAME                      FLAGS  CUR WRST THR  RAW"
    );
    for a in attrs {
        let _ = writeln!(
            o,
            "{:>4}  {:<24}  {:#06x} {:>4} {:>4} {:>3}  {}{}",
            a.id,
            a.name,
            a.flags,
            a.current,
            a.worst,
            a.threshold,
            a.raw,
            if a.failing { "  <-- FAILING" } else { "" }
        );
    }
    if let Some(t) = attrs
        .iter()
        .find(|a| a.id == 194)
        .or_else(|| attrs.iter().find(|a| a.id == 190))
    {
        let _ = writeln!(o, "Temperature: {} C", t.raw & 0xff);
    }
    o
}
