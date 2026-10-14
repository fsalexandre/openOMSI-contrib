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
    /// Files refused for what they are (`quarantine`), by SHA-256: why. A bus is kept and
    /// drawn without them (`withhold`).
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
                    withheld.insert(e.sha256.clone(), w.why.clone());
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
    collect(0);
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
    m.entries.iter().enumerate().filter(|(_, e)| v.store.has(e).is_none() && !v.withheld.contains_key(&e.sha256)).map(|(k, _)| k).collect()
}

/// `e` was refused for what it is (`why`, `quarantine`): a bus is kept and drawn without it -
/// never with another file in its place - and it is not asked for again.
pub(super) fn withhold(e: &Entry, why: &str) {
    if let Some(v) = lock().as_mut() {
        v.withheld.insert(e.sha256.to_ascii_lowercase(), why.to_string());
    }
}

/// Was `e` refused for what it is (`withhold`)?
pub(super) fn is_withheld(e: &Entry) -> bool {
    lock().as_ref().is_some_and(|v| v.withheld.contains_key(&e.sha256.to_ascii_lowercase()))
}

/// The files the kept bus `fp` is without (`withhold`).
pub(crate) fn withheld_of(fp: &str) -> Vec<Withheld> {
    lock().as_ref().and_then(|v| v.withheld_of.get(fp).cloned()).unwrap_or_default()
}

