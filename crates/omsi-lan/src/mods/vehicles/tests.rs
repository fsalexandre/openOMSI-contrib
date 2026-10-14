use super::super::channel::{Handshake, SecureReader, SecureWriter, MAGIC};
use super::*;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("openomsi-vehicles-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn entry(path: &str, data: &[u8]) -> Entry {
    Entry { path: path.into(), size: data.len() as u64, sha256: super::super::sha256_of(data) }
}

/// A host that answers MANIFEST with `list`, and BLOB and PIECE from `files` (by
/// SHA-256), on the encrypted mods connection - a host with a store of its own, apart from
/// this test's.
fn host(key: std::sync::Arc<omsi_net::hostkey::HostKey>, list: VehicleManifest, files: HashMap<String, Vec<u8>>) -> SocketAddr {
    host_of(key, list, files, Version::Zstd, None)
}

/// What a host of each version sends.
#[derive(Clone, Copy, PartialEq)]
enum Version {
    /// Whole files only (before the pieces).
    Blobs,
    /// Pieces packed with zlib (before zstd: `ERR what` to `ZSTD`).
    Zlib,
    /// Pieces packed with zstd to a game that asks for it.
    Zstd,
}

/// Pieces the test hosts sent packed with zstd.
static SENT_ZSTD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// `host`, of `version`; one that breaks the connection after `cut` pieces (counted
/// over all its connections) when told.
fn host_of(key: std::sync::Arc<omsi_net::hostkey::HostKey>, list: VehicleManifest, files: HashMap<String, Vec<u8>>, version: Version, cut: Option<usize>) -> SocketAddr {
    let session = key.session();
    let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (list, files, served, key) = (list.clone(), files.clone(), served.clone(), key.clone());
            std::thread::spawn(move || -> std::io::Result<()> {
                let mut out = stream.try_clone()?;
                let mut input = BufReader::new(stream);
                let mut line = String::new();
                input.read_line(&mut line)?;
                let hello: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(hello[0], MAGIC);
                let keys = Handshake::new()?;
                // (with the proof that this host runs the session, as a host greets)
                writeln!(out, "OK {} {}", keys.public_hex(), super::super::channel::prove(&key, hello[2], &keys.public_hex()))?;
                let (send, receive) = keys.finish(session, false, hello[2])?;
                let mut out = SecureWriter::new(out, send);
                let mut input = SecureReader::new(input, receive);
                let mut zstd = false;
                loop {
                    out.flush()?;
                    line.clear();
                    if input.read_line(&mut line)? == 0 {
                        return Ok(());
                    }
                    let req: Vec<&str> = line.split_whitespace().collect();
                    match req.as_slice() {
                        ["MANIFEST", _] => {
                            let body = serde_json::to_vec(&list).unwrap();
                            writeln!(out, "OK {}", body.len())?;
                            out.write_all(&body)?;
                        }
                        ["BLOB", sha] => match files.get(*sha) {
                            Some(d) => {
                                writeln!(out, "OK {}", d.len())?;
                                out.write_all(d)?;
                            }
                            None => writeln!(out, "ERR no such file")?,
                        },
                        ["ZSTD"] if version == Version::Zstd => {
                            zstd = true;
                            writeln!(out, "OK")?;
                        }
                        ["PIECE", sha, i] if version != Version::Blobs => {
                            if cut.is_some_and(|c| served.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= c) {
                                return Ok(());
                            }
                            match (files.get(*sha), i.parse::<u64>()) {
                                (Some(d), Ok(i)) => {
                                    let (start, len) = pieces::span(d.len() as u64, i);
                                    let piece = &d[start as usize..(start + len) as usize];
                                    if zstd {
                                        let (codec, wire) = pieces::pack_zstd(piece, pieces::ZSTD_FAST);
                                        if codec == 's' {
                                            SENT_ZSTD.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        }
                                        pieces::send_packed(&mut out, len, codec, &wire)?;
                                    } else {
                                        pieces::send(&mut out, piece)?;
                                    }
                                }
                                _ => writeln!(out, "ERR no such file")?,
                            }
                        }
                        ["BYE"] => return out.flush(),
                        // (what every host answers to what it does not know)
                        _ => writeln!(out, "ERR what")?,
                    }
                }
            });
        }
    });
    addr
}

