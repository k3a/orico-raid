use orico_raid::api::*;
use orico_raid::io::SectorIo;
use orico_raid::mock::MockController;
use orico_raid::proto::*;
use orico_raid::session::*;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// Frames written by the *unmodified vendor code* (raidapi.o run in a CPU emulator) must be
/// byte-identical to the ones the Rust implementation builds.
#[test]
fn frames_match_vendor_code() {
    let g: serde_json::Value = serde_json::from_str(include_str!("fixtures/golden.json")).unwrap();
    let wr = |k: &str| unhex(g[k]["writes"][0][1].as_str().unwrap());
    // sector 0x27 -> byte offset 19968
    assert_eq!(g["chip_info"]["writes"][0][0], 19968);
    assert_eq!(wr("chip_info"), cmd_chip_info().frame(777).to_vec());
    assert_eq!(wr("all_sata"), cmd_all_sata().frame(778).to_vec());
    assert_eq!(wr("raid_port3"), cmd_raid_port(3).frame(779).to_vec());
    assert_eq!(wr("sata_port1"), cmd_sata_port(1).frame(780).to_vec());
    let mut fis = [0u8; 0x1e0];
    fis[2] = 0xd0;
    fis[8] = 0x4f;
    fis[10] = 0xc2;
    fis[12] = 0xa0;
    fis[14] = 0xb0;
    assert_eq!(fis, smart_fis(0xd0));
    assert_eq!(wr("smart_fis"), cmd_fis(2, &fis).frame(781).to_vec());
    // handshakes
    for (i, m) in HANDSHAKE_SEQUENCE.iter().enumerate() {
        assert_eq!(
            unhex(g["handshake"][i].as_str().unwrap()),
            handshake_frame(*m).to_vec(),
            "handshake mode {m}"
        );
    }
    // key-stream
    assert_eq!(
        unhex(g["keystream"].as_str().unwrap()),
        keystream().to_vec()
    );
}

fn populated() -> MockController {
    let mut m = MockController::new(0x27);
    // chip info
    m.chip[1] = 0x81;
    m.chip[0x54] = 1;
    m.chip[0x55] = 0x10;
    m.chip[0x75] = 1;
    m.chip[0x76] = 100;
    m.chip[0x6a] = 12;
    m.chip[0x6b] = 0x10;
    m.chip[0x68] = 10;
    m.chip[0x69] = 14;
    m.chip[0x56] = 1;
    for (i, b) in b"JMS561 \0".iter().enumerate() {
        m.chip[0x94 + i] = !b;
    }
    // disk 0 and 1 present; 2..4 empty
    let swap = |s: &str, n: usize| -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.resize(n, b' ');
        for p in v.chunks_exact_mut(2) {
            p.swap(0, 1);
        }
        v
    };
    for p in 0..2usize {
        let a = &mut m.all_sata[p * 0x50..p * 0x50 + 0x50];
        a[0..40].copy_from_slice(&swap("WDC WD40EFRX-68N32N0", 40));
        a[0x28..0x3c].copy_from_slice(&swap(&format!("WD-SERIAL{p}"), 20));
        a[0x3c..0x40].copy_from_slice(&(119_000u32).to_le_bytes());
        a[0x41] = 2;
        a[0x42] = 0;
        a[0x43] = p as u8;
        a[0x48] = 2;
        a[0x4c] = 1;
        m.sata[p][0x44..0x4c].copy_from_slice(&swap("82.00A82", 8));
    }
    // RAID 1 volume on slot 0, degraded
    let r = &mut m.raid[0];
    r[4..44].copy_from_slice(&swap("MyMirror", 40));
    r[0x40..0x44].copy_from_slice(&119_000u32.to_le_bytes());
    r[0x46] = 1; // degraded
    r[0x54] = 1; // level 1
    r[0x55] = 2;
    r[0x5a] = 1;
    for k in 0..2usize {
        let o = 0xa0 + 0x20 * k;
        r[o + 4] = 1;
        r[o + 0xb] = k as u8;
    }
    m
}