/// The files of `m` not kept yet that this machine has itself in the same version (in its
/// own content or the installation, at the same path) are kept from there: nothing to send.
/// How many.
pub(super) fn adopt_local(m: &VehicleManifest) -> usize {
    let mut n = 0;
    for k in missing(m) {
        let e = &m.entries[k];
        let same = omsi_cfg::find_in_roots(&e.path)
            .filter(|(r, _)| !omsi_cfg::is_sandbox(r))
            .and_then(|(_, p)| omsi_cfg::vfs::read(&p).ok())
            .filter(|d| d.len() as u64 == e.size && super::sha256_of(d) == e.sha256);
        if let Some(d) = same {
            if put(e, &d).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Keep a file (checked against `e` already).
pub(super) fn put(e: &Entry, data: &[u8]) -> Result<(), String> {
    make_room(e.size)?;
    let mut g = lock();
    let v = g.as_mut().ok_or("no store")?;
    v.store.put(e, data).map_err(|x| x.to_string())?;
    v.sizes.insert(e.sha256.to_ascii_lowercase(), e.size);
    Ok(())
}

/// The file with that SHA-256, if it is kept here.
pub(super) fn blob(sha: &str) -> Option<Vec<u8>> {
    let sha = sha.to_ascii_lowercase();
    let g = lock();
    let v = g.as_ref()?;
    let size = *v.sizes.get(&sha)?;
    v.store.read(&Entry { path: String::new(), size, sha256: sha }).ok()
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
    if v.store.has(e).is_some() || v.withheld.contains_key(&e.sha256) {
        return Vec::new();
    }
    (0..pieces::count(e.size)).filter(|i| !v.store.has_piece(&e.sha256, *i, pieces::span(e.size, *i).1)).collect()
}

/// Keep piece `i` of `e` as it came; once every piece is here the file is checked and kept
/// whole (`finish`). True when it is.
pub(super) fn put_piece(e: &Entry, i: u64, data: &[u8]) -> Result<bool, String> {
    if i >= pieces::count(e.size) || data.len() as u64 != pieces::span(e.size, i).1 {
        return Err(format!("{}: not a piece of it", e.path));
    }
    {
        let g = lock();
        let v = g.as_ref().ok_or("no store")?;
        v.store.put_piece(&e.sha256, i, data).map_err(|x| format!("{}: {x}", e.path))?;
    }
    finish(e)
}

/// One file at a time is put together from its pieces.
static FINISHING: Mutex<()> = Mutex::new(());

/// `e` put together from its pieces, if they are all here: checked (its SHA-256, no
/// program, what it says it is), kept whole, its pieces gone. True when it is kept.
fn finish(e: &Entry) -> Result<bool, String> {
    let _one = FINISHING.lock().unwrap_or_else(|x| x.into_inner());
    let count = pieces::count(e.size);
    let data = {
        let g = lock();
        let v = g.as_ref().ok_or("no store")?;
        if v.store.has(e).is_some() {
            return Ok(true);
        }
        if (0..count).any(|i| !v.store.has_piece(&e.sha256, i, pieces::span(e.size, i).1)) {
            return Ok(false);
        }
        let mut data = Vec::with_capacity(e.size as usize);
        for i in 0..count {
            data.extend(v.store.piece(&e.sha256, i, pieces::span(e.size, i).1).map_err(|x| format!("{}: {x}", e.path))?);
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
    put(e, &data)?;
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
        match v.withheld.get(&e.sha256) {
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
pub(super) fn wait_for(fp: &str, wait: std::time::Duration) -> Option<VehicleManifest> {
    let until = std::time::Instant::now() + wait;
    let (count, arrived) = &ARRIVED;
    let mut seen = count.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(m) = manifest(fp) {
            return Some(m);
        }
        let left = until.checked_duration_since(std::time::Instant::now())?;
        seen = arrived.wait_timeout(seen, left).unwrap_or_else(|e| e.into_inner()).0;
    }
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
    why.contains("no such bus") || unreached(why)
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
    if why.contains("no such bus") || why.contains("no host to ask") {
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
    drop(g);
    // (a stand-in shown for it is made again: see `lan::tick`)
    if ready {
        omsi_cfg::content_changed();
    }
}

/// Downloads of buses under way at once, at most.
const DOWNLOADS: usize = 3;
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
pub(crate) fn want(root: &Path, fp: &str, bus: &str, paint: &str) -> Wanted {
    if !omsi_net::is_fingerprint(fp) {
        return Wanted::Failed("no fingerprint".into());
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
        let own = omsi_cfg::find_in_roots(&e.path)
            .filter(|(r, _)| !omsi_cfg::is_sandbox(r))
            .and_then(|(_, p)| omsi_cfg::vfs::read(&p).ok())
            .filter(|d| d.len() as u64 == e.size && super::sha256_of(d) == e.sha256);
        match own {
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
            finish(e)?;
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
/// bytes must be freed, of more, the least lately used first, but never one in use now nor a
/// file an upload under way holds. A bus's list goes before its files (a collection broken off
/// leaves no list without its files); a file goes only when no bus kept uses it.
pub(super) fn collect(need: u64) -> Collected {
    let keep = *KEEP.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::SystemTime::now();
    let mut g = lock();
    let Some(v) = g.as_mut() else { return Collected::default() };
    // (pieces of a file left unfinished that long - its player gone - go too)
    let stale = v.store.remove_stale_pieces(keep);
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
        let wanted_room = need > 0 && freed < need && age(t) > IN_USE;
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

/// Room on the disk for a file of `size`: the buses nobody uses go until there is; an error
/// only when even then the disk cannot hold it.
pub(super) fn make_room(size: u64) -> Result<(), String> {
    let Some(dir) = lock().as_ref().map(|v| v.store.dir().to_path_buf()) else { return Ok(()) };
    let free = || super::store::free_space(&dir);
    let Some(have) = free() else { return Ok(()) };
    if have >= size + KEEP_FREE {
        return Ok(());
    }
    collect(size + KEEP_FREE - have);
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
mod tests {
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
        let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store = temp("restart-store");
        open_for_test(&store);
        keep_for_days(30);
        let b = b"[model]\nmodel\\model.cfg\n".to_vec();
        let t = vec![5u8; 4000];
        let files = [(entry("Vehicles/Before/before.bus", &b), b.clone()), (entry("Vehicles/Before/t.dat", &t), t.clone())];
        let m = VehicleManifest { bus: "Vehicles/Before/before.bus".into(), entries: files.iter().map(|f| f.0.clone()).collect() }.validated().unwrap();
        // its files come (or are the server's own), but the bus is not finished...
        for (e, d) in &files {
            put(e, d).unwrap();
        }
        // ...and the server starts again: the files are in the store, no list names them
        open_for_test(&store);
        assert!(missing(&m).is_empty(), "the files are there");
        let (fp, new) = commit(m.clone()).unwrap();
        assert!(new);
        for (e, d) in &files {
            assert_eq!(blob(&e.sha256).as_deref(), Some(d.as_slice()), "{} is served", e.path);
        }
        assert_eq!(manifest(&fp), Some(m));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn buses_nobody_uses_go_but_never_one_in_use_a_shared_file_or_one_on_its_way() {
        let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store = temp("gc-store");
        open_for_test(&store);
        keep_for_days(30);
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
                put(e, d).unwrap();
            }
            let (fp, _) = commit(m.clone()).unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(store.join("vehicles").join(format!("{fp}.json"))).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(days_ago * 86_400 + 3600)).unwrap();
            (fp, m)
        };
        let (old, m_old) = keep("Old", &[2u8; 500], 40);
        let (driven, _) = keep("Driven", &[3u8; 500], 40);
        let (recent, m_recent) = keep("Recent", &[4u8; 500], 1);
        let (coming, m_coming) = keep("Coming", &[5u8; 500], 40);
        // one is being driven now; one is being sent again (its files held)
        in_use([driven.as_str()].into_iter());
        let lease = lease(&m_coming);
        let c = collect(0);
        assert_eq!(c.buses, 1, "only the old one nobody uses");
        assert!(manifest(&old).is_none() && manifest(&driven).is_some() && manifest(&recent).is_some() && manifest(&coming).is_some());
        // its own file went, the shared one (still used by the others) stayed
        assert!(blob(&m_old.entries.iter().find(|e| e.path.ends_with("t.dat")).unwrap().sha256).is_none());
        assert_eq!(blob(&super::super::sha256_of(&shared)), Some(shared.clone()));
        drop(lease);
        // short of room: the least lately used that nobody drives go (here: the one
        // that was on its way, then the recent one) - never the one being driven
        let c = collect(u64::MAX);
        assert_eq!(c.buses, 2);
        assert!(manifest(&coming).is_none() && manifest(&recent).is_none() && manifest(&driven).is_some());
        assert!(blob(&m_recent.entries.iter().find(|e| e.path.ends_with("t.dat")).unwrap().sha256).is_none());
        assert_eq!(blob(&super::super::sha256_of(&shared)), Some(shared));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_bus_that_keeps_failing_is_given_up_and_one_on_its_way_is_not() {
        let s = std::time::Duration::from_secs;
        assert!(!may_retry("bad list", s(10), 1), "a minute between tries");
        assert!(may_retry("bad list", s(61), 2));
        assert!(!may_retry("bad list", s(600), GIVE_UP), "given up");
        assert!(may_retry("the host says: no such bus", s(6), 50), "a bus on its way is asked for again");
        assert!(may_retry("no host to ask yet", s(6), 50), "asked once the way to the host is known");
        assert!(may_retry("cannot reach the host's mods on TCP 127.0.0.1:1: refused", s(16), 50), "a host out of reach is asked again");
        assert!(!may_retry("cannot reach the host's mods on TCP 127.0.0.1:1: refused", s(6), 50), "not at once");
        // a host with every connection taken, or a connection that broke off on the way
        // (the system's words in the player's language): asked again, never given up
        assert!(may_retry("the host says: busy", s(16), 50));
        assert!(may_retry("Vehicles/X/t.dds: connection lost: Foi forçado o cancelamento de uma conexão existente pelo host remoto. (os error 10054)", s(16), 50));
    }

    /// A box made while its bus was being fetched is made again once the bus is here
    /// (`lan::sync_remotes`): only a bus here, mounted or our own, is ready.
    #[test]
    fn a_bus_is_ready_once_mounted_or_our_own() {
        let fp = "v1-ready-test-0000";
        assert!(!ready(fp), "never heard of");
        set(fp, Wanted::Pending(1, 2));
        assert!(!ready(fp), "on its way");
        set(fp, Wanted::Mounted(PathBuf::from("/data/x/b.bus")));
        assert!(ready(fp));
        set(fp, Wanted::Failed("no".into()));
        assert!(!ready(fp));
        set(fp, Wanted::Local(PathBuf::from("/omsi/b.bus")));
        assert!(ready(fp));
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
        fetch(&fp).unwrap();
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
        assert!(fetch(&fp).is_err());
        let kept: usize = m.entries.iter().map(|e| pieces::count(e.size) as usize - missing_pieces(e).len()).sum();
        assert_eq!(kept, 3, "the pieces that came are kept");
        // again: a host that serves only as many pieces as are left is enough
        super::super::upload::remember_host(host_of(key.clone(), m.clone(), files.clone(), Version::Zstd, Some(all - kept)), session);
        fetch(&fp).unwrap();
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
        let why = fetch(&fp).unwrap_err();
        assert!(why.contains("not the file the list names"), "{why}");
        assert!(bus_file(&fp).is_none(), "no bus made of a file that is not its own");
        assert_eq!(missing_pieces(&m.entries[1]).len(), 3, "the pieces of the file that was not it went");
        // from an honest host it comes as it is
        let good: HashMap<String, Vec<u8>> = [&bus, &tex].into_iter().map(|d| (super::super::sha256_of(d), d.clone())).collect();
        let before = SENT_ZSTD.load(std::sync::atomic::Ordering::SeqCst);
        super::super::upload::remember_host(host(key.clone(), m.clone(), good), session);
        fetch(&fp).unwrap();
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
        fetch(&fp).unwrap();
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
        RECOMPRESSING.lock().unwrap().queued.contains(sha)
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
            super::super::upload::send_piece(&mut buf, sha, &i.to_string(), zstd).unwrap();
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
        super::super::upload::send_piece(&mut buf, &"0".repeat(64), "0", true).unwrap();
        assert!(buf.starts_with(b"ERR"));
        assert!(!queued(&"0".repeat(64)));
        recompress_queued();
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
        assert_eq!(collect(0).buses, 1);
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
        fetch(&fp).unwrap();
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
        fetch(&fp).unwrap();
        let bus_file = bus_file(&fp).unwrap();
        assert_eq!(omsi_cfg::vfs::read(&omsi_cfg::resolve_path(bus_file.parent().unwrap(), "texture/t.dat")).unwrap(), tex);
        // a host that passes off another bus under that fingerprint is not believed
        let other = VehicleManifest { bus: m.bus.clone(), entries: vec![entry("Vehicles/FetchBus/f.bus", &bus), entry("Vehicles/FetchBus/texture/t.dat", b"x")] }.validated().unwrap();
        let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
        let session = key.session();
        super::super::upload::remember_host(host(key.clone(), other.clone(), files.clone()), session);
        assert!(fetch(&format!("v1-{}", "1".repeat(64))).unwrap_err().contains("another bus"));
        // nor one whose file is not the one its list names
        let mut bad_files = files.clone();
        let tex2 = vec![6u8; 200_000];
        let m2 = VehicleManifest { bus: m.bus.clone(), entries: vec![entry("Vehicles/FetchBus/f.bus", &bus), entry("Vehicles/FetchBus/texture/t.dat", &tex2)] }.validated().unwrap();
        bad_files.insert(super::super::sha256_of(&tex2), vec![9u8; 200_000]);
        let key = std::sync::Arc::new(omsi_net::hostkey::HostKey::generate().unwrap());
        let session = key.session();
        super::super::upload::remember_host(host(key.clone(), m2.clone(), bad_files), session);
        assert!(fetch(&m2.fingerprint()).unwrap_err().contains("not the file the list names"));
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
}

/// A stock bus of an OMSI 2 installation (`OMSI_ROOT`), kept and mounted as a fetched bus is:
/// loading it reads nothing but its own files, its textures are found among them, and its
/// displays' fonts too.
#[cfg(test)]
mod mounted_stock_bus {
    use super::*;

    #[test]
    fn a_fetched_bus_reads_only_its_own_files() {
        let Some(root) = std::env::var_os("OMSI_ROOT").map(PathBuf::from) else { return };
        let _turn = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        omsi_cfg::add_content_root(root.clone());
        let store = std::env::temp_dir().join(format!("openomsi-mounted-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        open_for_test(&store);
        let bus = "Vehicles/MAN_SD200/MAN_SD77.bus";
        let ty = omsi_sim::VehicleType::load(&root, &omsi_cfg::resolve_path(&root, bus)).unwrap();
        let files = super::super::upload::listed(&root, super::super::bus_files::of_type(&root, &ty, Some(0), None));
        let m = VehicleManifest { bus: bus.into(), entries: super::super::upload::hashed(&files) }.validated().unwrap();
        for (rel, p) in &files {
            let e = m.entries.iter().find(|e| e.path == *rel).unwrap();
            put(e, &omsi_cfg::vfs::read(p).unwrap()).unwrap();
        }
        let (fp, _) = commit(m).unwrap();
        set(&fp, Wanted::Mounted(PathBuf::new()));
        let at = mount(&fp).unwrap();
        let audit = omsi_cfg::vfs::Audit::begin();
        let mt = omsi_sim::VehicleType::load(&root, &bus_file(&fp).unwrap()).unwrap();
        let reads = audit.end();
        assert!(reads.len() > 100);
        assert_eq!(escapes(&at, &root, &reads), Vec::<PathBuf>::new(), "read from outside its own files");
        assert_eq!(mt.meshes.len(), ty.meshes.len());
        // its textures and sounds, looked up as the renderer looks them up: all its own
        let outside: Vec<PathBuf> = super::super::bus_files::of_type(&root, &mt, Some(0), None)
            .into_iter()
            .filter(|p| !p.starts_with(&at) && !p.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Fonts")))
            .collect();
        assert!(outside.is_empty(), "{outside:?}");
        // and its displays write with its own fonts
        let mut fonts = omsi_sim::texttex::FontLibrary::only(&at);
        for t in &mt.model.text_textures {
            assert!(fonts.has_exact(&t.font), "font {} not among its own", t.font);
        }
        let _ = std::fs::remove_dir_all(&store);
    }
}