#[test]
fn a_bus_kept_after_a_restart_of_files_kept_before_it_is_served_whole() {
    let store = temp("restart-store");
    let kept = Buses::default();
    kept.open_in(&store).unwrap();
    let b = b"[model]\nmodel\\model.cfg\n".to_vec();
    let t = vec![5u8; 4000];
    let files = [(entry("Vehicles/Before/before.bus", &b), b.clone()), (entry("Vehicles/Before/t.dat", &t), t.clone())];
    let m = VehicleManifest { bus: "Vehicles/Before/before.bus".into(), entries: files.iter().map(|f| f.0.clone()).collect() }.validated().unwrap();
    // its files come (or are the server's own), but the bus is not finished...
    for (e, d) in &files {
        kept.put(e, d).unwrap();
    }
    // ...and the server starts again: the files are in the store, no list names them
    kept.open_in(&store).unwrap();
    assert!(kept.missing(&m).is_empty(), "the files are there");
    let (fp, new) = kept.commit(m.clone()).unwrap();
    assert!(new);
    for (e, d) in &files {
        assert_eq!(kept.blob(&e.sha256).as_deref(), Some(d.as_slice()), "{} is served", e.path);
    }
    assert_eq!(kept.manifest(&fp), Some(m));
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn buses_nobody_uses_go_but_never_one_in_use_a_shared_file_or_one_on_its_way() {
    let store = temp("gc-store");
    let kept = Buses::default();
    kept.open_in(&store).unwrap();
    let shared = vec![1u8; 1000];
    let bus = |name: &str, own: &[u8]| -> (VehicleManifest, Vec<(Entry, Vec<u8>)>) {
        let b = format!("[model]
{name}
").into_bytes();
        let files = vec![
            (entry(&format!("Vehicles/{name}/{name}.bus"), &b), b.clone()),
            (entry(&format!("Vehicles/{name}/t.dat"), own), own.to_vec()),
            (entry("Vehicles/Shared/s.dat", &shared), shared.clone()),
        ];
        let m = VehicleManifest { bus: format!("Vehicles/{name}/{name}.bus"), entries: files.iter().map(|f| f.0.clone()).collect() }.validated().unwrap();
        (m, files)
    };
    let keep = |name: &str, own: &[u8], days_ago: u64| -> (String, VehicleManifest) {
        let (m, files) = bus(name, own);
        for (e, d) in &files {
            kept.put(e, d).unwrap();
        }
        let (fp, _) = kept.commit(m.clone()).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(store.join("vehicles").join(format!("{fp}.json"))).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(days_ago * 86_400 + 3600)).unwrap();
        (fp, m)
    };
    let (old, m_old) = keep("Old", &[2u8; 500], 40);
    let (driven, _) = keep("Driven", &[3u8; 500], 40);
    let (recent, m_recent) = keep("Recent", &[4u8; 500], 1);
    let (coming, m_coming) = keep("Coming", &[5u8; 500], 40);
    // one is being driven now; one is being sent again (its files held)
    kept.in_use([driven.as_str()].into_iter());
    let lease = kept.lease(&m_coming);
    let c = kept.collect(0, IN_USE);
    assert_eq!(c.buses, 1, "only the old one nobody uses");
    assert!(kept.manifest(&old).is_none() && kept.manifest(&driven).is_some() && kept.manifest(&recent).is_some() && kept.manifest(&coming).is_some());
    // its own file went, the shared one (still used by the others) stayed
    assert!(kept.blob(&m_old.entries.iter().find(|e| e.path.ends_with("t.dat")).unwrap().sha256).is_none());
    assert_eq!(kept.blob(&super::super::sha256_of(&shared)), Some(shared.clone()));
    drop(lease);
    // short of room: the least lately used that nobody drives go (here: the one
    // that was on its way, then the recent one) - never the one being driven
    let c = kept.collect(u64::MAX, IN_USE);
    assert_eq!(c.buses, 2);
    assert!(kept.manifest(&coming).is_none() && kept.manifest(&recent).is_none() && kept.manifest(&driven).is_some());
    assert!(kept.blob(&m_recent.entries.iter().find(|e| e.path.ends_with("t.dat")).unwrap().sha256).is_none());
    assert_eq!(kept.blob(&super::super::sha256_of(&shared)), Some(shared));
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn a_bus_that_keeps_failing_is_given_up_and_one_on_its_way_is_not() {
    use crate::mods::failure::Failure;
    let s = std::time::Duration::from_secs;
    let (bad, out_of_reach) = (Failure::from("bad list"), Failure::from("cannot reach the host's mods on TCP 127.0.0.1:1: refused"));
    assert!(!may_retry(&bad, s(10), 1), "a minute between tries");
    assert!(may_retry(&bad, s(61), 2));
    assert!(!may_retry(&bad, s(600), GIVE_UP), "given up");
    assert!(may_retry(&Failure::from("the host says: no such bus"), s(6), 50), "a bus on its way is asked for again");
    assert!(may_retry(&Failure::NoHost, s(6), 50), "asked once the way to the host is known");
    assert!(may_retry(&out_of_reach, s(16), 50), "a host out of reach is asked again");
    assert!(!may_retry(&out_of_reach, s(6), 50), "not at once");
    // a host with every connection taken, or a connection that broke off on the way
    // (the system's words in the player's language, the file it was for in front):
    // asked again, never given up
    assert!(may_retry(&Failure::from("the host says: busy"), s(16), 50));
    let lost = Failure::ConnectionLost("Foi forçado o cancelamento de uma conexão existente pelo host remoto. (os error 10054)".into()).about("Vehicles/X/t.dds");
    assert!(may_retry(&lost, s(16), 50));
}

/// A box made while its bus was being fetched is made again once the bus is here
/// (`lan::sync_remotes`): only a bus here, mounted or our own, is ready.
#[test]
fn a_bus_is_ready_once_mounted_or_our_own() {
    let (v, fp) = (Viewer::default(), "v1-ready-test-0000");
    assert!(!v.ready(fp), "never heard of");
    v.set(fp, Wanted::Pending(1, 2));
    assert!(!v.ready(fp), "on its way");
    v.set(fp, Wanted::Mounted(PathBuf::from("/data/x/b.bus")));
    assert!(v.ready(fp));
    v.set(fp, Wanted::Failed("no".into()));
    assert!(!v.ready(fp));
    v.set(fp, Wanted::Local(PathBuf::from("/omsi/b.bus")));
    assert!(v.ready(fp));
}

#[test]
fn what_a_fetched_bus_read_from_outside_its_own_files_is_found() {
    let at = PathBuf::from("/data/lan-vehicles/v1-x");
    let root = PathBuf::from("/omsi");
    let reads = vec![at.join("Vehicles/B/b.bus"), root.join("program/varlist_roadvehicle.txt"), root.join("Texture/global.bmp"), root.join("Texture/global.bmp"), PathBuf::from("/mods/Fonts/x.oft")];
    assert_eq!(escapes(&at, &root, &reads), vec![PathBuf::from("/mods/Fonts/x.oft"), root.join("Texture/global.bmp")]);
}

/// Two players, the same paint file name (`Texture/repaints.cti`) with other skins in it:
/// this player fetches the other's bus with the other's `.cti` and sees its skin - and its
/// own `.cti` is not touched, nor hidden: its game still reads its own skins from it.
#[test]
fn another_players_paints_come_with_their_bus_and_ours_stay_ours() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("paints-store");
    open_for_test(&store);
    // the other player's bus: its paint file and the skin it names
    let mut skin = vec![0u8; 128];
    skin[..4].copy_from_slice(b"DDS ");
    skin[4..8].copy_from_slice(&124u32.to_le_bytes());
    skin[12..16].copy_from_slice(&512u32.to_le_bytes());
    skin[16..20].copy_from_slice(&512u32.to_le_bytes());
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let theirs = b"[item]\nSkin A\nfarbschema\nskin_a.dds\n".to_vec();
    let m = VehicleManifest {
        bus: "Vehicles/PaintBus/pb.bus".into(),
        entries: vec![entry("Vehicles/PaintBus/pb.bus", &bus), entry("Vehicles/PaintBus/Texture/repaints.cti", &theirs), entry("Vehicles/PaintBus/Texture/skin_a.dds", &skin)],
    }
    .validated()
    .unwrap();
    let fp = m.fingerprint();
    let files: HashMap<String, Vec<u8>> = [&bus, &theirs, &skin].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
    // this player has a paint file of the same name, with skins of its own
    let mine_root = temp("paints-mine");
    let mine_dir = mine_root.join("Vehicles").join("PaintBus").join("Texture");
    std::fs::create_dir_all(&mine_dir).unwrap();
    let mine = b"[item]\nSkin B\nfarbschema\nskin_b.dds\n".to_vec();
    std::fs::write(mine_dir.join("repaints.cti"), &mine).unwrap();
    omsi_cfg::add_content_root(mine_root.clone());
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), m.clone(), files), session);
    Viewer::default().fetch(&fp).unwrap();
    // the other's bus, as itself: its paint file, its skin
    let at = bus_file(&fp).unwrap();
    let their_dir = omsi_cfg::resolve_path(at.parent().unwrap(), "Texture");
    let names = |d: &Path| omsi_sim::vehicle::load_paint_schemes(d).into_iter().map(|s| s.name).collect::<Vec<_>>();
    assert_eq!(names(&their_dir), vec!["Skin A".to_string()]);
    assert_eq!(omsi_cfg::vfs::read(&their_dir.join("skin_a.dds")).unwrap(), skin);
    // ours: the same bytes on the disk, and still what our game reads
    assert_eq!(std::fs::read(mine_dir.join("repaints.cti")).unwrap(), mine);
    assert_eq!(names(&mine_dir), vec!["Skin B".to_string()]);
    omsi_cfg::remove_content_root(&mine_root);
    let _ = std::fs::remove_dir_all(&mine_root);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn a_fetch_that_broke_off_goes_on_where_it_stopped_whatever_the_size_of_the_file() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("resume-store");
    open_for_test(&store);
    // a big texture (several pieces, as noise: it does not get smaller) and small files
    let mut x = 0x9e37_79b9u32;
    let big: Vec<u8> = (0..(3 * pieces::PIECE + 12_345))
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let small = b"[mesh]\nbody.o3d\n".to_vec();
    let m = VehicleManifest {
        bus: "Vehicles/ResumeBus/r.bus".into(),
        entries: vec![entry("Vehicles/ResumeBus/r.bus", &bus), entry("Vehicles/ResumeBus/model/model.cfg", &small), entry("Vehicles/ResumeBus/texture/big.dat", &big)],
    }
    .validated()
    .unwrap();
    let fp = m.fingerprint();
    let files: HashMap<String, Vec<u8>> = [&bus, &small, &big].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
    let all: usize = m.entries.iter().map(|e| pieces::count(e.size) as usize).sum();
    assert_eq!(all, 6);
    // the line breaks after 3 pieces
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host_of(key.clone(), m.clone(), files.clone(), Version::Zlib, Some(3)), session);
    assert!(Viewer::default().fetch(&fp).is_err());
    let kept: usize = m.entries.iter().map(|e| pieces::count(e.size) as usize - missing_pieces(e).len()).sum();
    assert_eq!(kept, 3, "the pieces that came are kept");
    // again: a host that serves only as many pieces as are left is enough
    super::super::upload::remember_host(host_of(key.clone(), m.clone(), files.clone(), Version::Zstd, Some(all - kept)), session);
    Viewer::default().fetch(&fp).unwrap();
    let at = bus_file(&fp).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(at.parent().unwrap(), "texture/big.dat")).unwrap(), big);
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(at.parent().unwrap(), "model/model.cfg")).unwrap(), small);
    // the pieces went once their files were kept whole
    assert!(!std::fs::read_dir(&store).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "piece")), "pieces left in the store");
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn a_piece_that_is_not_what_it_should_be_never_makes_a_bus() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("tamper-store");
    open_for_test(&store);
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let tex: Vec<u8> = (0..(2 * pieces::PIECE + 99)).map(|i| (i % 251) as u8).collect();
    let m = VehicleManifest { bus: "Vehicles/TamperBus/t.bus".into(), entries: vec![entry("Vehicles/TamperBus/t.bus", &bus), entry("Vehicles/TamperBus/texture/t.dat", &tex)] }.validated().unwrap();
    let fp = m.fingerprint();
    // a host that serves the texture with one byte changed, under its right SHA-256
    let mut bad = tex.clone();
    bad[pieces::PIECE as usize + 7] ^= 0x40;
    let tampered: HashMap<String, Vec<u8>> = [(super::super::sha256_of(&bus), bus.clone()), (super::super::sha256_of(&tex), bad)].into_iter().collect();
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), m.clone(), tampered), session);
    let why = Viewer::default().fetch(&fp).unwrap_err();
    assert!(why.to_string().contains("not the file the list names"), "{why}");
    assert!(bus_file(&fp).is_none(), "no bus made of a file that is not its own");
    assert_eq!(missing_pieces(&m.entries[1]).len(), 3, "the pieces of the file that was not it went");
    // from an honest host it comes as it is
    let good: HashMap<String, Vec<u8>> = [&bus, &tex].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
    let before = SENT_ZSTD.load(std::sync::atomic::Ordering::SeqCst);
    super::super::upload::remember_host(host(key.clone(), m.clone(), good), session);
    Viewer::default().fetch(&fp).unwrap();
    assert!(SENT_ZSTD.load(std::sync::atomic::Ordering::SeqCst) > before, "the pieces came packed with zstd");
    let at = bus_file(&fp).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(at.parent().unwrap(), "texture/t.dat")).unwrap(), tex);
    let _ = std::fs::remove_dir_all(&store);
}

