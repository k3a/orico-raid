# orico-raid

Linux command-line replacement for the **ORICO RAID Manager** (JMicron "HW RAID Manager") –
status and health of JMicron JMB39x / JMS56x hardware-RAID enclosures such as the ORICO 9558RU3 /
9528RU3 family, without the Qt GUI. Written in Rust, no runtime dependencies besides libc.

```
sudo orico-raid list                     # candidate disks (sysfs only, no I/O)
sudo orico-raid status                   # controller + RAID volumes + disks + health
sudo orico-raid status -d /dev/sdb --json
sudo orico-raid raid                     # RAID level / state / members / rebuild %
sudo orico-raid disks
sudo orico-raid smart 0                  # SMART attributes of the disk on port 0
sudo orico-raid hwmon                    # fan rpm, voltage ranges, fan/temperature alarms
sudo orico-raid alarm [--mute]
sudo orico-raid events                   # pending event flags (not acknowledged)
sudo orico-raid monitor -i 5             # print state changes like the GUI event log
sudo orico-raid check                    # Nagios style: exit 0 OK / 1 WARN / 2 CRIT / 3 UNKNOWN
sudo orico-raid raw                      # hex dump of every response (for bug reports)
```

Needs root (raw block-device access). `-d/--device` selects the disk the enclosure exports; without it
every `sd*` that looks like a JMicron/ORICO bridge (USB id `152d:*`, or vendor string) is probed.

## What it does *not* do

Reconfiguration (create / delete RAID, spares, passwords, firmware flash, e-mail notification,
tray icon) is **not implemented**. The protocol for it is in the vendor code (`ConfigRaid`,
`SetSpareDisk`, `_FlashWriteData`, …) but I deliberately did not port destructive operations that
cannot be tested without hardware. The only write command is `alarm --mute`.

## How it works (reverse engineering summary)

See [`docs/PROTOCOL.md`](docs/PROTOCOL.md). Short version: the bridge has no SCSI/ATA vendor
commands for management. The host unlocks a "mailbox" by writing a handshake frame to a spare
sector of the exported disk (inside the gap before the first partition), then exchanges 512-byte
scrambled, CRC-protected command/response frames through that same sector.

## Safety

* The mailbox is a normal sector of the disk. The tool saves its content first and writes it back
  when done (also on error, in `Drop`). A copy is kept in `$TMPDIR/orico-raid-sector-backup*.bin`
  while the sector is borrowed – if the process is killed (SIGKILL / power loss) restore with
  `dd if=<backup> of=/dev/sdX bs=512 seek=<lba> count=1 oflag=direct`.
* Sectors 0x23..0x3f (depending on the partition table) are used. A disk that keeps boot-loader code there (GRUB `core.img` on an
  MBR disk with the first partition at LBA 63) is briefly overwritten.
* Never run it against disks you do not mean to probe: on a non-JMicron disk the handshake and
  command frames are ordinary writes to that sector (restored immediately, but still writes).
  Auto-detection only considers JMicron/ORICO-looking `sd*` devices.
* The vendor tool does exactly the same, for every physical drive it finds.

## Status of the decoding – what is verified and what is inferred

| Part | Evidence |
|---|---|
| Frame layout, CRC, key-stream scrambler, handshake, request encodings for chip-info / all-sata / sata-port / raid-port / SMART FIS | **Byte-identical** to what the unmodified vendor machine code (`raidapi.o`, run in a CPU emulator) writes (`tests/e2e.rs::frames_match_vendor_code`). Response framing checked the same way. |
| Mailbox sector selection (`_FindUnusedSector`) | Ported from the decompiled code, unit tested. |
| RAID state / level names, RAID 10 = mirror with >3 members, rebuild % formula, SMART layout, fan RPM formula, voltage range checks | Taken from the decompiled vendor API + GUI tables (`st_szRaidState`, `st_szRaidLevel`). |
| Capacity unit (32 MiB) | Inferred: the GUI prints these fields as `value / 32 GB`. Compare with `os_device_bytes` in `--json`. |
| Meaning of `status_flags`, membership `kind` codes, `recorded_member_*`, event bit masks, `capabilities` | Best effort from usage in the vendor code; exposed raw so they can be checked against real hardware. |

**This was developed without access to the hardware** – only the vendor binaries. The request/response
layers are verified against the vendor code; the interpretation of some response fields is not.
If something looks off on your enclosure, please run `orico-raid raw` and compare with
`docs/PROTOCOL.md`.

## Building

```
cargo build --release      # target/release/orico-raid
cargo test
```
