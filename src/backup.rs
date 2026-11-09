//! Persistent backups of the sector borrowed as mailbox, and an explicit restore.
//!
//! The tool never writes a backup back by itself: once the controller is unlocked it may
//! execute whatever valid frame lands in the mailbox, and the "original" content can be a stale
//! command frame left by a previous (vendor) session.

use crate::io::SectorIo;
use crate::proto::{crc_fast, scramble, FRAME_MAGIC, HANDSHAKE_MAGIC, SECTOR};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Backup {
    pub device: String,
    pub lba: u64,
    pub data_hex: String,
    pub unix_time: u64,
}

pub fn backup_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("ORICO_RAID_STATE_DIR") {
        return d.into();
    }
    let p = PathBuf::from("/var/lib/orico-raid");
    if std::fs::create_dir_all(&p).is_ok() {
        p
    } else {
        PathBuf::from("/var/tmp")
    }
}

pub fn backup_path(device: &str, lba: u64) -> PathBuf {
    let n: String = device
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    backup_dir().join(format!("sector-backup{n}-lba{lba}.json"))
}

/// Save the pristine sector. An existing backup is never overwritten: it is the oldest, and
/// the sector may by now contain our own frames.
pub fn save(device: &str, lba: u64, sector: &[u8; SECTOR]) -> Result<Option<PathBuf>> {
    let path = backup_path(device, lba);
    if path.exists() {
        // Reuse the original backup only if it is valid and belongs to this sector.
        // Never replace it: the on-disk sector may already contain our protocol frames.
        let existing = load(&path)
            .with_context(|| format!("validating existing backup {}", path.display()))?;
        if existing.device != device || existing.lba != lba {
            bail!(
                "existing backup {} identifies {} LBA {}, expected {} LBA {}",
                path.display(),
                existing.device,
                existing.lba,
                device,
                lba
            );
        }
        data(&existing)
            .with_context(|| format!("validating existing backup data in {}", path.display()))?;
        return Ok(Some(path));
    }
    let b = Backup {
        device: device.into(),
        lba,
        data_hex: sector.iter().map(|x| format!("{x:02x}")).collect(),
        unix_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&b)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path))
}

pub fn load(path: &Path) -> Result<Backup> {
    Ok(serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )?)
}

pub fn list() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(backup_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("sector-backup"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn data(b: &Backup) -> Result<[u8; SECTOR]> {
    if b.data_hex.len() != SECTOR * 2 {
        bail!("corrupt backup");
    }
    let mut d = [0u8; SECTOR];
    for (i, byte) in d.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&b.data_hex[2 * i..2 * i + 2], 16)?;
    }
    Ok(d)
}

/// Would the controller treat this sector content as a command / handshake?
pub fn looks_like_frame(d: &[u8; SECTOR]) -> bool {
    if u32::from_le_bytes(d[0..4].try_into().unwrap()) == HANDSHAKE_MAGIC {
        return true;
    }
    let mut f = *d;
    scramble(&mut f);
    u32::from_le_bytes(f[0..4].try_into().unwrap()) == FRAME_MAGIC
        && f[0x1fc..] == crc_fast(&f[..0x1fc]).to_le_bytes()
}

/// Write a backup back. Refuses content that looks like a controller frame unless `force`.
pub fn restore<IO: SectorIo + ?Sized>(io: &mut IO, b: &Backup, force: bool) -> Result<()> {
    let d = data(b)?;
    if looks_like_frame(&d) && !force {
        bail!("the backed-up sector looks like a controller command frame (probably left by an earlier session); writing it could make the controller execute it. Use --force only if you are sure");
    }
    io.write_sector(b.lba, &d)
}