/// A file this game refuses for what it is (a font that is no text) is left out: the bus
/// is fetched, mounted and drawn without it - never with another file in its place.
#[test]
fn a_bus_with_a_file_refused_for_what_it_is_is_drawn_without_it() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("held-store");
    open_for_test(&store);
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let cfg = b"[mesh]\nbody.o3d\n".to_vec();
    let font = b"[newfont]\0\0 binary, no text".to_vec();
    let m = VehicleManifest {
        bus: "Vehicles/HeldBus/h.bus".into(),
        entries: vec![entry("Vehicles/HeldBus/h.bus", &bus), entry("Vehicles/HeldBus/model/model.cfg", &cfg), entry("Fonts/held.oft", &font)],
    }
    .validated()
    .unwrap();
    let fp = m.fingerprint();
    let files: HashMap<String, Vec<u8>> = [&bus, &cfg, &font].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), m.clone(), files), session);
    Viewer::default().fetch(&fp).unwrap();
    let at = bus_file(&fp).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(at.parent().unwrap(), "model/model.cfg")).unwrap(), cfg);
    let held = withheld_of(&fp);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].path, "Fonts/held.oft");
    assert!(held[0].why.contains("not a text file"), "{held:?}");
    assert!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(&mount(&fp).unwrap(), "Fonts/held.oft")).is_err(), "the refused file is nowhere in its mount");
    let _ = std::fs::remove_dir_all(&store);
}

