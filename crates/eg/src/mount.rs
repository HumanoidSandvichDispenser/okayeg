//! `eg mount`: a remote project as a live FUSE filesystem.
//!
//! The mounted tree is the doc itself; there is no second on-disk copy and no
//! `.eg/` directory. State lives under `$XDG_DATA_HOME/eg/remote/<name>`, keyed
//! by the remote's name or its endpoint id. Reads always serve the local
//! replica and never block on the network: sync runs beside the mount, a
//! dropped link degrades to the cached state, and a redial catches back up.
//!
//! Writes buffer per file and commit as edits: syscall-sized
//! chunks collect in a dirty buffer, and `flush`, `fsync`, `release`, or the
//! [`CEILING`] diffs the buffer against the doc as of the frontier captured
//! when the buffer was seeded. A peer edit landing while a buffer is dirty
//! survives the merge (see [`FileTree::set_content_at`]). Tree operations
//! (create, mkdir, unlink, rmdir, rename) commit immediately.
//!
//! [`FileTree::set_content_at`]: okayeg::FileTree::set_content_at

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo, MountOption, Notifier,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use okayeg::{Change, Doc, Frontiers, FsError, NodeKind, TreeID, read_bytes, valid_name};
use okayeg_net::{EndpointId, Node, Perms, Shared, Transport, drive_live};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::keys;
use crate::to_io;
use crate::workspace::{CapWorkspace, Workspace};

/// The doc snapshot inside a remote's state dir.
const DOC_PATH: &str = "doc";

/// The scheme prefixing a raw endpoint id on the command line.
const IROH_SCHEME: &str = "iroh://";

/// How long the kernel may cache an entry or attribute before asking again.
const TTL: Duration = Duration::from_secs(1);

/// How long to wait before redialing a dropped or unreachable peer.
const RETRY: Duration = Duration::from_secs(5);

/// How long a dirty buffer may sit idle before it commits without waiting
/// for a flush.
const CEILING: Duration = Duration::from_millis(500);

/// Mount `target` on `mountpoint` and sync it live until interrupted.
///
/// `target` is a `[remote.<name>]` from the global config, or an
/// `iroh://<endpoint-id>`. The key and identity resolve as if a repo were
/// bound to that remote (see [`config::resolve`](crate::config::resolve)).
pub fn mount(target: &str, mountpoint: &Path, cli_key: Option<&str>) -> io::Result<()> {
    let global = keys::load_global()?;

    let (remote_name, peer) = match global.remotes.get(target) {
        Some(remote) => {
            let peer = remote.peer.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("eg config.toml: [remote.{target}] has no peer"),
                )
            })?;
            (Some(target.to_owned()), peer)
        }
        None => (None, target.to_owned()),
    };
    let id = parse_endpoint(&peer)?;

    let state_dir = data_dir()?
        .join("remote")
        .join(remote_name.clone().unwrap_or_else(|| id.to_string()));
    std::fs::create_dir_all(&state_dir)?;
    let state = std::rc::Rc::new(CapWorkspace::open(&state_dir)?);

    let repo = Config {
        remote: remote_name,
        ..Config::default()
    };
    let (_, secret) = keys::effective(&global, cli_key, &repo, false, &*state)?;

    let doc: Shared = Arc::new(load_state(&state)?);

    let (changed, _) = broadcast::channel::<()>(64);
    let writeback = Arc::new(Writeback {
        doc: doc.clone(),
        buffers: Mutex::new(HashMap::new()),
        changed: changed.clone(),
    });
    let revoked = Arc::new(AtomicBool::new(false));
    let mtimes = Arc::new(Mutex::new(HashMap::new()));

    let mountpoint = mountpoint.canonicalize()?;
    let meta = std::fs::metadata(&mountpoint)?;
    let inodes = Arc::new(Mutex::new(Inodes::new()));
    let fs = DocMount {
        doc: doc.clone(),
        inodes: inodes.clone(),
        writeback: writeback.clone(),
        revoked: revoked.clone(),
        mtimes: mtimes.clone(),
        uid: std::os::unix::fs::MetadataExt::uid(&meta),
        gid: std::os::unix::fs::MetadataExt::gid(&meta),
        epoch: SystemTime::now(),
    };
    let mut options = fuser::Config::default();
    options.mount_options = vec![MountOption::FSName(format!("eg:{target}"))];
    let session = fuser::Session::new(fs, &mountpoint, &options)?.spawn()?;
    let _watch = spawn_doc_watch(&doc, inodes, mtimes, session.notifier());

    let result = run_sync(SyncArgs {
        doc: &doc,
        state: state.clone(),
        id,
        secret,
        target,
        mountpoint: &mountpoint,
        changed,
        writeback: writeback.clone(),
        revoked,
    });

    writeback.commit_stale(Duration::ZERO);
    drop(session);
    if let Err(e) = store_state(&doc, &state) {
        eprintln!("eg mount: saving state: {e}");
    }
    result
}

