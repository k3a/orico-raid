//! Wire protocol of the JMicron "mailbox" RAID management channel.
//!
//! Reverse engineered from `raidapi.o` (`_InvokeCommand`, `_HandShakeCmd`,
//! `_Scrambling`, `_CrcFast`) shipped with ORICO RAID Manager 5.6.x.
//!
//! The controller (JMB39x / JMS56x) has no vendor SCSI/ATA command set for this.
//! Instead the host reads/writes one ordinary 512-byte sector of the *virtual*
//! disk it exports (a sector inside the unused gap before the first partition).
//! The bridge firmware intercepts accesses to that sector:
//!
//!  * a *handshake* frame (plain text, magic `0x197b0325`) unlocks the channel,
//!  * a *command* frame (magic `0x197b0562`, scrambled with a fixed XOR
//!    key-stream and CRC protected) is written,
//!  * the *response* frame is read back from the same sector.

pub const SECTOR: usize = 512;
/// First dword of every command / response frame.
pub const FRAME_MAGIC: u32 = 0x197b_0562;
/// First dword of a handshake frame.
pub const HANDSHAKE_MAGIC: u32 = 0x197b_0325;
/// Handshake "keys" in the order the vendor tool sends them (modes 2,3,0,1).
pub const HANDSHAKE_SEQUENCE: [u8; 4] = [2, 3, 0, 1];

fn handshake_key(mode: u8) -> u32 {
    match mode {
        1 => 0xe00c_523a,
        2 => 0x3c75_a80b,
        3 => 0x0388_e337,
        _ => 0x6897_05f3,
    }
}

// ---------------------------------------------------------------- CRC --------

/// MSB-first CRC-32 table, poly 0x04C11DB7 (built in `InitAllController`).
fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (i, e) in t.iter_mut().enumerate() {
        let mut c = (i as u32) << 24;
        for _ in 0..8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04c1_1db7
            } else {
                c << 1
            };
        }
        *e = c;
    }
    t
}

/// `_CrcFast`: seed 0x52325032, processes the buffer as little-endian dwords,
/// feeding the four bytes of every dword most-significant byte first.
pub fn crc_fast(data: &[u8]) -> u32 {
    assert!(data.len().is_multiple_of(4));
    let t = crc_table();
    let mut crc = 0x5232_5032u32;
    for w in data.as_chunks::<4>().0 {
        for &b in w.iter().rev() {
            crc = (crc << 8) ^ t[(((crc >> 24) as u8) ^ b) as usize];
        }
    }
    crc
}

// ----------------------------------------------------------- scrambler -------

/// Bit permutation applied to the running key (`_Scrambling`, `local_9c`).
const PERM: [u8; 32] = [
    11, 31, 13, 21, 7, 16, 0, 19, 4, 26, 6, 15, 24, 2, 14, 28, 12, 5, 20, 10, 22, 1, 17, 9, 29, 3,
    25, 8, 18, 23, 30, 27,
];
/// Key-advance matrix: new_key bit j = parity(key & NEXT[j]) (`local_5c`).
const NEXT: [u32; 32] = [
    0xf701_1641,
    0x1903_3ac3,
    0xc507_63c7,
    0x8a0e_c78e,
    0xe31c_995d,
    0x3138_24fb,
    0x6270_49f6,
    0x33e1_85ad,
    0x90c2_1d1b,
    0x2184_3a36,
    0xb409_622d,
    0x9f13_d21b,
    0xc926_b277,
    0x924d_64ee,
    0x249a_c9dc,
    0x4935_93b8,
    0x656a_3131,
    0xcad4_6262,
    0x95a8_c4c4,
    0x2b51_8988,
    0x56a3_1310,
    0xad46_2620,
    0xad8d_5a01,
    0xac1b_a243,
    0x5837_4486,
    0xb06e_890c,
    0x97dc_0459,
    0x2fb8_08b2,
    0x5f70_1164,
    0xbee0_22c8,
    0x7dc0_4590,
    0xfb80_8b20,
];

