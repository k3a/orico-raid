//! Test double: a fake JMicron controller behind a sector interface.
//! It enforces the same rules as the real bridge firmware as far as they are
//! known (handshake order, scrambling, CRC) so the host code is exercised end to end.

use crate::io::SectorIo;
use crate::proto::*;
use anyhow::Result;
use std::collections::HashMap;

pub struct MockController {
    pub sectors: HashMap<u64, [u8; SECTOR]>,
    pub mailbox: u64,
    hs_progress: usize,
    unlocked: bool,
    pub chip: Vec<u8>,
    pub all_sata: Vec<u8>,
    pub raid: Vec<Vec<u8>>,
    pub sata: Vec<Vec<u8>>,
    pub smart_values: Vec<u8>,
    pub smart_thresholds: Vec<u8>,
    pub alarm: bool,
    pub log: Vec<(u8, u8)>,
    /// reject everything (simulates a plain disk that merely stores the sector)
    pub plain_disk: bool,
}

impl MockController {
    pub fn new(mailbox: u64) -> Self {
        let mut sectors = HashMap::new();
        // protective MBR + GPT header so find_mailbox_lba picks 0x27
        let mut s0 = [0u8; SECTOR];
        s0[0x1c2] = 0xee;
        s0[0x1c6] = 1;
        s0[0x1fe] = 0x55;
        s0[0x1ff] = 0xaa;
        sectors.insert(0, s0);
        let mut s1 = [0u8; SECTOR];
        s1[0..8].copy_from_slice(b"EFI PART");
        sectors.insert(1, s1);
        sectors.insert(2, [0u8; SECTOR]);
        let mut marker = [0u8; SECTOR];
        marker[..16].copy_from_slice(b"user data in gap");
        sectors.insert(mailbox, marker);
        MockController {
            sectors,
            mailbox,
            hs_progress: 0,
            unlocked: false,
            chip: vec![0; 0x1f4],
            all_sata: vec![0; 0x190],
            raid: vec![vec![0; 0x1a4]; 5],
            sata: vec![vec![0; 0x184]; 5],
            smart_values: vec![0; 512],
            smart_thresholds: vec![0; 512],
            alarm: false,
            log: vec![],
            plain_disk: false,
        }
    }

    fn respond(&mut self, cmd_frame: &[u8; SECTOR]) -> [u8; SECTOR] {
        let (kind, sub) = (cmd_frame[9], cmd_frame[10]);
        self.log.push((kind, sub));
        let mut out = [0u8; SECTOR];
        out[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        out[0] = 0x5a; // pid
        out[4..8].copy_from_slice(&cmd_frame[4..8]);
        out[9] = kind;
        out[10] = sub;
        out[11] = 0;
        let port = cmd_frame[0xc] as usize;
        let (win_off, win_len): (usize, usize);
        let data: Vec<u8> = match (kind, sub) {
            (1, 1) => { win_off = 0xc; win_len = 0x1f4; self.chip.clone() }
            (2, 1) => { win_off = 0x10; win_len = 0x1f0; self.all_sata.clone() }
            (2, 2) => { assert_eq!(cmd_frame[0x10] as usize, port); win_off = 0xc; win_len = 0x1f4; self.sata[port].clone() }
            (3, 2) => { assert_eq!(cmd_frame[0x10] as usize, port); win_off = 0xc; win_len = 0x1f4; self.raid[port].clone() }
            (2, 3) => {
                win_off = 0x10; win_len = 0x1f0;
                assert_eq!(&cmd_frame[0xc..0x10], &[port as u8, 2, 0, 0xe0]);
                let feat = cmd_frame[0x12];
                assert_eq!(cmd_frame[0x1e], 0xb0); // SMART opcode in the FIS
                if feat == 0xd0 { self.smart_values.clone() } else { self.smart_thresholds.clone() }
            }
            (1, 8) => {
                win_off = 0xc; win_len = 0x1f4;
                let mut d = vec![0u8; 0x1f4];
                if self.alarm { d[0] = 0; d[2] = 1 } else { d[0] = 1 }
                d
            }
            (1, 6) => { self.alarm = false; win_off = 0xc; win_len = 0x1f4; vec![0; 0x1f4] }
            _ => { out[11] = 0x80; return out; }
        };
        out[win_off..win_off + data.len().min(win_len)].copy_from_slice(&data[..data.len().min(win_len)]);
        let crc = crc_fast(&out[..0x1fc]);
        out[0x1fc..].copy_from_slice(&crc.to_le_bytes());
        out
    }
}

impl SectorIo for MockController {
    fn read_sector(&mut self, lba: u64) -> Result<[u8; SECTOR]> {
        Ok(*self.sectors.get(&lba).unwrap_or(&[0u8; SECTOR]))
    }

    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR]) -> Result<()> {
        if self.plain_disk || lba != self.mailbox {
            self.sectors.insert(lba, *data);
            return Ok(());
        }
        // handshake frames are plain
        if u32::from_le_bytes(data[0..4].try_into().unwrap()) == HANDSHAKE_MAGIC {
            let want = handshake_frame(HANDSHAKE_SEQUENCE[self.hs_progress.min(3)]);
            if *data == want {
                self.hs_progress += 1;
                if self.hs_progress == 4 {
                    self.unlocked = true;
                }
            } else {
                self.hs_progress = 0;
                self.unlocked = false;
            }
            self.sectors.insert(lba, *data);
            return Ok(());
        }
        if !self.unlocked {
            self.sectors.insert(lba, *data);
            return Ok(());
        }
        let mut f = *data;
        scramble(&mut f);
        if u32::from_le_bytes(f[0..4].try_into().unwrap()) != FRAME_MAGIC {
            // not a management frame: ordinary data write passes through to the disk
            self.sectors.insert(lba, *data);
            return Ok(());
        }
        let crc = crc_fast(&f[..0x1fc]);
        if f[0x1fc..] != crc.to_le_bytes() {
            // firmware silently ignores frames with a bad CRC: leave sector as written
            self.sectors.insert(lba, *data);
            return Ok(());
        }
        let mut resp = self.respond(&f);
        scramble(&mut resp);
        self.sectors.insert(lba, resp);
        Ok(())
    }

    fn name(&self) -> String {
        "mock".into()
    }
}