/// Parse an endpoint id, with or without the `iroh://` scheme.
fn parse_endpoint(s: &str) -> io::Result<EndpointId> {
    let bare = s.strip_prefix(IROH_SCHEME).unwrap_or(s);
    EndpointId::from_str(bare).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{s} is not a configured remote or an iroh://<endpoint-id>"),
        )
    })
}

/// The eg data directory: `$XDG_DATA_HOME/eg`, or `~/.local/share/eg`.
fn data_dir() -> io::Result<PathBuf> {
    keys::xdg_dir("XDG_DATA_HOME", &[".local", "share"])
}

/// Load the cached doc from the state dir, or start empty.
fn load_state(state: &CapWorkspace) -> io::Result<Doc> {
    match state.read_file(Path::new(DOC_PATH)) {
        Ok(bytes) => Doc::from_snapshot(&bytes).map_err(to_io),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Doc::new()),
        Err(e) => Err(e),
    }
}

/// Write the doc's snapshot into the state dir.
fn store_state(doc: &Doc, state: &CapWorkspace) -> io::Result<()> {
    state.write_private(Path::new(DOC_PATH), &doc.snapshot().map_err(to_io)?)
}

/// Watch the doc: stamp each changed file's mtime and drop the kernel's
/// cached pages for it.
fn spawn_doc_watch(
    doc: &Doc,
    inodes: Arc<Mutex<Inodes>>,
    mtimes: Arc<Mutex<HashMap<TreeID, SystemTime>>>,
    notifier: Notifier,
) -> okayeg::Subscription {
    // invalidations go through their own thread: sending one from a thread
    // that is servicing a write to the same inode can deadlock against the
    // kernel's page locks
    let (tx, rx) = std::sync::mpsc::channel::<INodeNo>();
    let tx = Mutex::new(tx);
    std::thread::spawn(move || {
        while let Ok(ino) = rx.recv() {
            let _ = notifier.inval_inode(ino, 0, -1);
        }
    });

    doc.fs().subscribe(move |changes| {
        // changes arrive batched per commit or import; lock once per batch
        let mut mtimes = mtimes.lock().unwrap();
        let inodes = inodes.lock().unwrap();
        let tx = tx.lock().unwrap();
        for change in changes {
            if let Change::Content { node } = change {
                mtimes.insert(*node, SystemTime::now());
                if let Some(ino) = inodes.existing(*node) {
                    let _ = tx.send(ino);
                }
            }
        }
    })
}

/// Everything [`run_sync`] needs.
struct SyncArgs<'a> {
    doc: &'a Shared,
    state: std::rc::Rc<CapWorkspace>,
    id: EndpointId,
    secret: [u8; 32],
    target: &'a str,
    mountpoint: &'a Path,
    changed: broadcast::Sender<()>,
    writeback: Arc<Writeback>,
    revoked: Arc<AtomicBool>,
}