/// The 512 byte key-stream. The key chain does not depend on the data, so the
/// vendor "scrambler" is a plain XOR with this constant, and it is its own inverse.
pub fn keystream() -> [u8; SECTOR] {
    let mut out = [0u8; SECTOR];
    let mut key = 0x5232_5032u32;
    for w in out.as_chunks_mut::<4>().0 {
        let mut o = 0u32;
        for (i, &p) in PERM.iter().enumerate() {
            o |= ((key >> p) & 1) << i;
        }
        w.copy_from_slice(&o.to_le_bytes());
        let mut nk = 0u32;
        for (j, &m) in NEXT.iter().enumerate() {
            nk |= ((key & m).count_ones() & 1) << j;
        }
        key = nk;
    }
    out
}

pub fn scramble(buf: &mut [u8; SECTOR]) {
    for (b, k) in buf.iter_mut().zip(keystream().iter()) {
        *b ^= *k;
    }
}

// -------------------------------------------------------------- frames -------

/// Handshake sector (`_HandShakeCmd`). Sent in clear.
pub fn handshake_frame(mode: u8) -> [u8; SECTOR] {
    let mut f = [0u8; SECTOR];
    f[0..4].copy_from_slice(&HANDSHAKE_MAGIC.to_le_bytes());
    f[4..8].copy_from_slice(&handshake_key(mode).to_le_bytes());
    f[0x10] = 0x10;
    for i in 0x11..0x1f8 {
        f[i] = f[i - 1].wrapping_add(1); // 0xff wraps to 0x00
    }
    // negated sum of the dwords at offsets 0,8,16,...,0x1f0 (the vendor code
    // strides by 8 – reproduced bug-for-bug)
    let mut sum = 0u32;
    for k in 0..63 {
        let o = k * 8;
        sum = sum.wrapping_add(u32::from_le_bytes(f[o..o + 4].try_into().unwrap()));
    }
    f[0x1f8..0x1fc].copy_from_slice(&sum.wrapping_neg().to_le_bytes());
    let crc = crc_fast(&f[..0x1fc]);
    f[0x1fc..].copy_from_slice(&crc.to_le_bytes());
    f
}

/// A management command. `kind`/`sub` select the function, `args` is placed at
/// frame offset 0x0c and `payload` at 0x10 (see `_InvokeCommand`).
#[derive(Clone, Debug)]
pub struct Command {
    pub kind: u8,
    pub sub: u8,
    /// bytes placed at frame[0x0c..]
    pub args: Vec<u8>,
    /// bytes placed at frame[0x10..] (overlaps `args` past 4 bytes, as the vendor code does)
    pub payload: Vec<u8>,
}

impl Command {
    pub fn new(kind: u8, sub: u8) -> Self {
        Command {
            kind,
            sub,
            args: vec![],
            payload: vec![],
        }
    }
    pub fn args(mut self, a: &[u8]) -> Self {
        self.args = a.to_vec();
        self
    }
    pub fn payload(mut self, p: &[u8]) -> Self {
        self.payload = p.to_vec();
        self
    }

    /// Build the plain (not yet scrambled) 512-byte frame.
    pub fn frame(&self, seq: u32) -> [u8; SECTOR] {
        let mut f = [0u8; SECTOR];
        f[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        f[4..8].copy_from_slice(&seq.to_le_bytes());
        f[8] = 0;
        f[9] = self.kind;
        f[10] = self.sub;
        f[11] = 0xff;
        f[0x0c..0x0c + self.args.len()].copy_from_slice(&self.args);
        f[0x10..0x10 + self.payload.len()].copy_from_slice(&self.payload);
        let flags: u16 = if self.kind == 1 && self.sub == 1 {
            0
        } else {
            10
        };
        f[0x1fa..0x1fc].copy_from_slice(&flags.to_le_bytes());
        let crc = crc_fast(&f[..0x1fc]);
        f[0x1fc..].copy_from_slice(&crc.to_le_bytes());
        f
    }
}

/// Where the useful bytes of a response are, per command class
/// (the `memcpy` selection at the end of `_InvokeCommand`).
fn response_window(kind: u8, sub: u8) -> (usize, usize) {
    match (kind, sub) {
        (2, 1) | (2, 3) | (2, 6) | (3, 1) | (4, 1) | (4, 4) => (0x10, 0x1f0),
        _ => (0x0c, 0x1f4),
    }
}

#[derive(Debug)]
pub enum ResponseError {
    BadMagic(u32),
    BadSeq { want: u32, got: u32 },
    BadCommand { want: (u8, u8), got: (u8, u8) },
    Status(u8),
}

impl std::fmt::Display for ResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseError::BadMagic(m) => write!(
                f,
                "sector does not hold a controller response (first dword {m:#010x})"
            ),
            ResponseError::BadSeq { want, got } => write!(
                f,
                "stale/unknown response (sequence {got}, expected {want})"
            ),
            ResponseError::BadCommand { want, got } => {
                write!(f, "response is for command {got:?}, expected {want:?}")
            }
            ResponseError::Status(s) => write!(f, "controller returned error status {s:#04x}"),
        }
    }
}
impl std::error::Error for ResponseError {}

