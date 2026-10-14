//! Buses by their content (see `identity`): every version of every bus players sent, side by
//! side. A file is kept once, sealed in the store by its SHA-256 (`store`), however many
//! buses use it; a bus is its manifest, kept by its fingerprint in `vehicles/<fp>.json` beside
//! the store. Two players with two versions of the same mod are two manifests, and each is
//! served and mounted as itself - nothing is kept by its path, so no version takes the
//! place of another (what Git, Nix and an OCI registry do with their objects).
//!
//! A bus is mounted on its own (`mount`): its files under `lan-vehicles/<fp>`, at the paths of
//! its manifest, a sealed mount that is no content root - nothing of another bus or of the
//! machine's own mods is seen through it.

use super::identity::{self, VehicleManifest};
use super::pieces;
use super::store::Store;
use super::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Vehicles {
    store: Store,
    /// `vehicles/` beside the store: one `<fp>.json` per bus.
    dir: PathBuf,
    /// By fingerprint, in the order they came (the oldest first).
    manifests: Vec<(String, VehicleManifest)>,
    /// The size of each kept file by its SHA-256 (what a blob is asked for by).
    sizes: HashMap<String, u64>,
    mounted: HashSet<String>,
    /// Files refused for what they are (`quarantine`), by their content and kind
    /// (`held_key`): why. A bus is kept and drawn without them (`withhold`).
    withheld: HashMap<String, String>,
    /// By fingerprint: the files a kept bus is without (`<fp>.withheld.json` beside its list).
    withheld_of: HashMap<String, Vec<Withheld>>,
}

/// A file a bus is kept and drawn without: its path in the bus's list, and why it was refused
/// (`quarantine`). One bad font costs a bus its destination sign, not the whole bus; the file
/// itself reaches nobody.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Withheld {
    pub path: String,
    pub why: String,
}

/// The files a host's list says its bus is without (the `withheld` beside the list in the
/// answer to `MANIFEST`; a host before them says none).
#[derive(Default, serde::Deserialize)]
struct HeldBack {
    #[serde(default)]
    withheld: Vec<Withheld>,
}

static VEHICLES: Mutex<Option<Vehicles>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Vehicles>> {
    VEHICLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the buses are mounted (no folder on the disk).
fn mounts() -> Option<PathBuf> {
    super::sandbox_base().map(|b| b.with_file_name("lan-vehicles"))
}

/// The buses kept in `dir` (the store's folder) opened, each with all its files there.
/// False when there is no store.
pub(super) fn open() -> bool {
    match super::store_dir().map(|d| open_in(&d)) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            log::warn!("vehicles: {e}");
            false
        }
        None => false,
    }
}

fn open_in(dir: &Path) -> Result<(), String> {
    let store = Store::open(dir)?;
    let vdir = dir.join("vehicles");
    std::fs::create_dir_all(&vdir).map_err(|e| format!("{}: {e}", vdir.display()))?;
    let mut found: Vec<(std::time::SystemTime, String, VehicleManifest)> = Vec::new();
    let (mut withheld, mut withheld_of) = (HashMap::new(), HashMap::new());
    for e in std::fs::read_dir(&vdir).map_err(|e| e.to_string())?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(fp) = name.strip_suffix(".json").filter(|f| omsi_net::is_fingerprint(f)) else { continue };
        let Some(m) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<VehicleManifest>(&b).ok()).and_then(|m| m.validated().ok()) else {
            log::warn!("vehicles: {name} is no valid list; left aside");
            continue;
        };
        // (a file named by another fingerprint than its list's is not that bus)
        if m.fingerprint() != fp {
            log::warn!("vehicles: {name} lists another bus; left aside");
            continue;
        }
        // (the files it was kept without are none of its files to have)
        let held: Vec<Withheld> = std::fs::read(vdir.join(format!("{fp}.withheld.json"))).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Some(lost) = m.entries.iter().find(|e| store.has(e).is_none() && !held.iter().any(|w| w.path == e.path)) {
            log::warn!("vehicles: {name} lacks {} in the store; left aside", lost.path);
            continue;
        }
        if !held.is_empty() {
            for w in &held {
                if let Some(e) = m.entries.iter().find(|e| e.path == w.path) {
                    withheld.insert(held_key(e), w.why.clone());
                }
            }
            withheld_of.insert(fp.to_string(), held);
        }
        let t = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        found.push((t, fp.to_string(), m));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut sizes = HashMap::new();
    for (_, _, m) in &found {
        for e in &m.entries {
            sizes.insert(e.sha256.clone(), e.size);
        }
    }
    log::info!("vehicles: {} buses players sent are kept", found.len());
    *lock() = Some(Vehicles { store, dir: vdir, manifests: found.into_iter().map(|(_, fp, m)| (fp, m)).collect(), sizes, mounted: HashSet::new(), withheld, withheld_of });
    // (what nobody has used for longer than buses are kept goes at once)
    collect(0, IN_USE);
    Ok(())
}

pub(super) fn is_open() -> bool {
    lock().is_some()
}

/// The manifest of the bus `fp`, if it is kept here.
pub(crate) fn manifest(fp: &str) -> Option<VehicleManifest> {
    lock().as_ref()?.manifests.iter().find(|(f, _)| f == fp).map(|(_, m)| m.clone())
}

/// Which files of `m` are not kept here (their index in the list) - but for those refused for
/// what they are (`withhold`): the bus is kept without them.
pub(super) fn missing(m: &VehicleManifest) -> Vec<usize> {
    let g = lock();
    let Some(v) = g.as_ref() else { return (0..m.entries.len()).collect() };
    m.entries.iter().enumerate().filter(|(_, e)| v.store.has(e).is_none() && !v.withheld.contains_key(&held_key(e))).map(|(k, _)| k).collect()
}

/// What a refused file is held back by: its SHA-256 and the kind of file its name says it is.
/// `quarantine` judges a file by its name, so the same bytes under another name are another
/// case: a `.bus` file sent again as a `.wav` (refused: no sound) must not cost the bus its
/// own `.bus` - a player could otherwise make any bus they know undrawable for everybody.
fn held_key(e: &Entry) -> String {
    let name = e.path.rsplit('/').next().unwrap_or("");
    let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
    format!("{}.{ext}", e.sha256.to_ascii_lowercase())
}

/// `e` was refused for what it is (`why`, `quarantine`): a bus is kept and drawn without it -
/// never with another file in its place - and it is not asked for again.
pub(super) fn withhold(e: &Entry, why: &str) {
    if let Some(v) = lock().as_mut() {
        v.withheld.insert(held_key(e), why.to_string());
    }
}

/// Was `e` refused for what it is (`withhold`)?
pub(super) fn is_withheld(e: &Entry) -> bool {
    lock().as_ref().is_some_and(|v| v.withheld.contains_key(&held_key(e)))
}

/// The files the kept bus `fp` is without (`withhold`).
pub(crate) fn withheld_of(fp: &str) -> Vec<Withheld> {
    lock().as_ref().and_then(|v| v.withheld_of.get(fp).cloned()).unwrap_or_default()
}

