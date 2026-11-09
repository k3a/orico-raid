use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use orico_raid::api::{self, Snapshot};
use orico_raid::io::BlockDevice;
use orico_raid::io::SectorIo;
use orico_raid::session::Session;
use orico_raid::{backup, model, render, scan};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "orico-raid",
    version,
    about = "Status tool for ORICO / JMicron hardware RAID enclosures (JMB39x, JMS56x)"
)]
struct Cli {
    /// Block device(s) exported by the enclosure, e.g. /dev/sdb. Default: every disk that looks
    /// like a JMicron/ORICO bridge (see `list`).
    #[arg(short, long, global = true)]
    device: Vec<PathBuf>,
    /// JSON output
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List candidate block devices (reads sysfs only, no device access)
    List,
    /// Controller, RAID volumes, disks and health in one view
    Status,
    /// Details of every configured RAID volume
    Raid,
    /// Per-port disk information
    Disks,
    /// S.M.A.R.T. attributes of the disk on a port (0-4)
    Smart { port: u8 },
    /// Fan / voltage / temperature-alarm state of the enclosure
    Hwmon,
    /// Controller alarm; --mute silences it
    Alarm {
        #[arg(long)]
        mute: bool,
    },
    /// Show pending controller event flags (does not acknowledge them)
    Events,
    /// Poll continuously and report state changes
    Monitor {
        /// seconds between polls
        #[arg(short, long, default_value_t = 5)]
        interval: u64,
    },
    /// Nagios-style one-line check; exit code 0 OK / 1 WARNING / 2 CRITICAL / 3 UNKNOWN
    Check,
    /// Write a saved mailbox-sector backup back to the disk (never done automatically)
    Restore {
        /// backup file (default: the only/matching one for --device)
        #[arg(long)]
        backup: Option<PathBuf>,
        /// list saved backups and exit
        #[arg(long)]
        list: bool,
        /// restore even if the content looks like a controller command frame (dangerous)
        #[arg(long)]
        force: bool,
    },
    /// Hex dump of every raw controller response (for debugging / reverse engineering)
    Raw,
}

fn targets(cli: &Cli) -> Result<Vec<PathBuf>> {
    if !cli.device.is_empty() {
        return Ok(cli.device.clone());
    }
    let c: Vec<PathBuf> = scan::list()
        .into_iter()
        .filter(|c| c.likely)
        .map(|c| c.dev)
        .collect();
    if c.is_empty() {
        bail!("no JMicron/ORICO-looking disk found; pass the enclosure's disk with --device /dev/sdX (see `orico-raid list`)");
    }
    Ok(c)
}

fn open(path: &std::path::Path) -> Result<(Session<BlockDevice>, Option<u64>)> {
    let dev = BlockDevice::open(path, true)?;
    let size = dev.size_bytes();
    let s = match Session::open(dev) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "note: a mailbox sector on {} was overwritten while probing. The original was saved in {}; it is NOT restored automatically (see `orico-raid restore --list`).",
                path.display(),
                backup::backup_dir().display()
            );
            return Err(e).with_context(|| format!("probing {}", path.display()));
        }
    };
    if let Some(f) = &s.backup_file {
        eprintln!("note: original content of mailbox sector LBA {} saved to {} (restore with `orico-raid restore -d {}`)", s.lba, f.display(), path.display());
    }
    Ok((s, size))
}

fn main() {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(3);
        }
    }
}