fn queued(sha: &str) -> bool {
    buses().recompressing.lock().unwrap().queued.contains(sha)
}

/// A host packs a file at its smallest once a game that reads zstd asked for it (in the
/// background: the first get it at the speed of the moment); the bytes every piece
/// unpacks to stay the file's own, and the packed pieces go with their file.
#[test]
fn a_kept_file_goes_packed_at_its_smallest_once_it_has_been_asked_for() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("packed-store");
    open_for_test(&store);
    keep_for_days(30);
    // a texture of several pieces that packs, and noise that does not
    let tex: Vec<u8> = (0..(2 * pieces::PIECE + 999)).map(|i| ((i / 7) % 251) as u8 ^ (i % 13) as u8).collect();
    let mut x = 0x1357_9bdfu32;
    let noise: Vec<u8> = (0..300_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let files = [(entry("Vehicles/PackBus/p.bus", &bus), bus.clone()), (entry("Vehicles/PackBus/t.dat", &tex), tex.clone()), (entry("Vehicles/PackBus/n.dat", &noise), noise.clone())];
    for (e, d) in &files {
        put(e, d).unwrap();
    }
    let m = VehicleManifest { bus: "Vehicles/PackBus/p.bus".into(), entries: files.iter().map(|f| f.0.clone()).collect() }.validated().unwrap();
    let (fp, _) = commit(m).unwrap();
    let (sha_t, sha_n) = (super::super::sha256_of(&tex), super::super::sha256_of(&noise));
    // a piece as a game gets it: (codec, bytes on the wire, bytes)
    let ask = |sha: &str, i: u64, zstd: bool| -> (String, u64, Vec<u8>) {
        let mut buf = Vec::new();
        super::super::upload::send_piece(&mut buf, &super::super::upload::Sha256::parse(sha).unwrap(), i, zstd).unwrap();
        let end = buf.iter().position(|b| *b == b'\n').unwrap();
        let head = String::from_utf8(buf[..end].to_vec()).unwrap();
        let (raw, codec, wire) = pieces::head(head.strip_prefix("OK ").unwrap()).unwrap();
        (codec.to_string(), wire, pieces::read_piece(&mut &buf[end + 1..], codec, raw, wire).unwrap())
    };
    let piece = |i: u64| {
        let (start, len) = pieces::span(tex.len() as u64, i);
        tex[start as usize..(start + len) as usize].to_vec()
    };
    // a game before zstd: zlib, as ever, and nothing to pack
    assert_eq!(ask(&sha_t, 0, false).0, "z");
    assert!(!queued(&sha_t));
    // one that reads zstd, before the file is packed: at the speed of the moment, and the
    // file is queued
    let fast: Vec<(String, u64, Vec<u8>)> = (0..3).map(|i| ask(&sha_t, i, true)).collect();
    for (i, (codec, _, data)) in fast.iter().enumerate() {
        assert_eq!(codec, "s");
        assert_eq!(*data, piece(i as u64));
    }
    assert!(queued(&sha_t));
    assert_eq!(ask(&sha_n, 0, true).0, "-");
    // a file not kept here queues nothing
    let mut buf = Vec::new();
    super::super::upload::send_piece(&mut buf, &super::super::upload::Sha256::parse(&"0".repeat(64)).unwrap(), 0, true).unwrap();
    assert!(buf.starts_with(b"ERR"));
    assert!(!queued(&"0".repeat(64)));
    buses().recompress_queued();
    assert!(!queued(&sha_t) && !queued(&sha_n));
    // packed at its smallest: as small or smaller, the same bytes
    for i in 0..3 {
        let (codec, wire, data) = ask(&sha_t, i, true);
        assert_eq!(codec, "s");
        assert!(wire <= fast[i as usize].1, "piece {i}: {wire} packed, {} at once", fast[i as usize].1);
        assert_eq!(data, piece(i));
    }
    // the whole file: its own SHA-256 (the bus's fingerprint is made of these)
    let whole: Vec<u8> = (0..3).flat_map(|i| ask(&sha_t, i, true).2).collect();
    assert_eq!(super::super::sha256_of(&whole), sha_t);
    // noise is kept as a mark and goes as it is
    let (codec, wire, data) = ask(&sha_n, 0, true);
    assert_eq!((codec.as_str(), wire), ("-", noise.len() as u64));
    assert_eq!(data, noise);
    // the bus let go: its packed pieces go with its files
    let packed_here = || std::fs::read_dir(&store).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "zpiece")).count();
    assert_eq!(packed_here(), 4);
    let f = std::fs::OpenOptions::new().write(true).open(store.join("vehicles").join(format!("{fp}.json"))).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400)).unwrap();
    assert_eq!(buses().collect(0, IN_USE).buses, 1);
    assert_eq!(packed_here(), 0);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn a_host_of_an_older_version_sends_whole_files() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("older-store");
    open_for_test(&store);
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let tex = vec![9u8; 1_500_000];
    let m = VehicleManifest { bus: "Vehicles/OldHostBus/o.bus".into(), entries: vec![entry("Vehicles/OldHostBus/o.bus", &bus), entry("Vehicles/OldHostBus/texture/t.dat", &tex)] }.validated().unwrap();
    let fp = m.fingerprint();
    let files: HashMap<String, Vec<u8>> = [&bus, &tex].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host_of(key.clone(), m.clone(), files, Version::Blobs, None), session);
    Viewer::default().fetch(&fp).unwrap();
    let at = bus_file(&fp).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(at.parent().unwrap(), "texture/t.dat")).unwrap(), tex);
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn another_players_bus_is_fetched_by_its_content_and_checked() {
    let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let store = temp("client-store");
    open_for_test(&store);
    let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
    let tex = vec![5u8; 200_000];
    let m = VehicleManifest { bus: "Vehicles/FetchBus/f.bus".into(), entries: vec![entry("Vehicles/FetchBus/f.bus", &bus), entry("Vehicles/FetchBus/texture/t.dat", &tex)] }.validated().unwrap();
    let fp = m.fingerprint();
    let files: HashMap<String, Vec<u8>> = [(super::super::sha256_of(&bus), bus.clone()), (super::super::sha256_of(&tex), tex.clone())].into_iter().collect();
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), m.clone(), files.clone()), session);
    // fetched, kept under its fingerprint, and mounted on its own
    Viewer::default().fetch(&fp).unwrap();
    let bus_file = bus_file(&fp).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(bus_file.parent().unwrap(), "texture/t.dat")).unwrap(), tex);
    // a host that passes off another bus under that fingerprint is not believed
    let other = VehicleManifest { bus: m.bus.clone(), entries: vec![entry("Vehicles/FetchBus/f.bus", &bus), entry("Vehicles/FetchBus/texture/t.dat", b"x")] }.validated().unwrap();
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), other.clone(), files.clone()), session);
    assert!(Viewer::default().fetch(&format!("v1-{}", "1".repeat(64))).unwrap_err().to_string().contains("another bus"));
    // nor one whose file is not the one its list names
    let mut bad_files = files.clone();
    let tex2 = vec![6u8; 200_000];
    let m2 = VehicleManifest { bus: m.bus.clone(), entries: vec![entry("Vehicles/FetchBus/f.bus", &bus), entry("Vehicles/FetchBus/texture/t.dat", &tex2)] }.validated().unwrap();
    bad_files.insert(super::super::sha256_of(&tex2), vec![9u8; 200_000]);
    let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
    let session = key.session();
    super::super::upload::remember_host(host(key.clone(), m2.clone(), bad_files), session);
    assert!(Viewer::default().fetch(&m2.fingerprint()).unwrap_err().to_string().contains("not the file the list names"));
    assert!(manifest(&m2.fingerprint()).is_none(), "a bus that did not come whole is not kept");
    let _ = std::fs::remove_dir_all(&store);
}