/// The files of `m` not kept yet that this machine has itself in the same version (in its
/// own content or the installation, at the same path) are kept from there: nothing to send.
/// How many.
///
/// A list comes from any player, and names what it likes: one offer at a time compares (a
/// list of thousands of the server's own paths, sent over a hundred connections at once,
/// had the server read and hash its whole installation over and over), each file by its size
/// first and by its hash remembered (`local_sha`) - only a file that is the same is read.
pub(super) fn adopt_local(m: &VehicleManifest) -> usize {
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let mut n = 0;
    for k in missing(m) {
        let e = &m.entries[k];
        if let Some(d) = own_copy(e) {
            if put_sent(e, &d).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// This machine's own file at `e`'s path (in its content or the installation, never a
/// session's), if it is that file: read only once its size and hash say so.
fn own_copy(e: &Entry) -> Option<Vec<u8>> {
    let (_, p) = omsi_cfg::find_in_roots(&e.path).filter(|(r, _)| !omsi_cfg::is_sandbox(r))?;
    if !local_sha(&p, e.size).is_some_and(|h| h.eq_ignore_ascii_case(&e.sha256)) {
        return None;
    }
    omsi_cfg::vfs::read(&p).ok().filter(|d| d.len() as u64 == e.size && super::sha256_of(d).eq_ignore_ascii_case(&e.sha256))
}

/// Hashes of this machine's own files, by path: (size, modification time, SHA-256).
type LocalShas = HashMap<PathBuf, (u64, u64, String)>;
static LOCAL_SHAS: Mutex<Option<LocalShas>> = Mutex::new(None);

/// The SHA-256 of the local file `p` if it has `size` bytes: nothing is read for another
/// size, and a plain file's hash is remembered by its size and time.
fn local_sha(p: &Path, size: u64) -> Option<String> {
    let Ok(md) = std::fs::metadata(p) else {
        // (a file in a mounted archive: its size, then its bytes)
        if omsi_cfg::vfs::file_size(p) != Some(size) {
            return None;
        }
        return omsi_cfg::vfs::read(p).ok().filter(|d| d.len() as u64 == size).map(|d| super::sha256_of(&d));
    };
    if !md.is_file() || md.len() != size {
        return None;
    }
    let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    if let Some((_, _, h)) = LOCAL_SHAS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|c| c.get(p)).filter(|(s, t, _)| *s == size && *t == mtime) {
        return Some(h.clone());
    }
    let h = super::sha256_of(&std::fs::read(p).ok()?);
    LOCAL_SHAS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(p.to_path_buf(), (size, mtime, h.clone()));
    Some(h)
}

/// Keep a file (checked against `e` already) this game fetched or has itself.
pub(super) fn put(e: &Entry, data: &[u8]) -> Result<(), String> {
    put_making_room(e, data, IN_USE)
}

/// Keep a file a player sent (checked against `e` already): room is made of buses nobody
/// has used for a day at least (`SENT_MAKES_ROOM_OF`), never of the ones played lately.
pub(super) fn put_sent(e: &Entry, data: &[u8]) -> Result<(), String> {
    put_making_room(e, data, SENT_MAKES_ROOM_OF)
}

fn put_making_room(e: &Entry, data: &[u8], unused_for: std::time::Duration) -> Result<(), String> {
    make_room(e.size, unused_for)?;
    let mut g = lock();
    let v = g.as_mut().ok_or("no store")?;
    v.store.put(e, data).map_err(|x| x.to_string())?;
    v.sizes.insert(e.sha256.to_ascii_lowercase(), e.size);
    Ok(())
}

/// The file with that SHA-256, if it is kept here.
/// (Read and unsealed with the store let go: a file of a GB held every other request up.)
pub(super) fn blob(sha: &str) -> Option<Vec<u8>> {
    let sha = sha.to_ascii_lowercase();
    let (store, size) = {
        let g = lock();
        let v = g.as_ref()?;
        (v.store.clone(), *v.sizes.get(&sha)?)
    };
    store.read(&Entry { path: String::new(), size, sha256: sha }).ok()
}

/// The files served last, unsealed (`served`): a file asked for piece by piece is opened
/// once, not once a piece. At most `SERVED_BYTES` of them.
static SERVED: Mutex<VecDeque<(String, Arc<Vec<u8>>)>> = Mutex::new(VecDeque::new());
const SERVED_BYTES: usize = 512 << 20;

/// The file with that SHA-256, if it is kept here - from the files served last when it is
/// one of them.
pub(super) fn served(sha: &str) -> Option<Arc<Vec<u8>>> {
    let sha = sha.to_ascii_lowercase();
    {
        let mut c = SERVED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = c.iter().position(|(s, _)| *s == sha) {
            let hit = c.remove(k)?;
            let data = hit.1.clone();
            c.push_front(hit);
            return Some(data);
        }
    }
    // (one file opened at a time: asked for a hundred times at once, each a GB, they were
    // all read into memory together)
    static OPENING: Mutex<()> = Mutex::new(());
    let _one = OPENING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, d)) = SERVED.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(s, _)| *s == sha) {
        return Some(d.clone());
    }
    let data = Arc::new(blob(&sha)?);
    let mut c = SERVED.lock().unwrap_or_else(|e| e.into_inner());
    c.push_front((sha, data.clone()));
    let mut total = 0;
    c.retain(|(_, d)| {
        total += d.len();
        total <= SERVED_BYTES || Arc::ptr_eq(d, &data)
    });
    Some(data)
}

/// Piece `i` of the kept file `sha` packed at its smallest (`recompress`), if it is:
/// (its raw size, codec, bytes on the wire).
pub(super) fn packed(sha: &str, i: u64) -> Option<(u64, char, Vec<u8>)> {
    let sha = sha.to_ascii_lowercase();
    let g = lock();
    let v = g.as_ref()?;
    let size = *v.sizes.get(&sha)?;
    if i >= pieces::count(size) {
        return None;
    }
    let (codec, wire) = v.store.packed(&sha, i)?;
    Some((pieces::span(size, i).1, codec, wire))
}

/// The files to pack at their smallest, one after the other, off the game's way.
struct Recompressing {
    queue: VecDeque<String>,
    queued: std::collections::BTreeSet<String>,
    running: bool,
}

static RECOMPRESSING: Mutex<Recompressing> = Mutex::new(Recompressing { queue: VecDeque::new(), queued: std::collections::BTreeSet::new(), running: false });

/// A kept file was asked for piece by piece before it was packed at its smallest: it is,
/// in the background (one file at a time, on one thread) - the players who fetch it after
/// that get it smaller (`packed`), the bytes it unpacks to the same.
pub(super) fn recompress_later(sha: &str) {
    let mut r = RECOMPRESSING.lock().unwrap_or_else(|e| e.into_inner());
    let sha = sha.to_ascii_lowercase();
    if r.queued.insert(sha.clone()) {
        r.queue.push_back(sha);
    }
    // (the tests pack what is queued themselves: `recompress_queued`)
    if r.running || cfg!(test) {
        return;
    }
    r.running = true;
    let spawned = std::thread::Builder::new().name("recompress".into()).spawn(|| {
        recompress_queued();
        RECOMPRESSING.lock().unwrap_or_else(|e| e.into_inner()).running = false;
    });
    if spawned.is_err() {
        r.running = false;
    }
}