fn run(cli: &Cli) -> Result<i32> {
    if let Cmd::List = cli.cmd {
        let l = scan::list();
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&l)?);
        } else {
            for c in &l {
                println!(
                    "{:<10} {:<8} {:<16} {:>8.1} GiB  {:<9} {}",
                    c.dev.display(),
                    c.vendor,
                    c.model,
                    c.size_bytes as f64 / (1u64 << 30) as f64,
                    c.usb_id.clone().unwrap_or_else(|| c.transport.clone()),
                    if c.likely {
                        "<- JMicron/ORICO candidate"
                    } else {
                        ""
                    },
                );
            }
            if l.is_empty() {
                println!("no sd* devices found");
            }
        }
        return Ok(0);
    }

    if let Cmd::Restore {
        backup: bf,
        list,
        force,
    } = &cli.cmd
    {
        if *list {
            for p in backup::list() {
                match backup::load(&p) {
                    Ok(b) => println!(
                        "{}  device {} LBA {} saved {}",
                        p.display(),
                        b.device,
                        b.lba,
                        b.unix_time
                    ),
                    Err(e) => println!("{}  unreadable: {e}", p.display()),
                }
            }
            return Ok(0);
        }
        let dev = match cli.device.as_slice() {
            [d] => d.clone(),
            _ => bail!("restore needs exactly one --device"),
        };
        let files: Vec<PathBuf> = match bf {
            Some(f) => vec![f.clone()],
            None => backup::list()
                .into_iter()
                .filter(|p| {
                    backup::load(p)
                        .map(|b| b.device == dev.display().to_string())
                        .unwrap_or(false)
                })
                .collect(),
        };
        let file = match files.as_slice() {
            [f] => f.clone(),
            [] => bail!(
                "no backup found for {}; see `restore --list`",
                dev.display()
            ),
            _ => bail!("several backups match, choose one with --backup (see `restore --list`)"),
        };
        let b = backup::load(&file)?;
        if b.device != dev.display().to_string() {
            bail!("backup belongs to {}, not {}", b.device, dev.display());
        }
        let mut io = BlockDevice::open(&dev, true)?;
        backup::restore(&mut io, &b, *force)?;
        std::fs::remove_file(&file)?;
        println!(
            "restored LBA {} on {} (backup {} removed)",
            b.lba,
            dev.display(),
            file.display()
        );
        let _ = io.name();
        return Ok(0);
    }

    let mut worst = 0;
    let mut snaps: Vec<Snapshot> = vec![];
    for path in targets(cli)? {
        let (mut s, size) = match open(&path) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("{}: {e:#}", path.display());
                worst = worst.max(3);
                continue;
            }
        };
        match &cli.cmd {
            Cmd::Status
            | Cmd::Check
            | Cmd::Raid
            | Cmd::Disks
            | Cmd::Hwmon
            | Cmd::Events
            | Cmd::Raw => {
                let snap = api::collect(&mut s, size)?;
                worst = worst.max(snap.health.level as i32);
                if let Cmd::Check = cli.cmd {
                    let tag = ["OK", "WARNING", "CRITICAL"][snap.health.level.min(2) as usize];
                    let raids = snap.raids.iter().filter(|r| r.valid).count();
                    let present = snap
                        .disks
                        .iter()
                        .filter(|d| {
                            d.present && !(d.model.trim().is_empty() && d.serial.trim().is_empty())
                        })
                        .count();
                    println!(
                        "RAID {tag} - {}: {} volume(s), {} disk(s){}{}",
                        path.display(),
                        raids,
                        present,
                        if snap.health.issues.is_empty() {
                            ""
                        } else {
                            " - "
                        },
                        snap.health.issues.join("; ")
                    );
                    continue;
                }
                snaps.push(snap);
            }
            Cmd::Smart { port } => {
                let attrs = api::smart(&mut s, *port)?;
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&attrs)?);
                } else {
                    print!("{}", render::render_smart(*port, &attrs));
                }
                if attrs.iter().any(|a| a.failing) {
                    worst = worst.max(2);
                }
            }
            Cmd::Alarm { mute } => {
                if *mute {
                    api::mute_alarm(&mut s)?;
                    println!("{}: alarm muted", path.display());
                }
                let a = api::alarm_active(&mut s)?;
                println!(
                    "{}: alarm {}",
                    path.display(),
                    if a { "SOUNDING" } else { "off" }
                );
                if a {
                    worst = worst.max(1);
                }
            }
            Cmd::Monitor { interval } => {
                monitor(&mut s, size, *interval)?;
            }
            Cmd::List | Cmd::Restore { .. } => unreachable!(),
        }
    }

    if !snaps.is_empty() {
        match &cli.cmd {
            Cmd::Status => {
                if cli.json {
                    println!("{}", serde_json::to_string_pretty(&snaps)?);
                } else {
                    for s in &snaps {
                        print!("{}", render::render_snapshot(s));
                    }
                }
            }
            Cmd::Raid => {
                if cli.json {
                    let v: Vec<_> = snaps.iter().map(|s| &s.raids).collect();
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    for s in &snaps {
                        for r in s.raids.iter().filter(|r| r.valid) {
                            print!("{}", render::render_raid_detail(r));
                        }
                    }
                }
            }
            Cmd::Disks => {
                if cli.json {
                    let v: Vec<_> = snaps.iter().map(|s| &s.disks).collect();
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    for s in &snaps {
                        print!(
                            "{}",
                            render::render_snapshot(s)
                                .split("\nDisks\n")
                                .nth(1)
                                .unwrap_or("")
                        );
                    }
                }
            }
            Cmd::Hwmon => {
                for s in &snaps {
                    match &s.controller.hw_monitor {
                        Some(h) if cli.json => println!("{}", serde_json::to_string_pretty(h)?),
                        Some(h) => {
                            println!("{}", s.device);
                            println!(
                                "  fan: {}",
                                if h.fan_stopped {
                                    "stopped".into()
                                } else {
                                    format!("{} rpm", h.fan_rpm)
                                }
                            );
                            println!(
                                "  fan alarm: {}   temperature alarm: {}",
                                h.fan_alarm, h.temperature_alarm
                            );
                            for (i, v) in h.voltages.iter().enumerate() {
                                println!(
                                    "  voltage ch{}: {} (limits {}..{}) {}",
                                    i + 1,
                                    v.value,
                                    v.min,
                                    v.max,
                                    if v.in_range { "ok" } else { "OUT OF RANGE" }
                                );
                            }
                        }
                        None => println!("{}: no hardware monitor reported", s.device),
                    }
                }
            }
            Cmd::Events => {
                for s in &snaps {
                    let e = &s.controller.events;
                    if cli.json {
                        println!("{}", serde_json::to_string_pretty(e)?);
                    } else {
                        println!("{}: pending={} controller_changed={} disk_port_mask={:#04x} rebuild_port_mask={:#04x} words={:#010x}/{:#010x}/{:#06x}\n  vendor notification group {}: {}\n  (latched since the last acknowledgement by the vendor software; not an error)",
                            s.device, e.any_pending, e.controller_changed, e.disk_ports_mask, e.rebuild_ports_mask, e.word_1e4, e.word_1e8, e.word_1ec, e.notification_group(), e.group_name());
                    }
                }
            }
            Cmd::Raw => {
                for s in &snaps {
                    println!("== {} chip info\n{}", s.device, s.controller.raw_hex);
                    for d in &s.disks {
                        println!(
                            "== sata disk {}\n{}\n{}",
                            d.port, d.raw_all_sata_hex, d.raw_port_hex
                        );
                    }
                    for r in &s.raids {
                        println!("== raid port {}\n{}", r.index, r.raw_hex);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(worst)
}

fn monitor(s: &mut Session<BlockDevice>, size: Option<u64>, interval: u64) -> Result<()> {
    let mut prev: Option<Snapshot> = None;
    loop {
        let cur = api::collect(s, size)?;
        match &prev {
            None => print!("{}", render::render_snapshot(&cur)),
            Some(p) => {
                for e in diff(p, &cur) {
                    println!("{}", e);
                }
            }
        }
        for r in cur
            .raids
            .iter()
            .filter(|r| r.valid && matches!(r.state_code, 2 | 5))
        {
            if let Some(pct) = r.rebuild_percent {
                println!("RAID {} {}: {}%", r.index, r.state, pct);
            }
        }
        prev = Some(cur);
        std::thread::sleep(Duration::from_secs(interval));
    }
}

/// Same wording as the vendor event log (`st_szEventMsg`).
fn diff(a: &Snapshot, b: &Snapshot) -> Vec<String> {
    let mut v = vec![];
    for (x, y) in a.disks.iter().zip(b.disks.iter()) {
        if x.present != y.present {
            v.push(format!(
                "port {}: {}",
                y.port,
                if y.present {
                    "The disk is plugged in."
                } else {
                    "The disk is unplugged."
                }
            ));
        }
    }
    for (x, y) in a.raids.iter().zip(b.raids.iter()) {
        if !x.valid && y.valid {
            v.push(format!("RAID {} ({}) is created.", y.index, y.name));
        } else if x.valid && !y.valid {
            v.push(format!("RAID {} ({}) is deleted.", y.index, x.name));
        } else if y.valid && x.state_code != y.state_code {
            if matches!(x.state_code, 2 | 5) && y.state_code == 3 {
                v.push(format!(
                    "RAID {} ({}) has been rebuilt successfully.",
                    y.index, y.name
                ));
            }
            v.push(format!(
                "RAID {} ({}) is in {} state.",
                y.index,
                y.name,
                model::raid_state_name(y.state_code).to_uppercase()
            ));
        }
    }
    if let (Some(x), Some(y)) = (&a.controller.hw_monitor, &b.controller.hw_monitor) {
        if !x.fan_alarm && y.fan_alarm {
            v.push("The fan speed is abnormal.".into());
        }
        if !x.temperature_alarm && y.temperature_alarm {
            v.push("The temperature is abnormal.".into());
        }
        let (bx, by) = (
            x.voltages.iter().all(|c| c.in_range),
            y.voltages.iter().all(|c| c.in_range),
        );
        if bx && !by {
            v.push("The voltage is abnormal.".into());
        }
    }
    v
}