pub struct Response {
    /// `g_Pid`: first byte of the response frame.
    pub pid: u8,
    /// Useful data window (`0x1f0` or `0x1f4` bytes).
    pub data: Vec<u8>,
}

/// Validate and unwrap a response read back from the mailbox sector.
/// `raw` is the sector as read from disk (still scrambled).
pub fn parse_response(
    raw: &[u8; SECTOR],
    cmd: &Command,
    seq: u32,
) -> Result<Response, ResponseError> {
    let mut f = *raw;
    scramble(&mut f);
    let magic = u32::from_le_bytes(f[0..4].try_into().unwrap());
    let rseq = u32::from_le_bytes(f[4..8].try_into().unwrap());
    // the vendor code does not check the magic of the response, only the sequence
    if rseq != seq {
        return Err(if magic != FRAME_MAGIC && magic != 0 {
            ResponseError::BadMagic(magic)
        } else {
            ResponseError::BadSeq {
                want: seq,
                got: rseq,
            }
        });
    }
    if (f[9], f[10]) != (cmd.kind, cmd.sub) {
        return Err(ResponseError::BadCommand {
            want: (cmd.kind, cmd.sub),
            got: (f[9], f[10]),
        });
    }
    if f[11] != 0 {
        return Err(ResponseError::Status(f[11]));
    }
    let (off, len) = response_window(cmd.kind, cmd.sub);
    Ok(Response {
        pid: f[0],
        data: f[off..off + len].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystream_matches_original_machine_code() {
        // Fixture produced by emulating the unmodified `_Scrambling` from raidapi.o
        let want = include_str!("../tests/fixtures/keystream.hex").trim();
        let got: String = keystream().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn scramble_is_involution() {
        let mut b = [0u8; SECTOR];
        for (i, x) in b.iter_mut().enumerate() {
            *x = (i * 7 + 3) as u8;
        }
        let orig = b;
        scramble(&mut b);
        assert_ne!(b, orig);
        scramble(&mut b);
        assert_eq!(b, orig);
    }

    #[test]
    fn handshake_layout() {
        let f = handshake_frame(0);
        assert_eq!(&f[0..4], &HANDSHAKE_MAGIC.to_le_bytes());
        assert_eq!(&f[4..8], &0x689705f3u32.to_le_bytes());
        assert_eq!(f[0x10], 0x10);
        assert_eq!(f[0x11], 0x11);
        assert_eq!(f[0x1f7], ((0x10 + (0x1f7 - 0x10)) & 0xff) as u8);
        let crc = crc_fast(&f[..0x1fc]);
        assert_eq!(&f[0x1fc..], &crc.to_le_bytes());
    }

    #[test]
    fn chip_info_frame() {
        let c = Command::new(1, 1);
        let f = c.frame(5);
        assert_eq!(&f[0..4], &FRAME_MAGIC.to_le_bytes());
        assert_eq!(f[9], 1);
        assert_eq!(f[10], 1);
        assert_eq!(f[11], 0xff);
        assert_eq!(&f[0x1fa..0x1fc], &[0, 0]);
    }
}