/// Pack every file queued (`recompress_later`), until none is left.
pub(super) fn recompress_queued() {
    let (mut files, mut raw, mut wire) = (0, 0u64, 0u64);
    let t0 = std::time::Instant::now();
    loop {
        let next = {
            let mut r = RECOMPRESSING.lock().unwrap_or_else(|e| e.into_inner());
            let next = r.queue.pop_front();
            if let Some(s) = &next {
                r.queued.remove(s);
            }
            next
        };
        let Some(sha) = next else { break };
        if let Some((r, w)) = recompress(&sha) {
            files += 1;
            raw += r;
            wire += w;
        }
    }
    if files > 0 {
        log::info!("vehicles: {files} files packed at their smallest - {:.1} MB go as {:.1} MB ({:.0} s)", raw as f64 / 1e6, wire as f64 / 1e6, t0.elapsed().as_secs_f64());
    }
}

/// Every piece of the kept file `sha` packed at its smallest (`pieces::ZSTD_BEST`) and kept
/// so: (its bytes, what they take on the wire). None when it is not kept (any more).
fn recompress(sha: &str) -> Option<(u64, u64)> {
    let data = blob(sha)?;
    let size = data.len() as u64;
    let mut wire = 0u64;
    for i in 0..pieces::count(size) {
        let (start, len) = pieces::span(size, i);
        let piece = &data[start as usize..(start + len) as usize];
        // (the slowest is not the smallest every time: the smaller of the two is kept)
        let (codec, packed) = [pieces::ZSTD_BEST, pieces::ZSTD_FAST].map(|level| pieces::pack_zstd(piece, level)).into_iter().min_by_key(|(_, p)| p.len()).unwrap_or(('-', Vec::new()));
        wire += packed.len() as u64;
        // (kept with the store held: a file let go meanwhile leaves no packed pieces behind)
        let g = lock();
        let v = g.as_ref()?;
        if !v.sizes.contains_key(sha) {
            v.store.remove_packed(sha, i);
            return None;
        }
        // (one that does not get smaller is kept as a mark: it goes as it is)
        let kept = if codec == '-' { v.store.put_packed(sha, i, '-', &[]) } else { v.store.put_packed(sha, i, codec, &packed) };
        if let Err(e) = kept {
            log::warn!("vehicles: a packed piece not kept: {e}");
            return None;
        }
    }
    Some((size, wire))
}

/// The pieces of `e` not here (`pieces`): none when the file is kept whole.
pub(super) fn missing_pieces(e: &Entry) -> Vec<u64> {
    let g = lock();
    let Some(v) = g.as_ref() else { return (0..pieces::count(e.size)).collect() };
    if v.store.has(e).is_some() || v.withheld.contains_key(&held_key(e)) {
        return Vec::new();
    }
    (0..pieces::count(e.size)).filter(|i| !v.store.has_piece(&e.sha256, *i, pieces::span(e.size, *i).1)).collect()
}

/// Keep piece `i` of `e` as it came; once every piece is here the file is checked and kept
/// whole (`finish`). True when it is.
pub(super) fn put_piece(e: &Entry, i: u64, data: &[u8]) -> Result<bool, String> {
    put_piece_making_room(e, i, data, IN_USE)
}

/// `put_piece` for a piece a player sent: room only of buses unused for a day (`put_sent`).
pub(super) fn put_piece_sent(e: &Entry, i: u64, data: &[u8]) -> Result<bool, String> {
    put_piece_making_room(e, i, data, SENT_MAKES_ROOM_OF)
}

fn put_piece_making_room(e: &Entry, i: u64, data: &[u8], unused_for: std::time::Duration) -> Result<bool, String> {
    if i >= pieces::count(e.size) || data.len() as u64 != pieces::span(e.size, i).1 {
        return Err(format!("{}: not a piece of it", e.path));
    }
    // (a piece takes room as a file does: pieces of files never finished filled the disk)
    make_room(data.len() as u64, unused_for)?;
    let store = lock().as_ref().ok_or("no store")?.store.clone();
    store.put_piece(&e.sha256, i, data).map_err(|x| format!("{}: {x}", e.path))?;
    finish(e, unused_for)
}

/// One file at a time is put together from its pieces.
static FINISHING: Mutex<()> = Mutex::new(());

/// `e` put together from its pieces, if they are all here: checked (its SHA-256, no
/// program, what it says it is), kept whole, its pieces gone. True when it is kept.
fn finish(e: &Entry, unused_for: std::time::Duration) -> Result<bool, String> {
    let _one = FINISHING.lock().unwrap_or_else(|x| x.into_inner());
    let count = pieces::count(e.size);
    // (read with the store let go: a file of a GB held every other request up)
    let store = lock().as_ref().ok_or("no store")?.store.clone();
    let data = {
        if store.has(e).is_some() {
            return Ok(true);
        }
        if (0..count).any(|i| !store.has_piece(&e.sha256, i, pieces::span(e.size, i).1)) {
            return Ok(false);
        }
        let mut data = Vec::with_capacity(e.size as usize);
        for i in 0..count {
            data.extend(store.piece(&e.sha256, i, pieces::span(e.size, i).1).map_err(|x| format!("{}: {x}", e.path))?);
        }
        data
    };
    let drop_pieces = || {
        if let Some(v) = lock().as_ref() {
            v.store.remove_pieces(&e.sha256, count);
        }
    };
    if super::sha256_of(&data) != e.sha256 {
        drop_pieces();
        return Err(format!("{}: not the file the list names", e.path));
    }
    // (refused for what it is: the bus goes on without it - `withhold`)
    let refused = if super::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { super::quarantine::check(&e.path, &data).err() };
    if let Some(why) = refused {
        drop_pieces();
        withhold(e, &why);
        return Err(why);
    }
    put_making_room(e, &data, unused_for)?;
    drop_pieces();
    Ok(true)
}

