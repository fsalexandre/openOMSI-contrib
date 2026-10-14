//! The server's side: the buses its players send, kept and served by their content; and a
//! player's game hosting a session, which keeps its own bus for the others the same way.

use super::*;
use crate::mods::failure::says;

/// The server's side of the players' buses: whether it takes them, what the uploads hold in
/// memory now, the files that came in pieces, and the bus a hosting game keeps for the
/// others. The game has one (`uploads`, behind this module's functions).
#[derive(Default)]
pub(in crate::mods) struct Uploads {
    accept: AtomicBool,
    /// This game is the server its players send their buses to (not one that joined).
    serving: AtomicBool,
    /// Bytes of `UPLOAD_MEMORY` held now.
    held: std::sync::atomic::AtomicU64,
    /// Files of a bus that came whole in pieces (`PUTPIECE`), by the bus: its pieces come
    /// over several connections, its DONE on another.
    came: Mutex<HashMap<String, usize>>,
    /// The bus (and paint) a hosting game has kept for the others, and whether one is being kept.
    kept_own: Mutex<(String, bool)>,
}

static UPLOADS: std::sync::LazyLock<Uploads> = std::sync::LazyLock::new(Uploads::default);

/// The game's own `Uploads`.
pub(in crate::mods) fn uploads() -> &'static Uploads {
    &UPLOADS
}

/// A dedicated server takes the buses its players send (`accept_uploads`).
pub fn accept(on: bool) {
    uploads().accept(on);
}

pub fn accepting() -> bool {
    uploads().accepting()
}

/// The server starts: the buses players sent before are opened (`vehicles`).
pub(in crate::mods) fn open() {
    uploads().open();
}

pub(in crate::mods) fn serving() -> bool {
    uploads().serving()
}

/// What the uploads of every connection may hold in memory at once (a list, a file sent
/// whole): a hundred connections each announcing a list of 256 MB, or a file of a GB, took
/// the server's memory. One that would pass it is told `ERR busy` and comes back; one larger
/// than all of it (no size is refused) comes when nothing else is held.
pub(super) const UPLOAD_MEMORY: u64 = 512 << 20;

/// Bytes of `UPLOAD_MEMORY` held by one request, given back when it is done.
pub(super) struct Held<'a>(&'a std::sync::atomic::AtomicU64, u64);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::AcqRel);
    }
}

/// `len` bytes of the request read and let go (an answer that refuses it keeps the stream in step).
pub(super) fn skip(input: &mut impl Read, len: u64) -> std::io::Result<()> {
    let n = std::io::copy(&mut input.take(len), &mut std::io::sink())?;
    if n != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}

/// `len` bytes of the request, read as they come (nothing held for bytes that never come).
pub(super) fn read_body(input: &mut impl Read, len: u64) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(len.min(1 << 20) as usize);
    input.take(len).read_to_end(&mut body)?;
    if body.len() as u64 != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(body)
}


/// One connection's upload: the bus it offered, which of its files the server wants, and
/// which have come (each kept in the store as it comes: a file is a file, whichever bus).
#[derive(Default)]
pub(in crate::mods) struct Incoming {
    offer: Option<VehicleManifest>,
    /// Its files, held against a collection while they come (`vehicles::lease`).
    lease: Option<crate::mods::vehicles::Lease<'static>>,
    wanted: HashSet<usize>,
    got: usize,
}