/// Hold the doc in live sync with `id` until interrupted.
///
/// Dials in a loop: a dropped link or a failed dial retries after [`RETRY`],
/// while the mount keeps serving the local replica. A refusal ends the loop
/// for good; the mount stays up on the cached state. Each import persists the
/// doc to the state dir. Returns on ctrl-c.
fn run_sync(args: SyncArgs<'_>) -> io::Result<()> {
    let SyncArgs {
        doc,
        state,
        id,
        secret,
        target,
        mountpoint,
        changed,
        writeback,
        revoked,
    } = args;

    crate::net::block_on(async {
        let node = Node::bind_with_secret(secret).await.map_err(to_io)?;

        println!(
            "eg mount: {target} on {} (ctrl-c to unmount)",
            mountpoint.display()
        );

        let dial_doc = doc.clone();
        let dial_changed = changed.clone();
        tokio::task::spawn_local(async move {
            loop {
                match node.dial(id).await {
                    Ok((send, recv, _guard)) => {
                        println!("eg mount: connected to {id}");
                        let live = drive_live(
                            dial_doc.clone(),
                            send,
                            recv,
                            Perms::all(),
                            dial_changed.clone(),
                            None,
                        )
                        .await;
                        match live {
                            Err(okayeg_net::Error::Refused { message }) => {
                                match message {
                                    Some(msg) => eprintln!("eg mount: rejected by {id}: {msg}"),
                                    None => eprintln!("eg mount: rejected by {id}"),
                                }
                                eprintln!(
                                    "eg mount: serving the local copy read-only; unmount with ctrl-c"
                                );
                                revoked.store(true, Ordering::Relaxed);
                                return;
                            }
                            Err(e) => eprintln!("eg mount: link dropped: {e}"),
                            Ok(()) => eprintln!("eg mount: link closed"),
                        }
                    }
                    Err(e) => eprintln!("eg mount: {e}"),
                }
                tokio::time::sleep(RETRY).await;
            }
        });

        // the debounce ceiling: a file held open and written continuously
        // still commits, without shredding writes into per-syscall edits
        let ceiling = writeback.clone();
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(CEILING / 2);
            loop {
                tick.tick().await;
                ceiling.commit_stale(CEILING);
            }
        });

        let persist_doc = doc.clone();
        let mut nudged = changed.subscribe();
        tokio::task::spawn_local(async move {
            loop {
                match nudged.recv().await {
                    Err(broadcast::error::RecvError::Closed) => break,
                    _ => {
                        // drain queued nudges, so a burst becomes one snapshot
                        while nudged.try_recv().is_ok() {}
                        if let Err(e) = store_state(&persist_doc, &state) {
                            eprintln!("eg mount: saving state: {e}");
                        }
                    }
                }
            }
        });

        tokio::signal::ctrl_c().await?;
        println!("eg mount: unmounting");
        Ok(())
    })
}

/// One file's uncommitted bytes.
struct Buffer {
    bytes: Vec<u8>,

    /// The doc frontier when the buffer was seeded: the diff base at commit.
    base: Frontiers,

    /// When the buffer last took a write, for the [`CEILING`].
    last_write: Instant,
}

/// The dirty buffers between `write()` syscalls and doc edits.
///
/// A buffer seeds from the doc's content on the first write to a file and
/// shadows it until committed; a commit diffs the buffer against the content
/// at its base frontier and merges.
struct Writeback {
    doc: Shared,

    buffers: Mutex<HashMap<TreeID, Buffer>>,

    changed: broadcast::Sender<()>,
}

impl Writeback {
    /// Splice `data` into the file's buffer at byte `offset`, seeding it if
    /// this is the first write.
    fn write(&self, node: TreeID, offset: usize, data: &[u8]) -> Result<u32, Errno> {
        let mut buffers = self.buffers.lock().unwrap();
        let buf = Self::seeded(&self.doc, &mut buffers, node)?;

        let end = offset + data.len();
        if buf.bytes.len() < end {
            buf.bytes.resize(end, 0);
        }
        buf.bytes[offset..end].copy_from_slice(data);
        buf.last_write = Instant::now();
        Ok(data.len() as u32)
    }

    /// Resize the file's buffer to `size` bytes, seeding it first. Growth
    /// zero-fills.
    fn truncate(&self, node: TreeID, size: usize) -> Result<(), Errno> {
        let mut buffers = self.buffers.lock().unwrap();
        let buf = Self::seeded(&self.doc, &mut buffers, node)?;
        buf.bytes.resize(size, 0);
        buf.last_write = Instant::now();
        Ok(())
    }

