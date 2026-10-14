//! Players' buses for everybody: a player whose bus the server does not have sends the
//! server the files that bus uses, the server keeps them sealed and passes them on with its
//! own mods (see the parent module), and nobody in the session sees a stand-in for it.
//!
//! What goes: the bus's manifest (`identity`) - every file its description names, found as
//! the game finds them (`bus_files`): the variant and the paint driven, the folders its texts
//! point into (`..\..\Anzeigen`, shared sounds), the fonts of its displays; not the whole
//! pack. Its fingerprint goes along in INFO.
//!
//! Nobody has to take the fingerprint a player announces on trust, so the server passes it
//! on as it is: a game that draws the bus checks the list it gets against that fingerprint
//! and every file against the list (`vehicles::want`). A player who announced another bus's
//! fingerprint is drawn as that other bus - its own doing; no file goes unchecked.
//!
//! The server takes what it lacks, without asking anybody (`accept_uploads` of
//! `server.cfg`): it checks every file as a joining player checks the host's (`refuse_path`,
//! size, hash, no program) and keeps the bus by its content (`vehicles`): every version
//! of it, each under the fingerprint the server works out itself, still there after a
//! restart. What it has already - in any bus - is never sent again.
//!
//! The other players get the bus by its fingerprint when they draw its player
//! (`vehicles::want`): their own copy when it is the same, else fetched from the server.
//!
//! The player is told: a joining game sends its bus under the loading screen (`begin`,
//! `loading`: what is being done, and how far), so that the others see the real bus from
//! its first metre; what goes or comes later is a card over the HUD (`notices`).
//!
//! On the mods connection, after its greeting (encrypted as the rest):
//!
//! ```text
//! OFFER <len>          + JSON VehicleManifest            → OK <len> + JSON [wanted indices] | ERR
//! PUT <index> <len>    + the bytes                      → OK | ERR
//! DONE                                                  → OK <files that came> | ERR
//! MANIFEST <fp>                                         → OK <len> + JSON VehicleManifest | ERR
//! BLOB <sha256>                                         → OK <len> + the bytes | ERR
//! ```

use super::identity::VehicleManifest;
use super::Entry;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// -------------------------------------------------------------------------------------
// the server

static ACCEPT: AtomicBool = AtomicBool::new(false);

/// A dedicated server takes the buses its players send (`accept_uploads`).
pub fn accept(on: bool) {
    ACCEPT.store(on, Ordering::Relaxed);
}

pub fn accepting() -> bool {
    ACCEPT.load(Ordering::Relaxed)
}

/// The server starts: the buses players sent before are opened (`vehicles`).
pub(super) fn open() {
    if accepting() && !super::vehicles::open() {
        log::warn!("uploads: no store; players' buses are not taken");
        accept(false);
    }
    SERVING.store(accepting(), Ordering::Relaxed);
}

/// This game is the server its players send their buses to (not one that joined).
static SERVING: AtomicBool = AtomicBool::new(false);

pub(super) fn serving() -> bool {
    SERVING.load(Ordering::Relaxed)
}

/// What the uploads of every connection may hold in memory at once (a list, a file sent
/// whole): a hundred connections each announcing a list of 256 MB, or a file of a GB, took
/// the server's memory. One that would pass it is told `ERR busy` and comes back; one larger
/// than all of it (no size is refused) comes when nothing else is held.
const UPLOAD_MEMORY: u64 = 512 << 20;
static HELD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes of `UPLOAD_MEMORY` held by one request, given back when it is done.
struct Held(u64);

impl Held {
    fn take(n: u64) -> Option<Held> {
        HELD.fetch_update(Ordering::AcqRel, Ordering::Acquire, |h| (h + n <= UPLOAD_MEMORY || h == 0).then_some(h + n)).ok().map(|_| Held(n))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.fetch_sub(self.0, Ordering::AcqRel);
    }
}

/// `len` bytes of the request read and let go (an answer that refuses it keeps the stream in step).
fn skip(input: &mut impl Read, len: u64) -> std::io::Result<()> {
    let n = std::io::copy(&mut input.take(len), &mut std::io::sink())?;
    if n != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// `len` bytes of the request, read as they come (nothing held for bytes that never come).
fn read_body(input: &mut impl Read, len: u64) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(len.min(1 << 20) as usize);
    input.take(len).read_to_end(&mut body)?;
    if body.len() as u64 != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(body)
}

/// Files of a bus that came whole in pieces (`PUTPIECE`), by the bus: its pieces come over
/// several connections, its DONE on another.
static CAME: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// One connection's upload: the bus it offered, which of its files the server wants, and
/// which have come (each kept in the store as it comes: a file is a file, whichever bus).
#[derive(Default)]
pub(super) struct Incoming {
    offer: Option<VehicleManifest>,
    /// Its files, held against a collection while they come (`vehicles::lease`).
    lease: Option<super::vehicles::Lease>,
    wanted: HashSet<usize>,
    got: usize,
}

impl Incoming {
    /// `OFFER <len>`: a player's bus (its manifest). Answers which of its files the server
    /// lacks - by their content: a file of the same path in another version is another file.
    /// An empty list only asks whether the server takes uploads at all.
    pub(super) fn offer(&mut self, input: &mut impl Read, out: &mut impl Write, len: &str) -> std::io::Result<()> {
        let wanted = match self.take_offer(input, len)? {
            Ok(w) => w,
            Err(why) => return writeln!(out, "ERR {why}"),
        };
        let body = serde_json::to_vec(&wanted).unwrap_or_default();
        writeln!(out, "OK {}", body.len())?;
        out.write_all(&body)
    }

    /// `OFFER2`: as `OFFER`, answered with the pieces of each wanted file the server lacks
    /// (`pieces`: what came of it before - from this player or another - is not sent again).
    pub(super) fn offer_pieces(&mut self, input: &mut impl Read, out: &mut impl Write, len: &str) -> std::io::Result<()> {
        let wanted = match self.take_offer(input, len)? {
            Ok(w) => w,
            Err(why) => return writeln!(out, "ERR {why}"),
        };
        let plan: Vec<(usize, Vec<u64>)> = match self.offer.as_ref() {
            Some(m) => wanted.iter().map(|k| (*k, super::vehicles::missing_pieces(&m.entries[*k]))).collect(),
            None => Vec::new(),
        };
        let body = serde_json::to_vec(&plan).unwrap_or_default();
        writeln!(out, "OK {}", body.len())?;
        out.write_all(&body)
    }