impl Incoming {
    /// `OFFER <len>`: a player's bus (its manifest). Answers which of its files the server
    /// lacks - by their content: a file of the same path in another version is another file.
    /// An empty list only asks whether the server takes uploads at all.
    pub(in crate::mods) fn offer(&mut self, input: &mut impl Read, out: &mut impl Write, len: u64) -> std::io::Result<()> {
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
    pub(in crate::mods) fn offer_pieces(&mut self, input: &mut impl Read, out: &mut impl Write, len: u64) -> std::io::Result<()> {
        let wanted = match self.take_offer(input, len)? {
            Ok(w) => w,
            Err(why) => return writeln!(out, "ERR {why}"),
        };
        let plan: Vec<(usize, Vec<u64>)> = match self.offer.as_ref() {
            Some(m) => wanted.iter().map(|k| (*k, crate::mods::vehicles::missing_pieces(&m.entries[*k]))).collect(),
            None => Vec::new(),
        };
        let body = serde_json::to_vec(&plan).unwrap_or_default();
        writeln!(out, "OK {}", body.len())?;
        out.write_all(&body)
    }

    /// The offer's list read and taken: the files the server wants (or why it takes none).
    /// (`len`: checked already, `requests`.)
    fn take_offer(&mut self, input: &mut impl Read, len: u64) -> std::io::Result<Result<Vec<usize>, String>> {
        // (nothing kept of a list a server that takes none would refuse anyway)
        if !accepting() || !crate::mods::vehicles::is_open() {
            skip(input, len)?;
            return Ok(Err(says::TAKES_NO_UPLOADS.into()));
        }
        let Some(_held) = uploads().hold(len) else {
            skip(input, len)?;
            return Ok(Err(says::BUSY.into()));
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
        if m.entries.len() > crate::mods::MAX_FILES {
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
        if let Some(bad) = m.entries.iter().find(|e| crate::mods::refuse_path(&e.path).is_some() || e.size > crate::mods::MAX_FILE) {
            return Ok(Err(format!("{}: {}", bad.path, crate::mods::refuse_path(&bad.path).unwrap_or_else(|| "too large".into()))));
        }
        self.lease = Some(crate::mods::vehicles::lease(&m));
        // (what the server has itself in the same version - a stock bus, a pack installed
        // here - is taken from its own files, not sent)
        let adopted = crate::mods::vehicles::adopt_local(&m);
        let wanted = crate::mods::vehicles::missing(&m);
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
    pub(in crate::mods) fn put_piece(&mut self, input: &mut impl Read, out: &mut impl Write, p: &PutPiece) -> std::io::Result<()> {
        let (idx, i, raw, codec, wire) = (p.index, p.piece, p.raw, p.codec.as_str(), p.wire);
        let entry = self.offer.as_ref().and_then(|m| m.entries.get(idx)).cloned();
        // (a piece of no file of the offer, or of another size: the stream cannot be trusted)
        let Some(e) = entry.filter(|e| i < crate::mods::pieces::count(e.size) && crate::mods::pieces::span(e.size, i).1 == raw) else {
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
        let data = match crate::mods::pieces::unpack(codec, bytes, raw) {
            Ok(d) => d,
            Err(why) => return writeln!(out, "ERR {}: {why}", e.path),
        };
        match crate::mods::vehicles::put_piece_sent(&e, i, &data) {
            Ok(whole) => {
                if whole && self.wanted.remove(&idx) {
                    // (counted for the bus, whichever of its connections the file came on)
                    if let Some(m) = self.offer.as_ref() {
                        uploads().came(&m.bus);
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
    pub(in crate::mods) fn put(&mut self, input: &mut impl Read, out: &mut impl Write, idx: usize, len: u64) -> std::io::Result<()> {
        let entry = self.offer.as_ref().and_then(|m| m.entries.get(idx)).filter(|_| self.wanted.contains(&idx)).cloned();
        // (a file not asked for, or of another size: the stream cannot be trusted any more)
        let Some(e) = entry.filter(|e| e.size == len) else {
            return Err(std::io::Error::other("a file that was not asked for"));
        };
        let Some(_held) = uploads().hold(len) else {
            skip(input, len)?;
            return writeln!(out, "ERR {}", says::BUSY);
        };
        let data = read_body(input, len)?;
        self.wanted.remove(&idx);
        if crate::mods::sha256_of(&data) != e.sha256 {
            return writeln!(out, "ERR {}: not the file the list names", e.path);
        }
        // (refused for what it is: the bus is kept without it - `vehicles::withhold`)
        let refused = if crate::mods::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { crate::mods::quarantine::check(&e.path, &data).err() };
        if let Some(why) = refused {
            log::warn!("uploads: refused {why}");
            crate::mods::vehicles::withhold(&e, &why);
            return writeln!(out, "ERR {why}");
        }
        match crate::mods::vehicles::put_sent(&e, &data) {
            Ok(()) => {
                self.got += 1;
                writeln!(out, "OK")
            }
            Err(x) => writeln!(out, "ERR {}: {x}", e.path),
        }
    }

    /// `DONE`: the bus is kept once every file of it is here, by the fingerprint the server
    /// works out itself. The others ask for it by that fingerprint (`vehicles::want`).
    pub(in crate::mods) fn done(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let got = std::mem::take(&mut self.got);
        self.wanted.clear();
        let Some(m) = self.offer.take() else {
            return writeln!(out, "ERR nothing was offered");
        };
        let bus = m.bus.clone();
        let got = got + uploads().take_came(&bus);
        let kept = crate::mods::vehicles::commit(m);
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
pub(in crate::mods) const MANIFEST_WAIT: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 90 });

/// `MANIFEST <fp>`: the list of a kept bus - one on its way is waited for (`MANIFEST_WAIT`).
pub(in crate::mods) fn send_manifest(out: &mut impl Write, fp: &Fingerprint) -> std::io::Result<()> {
    let fp = fp.as_str();
    match crate::mods::vehicles::wait_for(fp, MANIFEST_WAIT) {
        Some(m) => {
            // (with the files it is kept without, beside the list: a game before them reads
            // the list alone, asks for those too and draws a box, as before)
            let mut list = serde_json::to_value(&m).unwrap_or_default();
            let held = crate::mods::vehicles::withheld_of(fp);
            if !held.is_empty() {
                list["withheld"] = serde_json::to_value(&held).unwrap_or_default();
            }
            let body = serde_json::to_vec(&list).unwrap_or_default();
            writeln!(out, "OK {}", body.len())?;
            out.write_all(&body)
        }
        None => writeln!(out, "ERR {}", says::NO_SUCH_BUS),
    }
}

/// `BLOB <sha256>`: one kept file, by its content.
/// `PIECE <sha> <i>`: one piece of a kept file (`pieces`). To a game that reads zstd
/// (`zstd`): packed at its smallest once it is (`vehicles::recompress_later`), till then at
/// the speed of the moment.
pub(in crate::mods) fn send_piece(out: &mut impl Write, sha: &Sha256, i: u64, zstd: bool) -> std::io::Result<()> {
    let sha = sha.as_str();
    let mut pack_later = false;
    if zstd {
        match crate::mods::vehicles::packed(sha, i) {
            // (a piece that does not get smaller is kept as a mark only: it goes as it is)
            Some((_, '-', _)) => {}
            Some((raw, codec, wire)) => return crate::mods::pieces::send_packed(out, raw, codec, &wire),
            None => pack_later = true,
        }
    }
    let Some(data) = crate::mods::vehicles::served(sha) else {
        return writeln!(out, "ERR {}", says::NO_SUCH_FILE);
    };
    let size = data.len() as u64;
    if i >= crate::mods::pieces::count(size) {
        return writeln!(out, "ERR no such piece");
    }
    let (start, len) = crate::mods::pieces::span(size, i);
    let piece = &data[start as usize..(start + len) as usize];
    if zstd {
        // (only a file kept here is queued: asking for others queues nothing)
        if pack_later {
            crate::mods::vehicles::recompress_later(sha);
        }
        let (codec, wire) = crate::mods::pieces::pack_zstd(piece, crate::mods::pieces::ZSTD_FAST);
        return crate::mods::pieces::send_packed(out, len, codec, &wire);
    }
    crate::mods::pieces::send(out, piece)
}

pub(in crate::mods) fn send_blob(out: &mut impl Write, sha: &Sha256) -> std::io::Result<()> {
    let sha = sha.as_str();
    match crate::mods::vehicles::blob(sha) {
        Some(data) => {
            writeln!(out, "OK {}", data.len())?;
            out.write_all(&data)
        }
        None => writeln!(out, "ERR {}", says::NO_SUCH_FILE),
    }
}

/// Once a frame (a player's game hosting a session): our own bus into our store, by its
/// fingerprint, once it is loaded - the joining players ask this game for it as they would a
/// server (`vehicles::fetch`). A server has no bus of its own; a joining player sends its
/// bus to the host instead (`tick`).
pub(crate) fn keep_own(root: &Path, bus: &str, paint: &str, own: impl FnOnce() -> Vec<PathBuf>) {
    uploads().keep_own(root, bus, paint, own);
}

impl Uploads {
    pub(in crate::mods) fn accept(&self, on: bool) {
        self.accept.store(on, Ordering::Relaxed);
    }

    pub(in crate::mods) fn accepting(&self) -> bool {
        self.accept.load(Ordering::Relaxed)
    }

    pub(in crate::mods) fn open(&self) {
        if self.accepting() && !crate::mods::vehicles::open() {
            log::warn!("uploads: no store; players' buses are not taken");
            self.accept(false);
        }
        self.serving.store(self.accepting(), Ordering::Relaxed);
    }

    pub(in crate::mods) fn serving(&self) -> bool {
        self.serving.load(Ordering::Relaxed)
    }

    /// `n` bytes of `UPLOAD_MEMORY` for one request, if they are free (or nothing is held:
    /// a request larger than all of it comes alone).
    pub(super) fn hold(&self, n: u64) -> Option<Held<'_>> {
        self.held.fetch_update(Ordering::AcqRel, Ordering::Acquire, |h| (h + n <= UPLOAD_MEMORY || h == 0).then_some(h + n)).ok().map(|_| Held(&self.held, n))
    }

    /// One more file of `bus` came whole in pieces.
    pub(super) fn came(&self, bus: &str) {
        *self.came.lock().unwrap_or_else(|p| p.into_inner()).entry(bus.to_string()).or_default() += 1;
    }

    /// The files of `bus` that came whole in pieces, counted and let go.
    pub(super) fn take_came(&self, bus: &str) -> usize {
        self.came.lock().unwrap_or_else(|p| p.into_inner()).remove(bus).unwrap_or(0)
    }

    pub(in crate::mods) fn keep_own(&'static self, root: &Path, bus: &str, paint: &str, own: impl FnOnce() -> Vec<PathBuf>) {
        if bus.is_empty() || !self.serving() {
            return;
        }
        let key = format!("{bus}|{paint}");
        {
            let mut k = self.kept_own.lock().unwrap_or_else(|e| e.into_inner());
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
            self.kept_own.lock().unwrap_or_else(|e| e.into_inner()).1 = false;
        });
        if spawned.is_err() {
            *self.kept_own.lock().unwrap_or_else(|e| e.into_inner()) = (String::new(), false);
        }
    }
}

/// Our bus's files (`files`: relative path, where each is read from) into this game's store
/// as a server keeps a bus that was sent to it: each checked as one that came would be (a
/// file refused for what it is is withheld, the bus kept without it), the bus kept by the
/// fingerprint worked out from them. The fingerprint.
pub(in crate::mods) fn keep_files(bus: &str, files: Vec<(String, PathBuf)>) -> Result<String, String> {
    if !crate::mods::vehicles::is_open() && !crate::mods::vehicles::open() {
        return Err("no store".into());
    }
    let bus_rel = omsi_net::vehicle_path(bus).unwrap_or_else(|| bus.to_string());
    let manifest = VehicleManifest { bus: bus_rel, entries: hashed(&files) }.validated()?;
    let source: HashMap<String, PathBuf> = files.iter().map(|(rel, p)| (rel.to_lowercase(), p.clone())).collect();
    for k in crate::mods::vehicles::missing(&manifest) {
        let e = &manifest.entries[k];
        let path = source.get(&e.path.to_lowercase()).ok_or_else(|| format!("{}: not among our files", e.path))?;
        let data = omsi_cfg::vfs::read(path).map_err(|x| format!("{}: {x}", path.display()))?;
        if data.len() as u64 != e.size || crate::mods::sha256_of(&data) != e.sha256 {
            return Err(format!("{} changed while it was kept", e.path));
        }
        let refused = if crate::mods::looks_executable(&data[..data.len().min(8)]) { Some(format!("{}: a program", e.path)) } else { crate::mods::quarantine::check(&e.path, &data).err() };
        match refused {
            Some(why) => {
                log::warn!("uploads: {why}; our bus is kept without it");
                crate::mods::vehicles::withhold(e, &why);
            }
            None => crate::mods::vehicles::put(e, &data)?,
        }
    }
    crate::mods::vehicles::commit(manifest).map(|(fp, _)| fp)
}