/// A bus all of whose files are here becomes one of the kept ones: its fingerprint, worked
/// out here from what was checked here (never the one its sender says).
pub(super) fn commit(m: VehicleManifest) -> Result<(String, bool), String> {
    let m = m.validated()?;
    let fp = m.fingerprint();
    let mut g = lock();
    let v = g.as_mut().ok_or("no store")?;
    if v.manifests.iter().any(|(f, _)| *f == fp) {
        return Ok((fp, false));
    }
    // (a file refused for what it is is left out - never the bus file itself: without it
    // there is no bus)
    let bus_key = identity::key(&m.bus);
    let mut held = Vec::new();
    for e in m.entries.iter().filter(|e| v.store.has(e).is_none()) {
        match v.withheld.get(&held_key(e)) {
            Some(why) if identity::key(&e.path) != bus_key => held.push(Withheld { path: e.path.clone(), why: why.clone() }),
            _ => return Err(format!("{} has not come", e.path)),
        }
    }
    if !held.is_empty() {
        let file = v.dir.join(format!("{fp}.withheld.json"));
        std::fs::write(&file, serde_json::to_vec(&held).unwrap_or_default()).map_err(|e| format!("{}: {e}", file.display()))?;
        log::warn!("vehicles: {} is kept without {} of its files ({})", m.bus, held.len(), held.iter().take(4).map(|w| w.why.as_str()).collect::<Vec<_>>().join("; "));
        v.withheld_of.insert(fp.clone(), held);
    }
    // (what is served by its SHA-256 is known by the kept lists after a restart: a file kept
    // before one - for a bus never finished then - was in the store but not served, and the
    // bus kept now that has it was asked for in vain: "no such file")
    for e in &m.entries {
        v.sizes.insert(e.sha256.to_ascii_lowercase(), e.size);
    }
    let file = v.dir.join(format!("{fp}.json"));
    let tmp = file.with_extension("part");
    std::fs::write(&tmp, serde_json::to_vec(&m).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, &file)).map_err(|e| format!("{}: {e}", file.display()))?;
    v.manifests.push((fp.clone(), m));
    drop(g);
    // (whoever waits for a bus hears of it at once: `wait_for`)
    let (count, arrived) = &ARRIVED;
    *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    arrived.notify_all();
    Ok((fp, true))
}

/// A bus has been kept (`commit`): a count, and the waiters on it.
static ARRIVED: (Mutex<u64>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

/// The manifest of `fp`, waiting for it up to `wait` when it is not kept yet - a bus asked
/// for by another player is most likely on its way here from its own (it sends it under its
/// loading screen): the answer goes the moment it is kept, not when the asker asks again.
///
/// Only a bus on its way is waited for (`on_its_way`): one a player of the session
/// announces, or one being sent. A player who announced a new made-up fingerprint every
/// second had every other game wait 90 s for each, a connection held here for each.
pub(super) fn wait_for(fp: &str, wait: std::time::Duration) -> Option<VehicleManifest> {
    let t0 = std::time::Instant::now();
    let until = t0 + wait;
    let (count, arrived) = &ARRIVED;
    let mut seen = count.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(m) = manifest(fp) {
            return Some(m);
        }
        // (a moment's grace: the server says its players' buses every second, and a game
        // may ask for one it heard of a moment before that)
        if !on_its_way(fp) && t0.elapsed() >= ANNOUNCE_GRACE {
            return None;
        }
        let left = until.checked_duration_since(std::time::Instant::now())?;
        seen = arrived.wait_timeout(seen, left.min(std::time::Duration::from_secs(1))).unwrap_or_else(|e| e.into_inner()).0;
    }
}

const ANNOUNCE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The fingerprints the session's players announce now (`announce`); None before the first.
static ANNOUNCED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// A server's game, every second: the buses its players announce.
pub(crate) fn announce<'a>(fps: impl Iterator<Item = &'a str>) {
    *ANNOUNCED.lock().unwrap_or_else(|e| e.into_inner()) = Some(fps.map(str::to_string).collect());
}

/// Is the bus `fp` on its way here: announced by a player of the session, or being sent
/// (`lease`)? (Before any player was announced - no game of a session - every one is.)
fn on_its_way(fp: &str) -> bool {
    leased(fp) || ANNOUNCED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|a| a.contains(fp))
}

/// Where the bus `fp` can be read: its files under `lan-vehicles/<fp>`, mounted once.
pub(crate) fn mount(fp: &str) -> Option<PathBuf> {
    if !omsi_net::is_fingerprint(fp) {
        return None;
    }
    let base = mounts()?;
    let at = base.join(fp);
    let mut g = lock();
    let v = g.as_mut()?;
    if v.mounted.contains(fp) {
        return Some(at);
    }
    let m = &v.manifests.iter().find(|(f, _)| f == fp)?.1;
    let files: Vec<_> = m.entries.iter().filter_map(|e| v.store.has(e)).collect();
    // (every file of it, but those it is kept without)
    let held = v.withheld_of.get(fp).map(Vec::len).unwrap_or(0);
    if files.len() + held != m.entries.len() {
        return None;
    }
    omsi_cfg::vfs::mount_sealed(&at, v.store.key(), files);
    v.mounted.insert(fp.to_string());
    drop(g);
    // (what the folder listings knew of that place is old now)
    omsi_cfg::content_changed();
    Some(at)
}

/// The bus file of `fp` where it is mounted.
pub(crate) fn bus_file(fp: &str) -> Option<PathBuf> {
    let m = manifest(fp)?;
    let at = mount(fp)?;
    let rel = m.entries.iter().find(|e| identity::key(&e.path) == identity::key(&m.bus))?.path.clone();
    Some(rel.split('/').fold(at, |p, c| p.join(c)))
}

// -------------------------------------------------------------------------------------
// a player's game: another player's bus, by its fingerprint

/// Where another player's bus stands on this machine.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Wanted {
    /// Being made sure of: (bytes here, bytes in all) once a download is under way.
    Pending(u64, u64),
    /// This machine's own copy is that bus, file for file: loaded from its own place.
    Local(PathBuf),
    /// Fetched by its content: loaded from its mount.
    Mounted(PathBuf),
    /// Neither here nor to be had (why); tried again after a while.
    Failed(String),
}

/// By fingerprint: where it stands, since when, and how many times running it could not be had.
static WANTED: Mutex<Option<HashMap<String, (Wanted, std::time::Instant, u32)>>> = Mutex::new(None);

/// A bus that could not be had this many times running (for another reason than not being
/// on the server yet) is not asked for again this session: a bad list or file is not fetched
/// over and over.
const GIVE_UP: u32 = 3;
const RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// Is a bus that could not be had (`why`, `since` how long, `fails` times running) to be
/// asked for again now?
fn may_retry(why: &str, since: std::time::Duration, fails: u32) -> bool {
    // (a bus on its way, or a host not reached - never given up: what can come, comes)
    let waiting = coming(why);
    (waiting || fails < GIVE_UP) && since >= retry_after(why)
}

/// Is a bus that could not be had for `why` still to come (asked for again, never given up)?
pub(crate) fn coming(why: &str) -> bool {
    why.contains("no such bus") || why.contains(NOT_DRIVEN) || unreached(why)
}

/// Did asking fail before the host could answer (no way to it yet, or the connection)?
fn unreached(why: &str) -> bool {
    why.contains("no host to ask") || why.contains("cannot reach") || why.contains(super::CONNECTION_LOST) || why.ends_with("the host says: busy")
}

/// Is the bus `fp` here to be drawn (this machine's own copy, or fetched and mounted)? A
/// player's box is made again then: the mount made it again a moment before the fetch said
/// so, as a box still, and nothing else did - the box stayed with the bus right here.
pub(crate) fn ready(fp: &str) -> bool {
    matches!(wanted_map().as_ref().and_then(|m| m.get(fp)), Some((Wanted::Local(_) | Wanted::Mounted(_), _, _)))
}