/// A stock bus of an OMSI 2 installation (`OMSI_ROOT`): what its driver's game works out
/// for it is what another game with the same files works out - so that game draws its
/// own copy, without fetching anything; another fingerprint is not that copy.
#[test]
fn the_same_copy_here_is_known_without_fetching() {
    let Some(root) = std::env::var_os("OMSI_ROOT").map(PathBuf::from) else { return };
    omsi_cfg::add_content_root(root.clone());
    let bus = "Vehicles/MAN_SD200/MAN_SD77.bus";
    let path = omsi_cfg::resolve_path(&root, bus);
    let Ok(ty) = omsi_sim::VehicleType::load(&root, &path) else { return };
    // the driver's side: in its second paint
    let files = super::super::upload::listed(&root, super::super::bus_files::of_type(&root, &ty, Some(1), None));
    let fp = VehicleManifest { bus: bus.into(), entries: super::super::upload::hashed(&files) }.validated().unwrap().fingerprint();
    // the viewer's side, from the bus path and the paint's name the driver announces
    let paint = ty.paint_schemes[1].name.clone();
    let got = local_copy(&root, &fp, bus, &paint).unwrap();
    assert_eq!(omsi_cfg::vfs::read(&got).unwrap(), omsi_cfg::vfs::read(&path).unwrap());
    assert!(local_copy(&root, &fp, bus, &ty.paint_schemes[2].name).is_err(), "another paint is another bus");
    assert!(local_copy(&root, &format!("v1-{}", "2".repeat(64)), bus, &paint).is_err());
}

