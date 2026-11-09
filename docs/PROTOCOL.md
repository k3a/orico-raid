# JMicron RAID management protocol (as used by ORICO RAID Manager 5.6.x)

Source material: `Win/…/Application/raidapi.o` (MinGW object with full C++ symbols – the vendor API layer)
and `Mac/…/HWRaidManager` (Qt GUI, symbols for GUI classes and string/enum tables). Tools used: Ghidra
(decompilation), Unicorn (running the vendor machine code as a test oracle). See `re/`.

## Transport

Every operation is "write one 512-byte sector, read it back" on a sector of the disk that the bridge
exports (`\\.\PhysicalDriveN`, here `/dev/sdX`). Byte offset = LBA × 512; sectors below 10 are refused by the
vendor code.

**Mailbox LBA** (`_FindUnusedSector(slot)`), slot 0..4:

| Disk | LBA |
|---|---|
| sector 0 read fails | `0x27 - slot` |
| protective MBR (boot flag 0, type 0xEE, start LBA low byte 1) + `EFI PART` at LBA 1 | `0x27`, or (low byte of first entry's FirstLBA)−1 if >1; minus slot |
| `55 AA` MBR | `0x3e` if first partition LBA low byte <2, else min(low byte−1, 0x3e); minus slot |
| Apple `ER\2\0` | `0x3f - slot` |
| otherwise | `0x27 - slot` |
| result < 0xb | `0x27 - slot` |

The vendor tool skips a slot whose sector already starts with `0x197b0562`.

**Session**

1. save the sector, 2. write four handshake frames (modes 2,3,0,1), 3. `GetChipInfo`; if that fails the
disk is not a controller and the sector is restored, 4. run commands, 5. restore the sector.

## Frames

Handshake (plain, `_HandShakeCmd`):

```
00  u32 0x197b0325
04  u32 key[mode]    0:0x689705f3 1:0xe00c523a 2:0x3c75a80b 3:0x0388e337
10  u8 0x10, 0x11, 0x12 … up to offset 0x1f7 (byte counter, wraps 0xff→0x00)
1f8 u32 -(sum of u32 at offsets 0,8,16,…,0x1f0)      (vendor strides by 8; reproduced)
1fc u32 CRC
```

Command (`_InvokeCommand`) before scrambling:

```
00  u32 0x197b0562 ("g_VendorID")
04  u32 sequence (host counter 1..0x7fffffff, echoed by the controller)
08  u8  0
09  u8  kind        0a u8 sub        0b u8 0xff (controller overwrites with status, 0 = OK)
0c.. arguments / 10.. payload (per command, below)
1fa u16 10  (0 for kind 1 / sub 1)
1fc u32 CRC
```

* **CRC** (`_CrcFast`): CRC-32, poly 0x04C11DB7 MSB-first table, seed `0x52325032`, no final xor,
  computed over bytes 0..0x1fc where each little-endian dword is fed MSB-first.
* **Scrambling** (`_Scrambling`): a 32-bit key register (seed 0x52325032) is advanced per dword by a fixed
  GF(2) matrix; the dword is XORed with a fixed bit permutation of the key. The key chain does not
  depend on the data, so the whole thing is XOR with a constant 512-byte key-stream, identical for write
  and read (`proto.rs`, matrices recovered from the decompilation and checked against the original code).
* The response is the same sector re-read, unscrambled. It is valid if `seq` matches, `kind/sub` match and
  byte 0x0b is 0. Byte 0 is a product id (`g_Pid`). Data window: `0x1f0` bytes at 0x10 for
  (2,1) (2,3) (2,6) (3,1) (4,1) (4,4); otherwise `0x1f4` bytes at 0x0c.
* Retries: 3 attempts, 200 ms apart.

## Commands used by the read-only tool

| kind,sub | name | arguments |
|---|---|---|
| 1,1 | GetChipInfo | – |
| 2,1 | GetAllSataInfo | – |
| 2,2 | GetSataPortInfo | port at 0x0c **and** 0x10 |
| 3,2 | GetRaidPortInfo | port at 0x0c **and** 0x10 |
| 2,3 | SentFisCmd | `0c: port, 02, 00, e0`, 0x1e0-byte FIS at 0x10 |
| 1,8 | CheckAlarmStatus | zeros; reply data[0]==0 && data[2]==1 ⇒ alarm sounding |
| 1,6 | SetAlarmMute | 0c: 1 |

Other commands seen in the vendor code (not used): (1,2) set chip id, (1,3) acknowledge notifications,
(1,4) access chip register, (1,7) auto-rebuild flag, (3,4) set RAID config, (3,6) modify RAID parameters
(password, name, rebuild priority, standby timer), (2,6) identify/blink, SPI flash read/write/erase
(firmware update), (4,x) flash.

SMART: FIS bytes `[2]=0xd0` (data) / `0xd1` (thresholds), `[8]=0x4f`, `[10]=0xc2`, `[0xc]=0xa0`, `[0xe]=0xb0`.
Response = standard 512-byte SMART sector (first 0x16a bytes): 2-byte revision + 30×12-byte attributes.

## Response structures (offsets in the data window)

**Chip info (1,1), 0x1f4**: `[1]` flags (bit7 = hardware monitor present), `[2..4]` chip code, `[4..8]`
capability bits, `[0x14..0x54]` firmware info blob, `[0x54..0x77]` hardware-monitor registers,
`[0x94..0x9c]` IC signature (bitwise inverted ASCII), `[0x1e4..0x1ee]` pending event words,
`[0x1ee]` assigned chip id.

Hardware monitor: `[0x54]!=0`, flags `[0x55]` (0x10 voltage monitor, 0x20 fan alarm, 0x40 temperature alarm),
multiplier u32 `[0x56]`; three voltage channels `value = adc·(hi + lo/10)·m` with `(adc,scale,min,max)` =
`([6a],[6b],[68],[69])`, `([6e],[6f],[6c],[6d])`, `([72],[73],[70],[71])`, scale nibbles hi/lo; fan:
`rpm = 1350000 / ((2<<([75]-1))·[76])`, `[76]==0xff` ⇒ stopped.

**All-SATA (2,1)**, 5 × 0x50: `[0..0x28]` model, `[0x28..0x3c]` serial (ATA pair-swapped), `[0x3c]` u32 capacity
(×32 MiB), `[0x41..0x43]` membership A (kind, RAID index, slot), `[0x45..0x47]` membership B, `[0x48]` usage
(1 free, 2 in RAID), `[0x4b]`/`[0x4d]` status flag bytes, `[0x4c]&1` disk present.

**SATA port (2,2)**, 0x184: `[0x44..0x4d]` firmware revision, `[0x84]/[0xac]` model/serial recorded for member A,
`[0xc6]` flag, `[0xcc]` 8-byte id; same at `[0x104]/[0x12c]/[0x146]/[0x14c]` for member B.

**RAID port (3,2)**, 0x1a4: `[4..0x2d]` name, `[0x2c..0x41]` serial, `[0x40]` u32 capacity, `[0x46]` state
(0 Broken, 1 Degraded, 2 Rebuilding, 3 Normal, 4 Expansion, 5 Backup; 2 + flags 0xa0 ⇒ Backup),
`[0x54]` level (0 RAID 0, 1 RAID 1, 2 LARGE, 3 RAID 3, 4 CLONE, 5 RAID 5, 6 RAID 10, 7 RAID 0 Opt; level 1
with >3 members = RAID 10), `[0x55]` member count, `[0x5a]` u16 flags (bit0 volume exists),
`[0x60]` u32 rebuild progress (percent = progress·100/capacity), `[0x64]` rebuild priority one-hot
(0x4000…0x400 = 1…5), `[0x66]` standby minutes, `[0x7c..0x84]` password hash, members at
`0xa0 + 0x20·k`: valid `[+4]`, group `[+0xa]`, disk port `[+0xb]`, used capacity u32 `[+0x10]`.

## Reproducing the analysis

* `re/raidapi_decompiled.c` – Ghidra decompilation of every function in `raidapi.o` (`re/Dump.py`).
* `re/derive_lfsr.py` – extracts the scrambler matrices from that decompilation and checks them
  against the original machine code.
* `re/oracle.py` – loads `raidapi.o` into Unicorn, relocates it, stubs the Win32 imports, runs the
  vendor wrappers against a fake disk and dumps the exact sectors they write (`tests/fixtures/golden.json`).