/// Is the bus `fp`, which could not be had, to be asked for again now? (A player's box is
/// made again then: else only another bus coming asked for it once more.)
pub(crate) fn retry_due(fp: &str) -> bool {
    matches!(wanted_map().as_ref().and_then(|m| m.get(fp)), Some((Wanted::Failed(why), at, fails)) if may_retry(why, at.elapsed(), *fails))
}

/// How long a bus that could not be had waits before it is asked for again: one the host
/// did not have even after waiting for it (`upload::MANIFEST_WAIT`) - asked again soon, the
/// host waiting again; anything else - in a minute.
fn retry_after(why: &str) -> std::time::Duration {
    if why.contains("no such bus") || why.contains("no host to ask") || why.contains(NOT_DRIVEN) {
        std::time::Duration::from_secs(5)
    } else if unreached(why) {
        std::time::Duration::from_secs(15)
    } else {
        RETRY
    }
}

fn wanted_map() -> std::sync::MutexGuard<'static, Option<HashMap<String, (Wanted, std::time::Instant, u32)>>> {
    WANTED.lock().unwrap_or_else(|e| e.into_inner())
}

fn set(fp: &str, w: Wanted) {
    let ready = matches!(w, Wanted::Local(_) | Wanted::Mounted(_));
    let mut g = wanted_map();
    let map = g.get_or_insert_with(HashMap::new);
    let before = map.get(fp).map(|e| e.2).unwrap_or(0);
    let fails = match &w {
        Wanted::Failed(_) => before + 1,
        Wanted::Pending(..) => before,
        _ => 0,
    };
    map.insert(fp.to_string(), (w, std::time::Instant::now(), fails));
    // (buses that could not be had, long ago: a player who made fingerprints up left one
    // behind for each)
    if map.len() > 1024 {
        map.retain(|_, (w, at, _)| !matches!(w, Wanted::Failed(_)) || at.elapsed() < std::time::Duration::from_secs(600));
    }
    drop(g);
    // (a stand-in shown for it is made again: see `lan::tick`)
    if ready {
        omsi_cfg::content_changed();
    }
}

/// Downloads of buses under way at once, at most.
const DOWNLOADS: usize = 3;

/// What a bus nobody here drives any more says (asked for again as soon as one does).
const NOT_DRIVEN: &str = "no longer driven here";

/// The bus each other player drives (by player): its fingerprint, and when it changed lately.
type Drivers = HashMap<u32, (String, VecDeque<std::time::Instant>)>;
static DRIVERS: Mutex<Option<Drivers>> = Mutex::new(None);

/// A player changes bus or paint this many times within `CHANGES_WITHIN` at most.
const CHANGES: usize = 6;
const CHANGES_WITHIN: std::time::Duration = std::time::Duration::from_secs(300);

/// `player` drives the bus `fp`: false when the bus has changed too often of late.
fn driven_by(player: u32, fp: &str) -> bool {
    let mut g = DRIVERS.lock().unwrap_or_else(|e| e.into_inner());
    let (now, (was, changes)) = (std::time::Instant::now(), g.get_or_insert_with(HashMap::new).entry(player).or_insert_with(|| (fp.to_string(), VecDeque::new())));
    while changes.front().is_some_and(|t| now.duration_since(*t) > CHANGES_WITHIN) {
        changes.pop_front();
    }
    if was.as_str() != fp {
        *was = fp.to_string();
        changes.push_back(now);
    }
    changes.len() <= CHANGES
}

/// Does some player here drive the bus `fp`? (Each one asked for by `want`.)
fn driven(fp: &str) -> bool {
    DRIVERS.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_none_or(|d| d.values().any(|(f, _)| f == fp))
}
static SLOTS: (Mutex<usize>, std::sync::Condvar) = (Mutex::new(0), std::sync::Condvar::new());

/// One of the `DOWNLOADS` places, held while a bus is fetched.
struct Slot;

impl Slot {
    fn take() -> Slot {
        let (n, freed) = &SLOTS;
        let mut busy = n.lock().unwrap_or_else(|e| e.into_inner());
        while *busy >= DOWNLOADS {
            busy = freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        *busy += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (n, freed) = &SLOTS;
        *n.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        freed.notify_one();
    }
}

/// Where the bus `fp` stands, as far as it has been asked for (nothing is started).
pub(crate) fn status(fp: &str) -> Option<Wanted> {
    wanted_map().as_ref()?.get(fp).map(|(w, _, _)| w.clone())
}

/// What loading a fetched bus read from outside its own files (`reads`, see
/// `omsi_cfg::vfs::Audit`): the game's own variable lists aside, every such file is one this
/// player may have in another version than its driver - a way in for a bus that is not quite
/// the same. Logged, once for each bus.
pub(crate) fn audit(fp: &str, root: &Path, reads: &[PathBuf]) -> Vec<PathBuf> {
    if !matches!(status(fp), Some(Wanted::Mounted(_))) {
        return Vec::new();
    }
    let Some(at) = mounts().map(|m| m.join(fp)) else { return Vec::new() };
    let out = escapes(&at, root, reads);
    static TOLD: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if !out.is_empty() && TOLD.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashSet::new).insert(fp.to_string()) {
        let some: Vec<String> = out.iter().take(10).map(|p| p.display().to_string()).collect();
        log::warn!("vehicles: {fp} read {} files from outside its own while it loaded (they may differ from its driver's): {}", out.len(), some.join(", "));
    }
    out
}

/// The files of `reads` outside the bus's mount `at` - but for the game's own program files.
fn escapes(at: &Path, root: &Path, reads: &[PathBuf]) -> Vec<PathBuf> {
    let program = root.join("program");
    let mut out: Vec<PathBuf> = reads.iter().filter(|p| !p.starts_with(at) && !p.starts_with(&program)).cloned().collect();
    out.sort();
    out.dedup();
    out
}

/// The bus with fingerprint `fp` that a player announces as `bus` in `paint`: where it is to
/// be loaded from, once that is known for sure - this machine's own copy when it is the same
/// bus file for file, else the bus fetched from the host by its content. Until then (and when
/// it cannot be had) the state says how far it is; the work goes on in the background.
///
/// `player` is who drives it: a player whose bus changes more often than a player changes
/// bus (`driven_by`) is drawn as a box - each made-up fingerprint started a download here.
pub(crate) fn want(root: &Path, player: u32, fp: &str, bus: &str, paint: &str) -> Wanted {
    if !omsi_net::is_fingerprint(fp) {
        return Wanted::Failed("no fingerprint".into());
    }
    if !driven_by(player, fp) {
        return Wanted::Failed("its bus changes too often".into());
    }
    {
        let mut g = wanted_map();
        let map = g.get_or_insert_with(HashMap::new);
        match map.get(fp) {
            Some((Wanted::Failed(why), at, fails)) if !may_retry(why, at.elapsed(), *fails) => return map[fp].0.clone(),
            Some((Wanted::Failed(_), _, fails)) => {
                let fails = *fails;
                map.insert(fp.to_string(), (Wanted::Pending(0, 0), std::time::Instant::now(), fails));
            }
            None => {
                map.insert(fp.to_string(), (Wanted::Pending(0, 0), std::time::Instant::now(), 0));
            }
            Some((w, _, _)) => return w.clone(),
        }
    }
    let (root, key, bus, paint) = (root.to_path_buf(), fp.to_string(), bus.to_string(), paint.to_string());
    let fp = key.clone();
    let spawned = std::thread::Builder::new().name("bus-by-content".into()).spawn(move || {
        let w = make_sure(&root, &fp, &bus, &paint);
        match &w {
            Wanted::Local(_) => log::info!("vehicles: {bus} ({paint}) is the same here ({fp})"),
            Wanted::Mounted(_) => log::info!("vehicles: {bus} ({paint}) fetched by its content ({fp})"),
            Wanted::Failed(e) => log::warn!("vehicles: {bus} ({paint}) as {fp}: {e}"),
            Wanted::Pending(..) => {}
        }
        set(&fp, w);
    });
    if spawned.is_err() {
        set(&key, Wanted::Failed("no thread".into()));
    }
    Wanted::Pending(0, 0)
}