    /// The file's buffer, seeded from the doc's current content and frontier
    /// if absent.
    fn seeded<'m>(
        doc: &Doc,
        buffers: &'m mut HashMap<TreeID, Buffer>,
        node: TreeID,
    ) -> Result<&'m mut Buffer, Errno> {
        use std::collections::hash_map::Entry;
        match buffers.entry(node) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let text = doc.files().content(node).ok_or(Errno::EISDIR)?;
                Ok(e.insert(Buffer {
                    bytes: text.to_string().into_bytes(),
                    base: doc.frontiers(),
                    last_write: Instant::now(),
                }))
            }
        }
    }

    /// Commit the file's buffer into the doc, if one is dirty.
    fn commit(&self, node: TreeID) {
        // hold the lock across apply, so a racing write cannot seed a new
        // buffer from the doc state this commit is halfway through changing
        let mut buffers = self.buffers.lock().unwrap();
        if let Some(buf) = buffers.remove(&node) {
            self.apply(node, buf);
        }
    }

    /// Commit every buffer whose last write is at least `ceiling` ago.
    fn commit_stale(&self, ceiling: Duration) {
        let mut buffers = self.buffers.lock().unwrap();
        let stale: Vec<TreeID> = buffers
            .iter()
            .filter(|(_, b)| b.last_write.elapsed() >= ceiling)
            .map(|(node, _)| *node)
            .collect();
        for node in stale {
            let buf = buffers.remove(&node).expect("collected from this map");
            self.apply(node, buf);
        }
    }

    fn apply(&self, node: TreeID, buf: Buffer) {
        let text = String::from_utf8_lossy(&buf.bytes);
        if !self.doc.files().set_content_at(node, &text, &buf.base) {
            eprintln!("eg mount: dropped an edit to a file that no longer exists");
            return;
        }
        self.doc.commit();
        let _ = self.changed.send(());
    }

    /// Up to `size` bytes at `offset` of the file's dirty buffer, or `None`
    /// when the file has none.
    fn read(&self, node: TreeID, offset: usize, size: usize) -> Option<Vec<u8>> {
        let buffers = self.buffers.lock().unwrap();
        let buf = buffers.get(&node)?;
        let end = buf.bytes.len().min(offset.saturating_add(size));
        Some(buf.bytes.get(offset..end).unwrap_or_default().to_vec())
    }

    /// The dirty buffer's size in bytes, when one exists.
    fn len(&self, node: TreeID) -> Option<u64> {
        let buffers = self.buffers.lock().unwrap();
        Some(buffers.get(&node)?.bytes.len() as u64)
    }

    /// Drop the file's buffer without committing, for an unlinked file.
    fn discard(&self, node: TreeID) {
        self.buffers.lock().unwrap().remove(&node);
    }
}

/// The ino <-> tree node mapping. Inode 1 is the root, which is not a node;
/// nodes get inodes on first sight and keep them for the mount's lifetime.
struct Inodes {
    by_ino: HashMap<u64, TreeID>,
    by_node: HashMap<TreeID, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        Self {
            by_ino: HashMap::new(),
            by_node: HashMap::new(),
            next: 2,
        }
    }

    fn ino(&mut self, node: TreeID) -> INodeNo {
        if let Some(ino) = self.by_node.get(&node) {
            return INodeNo(*ino);
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(ino, node);
        self.by_node.insert(node, ino);
        INodeNo(ino)
    }

    fn existing(&self, node: TreeID) -> Option<INodeNo> {
        self.by_node.get(&node).map(|ino| INodeNo(*ino))
    }

    fn node(&self, ino: INodeNo) -> Option<TreeID> {
        self.by_ino.get(&ino.0).copied()
    }
}

/// The FUSE view of a doc. Every operation reads the live doc (shadowed per
/// file by a dirty buffer); the kernel's own caches sit above, bounded by
/// [`TTL`] and dropped by the invalidation subscription.
struct DocMount {
    doc: Shared,

    inodes: Arc<Mutex<Inodes>>,

    writeback: Arc<Writeback>,

    /// Set when the host permanently refused this key; every mutation then
    /// fails with `EACCES`.
    revoked: Arc<AtomicBool>,

    /// Per-node modification times, stamped on each content change. Nothing
    /// here survives a remount; the doc carries no timestamps.
    mtimes: Arc<Mutex<HashMap<TreeID, SystemTime>>>,

    /// Owner of every file, taken from the mountpoint.
    uid: u32,

    /// Group of every file, taken from the mountpoint.
    gid: u32,

    /// The timestamp on anything never modified in this session.
    epoch: SystemTime,
}