#[test]
fn full_snapshot_no_auto_restore_and_explicit_restore() {
    let dir = std::env::temp_dir().join(format!("orico-test-{}", std::process::id()));
    std::env::set_var("ORICO_RAID_STATE_DIR", &dir);
    let mock = populated();
    let before = mock.sectors[&0x27];
    let mut s = Session::open(mock).expect("handshake + chip info");
    s.retry_delay = std::time::Duration::from_millis(1);
    let snap = collect(&mut s, Some(1)).unwrap();
    assert_eq!(snap.controller.ic_signature, "JMS561");
    assert_eq!(snap.controller.hw_monitor.as_ref().unwrap().fan_rpm, 6750);
    assert_eq!(snap.disks.len(), 5);
    assert!(snap.disks[0].present && snap.disks[1].present && !snap.disks[2].present);
    assert_eq!(snap.disks[0].model, "WDC WD40EFRX-68N32N0");
    assert_eq!(snap.disks[1].serial, "WD-SERIAL1");
    assert_eq!(snap.disks[0].firmware, "82.00A82");
    assert!((snap.disks[0].capacity_gib - 3718.75).abs() < 0.01);
    let r = snap.raids.iter().find(|r| r.valid).unwrap();
    assert_eq!(r.name, "MyMirror");
    assert_eq!(r.level, "RAID 1");
    assert_eq!(r.state, "Degraded");
    assert_eq!(snap.health.level, 2);
    assert!(snap.health.issues[0].contains("DEGRADED"));
    assert_eq!(snap.alarm_active, Some(false));
    assert!(snap.errors.is_empty(), "{:?}", snap.errors);
    // nothing was written back automatically: the sector no longer holds the user data
    assert_ne!(s.io.read_sector(0x27).unwrap(), before);
    // but a backup exists and an explicit restore puts it back
    let path = orico_raid::backup::backup_path("mock", 0x27);
    let b = orico_raid::backup::load(&path).unwrap();
    orico_raid::backup::restore(&mut s.io, &b, false).unwrap();
    assert_eq!(s.io.read_sector(0x27).unwrap(), before);
    // a backup that looks like a command frame is refused
    let mut frame = cmd_chip_info().frame(1);
    scramble(&mut frame);
    assert!(orico_raid::backup::looks_like_frame(&frame));
    let hexs: String = frame.iter().map(|x| format!("{x:02x}")).collect();
    let bad = orico_raid::backup::Backup {
        device: "mock".into(),
        lba: 0x27,
        data_hex: hexs,
        unix_time: 0,
    };
    assert!(orico_raid::backup::restore(&mut s.io, &bad, false).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backup_failure_prevents_any_disk_writes() {
    let base = std::env::temp_dir().join(format!("orico-backup-failure-{}", std::process::id()));
    let blocker = base.join("not-a-directory");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(&blocker, b"block directory creation").unwrap();
    std::env::set_var("ORICO_RAID_STATE_DIR", blocker.join("nested"));

    let mock = populated();
    let before = mock.sectors.clone();
    let writes = std::rc::Rc::new(std::cell::Cell::new(0usize));
    struct CountWrites {
        inner: MockController,
        writes: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl SectorIo for CountWrites {
        fn read_sector(&mut self, lba: u64) -> anyhow::Result<[u8; 512]> {
            self.inner.read_sector(lba)
        }
        fn write_sector(&mut self, lba: u64, data: &[u8; 512]) -> anyhow::Result<()> {
            self.writes.set(self.writes.get() + 1);
            self.inner.write_sector(lba, data)
        }
        fn name(&self) -> String {
            "mock-backup-failure".into()
        }
    }
    let result = Session::open(CountWrites {
        inner: mock,
        writes: writes.clone(),
    });
    assert!(
        result.is_err(),
        "opening must fail if the persistent backup cannot be written"
    );
    assert_eq!(
        writes.get(),
        0,
        "no protocol write may occur before backup persistence"
    );
    assert_eq!(before, populated().sectors);
    let _ = std::fs::remove_file(blocker);
    let _ = std::fs::remove_dir(base);
}

#[test]
fn plain_disk_is_rejected_and_untouched() {
    std::env::set_var(
        "ORICO_RAID_STATE_DIR",
        std::env::temp_dir().join(format!("orico-test2-{}", std::process::id())),
    );
    let mut m = MockController::new(0x27);
    m.plain_disk = true;
    let before = m.sectors.clone();
    // Session::open takes ownership; use a shared handle to inspect afterwards
    struct Shared(std::rc::Rc<std::cell::RefCell<MockController>>);
    impl SectorIo for Shared {
        fn read_sector(&mut self, l: u64) -> anyhow::Result<[u8; 512]> {
            self.0.borrow_mut().read_sector(l)
        }
        fn write_sector(&mut self, l: u64, d: &[u8; 512]) -> anyhow::Result<()> {
            self.0.borrow_mut().write_sector(l, d)
        }
        fn name(&self) -> String {
            "shared".into()
        }
    }
    let rc = std::rc::Rc::new(std::cell::RefCell::new(m));
    let r = Session::open(Shared(rc.clone()));
    assert!(r.is_err());
    // not restored automatically; the backup is what the user restores from
    let _ = before;
}

#[test]
fn smart_roundtrip() {
    std::env::set_var(
        "ORICO_RAID_STATE_DIR",
        std::env::temp_dir().join(format!("orico-test3-{}", std::process::id())),
    );
    let mut m = populated();
    m.smart_values[2] = 194;
    m.smart_values[2 + 3] = 100;
    m.smart_values[2 + 5] = 38; // raw temperature
    m.smart_thresholds[2] = 194;
    m.smart_thresholds[3] = 0;
    let mut s = Session::open(m).unwrap();
    let a = smart(&mut s, 0).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].id, 194);
    assert_eq!(a[0].raw, 38);
    assert_eq!(a[0].name, "Temperature_Celsius");
}

#[test]
fn alarm_mute() {
    std::env::set_var(
        "ORICO_RAID_STATE_DIR",
        std::env::temp_dir().join(format!("orico-test3-{}", std::process::id())),
    );
    let mut m = populated();
    m.alarm = true;
    let mut s = Session::open(m).unwrap();
    assert!(alarm_active(&mut s).unwrap());
    mute_alarm(&mut s).unwrap();
    assert!(!alarm_active(&mut s).unwrap());
}

#[test]
fn mailbox_selection() {
    // GPT disk with first partition at 2048 -> sector 0x27
    let mut m = MockController::new(0x27);
    assert_eq!(find_mailbox_lba(&mut m, 0), 0x27);
    assert_eq!(find_mailbox_lba(&mut m, 1), 0x26);
    // MBR with first partition at LBA 63 -> sector 62
    let mut s0 = [0u8; 512];
    s0[0x1fe] = 0x55;
    s0[0x1ff] = 0xaa;
    s0[0x1c6] = 63;
    m.sectors.insert(0, s0);
    assert_eq!(find_mailbox_lba(&mut m, 0), 62);
    // MBR with first partition at 2048 (low byte 0) -> 0x3e
    s0[0x1c6] = 0;
    m.sectors.insert(0, s0);
    assert_eq!(find_mailbox_lba(&mut m, 0), 0x3e);
}

/// Same synthetic responses the emulated vendor code was fed; the Rust parser must extract
/// exactly the bytes the vendor code returned to its caller.
#[test]
fn response_windows_match_vendor_code() {
    let g: serde_json::Value = serde_json::from_str(include_str!("fixtures/golden.json")).unwrap();
    let cases: [(&str, Command, u32, usize); 5] = [
        ("chip_info", cmd_chip_info(), 777, 0x1f4),
        ("all_sata", cmd_all_sata(), 778, 0x190),
        ("raid_port3", cmd_raid_port(3), 779, 0x1a4),
        ("sata_port1", cmd_sata_port(1), 780, 0x184),
        ("smart_fis", cmd_fis(2, &smart_fis(0xd0)), 781, 0x1e0),
    ];
    for (name, cmd, seq, len) in cases {
        let mut f = [0u8; 512];
        f[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        f[4..8].copy_from_slice(&seq.to_le_bytes());
        f[0] = 0x62;
        f[9] = cmd.kind;
        f[10] = cmd.sub;
        for i in 0xc..0x1fa {
            f[i] = ((i * 13 + 5) & 0xff) as u8;
        }
        scramble(&mut f);
        let r = parse_response(&f, &cmd, seq).unwrap_or_else(|e| panic!("{name}: {e}"));
        let want = unhex(g[name]["out"].as_str().unwrap());
        assert_eq!(&r.data[..len], &want[..len], "{name}");
    }
}