    /// The offer's list read and taken: the files the server wants (or why it takes none).
    fn take_offer(&mut self, input: &mut impl Read, len: &str) -> std::io::Result<Result<Vec<usize>, String>> {
        let Some(len) = len.parse::<u64>().ok().filter(|l| *l <= 256 << 20) else {
            return Err(std::io::Error::other("bad offer"));
        };
        // (nothing kept of a list a server that takes none would refuse anyway)
        if !accepting() || !super::vehicles::is_open() {
            skip(input, len)?;
            return Ok(Err("this server takes no uploads".into()));
        }
        let Some(_held) = Held::take(len) else {
            skip(input, len)?;
            return Ok(Err("busy".into()));
        };
        let body = read_body(input, len)?;
        let Ok(m) = serde_json::from_slice::<VehicleManifest>(&body) else {
            return Ok(Err("bad list".into()));
        };
        self.wanted.clear();
        self.got = 0;
        self.offer = None;
        if m.entries.is_empty() {
            return Ok(Ok(Vec::new()));
        }
        if m.entries.len() > super::MAX_FILES {
            return Ok(Err("too many files".into()));
        }
        if let Some(list) = crate::server_cfg::SERVER_VEHICLES.get().filter(|l| !l.is_empty()) {
            if !crate::server_cfg::allows(list, &m.bus) {
                return Ok(Err("this server does not offer that bus".into()));
            }
        }
        let m = match m.validated() {
            Ok(m) => m,
            Err(e) => return Ok(Err(e)),
        };
        if let Some(bad) = m.entries.iter().find(|e| super::refuse_path(&e.path).is_some() || e.size > super::MAX_FILE) {
            return Ok(Err(format!("{}: {}", bad.path, super::refuse_path(&bad.path).unwrap_or_else(|| "too large".into()))));
        }
        self.lease = Some(super::vehicles::lease(&m));
        // (what the server has itself in the same version - a stock bus, a pack installed
        // here - is taken from its own files, not sent)
        let adopted = super::vehicles::adopt_local(&m);
        let wanted = super::vehicles::missing(&m);
        if adopted > 0 {
            log::info!("uploads: {adopted} files of {} are the server's own, not sent", m.bus);
        }
        let bytes: u64 = wanted.iter().map(|k| m.entries[*k].size).sum();
        log::info!("uploads: a player offers {} ({} files): {} wanted ({:.1} MB)", m.bus, m.entries.len(), wanted.len(), bytes as f64 / 1e6);
        self.wanted = wanted.iter().copied().collect();
        self.offer = Some(m);
        Ok(Ok(wanted))
    }

    /// `PUTPIECE <index> <i> <raw> <codec> <wire>`: one piece of a wanted file, kept as it
    /// comes; the file is checked and kept once all its pieces are here.
    pub(super) fn put_piece(&mut self, input: &mut impl Read, out: &mut impl Write, idx: &str, i: &str, head: &str) -> std::io::Result<()> {
        let (Ok(idx), Ok(i), Some((raw, codec, wire))) = (idx.parse::<usize>(), i.parse::<u64>(), super::pieces::head(head)) else {
            return Err(std::io::Error::other("bad piece"));
        };
        let entry = self.offer.as_ref().and_then(|m| m.entries.get(idx)).cloned();
        // (a piece of no file of the offer, or of another size: the stream cannot be trusted)
        let Some(e) = entry.filter(|e| i < super::pieces::count(e.size) && super::pieces::span(e.size, i).1 == raw) else {
            return Err(std::io::Error::other("a piece that was not asked for"));
        };
        // (more on the wire than the piece has: no piece of ours, and the stream is out of step)
        if wire > raw.max(64) {
            return Err(std::io::Error::other("a piece bigger than it is"));
        }
        let mut bytes = vec![0u8; wire as usize];
        input.read_exact(&mut bytes)?;
        // a file of the offer that was here whole already when this connection offered it -
        // the same file under another path, finished on another of the player's connections
        // (pieces are kept by their content): nothing to keep. Taken as done; it ended the
        // whole upload before, half of a bus left behind.
        if !self.wanted.contains(&idx) {
            return writeln!(out, "OK");
        }
        let data = match super::pieces::unpack(codec, bytes, raw) {
            Ok(d) => d,
            Err(why) => return writeln!(out, "ERR {}: {why}", e.path),
        };
        match super::vehicles::put_piece_sent(&e, i, &data) {
            Ok(whole) => {
                if whole && self.wanted.remove(&idx) {
                    // (counted for the bus, whichever of its connections the file came on)
                    if let Some(m) = self.offer.as_ref() {
                        *CAME.lock().unwrap_or_else(|p| p.into_inner()).get_or_insert_with(HashMap::new).entry(m.bus.clone()).or_default() += 1;
                    }
                }
                writeln!(out, "OK")
            }
            Err(why) => {
                log::warn!("uploads: refused {why}");
                writeln!(out, "ERR {why}")
            }
        }
    }

    /// `PUT <index> <len>`: one wanted file. Checked, then kept in the store.
    pub(super) fn put(&mut self, input: &mut impl Read, out: &mut impl Write, idx: &str, len: &str) -> std::io::Result<()> {
        let (Ok(idx), Ok(len)) = (idx.parse::<usize>(), len.parse::<u64>()) else {
            return Err(std::io::Error::other("bad put"));
        };
        let entry = self.offer.as_ref().and_then(|m| m.entries.get(idx)).filter(|_| self.wanted.contains(&idx)).cloned();
        // (a file not asked for, or of another size: the stream cannot be trusted any more)
        let Some(e) = entry.filter(|e| e.size == len) else {
            return Err(std::io::Error::other("a file that was not asked for"));
        };
        let Some(_held) = Held::take(len) else {
            skip(input, len)?;
            return writeln!(out, "ERR busy");
        };
        let data = read_body(input, len)?;
        self.wanted.remove(&idx);
        if super::sha256_of(&data) != e.sha256 {
            return writeln!(out, "ERR {}: not the file the list names", e.path);
        }
        // (refused for what it is: the bus is kept without it - `vehicles::withhold`)
        let refused = if super::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { super::quarantine::check(&e.path, &data).err() };
        if let Some(why) = refused {
            log::warn!("uploads: refused {why}");
            super::vehicles::withhold(&e, &why);
            return writeln!(out, "ERR {why}");
        }
        match super::vehicles::put_sent(&e, &data) {
            Ok(()) => {
                self.got += 1;
                writeln!(out, "OK")
            }
            Err(x) => writeln!(out, "ERR {}: {x}", e.path),
        }
    }

    /// `DONE`: the bus is kept once every file of it is here, by the fingerprint the server
    /// works out itself. The others ask for it by that fingerprint (`vehicles::want`).
    pub(super) fn done(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let got = std::mem::take(&mut self.got);
        self.wanted.clear();
        let Some(m) = self.offer.take() else {
            return writeln!(out, "ERR nothing was offered");
        };
        let bus = m.bus.clone();
        let got = got + CAME.lock().unwrap_or_else(|p| p.into_inner()).as_mut().and_then(|c| c.remove(&bus)).unwrap_or(0);
        let kept = super::vehicles::commit(m);
        self.lease = None;
        let (fp, new) = match kept {
            Ok(r) => r,
            Err(e) => return writeln!(out, "ERR {e}"),
        };
        if new {
            log::info!("uploads: {bus} is kept as {fp} ({got} files came), for every player who draws it");
        } else {
            log::info!("uploads: {bus} is kept already as {fp}");
        }
        writeln!(out, "OK {got}")
    }
}

