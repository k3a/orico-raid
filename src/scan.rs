//! Finding candidate block devices through sysfs (no device I/O).

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub dev: PathBuf,
    pub vendor: String,
    pub model: String,
    pub size_bytes: u64,
    pub usb_id: Option<String>,
    pub transport: String,
    /// looks like a JMicron / ORICO bridge
    pub likely: bool,
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Walk up from the block device's sysfs node looking for a USB device
/// (`idVendor`/`idProduct`).
fn usb_id(block: &Path) -> Option<String> {
    let mut p = fs::canonicalize(block.join("device")).ok()?;
    for _ in 0..8 {
        let v = p.join("idVendor");
        if v.exists() {
            return Some(format!("{}:{}", read(&v), read(&p.join("idProduct"))));
        }
        if !p.pop() {
            break;
        }
    }
    None
}

pub fn list() -> Vec<Candidate> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir("/sys/block") else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("sd") {
            continue; // the mailbox protocol is SCSI/SATA/USB-bridge only
        }
        let base = e.path();
        let vendor = read(&base.join("device/vendor"));
        let model = read(&base.join("device/model"));
        let sectors: u64 = read(&base.join("size")).parse().unwrap_or(0);
        let usb = usb_id(&base);
        let transport = if usb.is_some() { "usb" } else { "scsi/sata" }.to_string();
        let jm = usb.as_deref().map(|u| u.starts_with("152d:")).unwrap_or(false);
        let nm = format!("{vendor} {model}").to_ascii_lowercase();
        let likely = jm || nm.contains("jmicron") || nm.contains("orico");
        out.push(Candidate { dev: PathBuf::from("/dev").join(&name), vendor, model, size_bytes: sectors * 512, usb_id: usb, transport, likely });
    }
    out.sort_by(|a, b| a.dev.cmp(&b.dev));
    out
}