/// The errno a [`FsError`] surfaces as.
fn errno(e: FsError) -> Errno {
    match e {
        FsError::InvalidPath => Errno::EINVAL,
        FsError::NotFound => Errno::ENOENT,
        FsError::NotADirectory => Errno::ENOTDIR,
        FsError::NotAFile => Errno::EISDIR,
        FsError::AlreadyExists => Errno::EEXIST,
        FsError::NotEmpty => Errno::ENOTEMPTY,
        FsError::InvalidMove => Errno::EINVAL,
    }
}

impl DocMount {
    /// The attributes at `ino`, or `None` if it does not resolve to a live
    /// node.
    fn attr_of(&self, ino: INodeNo) -> Option<FileAttr> {
        if ino == INodeNo::ROOT {
            return Some(self.attr(ino, FileType::Directory, 0, self.epoch));
        }
        let node = self.inodes.lock().unwrap().node(ino)?;
        let tree = self.doc.files();
        let mtime = self
            .mtimes
            .lock()
            .unwrap()
            .get(&node)
            .copied()
            .unwrap_or(self.epoch);
        match tree.kind(node)? {
            NodeKind::Dir => Some(self.attr(ino, FileType::Directory, 0, mtime)),
            _ => {
                let size = self
                    .writeback
                    .len(node)
                    .or_else(|| tree.content(node).map(|t| t.len_utf8() as u64))?;
                Some(self.attr(ino, FileType::RegularFile, size, mtime))
            }
        }
    }