/// How long a `MANIFEST` asked for a bus not kept yet waits for it to come.
pub(super) const MANIFEST_WAIT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 90 });

/// `MANIFEST <fp>`: the list of a kept bus - one on its way is waited for (`MANIFEST_WAIT`).
pub(super) fn send_manifest(out: &mut impl Write, fp: &str) -> std::io::Result<()> {
    if !omsi_net::is_fingerprint(fp) {
        return writeln!(out, "ERR no such bus");
    }
    match super::vehicles::wait_for(fp, MANIFEST_WAIT) {
        Some(m) => {
            // (with the files it is kept without, beside the list: a game before them reads
            // the list alone, asks for those too and draws a box, as before)
            let mut list = serde_json::to_value(&m).unwrap_or_default();
            let held = super::vehicles::withheld_of(fp);
            if !held.is_empty() {
                list["withheld"] = serde_json::to_value(&held).unwrap_or_default();
            }
            let body = serde_json::to_vec(&list).unwrap_or_default();
            writeln!(out, "OK {}", body.len())?;
            out.write_all(&body)
        }
        None => writeln!(out, "ERR no such bus"),
    }
}

/// `BLOB <sha256>`: one kept file, by its content.
/// `PIECE <sha> <i>`: one piece of a kept file (`pieces`). To a game that reads zstd
/// (`zstd`): packed at its smallest once it is (`vehicles::recompress_later`), till then at
/// the speed of the moment.
pub(super) fn send_piece(out: &mut impl Write, sha: &str, i: &str, zstd: bool) -> std::io::Result<()> {
    let (true, Ok(i)) = (sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), i.parse::<u64>()) else {
        return writeln!(out, "ERR no such file");
    };
    let mut pack_later = false;
    if zstd {
        match super::vehicles::packed(sha, i) {
            // (a piece that does not get smaller is kept as a mark only: it goes as it is)
            Some((_, '-', _)) => {}
            Some((raw, codec, wire)) => return super::pieces::send_packed(out, raw, codec, &wire),
            None => pack_later = true,
        }
    }
    let Some(data) = super::vehicles::served(sha) else {
        return writeln!(out, "ERR no such file");
    };
    let size = data.len() as u64;
    if i >= super::pieces::count(size) {
        return writeln!(out, "ERR no such piece");
    }
    let (start, len) = super::pieces::span(size, i);
    let piece = &data[start as usize..(start + len) as usize];
    if zstd {
        // (only a file kept here is queued: asking for others queues nothing)
        if pack_later {
            super::vehicles::recompress_later(sha);
        }
        let (codec, wire) = super::pieces::pack_zstd(piece, super::pieces::ZSTD_FAST);
        return super::pieces::send_packed(out, len, codec, &wire);
    }
    super::pieces::send(out, piece)
}

pub(super) fn send_blob(out: &mut impl Write, sha: &str) -> std::io::Result<()> {
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return writeln!(out, "ERR no such file");
    }
    match super::vehicles::blob(sha) {
        Some(data) => {
            writeln!(out, "OK {}", data.len())?;
            out.write_all(&data)
        }
        None => writeln!(out, "ERR no such file"),
    }
}

// -------------------------------------------------------------------------------------
// the player

/// Our own bus as the others are to know it: its files and their fingerprint, worked out
/// once for each bus and paint (`identify`, `begin`). The list stays as it was made, so the
/// fingerprint does not change under the others while the game reads on; what is read later
/// that the list lacks is a miss of `bus_files` to be found, not a new bus.
struct Own {
    key: String,
    manifest: super::identity::VehicleManifest,
    fingerprint: String,
    files: Vec<(String, PathBuf)>,
}

static OWN: Mutex<Option<Own>> = Mutex::new(None);
/// (key, fingerprint) of `OWN`, for the frame: it never waits for one being worked out.
static OWN_FP: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
static IDENTIFYING: AtomicBool = AtomicBool::new(false);

/// The fingerprint of our bus in that paint, once it is known (else empty).
pub(crate) fn own_fingerprint(bus: &str, paint: &str) -> String {
    let fp = OWN_FP.lock().unwrap_or_else(|e| e.into_inner());
    if fp.0 == format!("{bus}|{paint}") {
        fp.1.clone()
    } else {
        String::new()
    }
}

/// Once a frame: work out our bus's fingerprint in the background when the bus or its
/// paint is new.
pub(crate) fn identify(root: &Path, bus: &str, paint: &str, own: impl FnOnce() -> Vec<PathBuf>) {
    if bus.is_empty() || IDENTIFYING.load(Ordering::Relaxed) {
        return;
    }
    let key = format!("{bus}|{paint}");
    if OWN_FP.lock().unwrap_or_else(|e| e.into_inner()).0 == key {
        return;
    }
    IDENTIFYING.store(true, Ordering::Relaxed);
    let (root, bus, paint, files) = (root.to_path_buf(), bus.to_string(), paint.to_string(), own());
    let spawned = std::thread::Builder::new().name("bus-identity".into()).spawn(move || {
        own_manifest(&root, &bus, &paint, files);
        IDENTIFYING.store(false, Ordering::Relaxed);
    });
    if spawned.is_err() {
        IDENTIFYING.store(false, Ordering::Relaxed);
    }
}

/// Our bus's manifest in that paint, worked out if it is not yet: (manifest, fingerprint,
/// files). None when the bus's files make no valid list.
fn own_manifest(root: &Path, bus: &str, paint: &str, own: Vec<PathBuf>) -> Option<(super::identity::VehicleManifest, String, Vec<(String, PathBuf)>)> {
    let key = format!("{bus}|{paint}");
    let mut guard = OWN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(o) = guard.as_ref().filter(|o| o.key == key) {
        return Some((o.manifest.clone(), o.fingerprint.clone(), o.files.clone()));
    }
    let files = chosen(root, bus, own);
    let bus_rel = omsi_net::vehicle_path(bus).unwrap_or_else(|| bus.to_string());
    let made = super::identity::VehicleManifest { bus: bus_rel, entries: hashed(&files) }.validated();
    let manifest = match made {
        Ok(m) => m,
        Err(e) => {
            log::warn!("bus identity: {bus}: {e}");
            return None;
        }
    };
    let fingerprint = manifest.fingerprint();
    log::info!("bus identity: {bus} ({paint}) is {fingerprint}: {} files, {:.1} MB", manifest.entries.len(), manifest.size() as f64 / 1e6);
    *OWN_FP.lock().unwrap_or_else(|e| e.into_inner()) = (key.clone(), fingerprint.clone());
    *guard = Some(Own { key, manifest: manifest.clone(), fingerprint: fingerprint.clone(), files: files.clone() });
    Some((manifest, fingerprint, files))
}

