//! Sector I/O abstraction: real block devices (O_DIRECT) and a test double.

use crate::proto::SECTOR;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub trait SectorIo {
    fn read_sector(&mut self, lba: u64) -> Result<[u8; SECTOR]>;
    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR]) -> Result<()>;
    fn name(&self) -> String;
}

#[repr(C, align(4096))]
struct Aligned([u8; SECTOR]);

/// A whole-disk block device opened for uncached 512-byte sector access.
pub struct BlockDevice {
    file: File,
    path: PathBuf,
}

impl BlockDevice {
    /// `writable` is needed for the mailbox protocol (it writes command frames).
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .custom_flags(libc::O_DIRECT | libc::O_SYNC)
            .open(path)
            .with_context(|| format!("cannot open {} (need root?)", path.display()))?;
        let dev = BlockDevice { file, path: path.to_path_buf() };
        dev.check_geometry()?;
        Ok(dev)
    }

    fn check_geometry(&self) -> Result<()> {
        const BLKSSZGET: libc::c_ulong = 0x1268;
        let mut ss: libc::c_int = 0;
        // SAFETY: BLKSSZGET writes one c_int.
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKSSZGET, &mut ss) };
        if r == 0 && ss as usize != SECTOR {
            bail!(
                "{}: logical sector size is {} bytes; the JMicron mailbox protocol needs 512",
                self.path.display(),
                ss
            );
        }
        Ok(())
    }

    /// Size in bytes (BLKGETSIZE64).
    pub fn size_bytes(&self) -> Option<u64> {
        const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
        let mut sz: u64 = 0;
        let r = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKGETSIZE64, &mut sz) };
        (r == 0).then_some(sz)
    }
}

impl SectorIo for BlockDevice {
    fn read_sector(&mut self, lba: u64) -> Result<[u8; SECTOR]> {
        let mut buf = Aligned([0; SECTOR]);
        let n = unsafe {
            libc::pread(
                self.file.as_raw_fd(),
                buf.0.as_mut_ptr() as *mut libc::c_void,
                SECTOR,
                (lba * SECTOR as u64) as libc::off_t,
            )
        };
        if n != SECTOR as isize {
            bail!("short/failed read at LBA {lba}: {}", std::io::Error::last_os_error());
        }
        Ok(buf.0)
    }

    fn write_sector(&mut self, lba: u64, data: &[u8; SECTOR]) -> Result<()> {
        let mut buf = Aligned(*data);
        let n = unsafe {
            libc::pwrite(
                self.file.as_raw_fd(),
                buf.0.as_mut_ptr() as *const libc::c_void,
                SECTOR,
                (lba * SECTOR as u64) as libc::off_t,
            )
        };
        if n != SECTOR as isize {
            bail!("short/failed write at LBA {lba}: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn name(&self) -> String {
        self.path.display().to_string()
    }
}