fn make_sure(root: &Path, fp: &str, bus: &str, paint: &str) -> Wanted {
    // kept here already (fetched before, or sent by this machine's own player)
    if !is_open() && !open() {
        return Wanted::Failed("no store".into());
    }
    if let Some(p) = bus_file(fp) {
        return Wanted::Mounted(p);
    }
    // this machine's own copy, if it is that bus file for file
    match local_copy(root, fp, bus, paint) {
        Ok(path) => return Wanted::Local(path),
        Err(why) => log::info!("vehicles: {bus} ({paint}): {why}; asking the host for it by its content"),
    }
    // the host itself (a server): nobody to ask - its player is most likely still sending it
    // here; waited for as a joining game's question is (`wait_for`)
    if super::upload::host().is_none() {
        // (a joining game whose way to the host is still being found asks once it is: the
        // store is open on every game, only the server waits here)
        if !super::upload::serving() {
            return Wanted::Failed("no host to ask yet".into());
        }
        return match wait_for(fp, super::upload::MANIFEST_WAIT).and_then(|_| bus_file(fp)) {
            Some(p) => Wanted::Mounted(p),
            None => Wanted::Failed("no such bus here yet".into()),
        };
    }
    match fetch(fp) {
        Ok(()) => bus_file(fp).map(Wanted::Mounted).unwrap_or_else(|| Wanted::Failed("fetched, but not to be mounted".into())),
        Err(e) => Wanted::Failed(e),
    }
}

/// This machine's copy of `bus`, if its files in `paint` - listed as its driver's game lists
/// them (`bus_files`) - are the bus `fp`.
fn local_copy(root: &Path, fp: &str, bus: &str, paint: &str) -> Result<PathBuf, String> {
    let rel = omsi_net::vehicle_path(bus).ok_or("no vehicle path")?;
    let path = omsi_cfg::find_in_roots(&rel).map(|(_, p)| p).ok_or("not installed here")?;
    let ty = omsi_sim::VehicleType::load(root, &path).map_err(|e| e.to_string())?;
    // (the paint by its name; a player who picked none may be in the type's own look)
    let named = ty.paint_schemes.iter().position(|s| s.name.eq_ignore_ascii_case(paint));
    let mut tries = vec![named];
    if named.is_some() {
        tries.push(None);
    }    for scheme in tries {
        let files = super::upload::listed(root, super::bus_files::of_type(root, &ty, scheme, None));
        let m = VehicleManifest { bus: rel.clone(), entries: super::upload::hashed(&files) };
        if m.validated().is_ok_and(|m| m.fingerprint() == fp) {
            return Ok(path);
        }
    }
    Err("the copy here is another version".into())
}

/// The bus `fp` from the host: its list (checked to be that bus), and each of its files this
/// machine does not keep - taken from its own copy where that is the same file, else fetched.
fn fetch(fp: &str) -> Result<(), String> {
    let (host, session) = super::upload::host().ok_or("no host to ask")?;
    // (a few buses at a time: the nearest players first come first, the line is not choked)
    let _slot = Slot::take();
    // (the player may have gone, or changed bus, while this waited for its turn)
    if !driven(fp) {
        return Err(NOT_DRIVEN.into());
    }
    // (the host waits for a bus on its way before it answers: the answer was given up at
    // 25 s while the host still waited for it)
    let (raw, mut out, mut input) = super::open_waiting(host, session, super::upload::MANIFEST_WAIT + std::time::Duration::from_secs(30))?;
    writeln!(out, "MANIFEST {fp}").and_then(|_| out.flush()).map_err(|e| e.to_string())?;
    let len = super::read_reply(&mut input)?.ok_or("no list")?;
    if len > 256 << 20 {
        return Err("the list is too large".into());
    }
    let mut body = vec![0u8; len as usize];
    input.read_exact(&mut body).map_err(|e| e.to_string())?;
    let m: VehicleManifest = serde_json::from_slice(&body).map_err(|e| format!("bad list: {e}"))?;
    let m = m.validated()?;
    // (the list must be the bus asked for: the host could not pass off another one)
    if m.fingerprint() != fp {
        return Err("the host's list is another bus".into());
    }
    if let Some(bad) = m.entries.iter().find(|e| super::refuse_path(&e.path).is_some() || e.size > super::MAX_FILE) {
        return Err(format!("{}: refused", bad.path));
    }
    // the files the host keeps it without (refused there for what they are): not asked for,
    // and never taken from anywhere else - the bus is drawn without them
    let held = serde_json::from_slice::<HeldBack>(&body).unwrap_or_default().withheld;
    for w in &held {
        if let Some(e) = m.entries.iter().find(|e| e.path == w.path).filter(|e| identity::key(&e.path) != identity::key(&m.bus)) {
            withhold(e, &format!("{} (on the host)", w.why));
        }
    }
    let missing = missing(&m);
    let total: u64 = missing.iter().map(|k| m.entries[*k].size).sum();
    set(fp, Wanted::Pending(0, total));
    let mut done = 0u64;
    // what this machine has itself, in the same version
    let mut todo = Vec::new();
    for k in missing {
        let e = &m.entries[k];
        match own_copy(e) {
            Some(d) => {
                put(e, &d)?;
                done += e.size;
            }
            None => todo.push(k),
        }
    }
    let _ = writeln!(out, "BYE").and_then(|_| out.flush());
    let _ = raw.shutdown(std::net::Shutdown::Both);
    // the rest from the host, in pieces over several connections; a host of an older
    // version, file by file
    let progress = Progress::new(fp, done, total);
    match fetch_pieces(&m, &todo, host, session, &progress) {
        Err(e) if e == OLDER_HOST => fetch_blobs(&m, &todo, host, session, &progress)?,
        r => r?,
    }
    commit(m)?;
    if total > 0 {
        super::upload::show(format!("Another player's bus is here ({}): it is shown now", super::upload::mb(total)), 1, 1, false);
    }
    Ok(())
}