/// What the player is told about the bus files going either way: one line, how far
/// (bytes), and whether it is still going on.
struct Shown {
    seq: u64,
    text: String,
    done: u64,
    total: u64,
    busy: bool,
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);

pub(super) fn show(text: impl Into<String>, done: u64, total: u64, busy: bool) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Shown { seq, text: text.into(), done, total, busy });
}

pub(super) fn mb(b: u64) -> String {
    format!("{:.0} MB", (b as f64 / 1e6).max(if b > 0 { 1.0 } else { 0.0 }))
}

/// The loading screen waits for the sending of our bus at the start (then the others see it
/// at once), but not for ever: a big bus over a slow line goes on in the background.
static AT_START: Mutex<Option<Instant>> = Mutex::new(None);
const START_WAIT: Duration = Duration::from_secs(180);

/// Our world and bus have just been made (a joining player, still under the loading screen):
/// the bus goes to the host now, if it lacks it.
pub fn begin(root: &Path, bus: &str, paint: &str, own: Vec<PathBuf>) {
    let Some((host, session)) = host() else { return };
    if bus.is_empty() || REFUSED.load(Ordering::Relaxed) || SENDING.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|s| s.busy) {
        return;
    }
    *AT_START.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    show("Preparing your bus for the other players...", 0, 0, true);
    let (root, bus, paint) = (root.to_path_buf(), bus.to_string(), paint.to_string());
    let spawned = std::thread::Builder::new().name("lan-upload".into()).spawn(move || {
        // (what is read on worker threads still coming in: the list holds still for a moment)
        let t0 = Instant::now();
        let mut n = omsi_cfg::vfs::reads().len();
        let mut still = Instant::now();
        while still.elapsed() < Duration::from_millis(1500) && t0.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(250));
            let now = omsi_cfg::vfs::reads().len();
            if now != n {
                n = now;
                still = Instant::now();
            }
        }
        let r = match own_manifest(&root, &bus, &paint, own) {
            Some((_, _, files)) => send_files(&bus, files, host, session, false),
            None => Err("its files make no valid list".to_string()),
        };
        finish(&bus, r, false);
        *AT_START.lock().unwrap_or_else(|e| e.into_inner()) = None;
    });
    if spawned.is_err() {
        *AT_START.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// While the start waits for our bus to be sent: the loading screen's line and how far.
pub fn loading() -> Option<(String, f32)> {
    let started = (*AT_START.lock().unwrap_or_else(|e| e.into_inner()))?;
    if started.elapsed() > START_WAIT {
        return None;
    }
    let s = SHOWN.lock().unwrap_or_else(|e| e.into_inner());
    let s = s.as_ref()?;
    let p = if s.total > 0 { s.done as f32 / s.total as f32 } else { 0.0 };
    Some((s.text.clone(), p))
}

/// Once a frame: the line about our bus or the others' for a card over the HUD, when there
/// is a new one - (the text of the card before, to take down; the new text; whether it is
/// still going on: the card stays while it does, its text following the progress). None
/// while the line is the one shown already, or while the loading screen says it.
pub fn notice() -> Option<(String, String, bool)> {
    static LAST: Mutex<(u64, String)> = Mutex::new((0, String::new()));
    let (seq, text, busy) = match SHOWN.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        Some(s) => (s.seq, s.text.clone(), s.busy),
        None => return None,
    };
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    // (under the loading screen the screen says it)
    if last.0 == seq || (busy && loading().is_some()) {
        return None;
    }
    let before = std::mem::replace(&mut last.1, text.clone());
    last.0 = seq;
    Some((before, text, busy))
}

/// What became of sending our bus, for the player (`quiet`: a look again after a while,
/// which says nothing when the server had everything).
fn finish(bus: &str, r: Result<usize, String>, quiet: bool) {
    let name = bus.rsplit(['/', '\\']).next().unwrap_or(bus).trim_end_matches(".bus");
    match r {
        Ok(0) if quiet => {}
        Ok(0) => show(format!("The server has your bus ({name}): everybody sees it"), 0, 0, false),
        Ok(n) => show(format!("Your bus ({name}) is on the server now ({n} files): everybody sees it"), 1, 1, false),
        Err(e) if no_uploads(&e) => {
            log::info!("uploads: {bus}: {e}");
            REFUSED.store(true, Ordering::Relaxed);
            *SHOWN.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        Err(e) => {
            log::warn!("uploads: {bus}: {e}");
            show(format!("Your bus could not be sent to the server ({e}): the others see a box of its size"), 0, 0, false);
        }
    }
}

/// The host's answer says it takes no uploads: switched off, or a host of a version
/// without them (which does not know the request).
fn no_uploads(e: &str) -> bool {
    e.contains("takes no uploads") || e.trim_end().ends_with("says: what")
}

/// The host takes no uploads (a player's game hosting, or `accept_uploads = 0`).
static REFUSED: AtomicBool = AtomicBool::new(false);

/// The host whose mods this game fetched (its mods connection, the session).
static HOST: Mutex<Option<(SocketAddr, u64)>> = Mutex::new(None);

pub(super) fn remember_host(host: SocketAddr, session: u64) {
    *HOST.lock().unwrap_or_else(|e| e.into_inner()) = Some((host, session));
}

/// The host of the session this game joined, when the start's fetch of its mods did not
/// say it (a welcome that came late leaves that fetch out): its mods connection is on the
/// session's port number. One the fetch found (a tunnel's) is kept.
pub(crate) fn session_host(host: SocketAddr, session: u64) {
    let mut h = HOST.lock().unwrap_or_else(|e| e.into_inner());
    if h.is_none_or(|(_, s)| s != session) {
        log::info!("uploads: the host's mods connection is {host}");
        *h = Some((host, session));
    }
}

/// Is the way to the host's mods of `session` known?
pub(crate) fn knows_host(session: u64) -> bool {
    HOST.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|(_, s)| s == session)
}

pub(super) fn host() -> Option<(SocketAddr, u64)> {
    *HOST.lock().unwrap_or_else(|e| e.into_inner())
}

/// After the bus (or its paint) changed in the game: how long it is left before it is
/// offered (a player going through the paints sends only the one kept), and how often after
/// that it is offered again - in case the server lost it; what it has costs only the list.
const SETTLE: Duration = Duration::from_secs(3);
const AGAIN: Duration = Duration::from_secs(300);

struct Sending {
    key: String,
    since: Instant,
    last: Option<Instant>,
    busy: bool,
}

static SENDING: Mutex<Option<Sending>> = Mutex::new(None);

/// The bus (and paint) a hosting game has kept for the others, and whether one is being kept.
static KEPT_OWN: Mutex<(String, bool)> = Mutex::new((String::new(), false));

