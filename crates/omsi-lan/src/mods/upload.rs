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
        let mut body = vec![0u8; len as usize];
        input.read_exact(&mut body)?;
        if !accepting() || !super::vehicles::is_open() {
            return Ok(Err("this server takes no uploads".into()));
        }
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
        match super::vehicles::put_piece(&e, i, &data) {
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
        let mut data = vec![0u8; len as usize];
        input.read_exact(&mut data)?;
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
        match super::vehicles::put(&e, &data) {
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
mod tests {
    use super::super::Manifest;
    use super::*;
    use std::net::TcpListener;
    use std::sync::Arc;

    /// The served list of the parent module: (list, where each file is read from).
    type Ready = Mutex<Option<Arc<(Manifest, Vec<PathBuf>)>>>;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("openomsi-upload-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn entry(path: &str, data: &[u8]) -> Entry {
        Entry { path: path.into(), size: data.len() as u64, sha256: super::super::sha256_of(data) }
    }

    /// OFFER → the indices wanted (or the host's refusal).
    fn offer(host: SocketAddr, session: u64, m: &VehicleManifest) -> (super::super::Connection, Result<Vec<usize>, String>) {
        let (raw, mut out, mut input) = super::super::open(host, session).unwrap();
        let body = serde_json::to_vec(m).unwrap();
        writeln!(out, "OFFER {}", body.len()).unwrap();
        out.write_all(&body).unwrap();
        out.flush().unwrap();
        let r = super::super::read_reply(&mut input).map(|len| {
            let mut a = vec![0u8; len.unwrap() as usize];
            input.read_exact(&mut a).unwrap();
            serde_json::from_slice(&a).unwrap()
        });
        ((raw, out, input), r)
    }

    /// Offer `m`, send the wanted files out of `data` (by index) and finish: the files that came.
    fn upload(host: SocketAddr, session: u64, m: &VehicleManifest, data: &[Vec<u8>]) -> (Vec<usize>, Result<Option<u64>, String>) {
        let ((raw, mut out, mut input), wanted) = offer(host, session, m);
        let wanted = wanted.unwrap();
        for k in &wanted {
            writeln!(out, "PUT {k} {}", data[*k].len()).unwrap();
            out.write_all(&data[*k]).unwrap();
        }
        writeln!(out, "DONE").unwrap();
        out.flush().unwrap();
        for _ in &wanted {
            assert_eq!(super::super::read_reply(&mut input).unwrap(), None);
        }
        let done = super::super::read_reply(&mut input);
        let _ = raw.shutdown(std::net::Shutdown::Both);
        (wanted, done)
    }

    /// One request on the mods connection, its answer's bytes.
    fn ask(host: SocketAddr, session: u64, request: &str) -> Result<Vec<u8>, String> {
        let (raw, mut out, mut input) = super::super::open(host, session).unwrap();
        writeln!(out, "{request}").unwrap();
        out.flush().unwrap();
        let r = super::super::read_reply(&mut input).map(|len| {
            let mut a = vec![0u8; len.unwrap() as usize];
            input.read_exact(&mut a).unwrap();
            a
        });
        let _ = raw.shutdown(std::net::Shutdown::Both);
        r
    }

    /// `OFFER2` on a fresh connection: the plan, and the connection.
    fn offer2(host: SocketAddr, session: u64, m: &VehicleManifest) -> (super::super::Connection, Vec<(usize, Vec<u64>)>) {
        let (raw, mut out, mut input) = super::super::open(host, session).unwrap();
        let plan = offer_pieces(&mut out, &mut input, &serde_json::to_vec(m).unwrap()).unwrap();
        ((raw, out, input), plan)
    }

    /// A player's game hosting a session keeps its own bus in its store as a server keeps one
    /// sent to it - by the fingerprint its player announces, so the joining players fetch it
    /// by that; a file refused for what it is is withheld, as ever.
    #[test]
    fn a_hosting_game_keeps_its_own_bus_for_the_others() {
        let _turn = super::super::vehicles::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store_dir = temp("own-store");
        super::super::vehicles::open_for_test(&store_dir);
        let content = temp("own-content");
        let files = [
            ("Vehicles/OwnBus/own.bus", b"[model]\nmodel\\model.cfg\nown\n".to_vec()),
            ("Vehicles/OwnBus/model/model.cfg", b"[mesh]\nx.o3d\n".to_vec()),
            ("Vehicles/OwnBus/texture/x.dat", vec![4u8; 50_000]),
            ("Vehicles/OwnBus/texture/bad.dds", b"no texture at all".to_vec()),
        ];
        let mut chosen = Vec::new();
        for (rel, data) in &files {
            let p = rel.split('/').fold(content.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            chosen.push((rel.to_string(), p));
        }
        let fp = keep_files("Vehicles/OwnBus/own.bus", chosen.clone()).unwrap();
        let m = VehicleManifest { bus: "Vehicles/OwnBus/own.bus".into(), entries: chosen.iter().map(|(r, p)| entry(r, &std::fs::read(p).unwrap())).collect() }.validated().unwrap();
        assert_eq!(fp, m.fingerprint(), "the fingerprint its player's game announces");
        assert_eq!(super::super::vehicles::manifest(&fp), Some(m));
        // mounted and served as a server would, without the refused file
        assert!(super::super::vehicles::bus_file(&fp).is_some());
        let held = super::super::vehicles::withheld_of(&fp);
        assert_eq!(held.len(), 1, "{held:?}");
        assert_eq!(held[0].path, "Vehicles/OwnBus/texture/bad.dds");
        assert!(super::super::vehicles::blob(&super::super::sha256_of(b"no texture at all")).is_none());
        assert_eq!(super::super::vehicles::blob(&super::super::sha256_of(&files[2].1)), Some(files[2].1.clone()));
        // the same bus again: kept already
        assert_eq!(keep_files("Vehicles/OwnBus/own.bus", chosen).unwrap(), fp);
        let _ = std::fs::remove_dir_all(&content);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn an_upload_that_broke_off_goes_on_where_it_stopped() {
        let _turn = super::super::vehicles::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session = 0xb07;
        let store_dir = temp("resume-up-store");
        super::super::vehicles::open_for_test(&store_dir);
        accept(true);
        let ready: Arc<Ready> = Arc::new(Mutex::new(Some(Arc::new((Manifest::default(), Vec::new())))));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host = listener.local_addr().unwrap();
        {
            let ready = ready.clone();
            std::thread::spawn(move || {
                for s in listener.incoming().flatten() {
                    let ready = ready.clone();
                    std::thread::spawn(move || super::super::handle(s, session, &ready));
                }
            });
        }
        // a bus with a texture of several pieces, and a small file
        let content = temp("resume-up-content");
        let mut x = 0x2545_f491u32;
        let big: Vec<u8> = (0..(2 * super::super::pieces::PIECE + 777))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let files = [("Vehicles/UpResume/u.bus", b"[model]\nmodel\\model.cfg\nup resume\n".to_vec()), ("Vehicles/UpResume/texture/big.dat", big.clone())];
        let mut chosen = Vec::new();
        for (rel, data) in &files {
            let p = rel.split('/').fold(content.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            chosen.push((rel.to_string(), p));
        }
        let m = VehicleManifest { bus: "Vehicles/UpResume/u.bus".into(), entries: chosen.iter().map(|(r, p)| entry(r, &std::fs::read(p).unwrap())).collect() }.validated().unwrap();
        let k_big = m.entries.iter().position(|e| e.path.ends_with("big.dat")).unwrap();
        // the first attempt: the server lacks every piece; two of the big file's go, then the
        // line breaks
        let ((raw, mut out, mut input), plan) = offer2(host, session, &m);
        assert_eq!(plan.iter().map(|(_, p)| p.len()).sum::<usize>(), 4, "{plan:?}");
        for i in [0u64, 2] {
            let (start, len) = super::super::pieces::span(big.len() as u64, i);
            let data = &big[start as usize..(start + len) as usize];
            let (codec, wire) = super::super::pieces::pack(data);
            writeln!(out, "PUTPIECE {k_big} {i} {} {codec} {}", data.len(), wire.len()).unwrap();
            out.write_all(&wire).unwrap();
        }
        out.flush().unwrap();
        for _ in 0..2 {
            assert_eq!(super::super::read_reply(&mut input).unwrap(), None);
        }
        let _ = raw.shutdown(std::net::Shutdown::Both);
        // again: only what did not come is asked for
        let ((raw, _, _), plan) = offer2(host, session, &m);
        let big_left = plan.iter().find(|(k, _)| *k == k_big).map(|(_, p)| p.clone()).unwrap();
        assert_eq!(big_left, vec![1], "the pieces that came are not sent again");
        assert_eq!(plan.len(), 2, "the small file is still wanted whole");
        let _ = raw.shutdown(std::net::Shutdown::Both);
        // the player's game finishes it, and the server keeps the bus
        assert_eq!(send_files("Vehicles/UpResume/u.bus", chosen, host, session, true), Ok(2));
        let fp = m.fingerprint();
        assert_eq!(super::super::vehicles::manifest(&fp), Some(m.clone()));
        assert_eq!(super::super::vehicles::blob(&m.entries[k_big].sha256), Some(big));
        accept(false);
        let _ = std::fs::remove_dir_all(&content);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn a_file_the_bus_has_twice_never_ends_the_upload() {
        let _turn = super::super::vehicles::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session = 0xd0b;
        let store_dir = temp("twice-store");
        super::super::vehicles::open_for_test(&store_dir);
        accept(true);
        let ready: Arc<Ready> = Arc::new(Mutex::new(Some(Arc::new((Manifest::default(), Vec::new())))));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host = listener.local_addr().unwrap();
        {
            let ready = ready.clone();
            std::thread::spawn(move || {
                for s in listener.incoming().flatten() {
                    let ready = ready.clone();
                    std::thread::spawn(move || super::super::handle(s, session, &ready));
                }
            });
        }
        // the same texture in two folders of the bus
        let content = temp("twice-content");
        let tex: Vec<u8> = (0..(super::super::pieces::PIECE + 4321)).map(|i| (i * 7 % 251) as u8).collect();
        let files = [("Vehicles/Twice/t.bus", b"[model]\nmodel\\model.cfg\ntwice\n".to_vec()), ("Vehicles/Twice/a/tex.dat", tex.clone()), ("Vehicles/Twice/b/tex.dat", tex.clone())];
        let mut chosen = Vec::new();
        for (rel, data) in &files {
            let p = rel.split('/').fold(content.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            chosen.push((rel.to_string(), p));
        }
        let m = VehicleManifest { bus: "Vehicles/Twice/t.bus".into(), entries: chosen.iter().map(|(r, p)| entry(r, &std::fs::read(p).unwrap())).collect() }.validated().unwrap();
        let (ka, kb) = (m.entries.iter().position(|e| e.path.ends_with("a/tex.dat")).unwrap(), m.entries.iter().position(|e| e.path.ends_with("b/tex.dat")).unwrap());
        let put = |out: &mut super::super::channel::SecureWriter<std::net::TcpStream>, k: usize, i: u64| {
            let (start, len) = super::super::pieces::span(tex.len() as u64, i);
            let data = &tex[start as usize..(start + len) as usize];
            let (codec, wire) = super::super::pieces::pack(data);
            writeln!(out, "PUTPIECE {k} {i} {} {codec} {}", data.len(), wire.len()).unwrap();
            out.write_all(&wire).unwrap();
            out.flush().unwrap();
        };
        // one connection of the player offers the bus; another finishes the texture in `a`
        let ((raw_late, mut out_late, mut in_late), plan) = offer2(host, session, &m);
        assert_eq!(plan.len(), 3);
        let ((raw, mut out, mut input), _) = offer2(host, session, &m);
        for i in 0..2 {
            put(&mut out, ka, i);
            assert_eq!(super::super::read_reply(&mut input).unwrap(), None);
        }
        let _ = raw.shutdown(std::net::Shutdown::Both);
        // a connection that offered after that: the texture in `b` is whole with it...
        let ((raw2, mut out2, mut in2), plan2) = offer2(host, session, &m);
        assert!(!plan2.iter().any(|(k, _)| *k == kb), "{plan2:?}");
        // ...and a piece of it still on its way there is taken, not the end of the upload
        put(&mut out2, kb, 0);
        assert_eq!(super::super::read_reply(&mut in2).unwrap(), None, "a piece of a file here whole already");
        put(&mut out_late, kb, 1);
        assert_eq!(super::super::read_reply(&mut in_late).unwrap(), None);
        let _ = raw2.shutdown(std::net::Shutdown::Both);
        let _ = raw_late.shutdown(std::net::Shutdown::Both);
        // the player's game the whole way: the texture goes once, the bus is kept
        assert_eq!(send_files("Vehicles/Twice/t.bus", chosen, host, session, true).map(|_| ()), Ok(()));
        assert_eq!(super::super::vehicles::manifest(&m.fingerprint()), Some(m));
        accept(false);
        let _ = std::fs::remove_dir_all(&content);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    #[test]
    fn two_versions_of_a_bus_are_kept_side_by_side_by_their_content() {
        let _turn = super::super::vehicles::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session = 0xb05;
        let store_dir = temp("store");
        super::super::vehicles::open_for_test(&store_dir);
        accept(true);
        let ready: Arc<Ready> = Arc::new(Mutex::new(Some(Arc::new((Manifest::default(), Vec::new())))));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host = listener.local_addr().unwrap();
        {
            let ready = ready.clone();
            std::thread::spawn(move || {
                for s in listener.incoming().flatten() {
                    let ready = ready.clone();
                    std::thread::spawn(move || super::super::handle(s, session, &ready));
                }
            });
        }
        let bus = b"[model]\nmodel\\model.cfg\n".to_vec();
        let cfg = b"[mesh]\nbody.o3d\n".to_vec();
        let tex1 = vec![7u8; 300_000];
        let tex2 = vec![8u8; 300_000];
        let version = |tex: &Vec<u8>| -> (VehicleManifest, Vec<Vec<u8>>) {
            let m = VehicleManifest {
                bus: "Vehicles/UpTestBus/bus.bus".into(),
                entries: vec![entry("Vehicles/UpTestBus/bus.bus", &bus), entry("Vehicles/UpTestBus/model/model.cfg", &cfg), entry("Vehicles/UpTestBus/texture/body.dat", tex)],
            }
            .validated()
            .unwrap();
            // (the data in the list's order)
            let data = m.entries.iter().map(|e| if e.path.ends_with(".bus") { bus.clone() } else if e.path.ends_with(".cfg") { cfg.clone() } else { tex.clone() }).collect();
            (m, data)
        };
        // one player's version - asked for by another player before it has come: the answer
        // goes the moment it is kept, not when that player asks again
        let (m1, d1) = version(&tex1);
        let early = {
            let fp = m1.fingerprint();
            std::thread::spawn(move || {
                let t0 = std::time::Instant::now();
                (ask(host, session, &format!("MANIFEST {fp}")), t0.elapsed())
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        let (wanted, done) = upload(host, session, &m1, &d1);
        let (answer, took) = early.join().unwrap();
        let got: VehicleManifest = serde_json::from_slice(&answer.unwrap()).unwrap();
        assert_eq!(got, m1);
        assert!(took < std::time::Duration::from_millis(1900), "answered when it came, not at the end of the wait ({took:?})");
        assert_eq!(wanted.len(), 3);
        assert_eq!(done, Ok(Some(3)));
        let fp1 = m1.fingerprint();
        assert_eq!(super::super::vehicles::manifest(&fp1), Some(m1.clone()));
        // another player's version of the same mod (same paths, another texture): only what
        // differs is wanted - and it does not take the place of the first
        let (m2, d2) = version(&tex2);
        let (wanted, done) = upload(host, session, &m2, &d2);
        assert_eq!(wanted.len(), 1, "the files both versions share are kept once");
        assert_eq!(done, Ok(Some(1)));
        let fp2 = m2.fingerprint();
        assert_ne!(fp1, fp2);
        // each one mounted as itself: the texture of each version where its bus is
        for (fp, tex) in [(&fp1, &tex1), (&fp2, &tex2)] {
            let bus_file = super::super::vehicles::bus_file(fp).unwrap();
            let body = omsi_cfg::resolve_path(bus_file.parent().unwrap(), "texture/body.dat");
            assert_eq!(&omsi_cfg::vfs::read(&body).unwrap(), tex, "{fp}");
        }
        // asked for by its content, over the wire
        let got: VehicleManifest = serde_json::from_slice(&ask(host, session, &format!("MANIFEST {fp2}")).unwrap()).unwrap();
        assert_eq!(got, m2);
        assert_eq!(ask(host, session, &format!("BLOB {}", super::super::sha256_of(&tex2))).unwrap(), tex2);
        assert!(ask(host, session, &format!("BLOB {}", "0".repeat(64))).is_err());
        assert!(ask(host, session, &format!("MANIFEST v1-{}", "0".repeat(64))).is_err());
        // nothing plain in the store
        let all: Vec<PathBuf> = std::fs::read_dir(&store_dir).unwrap().flatten().map(|e| e.path()).collect();
        assert!(!all.iter().any(|p| std::fs::read(p).map(|b| b == tex1 || b == tex2).unwrap_or(false)), "a plain file in the store");
        // still there after a restart
        super::super::vehicles::open_for_test(&store_dir);
        assert!(super::super::vehicles::manifest(&fp1).is_some() && super::super::vehicles::manifest(&fp2).is_some());
        // (not on the list every joining game fetches: they ask for it by its fingerprint)
        assert!(ready.lock().unwrap().as_ref().unwrap().0.entries.is_empty());
        // a list with a program in it is refused whole
        let mut bad = m1.clone();
        bad.entries.push(entry("Vehicles/UpTestBus/setup.exe", b"MZ"));
        let ((raw, _, _), r) = offer(host, session, &bad);
        assert!(r.unwrap_err().contains("setup.exe"));
        let _ = raw.shutdown(std::net::Shutdown::Both);
        // a file that is not what its name says is refused - and the bus is kept without it,
        // the others told so with its list (one bad texture costs a texture, not the bus)
        let fake = b"this is no texture at all, whatever its name".to_vec();
        let bad_bus = b"[model]\nbad\n".to_vec();
        let mb = VehicleManifest { bus: "Vehicles/Bad/bad.bus".into(), entries: vec![entry("Vehicles/Bad/bad.bus", &bad_bus), entry("Vehicles/Bad/t.dds", &fake)] }.validated().unwrap();
        let ((raw, mut out, mut input), wanted) = offer(host, session, &mb);
        assert_eq!(wanted.unwrap().len(), 2);
        for e in &mb.entries {
            let d = if e.path.ends_with(".bus") { &bad_bus } else { &fake };
            let k = mb.entries.iter().position(|x| x.path == e.path).unwrap();
            writeln!(out, "PUT {k} {}", d.len()).unwrap();
            out.write_all(d).unwrap();
        }
        writeln!(out, "DONE").unwrap();
        out.flush().unwrap();
        let replies: Vec<Result<Option<u64>, String>> = (0..3).map(|_| super::super::read_reply(&mut input)).collect();
        let _ = raw.shutdown(std::net::Shutdown::Both);
        assert!(replies.iter().any(|r| r.as_ref().is_err_and(|e| e.contains("t.dds") && e.contains("not a picture"))), "{replies:?}");
        assert_eq!(replies[2], Ok(Some(1)), "kept with the one file that came");
        assert_eq!(super::super::vehicles::manifest(&mb.fingerprint()), Some(mb.clone()), "kept by the fingerprint of all its files");
        let answer: serde_json::Value = serde_json::from_slice(&ask(host, session, &format!("MANIFEST {}", mb.fingerprint())).unwrap()).unwrap();
        assert_eq!(answer["withheld"][0]["path"], "Vehicles/Bad/t.dds", "{answer}");
        assert!(answer["withheld"][0]["why"].as_str().unwrap().contains("not a picture"));
        // (a game before the withheld files reads the list alone: the fingerprint is the same)
        assert_eq!(serde_json::from_value::<VehicleManifest>(answer).unwrap().validated().unwrap().fingerprint(), mb.fingerprint());
        // the file reaches nobody; its bus is mounted without it
        assert!(super::super::vehicles::blob(&super::super::sha256_of(&fake)).is_none());
        let bus_file = super::super::vehicles::bus_file(&mb.fingerprint()).unwrap();
        assert!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(bus_file.parent().unwrap(), "t.dds")).is_err());
        // and so after a restart
        super::super::vehicles::open_for_test(&store_dir);
        assert!(super::super::vehicles::bus_file(&mb.fingerprint()).is_some(), "kept without it after a restart");
        assert_eq!(super::super::vehicles::withheld_of(&mb.fingerprint()).len(), 1);
        // a bus whose bus file itself is refused is never kept
        let bad_head = VehicleManifest { bus: "Vehicles/Bad/head.bus".into(), entries: vec![entry("Vehicles/Bad/head.bus", b"[model]\0\0binary")] }.validated().unwrap();
        let ((raw, mut out, mut input), wanted) = offer(host, session, &bad_head);
        assert_eq!(wanted.unwrap().len(), 1);
        writeln!(out, "PUT 0 {}", b"[model]\0\0binary".len()).unwrap();
        out.write_all(b"[model]\0\0binary").unwrap();
        writeln!(out, "DONE").unwrap();
        out.flush().unwrap();
        assert!(super::super::read_reply(&mut input).unwrap_err().contains("not a text file"));
        assert!(super::super::read_reply(&mut input).unwrap_err().contains("has not come"));
        let _ = raw.shutdown(std::net::Shutdown::Both);
        assert!(super::super::vehicles::manifest(&bad_head.fingerprint()).is_none());
        // a player's game the whole way: offered, sent with its progress, kept
        let content = temp("send-content");
        let files = [("Vehicles/SendBus/send.bus", b"[model]\nmodel\\model.cfg\n".to_vec()), ("Vehicles/SendBus/model/model.cfg", b"[mesh]\nx.o3d\n".to_vec()), ("Vehicles/SendBus/texture/x.dat", vec![3u8; 70_000])];
        let mut chosen = Vec::new();
        for (rel, data) in &files {
            let p = rel.split('/').fold(content.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            chosen.push((rel.to_string(), p));
        }
        // (its .bus is byte for byte the test bus's above: kept once already, not sent again)
        assert_eq!(send_files("Vehicles/SendBus/send.bus", chosen.clone(), host, session, false), Ok(2));
        assert!(SHOWN.lock().unwrap().as_ref().is_some_and(|s| s.done == s.total && s.total == 70_000 + 13), "the progress reached the end");
        assert!(super::super::vehicles::manifest(&VehicleManifest { bus: "Vehicles/SendBus/send.bus".into(), entries: chosen.iter().map(|(r, p)| entry(r, &std::fs::read(p).unwrap())).collect() }.validated().unwrap().fingerprint()).is_some(), "kept by the fingerprint the player works out too");
        // again: the server has it all, nothing goes
        assert_eq!(send_files("Vehicles/SendBus/send.bus", chosen, host, session, true), Ok(0));
        // a bus whose files the server has every one of (by their content, from other buses)
        // is kept all the same: the others ask for it by its fingerprint
        let all_here = temp("all-here");
        let mut chosen = Vec::new();
        for (rel, data) in [("Vehicles/AllHere/here.bus", &bus), ("Vehicles/AllHere/model/model.cfg", &cfg), ("Vehicles/AllHere/texture/body.dat", &tex1)] {
            let p = rel.split('/').fold(all_here.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            chosen.push((rel.to_string(), p));
        }
        let fp_here = VehicleManifest { bus: "Vehicles/AllHere/here.bus".into(), entries: chosen.iter().map(|(r, p)| entry(r, &std::fs::read(p).unwrap())).collect() }.validated().unwrap().fingerprint();
        assert!(super::super::vehicles::manifest(&fp_here).is_none());
        assert_eq!(send_files("Vehicles/AllHere/here.bus", chosen, host, session, true), Ok(0));
        assert!(super::super::vehicles::manifest(&fp_here).is_some(), "a bus of files the server had is kept by its fingerprint");
        let got: VehicleManifest = serde_json::from_slice(&ask(host, session, &format!("MANIFEST {fp_here}")).unwrap()).unwrap();
        assert_eq!(got.fingerprint(), fp_here);
        let _ = std::fs::remove_dir_all(&all_here);
        let _ = std::fs::remove_dir_all(&content);
        // switched off: refused
        accept(false);
        let ((raw, _, _), wanted) = offer(host, session, &m1);
        assert!(no_uploads(&wanted.unwrap_err()));
        let _ = raw.shutdown(std::net::Shutdown::Both);
        let _ = std::fs::remove_dir_all(&store_dir);
    }

    /// A stock bus of an OMSI 2 installation (`OMSI_ROOT`): its fingerprint is the same each
    /// time it is worked out, and another paint is another bus.
    #[test]
    fn a_bus_in_a_paint_is_one_fingerprint() {
        let Some(root) = std::env::var_os("OMSI_ROOT").map(PathBuf::from) else { return };
        omsi_cfg::add_content_root(root.clone());
        let path = omsi_cfg::resolve_path(&root, "Vehicles/MAN_SD200/MAN_SD77.bus");
        let Ok(ty) = omsi_sim::VehicleType::load(&root, &path) else { return };
        let bus = "Vehicles/MAN_SD200/MAN_SD77.bus";
        let paint = |i: usize| ty.paint_schemes[i].name.clone();
        let files = |i: usize| super::super::bus_files::of_type(&root, &ty, Some(i), None);
        let (m, fp, sent) = own_manifest(&root, bus, &paint(0), files(0)).unwrap();
        assert!(omsi_net::is_fingerprint(&fp));
        assert_eq!(own_fingerprint(bus, &paint(0)), fp);
        assert_eq!(m.entries.len(), sent.len());
        // worked out again from scratch: the same
        *OWN.lock().unwrap() = None;
        assert_eq!(own_manifest(&root, bus, &paint(0), files(0)).unwrap().1, fp);
        // another paint
        let (_, other, _) = own_manifest(&root, bus, &paint(1), files(1)).unwrap();
        assert_ne!(other, fp);
        assert!(own_fingerprint(bus, &paint(0)).is_empty(), "the fingerprint of the paint no longer worn");
    }

    #[test]
    fn only_what_the_bus_read_goes() {
        let root = temp("content");
        let original = temp("original");
        let write = |rel: &str, data: &[u8]| {
            let p = rel.split('/').fold(root.clone(), |p, c| p.join(c));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
            p
        };
        let bus = write("Vehicles/ReadBus/bus.bus", b"[model]\nmodel\\model.cfg\n");
        let model = write("Vehicles/ReadBus/model/model.cfg", b"[mesh]\n..\\..\\SharedDisplays\\matrix.o3d\n\n[texttexture]\nX\nMatrixFont\n");
        let used = write("Vehicles/ReadBus/texture/red/body.dds", b"red");
        write("Vehicles/ReadBus/texture/blue/body.dds", b"blue");
        write("Vehicles/ReadBus/variant2.bus", b"[model]\n");
        let shared = write("Vehicles/SharedDisplays/matrix.o3d", b"o3d");
        let font = write("Fonts/matrix.oft", b"[newfont]\nMatrixFont\nmatrix.bmp\nmatrix_a.bmp\n");
        let bitmap = write("Fonts/matrix.bmp", b"bmp");
        let other = write("Vehicles/OtherBus/x.cfg", b"x");
        omsi_cfg::add_content_root(root.clone());
        omsi_cfg::vfs::record_reads(true);
        for p in [&bus, &model, &used, &shared, &font, &bitmap, &other] {
            omsi_cfg::vfs::read(p).unwrap();
        }
        let got: Vec<String> = files_of(&original, "Vehicles/ReadBus/bus.bus").into_iter().map(|(r, _)| r).collect();
        omsi_cfg::remove_content_root(&root);
        assert_eq!(
            got,
            vec!["Fonts/matrix.bmp", "Fonts/matrix.oft", "Vehicles/ReadBus/bus.bus", "Vehicles/ReadBus/model/model.cfg", "Vehicles/ReadBus/texture/red/body.dds", "Vehicles/SharedDisplays/matrix.o3d"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn folders_of_a_bus_and_what_it_points_into() {
        assert_eq!(owner_folder("Vehicles/MAN_SD200/model/model.cfg").as_deref(), Some("Vehicles/MAN_SD200"));
        assert_eq!(owner_folder("Sounds/Shared/x.wav").as_deref(), Some("Sounds/Shared"));
        assert_eq!(owner_folder("Fonts/x.oft"), None);
        assert_eq!(norm("Vehicles/MAN/model/../../Anzeigen/Matrix/x.o3d"), "Vehicles/Anzeigen/Matrix/x.o3d");
        assert_eq!(norm("a\\.\\b//c"), "a/b/c");
    }
}