/// How far a bus being fetched has come, told to the box that stands for it and to the
/// player (every connection adds to it).
struct Progress<'a> {
    fp: &'a str,
    done: std::sync::atomic::AtomicU64,
    total: u64,
    /// What it took on the wire (pieces packed).
    wire: std::sync::atomic::AtomicU64,
    said: Mutex<std::time::Instant>,
}

impl<'a> Progress<'a> {
    fn new(fp: &'a str, done: u64, total: u64) -> Self {
        Progress { fp, done: done.into(), total, wire: 0.into(), said: Mutex::new(std::time::Instant::now()) }
    }

    fn add(&self, n: u64) {
        let done = self.done.fetch_add(n, std::sync::atomic::Ordering::Relaxed) + n;
        let mut said = self.said.lock().unwrap_or_else(|e| e.into_inner());
        if said.elapsed() > std::time::Duration::from_millis(250) {
            *said = std::time::Instant::now();
            set(self.fp, Wanted::Pending(done, self.total));
            let pct = done * 100 / self.total.max(1);
            super::upload::show(format!("Downloading another player's bus: {pct}% ({} of {})", super::upload::mb(done), super::upload::mb(self.total)), done, self.total, true);
        }
    }
}

/// What a host of a version before the pieces answers to them.
const OLDER_HOST: &str = "the host sends whole files only";

/// The files `todo` of `m` from the host in pieces (`pieces`), several connections at once:
/// each piece kept as it comes - what came before, in an attempt that broke off, is not
/// asked for again - and each file checked and kept once it is whole.
fn fetch_pieces(m: &VehicleManifest, todo: &[usize], host: std::net::SocketAddr, session: u64, progress: &Progress) -> Result<(), String> {
    let mut items = Vec::new();
    let mut asked = HashSet::new();
    for k in todo {
        let e = &m.entries[*k];
        let left = missing_pieces(e);
        let had: u64 = (0..pieces::count(e.size)).filter(|i| !left.contains(i)).map(|i| pieces::span(e.size, i).1).sum();
        if had > 0 {
            progress.add(had);
        }
        // (a file whose every piece came before is put together now)
        if left.is_empty() {
            finish(e, IN_USE)?;
        }
        // (a file the bus has twice - the same texture in two folders - is fetched once: its
        // pieces are kept by their content, so the other is whole with it)
        for i in left {
            if asked.insert((e.sha256.clone(), i)) {
                items.push((*k, i));
            } else {
                progress.add(pieces::span(e.size, i).1);
            }
        }
    }
    if !items.is_empty() {
        log::info!("vehicles: {} pieces of {} files to fetch over up to {} connections", items.len(), todo.len(), pieces::CONNECTIONS);
    }
    let bytes: u64 = items.iter().map(|(k, i)| pieces::span(m.entries[*k].size, *i).1).sum();
    let t0 = std::time::Instant::now();
    let r = pieces::share_out(items, pieces::CONNECTIONS, |take| {
        let (raw, mut out, mut input) = super::open(host, session)?;
        // (we read pieces packed with zstd: an older host answers `ERR what` and packs them
        // as before - the answer is read ahead of the first pieces, no wait for it)
        let mut asked_zstd = writeln!(out, "ZSTD").is_ok();
        let r = (|| loop {
            let batch = take();
            if batch.is_empty() {
                return Ok(());
            }
            for (k, i) in &batch {
                writeln!(out, "PIECE {} {i}", m.entries[*k].sha256).map_err(|e| format!("{}: {e}", super::CONNECTION_LOST))?;
            }
            out.flush().map_err(|e| format!("{}: {e}", super::CONNECTION_LOST))?;
            if std::mem::take(&mut asked_zstd) {
                let mut line = String::new();
                super::read_line_within(&mut input, &mut line, super::MAX_REPLY).map_err(|e| format!("{}: {e}", super::CONNECTION_LOST))?;
            }
            for (k, i) in &batch {
                let e = &m.entries[*k];
                let len = pieces::span(e.size, *i).1;
                let (data, wire) = pieces::receive(&mut input, len).map_err(|x| if x.ends_with("says: what") { OLDER_HOST.to_string() } else { format!("{}: {x}", e.path) })?;
                match put_piece(e, *i, &data) {
                    // (refused here for what it is: the bus is kept without it - `withhold`)
                    Err(why) if is_withheld(e) => log::warn!("vehicles: {why}; the bus is drawn without it"),
                    r => {
                        r?;
                    }
                }
                progress.wire.fetch_add(wire, std::sync::atomic::Ordering::Relaxed);
                progress.add(len);
            }
        })();
        let _ = writeln!(out, "BYE").and_then(|_| out.flush());
        let _ = raw.shutdown(std::net::Shutdown::Both);
        r
    });
    if r.is_ok() && bytes > 0 {
        let wire = progress.wire.load(std::sync::atomic::Ordering::Relaxed);
        let secs = t0.elapsed().as_secs_f64();
        log::info!("vehicles: {:.1} MB fetched as {:.1} MB on the wire ({:.0}%) in {secs:.1} s", bytes as f64 / 1e6, wire as f64 / 1e6, wire as f64 * 100.0 / bytes as f64);
    }
    r
}

/// The files `todo` of `m` from a host of an older version: whole, one connection, every
/// request at once and the answers as they come.
fn fetch_blobs(m: &VehicleManifest, todo: &[usize], host: std::net::SocketAddr, session: u64, progress: &Progress) -> Result<(), String> {
    let (raw, mut out, mut input) = super::open(host, session)?;
    let todo: Vec<usize> = todo.iter().copied().filter(|k| bus_store_lacks(&m.entries[*k])).collect();
    let requests: String = todo.iter().map(|k| format!("BLOB {}\n", m.entries[*k].sha256)).chain(std::iter::once("BYE\n".to_string())).collect();
    let writer = std::thread::spawn(move || {
        let _ = out.write_all(requests.as_bytes()).and_then(|_| out.flush());
    });
    for k in &todo {
        let e = &m.entries[*k];
        let len = super::read_reply(&mut input)?.ok_or("no size")?;
        if len != e.size {
            return Err(format!("{}: {len} bytes, the list says {}", e.path, e.size));
        }
        let mut data = vec![0u8; len as usize];
        input.read_exact(&mut data).map_err(|x| format!("{}: {x}", e.path))?;
        if super::sha256_of(&data) != e.sha256 {
            return Err(format!("{}: not the file the list names", e.path));
        }
        // (refused for what it is: the bus is kept without it - `withhold`)
        let refused = if super::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { super::quarantine::check(&e.path, &data).err() };
        match refused {
            Some(why) => {
                log::warn!("vehicles: {why}; the bus is drawn without it");
                withhold(e, &why);
            }
            None => put(e, &data)?,
        }
        progress.add(e.size);
    }
    let _ = raw.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    Ok(())
}

/// `e` is not kept whole here.
fn bus_store_lacks(e: &Entry) -> bool {
    lock().as_ref().is_none_or(|v| v.store.has(e).is_none())
}

// -------------------------------------------------------------------------------------
// keeping the store within the disk: buses nobody uses go (no size is ever refused)

