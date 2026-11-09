//! A management session with one controller: mailbox sector discovery,
//! handshake, and command / response exchange.

use crate::io::SectorIo;
use crate::proto::{self, Command, Response, FRAME_MAGIC, HANDSHAKE_SEQUENCE, SECTOR};
use anyhow::{anyhow, bail, Result};
use std::time::Duration;

/// Which sector of the exported disk is used as mailbox (`_FindUnusedSector`).
///
/// The vendor tool parks the mailbox inside the unused gap in front of the first
/// partition: sector 0x27 for GPT (or no table), `first_partition_lba-1` (max 62)
/// for MBR, 0x3f for Apple partition maps. `slot` (0..4) walks downwards to find
/// a sector that does not still hold a stale controller frame.
pub fn find_mailbox_lba<IO: SectorIo + ?Sized>(io: &mut IO, slot: u8) -> u64 {
    let slot = slot as u32;
    let fallback = 0x27u32.wrapping_sub(slot);
    let finish = |c: u32| -> u64 { (if c < 0xb { fallback } else { c }) as u64 };
    let s0 = match io.read_sector(0) {
        Ok(s) => s,
        Err(_) => return fallback as u64,
    };
    let mut c: u32 = 0x27;
    if s0[0x1be] == 0 && s0[0x1c2] == 0xee && s0[0x1c6] == 1 {
        // protective MBR -> GPT
        let Ok(hdr) = io.read_sector(1) else {
            return fallback as u64;
        };
        if &hdr[0..8] != b"EFI PART" {
            return finish(c); // vendor skips the slot subtraction here
        }
        let Ok(ent) = io.read_sector(2) else {
            return fallback as u64;
        };
        // the vendor code looks only at the low byte of the first entry's FirstLBA
        if ent[0x20] > 1 {
            c = ent[0x20] as u32 - 1;
        }
    } else if s0[0x1fe] == 0x55 && s0[0x1ff] == 0xaa {
        // MBR: just in front of the first partition, at most sector 62
        c = if s0[0x1c6] < 2 {
            0x3e
        } else {
            (s0[0x1c6] as u32 - 1).min(0x3e)
        };
    } else if &s0[0..2] == b"ER" && s0[2] == 2 && s0[3] == 0 {
        c = 0x3f; // Apple partition map
    }
    finish(c.wrapping_sub(slot))
}

pub struct Session<IO: SectorIo> {
    pub io: IO,
    pub lba: u64,
    seq: u32,
    pub saved: [u8; SECTOR],
    pub backup_file: Option<std::path::PathBuf>,
    pub pid: u8,
    pub retry_delay: Duration,
}

impl<IO: SectorIo> Session<IO> {
    /// Probe `io` for a JMicron management channel. The sector backup must be persisted
    /// successfully before any protocol writes are issued.
    pub fn open(mut io: IO) -> Result<Self> {
        // pick a mailbox sector that is not already holding a controller frame
        let mut lba = None;
        for slot in 0..5u8 {
            let l = find_mailbox_lba(&mut io, slot);
            if let Ok(s) = io.read_sector(l) {
                if u32::from_le_bytes(s[0..4].try_into().unwrap()) != FRAME_MAGIC {
                    lba = Some(l);
                    break;
                }
            }
        }
        let lba = match lba {
            Some(l) => l,
            None => bail!("no usable mailbox sector found on {}", io.name()),
        };
        let saved = io.read_sector(lba)?;
        // The persistent copy must exist before the first write: the in-memory copy
        // cannot help if probing fails or the process/device stops responding.
        let backup_file = crate::backup::save(&io.name(), lba, &saved)?;
        let mut s = Session {
            io,
            lba,
            seq: 1,
            saved,
            backup_file,
            pid: 0,
            retry_delay: Duration::from_millis(200),
        };
        for m in HANDSHAKE_SEQUENCE {
            s.io.write_sector(lba, &proto::handshake_frame(m))?;
        }
        match s.command(&Command::new(1, 1)) {
            Ok(r) => {
                s.pid = r.pid;
                Ok(s)
            }
            Err(e) => Err(anyhow!(
                "{} does not answer as a JMicron RAID controller: {e}",
                s.io.name()
            )),
        }
    }

    fn next_seq(&mut self) -> u32 {
        let s = self.seq;
        self.seq = if self.seq >= 0x7fff_ffff {
            1
        } else {
            self.seq + 1
        };
        s
    }

    /// `_InvokeCommand`: write the frame, read it back, retry up to 3 times.
    pub fn command(&mut self, cmd: &Command) -> Result<Response> {
        let seq = self.next_seq();
        let mut last = anyhow!("no attempt made");
        for attempt in 0..3 {
            let mut f = cmd.frame(seq);
            proto::scramble(&mut f);
            if let Err(e) = self.io.write_sector(self.lba, &f) {
                last = e;
            } else {
                match self.io.read_sector(self.lba) {
                    Ok(raw) => match proto::parse_response(&raw, cmd, seq) {
                        Ok(r) => return Ok(r),
                        Err(e) => last = e.into(),
                    },
                    Err(e) => last = e,
                }
            }
            if attempt < 2 {
                std::thread::sleep(self.retry_delay);
            }
        }
        Err(last)
    }
}