#[test]
fn the_same_bytes_refused_under_another_name_do_not_cost_a_bus_its_own_file() {
    let store = temp("poison");
    let kept = Buses::default();
    kept.open_in(&store).unwrap();
    let b = b"[model]\nreal.cfg\n".to_vec();
    // a player sends a bus file's bytes as a sound: refused (no WAVE), held back as that
    let as_sound = entry("Vehicles/X/a.wav", &b);
    kept.withhold(&as_sound, "not a WAVE sound");
    assert!(kept.is_withheld(&as_sound));
    // the bus that has those bytes as its .bus still takes them, and is kept with them
    let bus_file = entry("Vehicles/Real/real.bus", &b);
    assert!(!kept.is_withheld(&bus_file));
    let m = VehicleManifest { bus: "Vehicles/Real/real.bus".into(), entries: vec![bus_file.clone()] }.validated().unwrap();
    assert_eq!(kept.missing(&m), vec![0]);
    kept.put(&bus_file, &b).unwrap();
    assert!(kept.commit(m).is_ok());
    let _ = std::fs::remove_dir_all(&store);
}

#[test]
fn only_a_bus_on_its_way_is_waited_for() {
    // (a store of its own, never opened: nothing is kept, the waiting is all there is)
    let kept = Buses::default();
    let (announced, made_up) = ("v1-announced", "v1-made-up");
    kept.announce([announced].into_iter());
    let t0 = std::time::Instant::now();
    assert!(kept.wait_for(made_up, std::time::Duration::from_secs(30)).is_none());
    assert!(t0.elapsed() < ANNOUNCE_GRACE + std::time::Duration::from_millis(1500), "a made-up bus is not waited for");
    let t0 = std::time::Instant::now();
    assert!(kept.wait_for(announced, std::time::Duration::from_secs(4)).is_none());
    assert!(t0.elapsed() >= std::time::Duration::from_millis(3900), "an announced one is");
    // one being sent is waited for too
    let b = b"[model]\nm.cfg\n".to_vec();
    let m = VehicleManifest { bus: "Vehicles/Sent/sent.bus".into(), entries: vec![entry("Vehicles/Sent/sent.bus", &b)] }.validated().unwrap();
    let _lease = kept.lease(&m);
    assert!(kept.on_its_way(&m.fingerprint()));
}