/// Once a frame (a player's game hosting a session): our own bus into our store, by its
/// fingerprint, once it is loaded - the joining players ask this game for it as they would a
/// server (`vehicles::fetch`). A server has no bus of its own; a joining player sends its
/// bus to the host instead (`tick`).
pub(crate) fn keep_own(root: &Path, bus: &str, paint: &str, own: impl FnOnce() -> Vec<PathBuf>) {
    if bus.is_empty() || !serving() {
        return;
    }
    let key = format!("{bus}|{paint}");
    {
        let mut k = KEPT_OWN.lock().unwrap_or_else(|e| e.into_inner());
        if k.0 == key || k.1 {
            return;
        }
        *k = (key.clone(), true);
    }
    let own = own();
    let (root, bus, paint) = (root.to_path_buf(), bus.to_string(), paint.to_string());
    let spawned = std::thread::Builder::new().name("lan-keep-own".into()).spawn(move || {
        let r = match own_manifest(&root, &bus, &paint, own) {
            Some((_, _, files)) => keep_files(&bus, files),
            None => Err("its files make no valid list".to_string()),
        };
        match r {
            Ok(fp) => log::info!("uploads: our bus {bus} is kept for the others as {fp}"),
            Err(e) => log::warn!("uploads: our bus {bus} could not be kept for the others: {e}"),
        }
        KEPT_OWN.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
    });
    if spawned.is_err() {
        *KEPT_OWN.lock().unwrap_or_else(|e| e.into_inner()) = (String::new(), false);
    }
}

/// Our bus's files (`files`: relative path, where each is read from) into this game's store
/// as a server keeps a bus that was sent to it: each checked as one that came would be (a
/// file refused for what it is is withheld, the bus kept without it), the bus kept by the
/// fingerprint worked out from them. The fingerprint.
pub(super) fn keep_files(bus: &str, files: Vec<(String, PathBuf)>) -> Result<String, String> {
    if !super::vehicles::is_open() && !super::vehicles::open() {
        return Err("no store".into());
    }
    let bus_rel = omsi_net::vehicle_path(bus).unwrap_or_else(|| bus.to_string());
    let manifest = VehicleManifest { bus: bus_rel, entries: hashed(&files) }.validated()?;
    let source: HashMap<String, PathBuf> = files.iter().map(|(rel, p)| (rel.to_lowercase(), p.clone())).collect();
    for k in super::vehicles::missing(&manifest) {
        let e = &manifest.entries[k];
        let path = source.get(&e.path.to_lowercase()).ok_or_else(|| format!("{}: not among our files", e.path))?;
        let data = omsi_cfg::vfs::read(path).map_err(|x| format!("{}: {x}", path.display()))?;
        if data.len() as u64 != e.size || super::sha256_of(&data) != e.sha256 {
            return Err(format!("{} changed while it was kept", e.path));
        }
        let refused = if super::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { super::quarantine::check(&e.path, &data).err() };
        match refused {
            Some(why) => {
                log::warn!("uploads: {why}; our bus is kept without it");
                super::vehicles::withhold(e, &why);
            }
            None => super::vehicles::put(e, &data)?,
        }
    }
    super::vehicles::commit(manifest).map(|(fp, _)| fp)
}

/// Once a frame (a joining player): offer our bus to the host once it is loaded.
pub(crate) fn tick(root: &Path, bus: &str, paint: &str, own: impl FnOnce() -> Vec<PathBuf>) {
    if bus.is_empty() {
        return;
    }
    let Some((host, session)) = host() else { return };
    let key = format!("{bus}|{paint}");
    let mut guard = SENDING.lock().unwrap_or_else(|e| e.into_inner());
    let s = guard.get_or_insert_with(|| Sending { key: String::new(), since: Instant::now(), last: None, busy: false });
    if REFUSED.load(Ordering::Relaxed) || s.busy || AT_START.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
        return;
    }
    if s.key != key {
        s.key = key;
        s.since = Instant::now();
        s.last = None;
        return;
    }
    let due = match s.last {
        None => s.since.elapsed() > SETTLE,
        Some(t) => t.elapsed() > AGAIN,
    };
    if !due {
        return;
    }
    s.busy = true;
    s.last = Some(Instant::now());
    let own = own();
    let (root, bus, paint) = (root.to_path_buf(), bus.to_string(), paint.to_string());
    let spawned = std::thread::Builder::new().name("lan-upload".into()).spawn(move || {
        let r = match own_manifest(&root, &bus, &paint, own) {
            Some((_, _, files)) => send_files(&bus, files, host, session, true),
            None => Err("its files make no valid list".to_string()),
        };
        finish(&bus, r, true);
        if let Some(s) = SENDING.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            s.busy = false;
        }
    });
    if spawned.is_err() {
        s.busy = false;
    }
}