    fn attr(&self, ino: INodeNo, kind: FileType, size: u64, mtime: SystemTime) -> FileAttr {
        let (perm, nlink) = match kind {
            FileType::Directory => (0o755, 2),
            _ => (0o644, 1),
        };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: self.epoch,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The child nodes of the directory at `ino`.
    fn children(&self, ino: INodeNo) -> Option<Vec<TreeID>> {
        let tree = self.doc.files();
        if ino == INodeNo::ROOT {
            return Some(tree.roots());
        }
        let node = self.inodes.lock().unwrap().node(ino)?;
        (tree.kind(node) == Some(NodeKind::Dir)).then(|| tree.children(node))
    }

    /// The doc path of the directory at `ino`; the root is the empty path.
    fn dir_path(&self, ino: INodeNo) -> Option<String> {
        if ino == INodeNo::ROOT {
            return Some(String::new());
        }
        let node = self.inodes.lock().unwrap().node(ino)?;
        self.doc.fs().path_of(node)
    }

    /// The doc path of `name` under the directory at `parent`.
    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Option<String> {
        let name = name.to_str().filter(|n| valid_name(n))?;
        let dir = self.dir_path(parent)?;
        if dir.is_empty() {
            Some(name.to_owned())
        } else {
            Some(format!("{dir}/{name}"))
        }
    }

    /// Whether mutations are refused because the host revoked this key.
    fn denied(&self) -> bool {
        self.revoked.load(Ordering::Relaxed)
    }

    /// Commit a tree operation and nudge sync and persistence.
    fn committed(&self) {
        self.doc.commit();
        let _ = self.writeback.changed.send(());
    }

    /// Commit the dirty buffer of the file at `ino`, if any.
    fn commit_ino(&self, ino: INodeNo) {
        // bind before committing: the doc subscription runs on this thread
        // and takes the inodes lock, so holding the guard across the commit
        // (as an if-let scrutinee temporary would) is a self-deadlock
        let node = self.inodes.lock().unwrap().node(ino);
        if let Some(node) = node {
            self.writeback.commit(node);
        }
    }

    /// Reply with `node`'s entry.
    fn entry_of(&self, node: TreeID, reply: ReplyEntry) {
        let ino = self.inodes.lock().unwrap().ino(node);
        match self.attr_of(ino) {
            Some(attr) => reply.entry(&TTL, &attr, fuser::Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }
}

impl Filesystem for DocMount {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str().filter(|n| valid_name(n)) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(children) = self.children(parent) else {
            return reply.error(Errno::ENOENT);
        };

        let tree = self.doc.files();
        let found = children
            .into_iter()
            .find(|node| tree.name(*node).as_deref() == Some(name));
        match found {
            Some(node) => self.entry_of(node, reply),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr_of(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if let Some(size) = size {
            if self.denied() {
                return reply.error(Errno::EACCES);
            }
            let node = self.inodes.lock().unwrap().node(ino);
            let Some(node) = node else {
                return reply.error(Errno::ENOENT);
            };
            if let Err(e) = self.writeback.truncate(node, size as usize) {
                return reply.error(e);
            }
            self.mtimes.lock().unwrap().insert(node, SystemTime::now());
        }

        // everything else (chmod, chown, utimes) has no doc representation;
        // accept it so editor save protocols do not fail, change nothing
        match self.attr_of(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(children) = self.children(ino) else {
            return reply.error(Errno::ENOTDIR);
        };

        let tree = self.doc.files();
        let mut inodes = self.inodes.lock().unwrap();
        let mut entries = vec![
            (ino, FileType::Directory, ".".to_owned()),
            (INodeNo::ROOT, FileType::Directory, "..".to_owned()),
        ];
        for node in children {
            let Some(name) = tree.name(node).filter(|n| valid_name(n)) else {
                continue;
            };
            let kind = match tree.kind(node) {
                Some(NodeKind::Dir) => FileType::Directory,
                _ => FileType::RegularFile,
            };
            entries.push((inodes.ino(node), kind, name));
        }
        drop(inodes);

        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, kind, &name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let node = self.inodes.lock().unwrap().node(ino);
        let Some(node) = node else {
            return reply.error(Errno::ENOENT);
        };

        // a dirty buffer shadows the doc, so a writer reads its own bytes back
        if let Some(bytes) = self.writeback.read(node, offset as usize, size as usize) {
            return reply.data(&bytes);
        }
        match self.doc.files().content(node) {
            Some(text) => reply.data(&read_bytes(&text, offset as usize, size as usize)),
            None => reply.error(Errno::EISDIR),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        let node = self.inodes.lock().unwrap().node(ino);
        let Some(node) = node else {
            return reply.error(Errno::ENOENT);
        };

        match self.writeback.write(node, offset as usize, data) {
            Ok(written) => {
                self.mtimes.lock().unwrap().insert(node, SystemTime::now());
                reply.written(written);
            }
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        self.commit_ino(ino);
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.commit_ino(ino);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.commit_ino(ino);
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        let Some(path) = self.child_path(parent, name) else {
            return reply.error(Errno::EINVAL);
        };

        match self.doc.fs().create_file(&path) {
            Ok(node) => {
                self.committed();
                let ino = self.inodes.lock().unwrap().ino(node);
                match self.attr_of(ino) {
                    Some(attr) => reply.created(
                        &TTL,
                        &attr,
                        fuser::Generation(0),
                        FileHandle(0),
                        FopenFlags::empty(),
                    ),
                    None => reply.error(Errno::ENOENT),
                }
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        let Some(path) = self.child_path(parent, name) else {
            return reply.error(Errno::EINVAL);
        };

        match self.doc.fs().create_dir(&path) {
            Ok(node) => {
                self.committed();
                self.entry_of(node, reply);
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        let Some(path) = self.child_path(parent, name) else {
            return reply.error(Errno::ENOENT);
        };

        // TODO: walks the path twice; have remove_file return the removed node
        let node = self.doc.fs().resolve(&path).ok();
        match self.doc.fs().remove_file(&path) {
            Ok(()) => {
                if let Some(node) = node {
                    self.writeback.discard(node);
                }
                self.committed();
                reply.ok();
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        let Some(path) = self.child_path(parent, name) else {
            return reply.error(Errno::ENOENT);
        };

        match self.doc.fs().remove_dir(&path) {
            Ok(()) => {
                self.committed();
                reply.ok();
            }
            Err(e) => reply.error(errno(e)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if self.denied() {
            return reply.error(Errno::EACCES);
        }
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let (Some(from), Some(to)) = (
            self.child_path(parent, name),
            self.child_path(newparent, newname),
        ) else {
            return reply.error(Errno::ENOENT);
        };
        if flags.contains(RenameFlags::RENAME_NOREPLACE) && self.doc.fs().resolve(&to).is_ok() {
            return reply.error(Errno::EEXIST);
        }

        match self.doc.fs().rename(&from, &to) {
            Ok(()) => {
                self.committed();
                reply.ok();
            }
            Err(e) => reply.error(errno(e)),
        }
    }
}