#[test]
fn a_player_whose_bus_changes_all_the_time_is_drawn_as_a_box() {
    // (a viewer of its own: nothing shared with the other tests, nothing to put back)
    let v = Viewer::default();
    assert!(v.driven("anything"), "before any player is known, every bus is driven");
    for k in 0..=CHANGES {
        assert!(v.driven_by(7_000_001, &format!("fp{k}")), "change {k}");
    }
    assert!(!v.driven_by(7_000_001, "fp-more"), "one change too many");
    // another player who keeps their bus is not
    assert!(v.driven_by(7_000_002, "steady") && v.driven_by(7_000_002, "steady"));
    // only what is driven now is fetched
    assert!(v.driven("fp-more") && v.driven("steady") && !v.driven("fp0"));
}

#[test]
fn what_players_send_makes_room_only_of_buses_unused_for_a_day() {
    let store = temp("sent-room");
    let kept = Buses::default();
    kept.open_in(&store).unwrap();
    let keep = |name: &str, hours_ago: u64| -> String {
        let b = format!("[model]\n{name}.cfg\n").into_bytes();
        let e = entry(&format!("Vehicles/{name}/{name}.bus"), &b);
        kept.put(&e, &b).unwrap();
        let m = VehicleManifest { bus: e.path.clone(), entries: vec![e] }.validated().unwrap();
        let (fp, _) = kept.commit(m).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(store.join("vehicles").join(format!("{fp}.json"))).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(hours_ago * 3600)).unwrap();
        fp
    };
    let (played, old) = (keep("Played", 2), keep("Old", 50));
    assert_eq!(kept.collect(u64::MAX, SENT_MAKES_ROOM_OF).buses, 1);
    assert!(kept.manifest(&played).is_some() && kept.manifest(&old).is_none(), "the bus played two hours ago stays");
    // what this game fetches for its own players makes room of more
    assert_eq!(kept.collect(u64::MAX, IN_USE).buses, 1);
    let _ = std::fs::remove_dir_all(&store);
}