/// Room left free on the disk after a file is kept.
const KEEP_FREE: u64 = 3 << 30;
/// A bus a connected player drives counts as in use for this long after it was seen.
const IN_USE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// What a player sends makes room only of buses nobody has used for this long: a player
/// sending big buses of no use pushed the buses played that day out, one after the other.
const SENT_MAKES_ROOM_OF: std::time::Duration = std::time::Duration::from_secs(86_400);
/// Pieces of a file left unfinished this long go (its player gone): they took room nothing
/// counted, kept as long as buses were.
const PIECES_KEEP: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// How long a bus nobody has used is kept (`uploads_keep_days` of `server.cfg`).
static KEEP: Mutex<std::time::Duration> = Mutex::new(std::time::Duration::from_secs(30 * 86_400));

pub fn keep_for_days(days: u64) {
    *KEEP.lock().unwrap_or_else(|e| e.into_inner()) = std::time::Duration::from_secs(days.max(1) * 86_400);
}

/// Files an upload under way holds (by SHA-256, how many uploads): a collection passes them by.
static LEASES: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);

/// A bus being sent - its fingerprint and its files - held from its offer to the end of its
/// connection: neither its list nor its files go meanwhile (another bus that shares a file
/// with it may go; the file stays).
pub(super) struct Lease(Vec<String>);

pub(super) fn lease(m: &VehicleManifest) -> Lease {
    let shas: Vec<String> = m.entries.iter().map(|e| e.sha256.to_ascii_lowercase()).chain(std::iter::once(m.fingerprint())).collect();
    let mut g = LEASES.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    for s in &shas {
        *map.entry(s.clone()).or_insert(0) += 1;
    }
    Lease(shas)
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut g = LEASES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(map) = g.as_mut() {
            for s in &self.0 {
                if let Some(n) = map.get_mut(s) {
                    *n -= 1;
                    if *n == 0 {
                        map.remove(s);
                    }
                }
            }
        }
    }
}

fn leased(sha: &str) -> bool {
    LEASES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.contains_key(sha))
}

/// When the bus `fp` was last used: its list's time (touched by `in_use`).
fn last_used(v: &Vehicles, fp: &str) -> std::time::SystemTime {
    std::fs::metadata(v.dir.join(format!("{fp}.json"))).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH)
}

/// The buses the connected players drive (their fingerprints): in use now.
pub(crate) fn in_use<'a>(fps: impl Iterator<Item = &'a str>) {
    let g = lock();
    let Some(v) = g.as_ref() else { return };
    for fp in fps {
        if v.manifests.iter().any(|(f, _)| f == fp) {
            let file = v.dir.join(format!("{fp}.json"));
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&file) {
                let _ = f.set_modified(std::time::SystemTime::now());
            }
        }
    }
}

/// What a collection did.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Collected {
    pub buses: usize,
    pub files: usize,
    pub bytes: u64,
}

/// Let go of the buses nobody has used for longer than they are kept - and, when `need`
/// bytes must be freed, of more, the least lately used first, of those unused for `unused_for`
/// at least (never one in use now) - never a file an upload under way holds. A bus's list goes before its files (a collection broken off
/// leaves no list without its files); a file goes only when no bus kept uses it.
pub(super) fn collect(need: u64, unused_for: std::time::Duration) -> Collected {
    let keep = *KEEP.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::SystemTime::now();
    let mut g = lock();
    let Some(v) = g.as_mut() else { return Collected::default() };
    // (pieces of a file left unfinished that long - its player gone - go too)
    let stale = v.store.remove_stale_pieces(keep.min(PIECES_KEEP));
    if stale > 0 {
        log::info!("vehicles: {stale} pieces of files never finished removed");
    }
    let mut by_age: Vec<(std::time::SystemTime, String)> = v.manifests.iter().map(|(f, _)| (last_used(v, f), f.clone())).collect();
    by_age.sort();
    let age = |t: std::time::SystemTime| now.duration_since(t).unwrap_or_default();
    let mut gone: Vec<VehicleManifest> = Vec::new();
    let mut freed = 0u64;
    for (t, fp) in by_age {
        let expired = age(t) > keep;
        let wanted_room = need > 0 && freed < need && age(t) > unused_for.max(IN_USE);
        if !(expired || wanted_room) {
            continue;
        }
        let Some(k) = v.manifests.iter().position(|(f, _)| *f == fp) else { continue };
        if leased(&fp) {
            continue;
        }
        let (_, m) = v.manifests.remove(k);
        let _ = std::fs::remove_file(v.dir.join(format!("{fp}.json")));
        if v.withheld_of.remove(&fp).is_some() {
            let _ = std::fs::remove_file(v.dir.join(format!("{fp}.withheld.json")));
        }
        if v.mounted.remove(&fp) {
            if let Some(base) = mounts() {
                omsi_cfg::vfs::unmount_sealed(&base.join(&fp));
            }
        }
        freed += m.size();
        gone.push(m);
    }
    let kept: HashSet<String> = v.manifests.iter().flat_map(|(_, m)| m.entries.iter().map(|e| e.sha256.to_ascii_lowercase())).collect();
    let mut out = Collected { buses: gone.len(), ..Default::default() };
    let mut done: HashSet<String> = HashSet::new();
    for e in gone.iter().flat_map(|m| m.entries.iter()) {
        let sha = e.sha256.to_ascii_lowercase();
        if kept.contains(&sha) || leased(&sha) || !done.insert(sha.clone()) {
            continue;
        }
        if let Some(f) = v.store.has(e) {
            if std::fs::remove_file(&f.blob).is_ok() {
                v.store.remove_packed(&sha, pieces::count(e.size));
                v.sizes.remove(&sha);
                out.files += 1;
                out.bytes += e.size;
            }
        }
    }
    if out.buses > 0 {
        log::info!("vehicles: {} buses nobody uses let go - {} files, {:.1} MB", out.buses, out.files, out.bytes as f64 / 1e6);
    }
    out
}

/// Room on the disk for a file of `size`: the buses nobody has used for `unused_for` go
/// until there is; an error only when even then the disk cannot hold it.
pub(super) fn make_room(size: u64, unused_for: std::time::Duration) -> Result<(), String> {
    let Some(dir) = lock().as_ref().map(|v| v.store.dir().to_path_buf()) else { return Ok(()) };
    let free = || super::store::free_space(&dir);
    let Some(have) = free() else { return Ok(()) };
    if have >= size + KEEP_FREE {
        return Ok(());
    }
    collect(size + KEEP_FREE - have, unused_for);
    match free() {
        Some(now) if now < size + KEEP_FREE => Err(format!("the server's disk is full ({:.1} GB free)", now as f64 / 1e9)),
        _ => Ok(()),
    }
}

#[cfg(test)]
pub(super) fn open_for_test(dir: &Path) {
    open_in(dir).unwrap();
}

/// The tests that open the store (one per process) take turns.
#[cfg(test)]
pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests;

/// A stock bus of an OMSI 2 installation (`OMSI_ROOT`), kept and mounted as a fetched bus is:
/// loading it reads nothing but its own files, its textures are found among them, and its
/// displays' fonts too.
#[cfg(test)]
mod mounted_stock_bus;