/// Offer `bus` to the host and send it what it lacks: the number of files it took.
/// What goes for `bus` - and what its fingerprint is over: the files its description names
/// (`bus_files`, `own`), each once by the path every machine knows it by. Only those: any
/// machine with the same files works out the same list, so a player who has the same version
/// of the bus knows it is the same without fetching anything (`vehicles::want`). What this
/// game read for the bus besides is no part of it: it is logged, a miss of `bus_files` to
/// look into.
fn chosen(root: &Path, bus: &str, own: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
    let out = listed(root, own);
    let named: HashSet<String> = out.iter().map(|(rel, _)| rel.to_lowercase()).collect();
    let extra: Vec<String> = files_of(root, bus).into_iter().map(|(rel, _)| rel).filter(|rel| !named.contains(&rel.to_lowercase())).collect();
    if !extra.is_empty() {
        log::info!("bus identity: {bus}: {} files were read for it that its description does not name (not in its fingerprint), e.g. {}", extra.len(), extra.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
    }
    out
}

/// `own` (absolute) as (path every machine knows it by, where it is), each once, in order.
pub(crate) fn listed(root: &Path, own: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for p in own {
        if let Some(rel) = super::bus_files::relative(root, &p).filter(|r| super::refuse_path(r).is_none()) {
            if seen.insert(rel.to_lowercase()) {
                out.push((rel, p));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The files of the bus `v` the player drives, in its paint (`bus_files`). (Not its depot
/// file: which one a player picked is no part of the bus, and the displays' texts go along
/// in INFO.)
pub fn own_files(root: &Path, v: &omsi_sim::VehicleInstance) -> Vec<PathBuf> {
    super::bus_files::of_type(root, &v.ty, v.host.paint_scheme.flatten(), None)
}

/// What a server of a version before the pieces answers to them.
const OLDER_HOST: &str = "the host takes whole files only";

/// `OFFER2` on a connection: for each file of `manifest` (as `body`) the host wants, the
/// pieces of it it lacks.
fn offer_pieces(out: &mut impl Write, input: &mut impl std::io::BufRead, body: &[u8]) -> Result<Vec<(usize, Vec<u64>)>, String> {
    writeln!(out, "OFFER2 {}", body.len()).and_then(|_| out.write_all(body)).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    let len = match super::read_reply(input) {
        Ok(l) => l.ok_or("no answer")?,
        Err(e) if e.ends_with("says: what") => return Err(OLDER_HOST.into()),
        Err(e) => return Err(e),
    };
    if len > 64 << 20 {
        return Err("the host's answer is too large".into());
    }
    let mut answer = vec![0u8; len as usize];
    input.read_exact(&mut answer).map_err(|e| e.to_string())?;
    serde_json::from_slice(&answer).map_err(|e| e.to_string())
}

/// Piece `i` of the file `e` of ours, read from `path`.
fn local_piece(path: &Path, e: &super::Entry, i: u64) -> Result<Vec<u8>, String> {
    let (start, len) = super::pieces::span(e.size, i);
    let changed = || format!("{} changed while it was sent", path.display());
    if omsi_cfg::vfs::archive_of(path).is_none() && !omsi_cfg::vfs::is_sealed(path) {
        use std::io::Seek;
        let mut f = std::fs::File::open(path).map_err(|x| format!("{}: {x}", path.display()))?;
        if f.metadata().map(|m| m.len()).ok() != Some(e.size) {
            return Err(changed());
        }
        f.seek(std::io::SeekFrom::Start(start)).map_err(|x| x.to_string())?;
        let mut data = vec![0u8; len as usize];
        f.read_exact(&mut data).map_err(|x| format!("{}: {x}", path.display()))?;
        return Ok(data);
    }
    // (a file in a mounted archive is read whole: archives hold small files)
    let all = omsi_cfg::vfs::read(path).map_err(|x| format!("{}: {x}", path.display()))?;
    if all.len() as u64 != e.size {
        return Err(changed());
    }
    Ok(all[start as usize..(start + len) as usize].to_vec())
}

/// Our bus to the host in pieces (`pieces`), several connections at once: only the pieces
/// it lacks - what came before, in an upload that broke off, is not sent again. Then the
/// host keeps the bus (`DONE`). How many files it took.
fn send_pieces(bus: &str, manifest: &VehicleManifest, source: &HashMap<String, PathBuf>, host: SocketAddr, session: u64) -> Result<usize, String> {
    let body = serde_json::to_vec(manifest).map_err(|e| e.to_string())?;
    let wait = Duration::from_secs(600);
    let (raw, mut out, mut input) = super::open_waiting(host, session, wait)?;
    let plan = offer_pieces(&mut out, &mut input, &body)?;
    let _ = writeln!(out, "BYE").and_then(|_| out.flush());
    let _ = raw.shutdown(std::net::Shutdown::Both);
    let sources: HashMap<usize, PathBuf> = plan
        .iter()
        .map(|(k, _)| manifest.entries.get(*k).and_then(|e| source.get(&e.path.to_lowercase())).map(|p| (*k, p.clone())).ok_or("the server asked for a file that is not ours"))
        .collect::<Result<_, _>>()?;
    let total: u64 = plan.iter().map(|(k, _)| manifest.entries[*k].size).sum();
    let left: u64 = plan.iter().map(|(k, ps)| ps.iter().map(|i| super::pieces::span(manifest.entries[*k].size, *i).1).sum::<u64>()).sum();
    // (a file the bus has twice - the same texture in two folders - goes once: the host keeps
    // pieces by their content, so the other is whole with it)
    let mut sent = HashSet::new();
    let mut twice = 0u64;
    let mut items: Vec<(usize, u64)> = Vec::new();
    for (k, ps) in &plan {
        let e = &manifest.entries[*k];
        for i in ps {
            if sent.insert((e.sha256.clone(), *i)) {
                items.push((*k, *i));
            } else {
                twice += super::pieces::span(e.size, *i).1;
            }
        }
    }
    let left = left - twice;
    let refused: Mutex<Vec<String>> = Mutex::new(Vec::new());
    if !items.is_empty() {
        log::info!(
            "uploads: the host lacks {} files of {bus} ({:.1} MB, {:.1} MB of it came before): {} pieces over up to {} connections",
            plan.len(),
            total as f64 / 1e6,
            (total - left) as f64 / 1e6,
            items.len(),
            super::pieces::CONNECTIONS
        );
        let done = std::sync::atomic::AtomicU64::new(total - left);
        let said = Mutex::new(Instant::now() - Duration::from_secs(1));
        let say = |n: u64| {
            let d = done.fetch_add(n, Ordering::Relaxed) + n;
            let mut s = said.lock().unwrap_or_else(|e| e.into_inner());
            if s.elapsed() > Duration::from_millis(250) || d == total {
                *s = Instant::now();
                let pct = d * 100 / total.max(1);
                show(format!("Sending your bus to the server for the other players: {pct}% ({} of {})", mb(d), mb(total)), d, total, true);
            }
        };
        say(0);
        super::pieces::share_out(items, super::pieces::CONNECTIONS, |take| {
            let (raw, mut out, mut input) = super::open_waiting(host, session, wait)?;
            let r = (|| {
                // (this connection's offer: the pieces it sends are of that list)
                offer_pieces(&mut out, &mut input, &body)?;
                loop {
                    let batch = take();
                    if batch.is_empty() {
                        return Ok(());
                    }
                    for (k, i) in &batch {
                        let data = local_piece(&sources[k], &manifest.entries[*k], *i)?;
                        let (codec, wire) = super::pieces::pack(&data);
                        writeln!(out, "PUTPIECE {k} {i} {} {codec} {}", data.len(), wire.len()).and_then(|_| out.write_all(&wire)).map_err(|e| e.to_string())?;
                    }
                    out.flush().map_err(|e| e.to_string())?;
                    for (k, i) in &batch {
                        let e = &manifest.entries[*k];
                        match super::read_reply(&mut input) {
                            Ok(_) => {}
                            // (a file the host refused: the others go on)
                            Err(x) if x.contains("the host says") => refused.lock().unwrap_or_else(|p| p.into_inner()).push(format!("{}: {x}", e.path)),
                            Err(x) => return Err(x),
                        }
                        say(super::pieces::span(e.size, *i).1);
                    }
                }
            })();
            let _ = writeln!(out, "BYE").and_then(|_| out.flush());
            let _ = raw.shutdown(std::net::Shutdown::Both);
            r
        })?;
    }
    let mut refused = refused.into_inner().unwrap_or_default();
    refused.dedup();
    if !refused.is_empty() {
        log::warn!("uploads: {} files refused by the host: {}", refused.len(), refused.iter().take(6).cloned().collect::<Vec<_>>().join("; "));
    }
    // every file there: the host keeps the bus by this fingerprint (also when it had every
    // file already - else the others asked it for this bus and waited for one never kept)
    let (raw, mut out, mut input) = super::open_waiting(host, session, wait)?;
    let r = (|| {
        let still = offer_pieces(&mut out, &mut input, &body)?;
        if !still.is_empty() {
            return Err(format!("{} files of {bus} did not come whole{}", still.len(), refused.first().map(|r| format!(" ({r})")).unwrap_or_default()));
        }
        writeln!(out, "DONE").and_then(|_| out.flush()).map_err(|e| e.to_string())?;
        super::read_reply(&mut input)
    })();
    let _ = writeln!(out, "BYE").and_then(|_| out.flush());
    let _ = raw.shutdown(std::net::Shutdown::Both);
    r?;
    let taken = plan.len().saturating_sub(refused.len());
    if taken > 0 {
        log::info!("uploads: {bus}: {taken} files taken by the host");
    }
    Ok(taken)
}

/// `send` with the files chosen: (relative path, where it is read from).
fn send_files(bus: &str, files: Vec<(String, PathBuf)>, host: SocketAddr, session: u64, quiet: bool) -> Result<usize, String> {
    if files.is_empty() {
        return Ok(0);
    }
    // (a big texture over a slow upload: the answer comes when all of it has arrived)
    let (raw, mut out, mut input) = super::open_waiting(host, session, Duration::from_secs(1800))?;
    // whether the host takes uploads at all, before a big bus is hashed for nothing
    let ask = serde_json::to_vec(&VehicleManifest { bus: bus.to_string(), entries: Vec::new() }).map_err(|e| e.to_string())?;
    writeln!(out, "OFFER {}", ask.len()).and_then(|_| out.write_all(&ask)).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    let len = super::read_reply(&mut input)?.ok_or("no answer")?;
    let mut none = vec![0u8; len.min(64) as usize];
    input.read_exact(&mut none).map_err(|e| e.to_string())?;
    if !quiet {
        show("Checking which files of your bus the server lacks...", 0, 0, true);
    }
    let bus_rel = omsi_net::vehicle_path(bus).unwrap_or_else(|| bus.to_string());
    // (in the order the server keeps it: the files it asks for are named by their place there)
    let manifest = VehicleManifest { bus: bus_rel, entries: hashed(&files) }.validated()?;
    let source: HashMap<String, PathBuf> = files.iter().map(|(rel, p)| (rel.to_lowercase(), p.clone())).collect();
    // in pieces (a server of this version); whole files to an older one, on this connection
    match send_pieces(bus, &manifest, &source, host, session) {
        Err(e) if e == OLDER_HOST => log::info!("uploads: the host takes whole files only"),
        r => {
            let _ = writeln!(out, "BYE").and_then(|_| out.flush());
            let _ = raw.shutdown(std::net::Shutdown::Both);
            return r;
        }
    }
    let body = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    writeln!(out, "OFFER {}", body.len()).and_then(|_| out.write_all(&body)).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    let len = super::read_reply(&mut input)?.ok_or("no answer")?;
    if len > 64 << 20 {
        return Err("the host's answer is too large".into());
    }
    let mut answer = vec![0u8; len as usize];
    input.read_exact(&mut answer).map_err(|e| e.to_string())?;
    let wanted: Vec<usize> = serde_json::from_slice(&answer).map_err(|e| e.to_string())?;
    let wanted: Vec<usize> = wanted.into_iter().filter(|k| *k < manifest.entries.len()).collect();
    if wanted.is_empty() {
        // it has every file (its own installation's, another player's): DONE all the same, so
        // that it keeps the bus by this fingerprint - else the others asked it for this bus
        // and waited for one that never came
        writeln!(out, "DONE").and_then(|_| out.flush()).map_err(|e| e.to_string())?;
        let kept = super::read_reply(&mut input);
        let _ = writeln!(out, "BYE").and_then(|_| out.flush());
        let _ = raw.shutdown(std::net::Shutdown::Both);
        kept?;
        return Ok(0);
    }
    let bytes: u64 = wanted.iter().map(|k| manifest.entries[*k].size).sum();
    log::info!("uploads: the host lacks {} files of {bus} ({:.1} MB): sending them", wanted.len(), bytes as f64 / 1e6);
    let say = |done: u64| {
        let pct = done * 100 / bytes.max(1);
        show(format!("Sending your bus to the server for the other players: {pct}% ({} of {})", mb(done), mb(bytes)), done, bytes, true);
    };
    say(0);
    // (a file that fails on our side ends the connection, so the answers stop waiting)
    let cut = raw.try_clone().map_err(|e| e.to_string())?;
    // every file at once, the answers read as they come (one round trip per file is slow
    // through a tunnel)
    let sources: Vec<(usize, u64, PathBuf)> = wanted.iter().filter_map(|k| {
        let e = &manifest.entries[*k];
        source.get(&e.path.to_lowercase()).map(|p| (*k, e.size, p.clone()))
    }).collect();
    if sources.len() != wanted.len() {
        return Err("the server asked for a file that is not ours".into());
    }
    let writer = std::thread::spawn(move || -> Result<(), String> {
        let r = (|| {
            for (k, size, path) in sources {
                let data = omsi_cfg::vfs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                if data.len() as u64 != size {
                    return Err(format!("{} changed while it was sent", path.display()));
                }
                writeln!(out, "PUT {k} {size}").and_then(|_| out.write_all(&data)).and_then(|_| out.flush()).map_err(|e| e.to_string())?;
            }
            writeln!(out, "DONE").and_then(|_| writeln!(out, "BYE")).and_then(|_| out.flush()).map_err(|e| e.to_string())
        })();
        if r.is_err() {
            let _ = cut.shutdown(std::net::Shutdown::Both);
        }
        r
    });
    let mut refused = Vec::new();
    let mut done = 0u64;
    for k in &wanted {
        let r = super::read_reply(&mut input);
        done += manifest.entries[*k].size;
        say(done);
        if let Err(e) = r {
            if !e.contains("the host says") {
                let _ = raw.shutdown(std::net::Shutdown::Both);
                // (our side's reason first: it is why the connection ended)
                return Err(match writer.join() {
                    Ok(Err(w)) => w,
                    _ => e,
                });
            }
            refused.push(format!("{}: {e}", manifest.entries[*k].path));
        }
    }
    let taken = super::read_reply(&mut input);
    // (the writer's last words, BYE, go before the connection is closed: closed first, they
    // failed now and then, and a whole upload was taken for one that failed)
    let written = writer.join().map_err(|_| "the upload stopped".to_string());
    let _ = raw.shutdown(std::net::Shutdown::Both);
    written??;
    if !refused.is_empty() {
        log::warn!("uploads: {} files refused by the host: {}", refused.len(), refused.iter().take(6).cloned().collect::<Vec<_>>().join("; "));
    }
    let taken = taken?.unwrap_or(0) as usize;
    log::info!("uploads: {bus}: {taken} files taken by the host");
    Ok(taken)
}

/// The files of ours this game has read for `bus`: its folder and the folders its texts
/// point into, and the fonts its text textures use. (relative path, where it is read from)
fn files_of(root: &Path, bus: &str) -> Vec<(String, PathBuf)> {
    let Some(bus_rel) = omsi_net::vehicle_path(bus).map(|b| norm(&b.replace('\\', "/"))) else { return Vec::new() };
    // our own content (not the original installation, not what the host sent)
    let roots: Vec<PathBuf> = omsi_cfg::content_roots().into_iter().filter(|r| r != root && !omsi_cfg::is_sandbox(r)).collect();
    let mut read: HashMap<String, (String, PathBuf)> = HashMap::new();
    for p in omsi_cfg::vfs::reads() {
        let Some(r) = roots.iter().filter(|r| p.starts_with(r)).max_by_key(|r| r.components().count()) else { continue };
        let Ok(rest) = p.strip_prefix(r) else { continue };
        let rel: Vec<String> = rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let rel = rel.join("/");
        read.insert(rel.to_lowercase(), (rel, p));
    }
    let Some(own) = owner_folder(&bus_rel) else { return Vec::new() };
    let mut prefixes: HashSet<String> = HashSet::from([own.to_lowercase()]);
    let inside = |prefixes: &HashSet<String>, lower: &str| prefixes.iter().any(|f| lower.starts_with(&format!("{f}/")));
    let mut scanned: HashSet<String> = HashSet::new();
    let mut fonts_used: HashSet<String> = HashSet::new();
    for _ in 0..6 {
        let before = prefixes.len();
        let texts: Vec<(String, PathBuf)> = read
            .iter()
            .filter(|(lower, _)| inside(&prefixes, lower) && !scanned.contains(*lower))
            .filter(|(lower, _)| [".bus", ".cfg", ".osc", ".txt", ".ovh", ".zug"].iter().any(|e| lower.ends_with(e)))
            .map(|(lower, (rel, p))| {
                let _ = lower;
                (rel.clone(), p.clone())
            })
            .collect();
        for (rel, path) in texts {
            scanned.insert(rel.to_lowercase());
            let Ok(text) = omsi_cfg::vfs::read_text(&path) else { continue };
            let here = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let lines: Vec<&str> = text.lines().collect();
            for (i, l) in lines.iter().enumerate() {
                let t = l.trim();
                if t.to_ascii_lowercase().starts_with("[texttexture") {
                    if let Some(f) = lines.get(i + 2) {
                        fonts_used.insert(f.trim().to_lowercase());
                    }
                }
                if !t.contains("..") || t.starts_with('[') || t.len() > 300 {
                    continue;
                }
                let target = norm(&format!("{here}/{t}"));
                let top = target.split('/').next().unwrap_or("").to_ascii_lowercase();
                if !super::FOLDERS.contains(&top.as_str()) {
                    continue;
                }
                if let Some(f) = owner_folder(&target).filter(|f| f.split('/').count() >= 2) {
                    prefixes.insert(f.to_lowercase());
                }
            }
        }
        if prefixes.len() == before {
            break;
        }
    }
    let mut out: Vec<(String, PathBuf)> = read.iter().filter(|(lower, _)| inside(&prefixes, lower)).map(|(_, v)| v.clone()).collect();
    // the fonts: each `.oft` read with a `[newfont]` of a name in use, and its bitmaps
    for (lower, (rel, path)) in &read {
        if !lower.starts_with("fonts/") || !lower.ends_with(".oft") {
            continue;
        }
        let Ok(text) = omsi_cfg::vfs::read_text(path) else { continue };
        let lines: Vec<&str> = text.lines().map(|l| l.trim()).collect();
        let mut wanted = false;
        for (i, l) in lines.iter().enumerate() {
            if l.eq_ignore_ascii_case("[newfont]") && lines.get(i + 1).is_some_and(|n| fonts_used.contains(&n.to_lowercase())) {
                wanted = true;
                for b in lines.iter().skip(i + 2).take(2) {
                    if let Some(v) = read.get(&format!("fonts/{}", b.to_lowercase())) {
                        out.push(v.clone());
                    }
                }
            }
        }
        if wanted {
            out.push((rel.clone(), path.clone()));
        }
    }
    let mut seen = HashSet::new();
    out.retain(|(rel, _)| super::refuse_path(rel).is_none() && seen.insert(rel.to_lowercase()));
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Size and SHA-256 of each file (the hashes remembered by size and time, as a join does).
pub(super) fn hashed(files: &[(String, PathBuf)]) -> Vec<Entry> {
    use rayon::prelude::*;
    let cache = super::hash_cache();
    let done: Vec<(Entry, Option<(String, (u64, u64, String))>)> = files
        .par_iter()
        .filter_map(|(rel, p)| {
            let key = p.to_string_lossy().to_string();
            if let Ok(md) = std::fs::metadata(p) {
                let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                if let Some((sz, mt, h)) = cache.get(&key) {
                    if *sz == md.len() && *mt == mtime {
                        // (checked as a file read now is: the hashes are remembered for every
                        // file a join compares, a program or one too large among them)
                        if md.len() > super::MAX_FILE || starts_as_program(p) {
                            return None;
                        }
                        return Some((Entry { path: rel.clone(), size: md.len(), sha256: h.clone() }, None));
                    }
                }
            }
            let data = omsi_cfg::vfs::read(p).ok()?;
            if data.len() as u64 > super::MAX_FILE || super::looks_executable(&data[..data.len().min(8)]) {
                return None;
            }
            let h = super::sha256_of(&data);
            let fresh = std::fs::metadata(p).ok().map(|md| {
                let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                (key, (md.len(), mtime, h.clone()))
            });
            Some((Entry { path: rel.clone(), size: data.len() as u64, sha256: h }, fresh))
        })
        .collect();
    let mut cache = cache;
    let mut entries = Vec::with_capacity(done.len());
    for (e, fresh) in done {
        if let Some((k, v)) = fresh {
            cache.insert(k, v);
        }
        entries.push(e);
    }
    super::save_hash_cache(&cache);
    entries
}

/// Does the plain file `p` start as a program does (`looks_executable`)? One that cannot be
/// read is taken for one.
fn starts_as_program(p: &Path) -> bool {
    let mut head = [0u8; 8];
    let Ok(mut f) = std::fs::File::open(p) else { return true };
    let mut n = 0;
    while n < head.len() {
        match f.read(&mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return true,
        }
    }
    super::looks_executable(&head[..n])
}

/// `a/b/../c` → `a/c`, `/`-separated.
fn norm(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split(['/', '\\']) {
        match c.trim() {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// The folder a file belongs to: a vehicle's whole folder (`Vehicles/<name>`), else the
/// folder the file is in.
fn owner_folder(rel: &str) -> Option<String> {
    let comps: Vec<&str> = rel.split('/').collect();
    if comps.len() < 3 {
        return None;
    }
    let top = comps[0].to_ascii_lowercase();
    if top == "vehicles" || top == "trains" {
        return Some(comps[..2].join("/"));
    }
    Some(comps[..comps.len() - 1].join("/"))
}

#[cfg(test)]
mod tests;
