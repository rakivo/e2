use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;
use std::panic::{catch_unwind, AssertUnwindSafe};

use crossbeam_channel::{Receiver, Sender};
use smallvec::SmallVec;
use wgpu::naga::FastHashMap;

const CHUNK_SIZE: usize = 64;  // @Tune

type Queue = Arc<(Mutex<Vec<ScanRequest>>, Condvar)>;

const MAX_ENTRIES_PER_DIR_IF_ITS_NOT_A_USER_INITIATED_SCAN: usize = 500;

const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", ".git", ".hg", ".svn",
    "vendor", "dist", "build", "__pycache__", ".cache",
    ".npm", ".cargo", "venv", ".venv", "env",
];

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)  // @Speed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Dir  = 1,
}

#[derive(Debug)]
pub struct CachedDir {
    pub entries:    DirEntries,
    pub scanned_at: SystemTime,
    pub mtime:      SystemTime,
    pub inode:      u64,
    pub state:      ScanState,
}

impl Deref for CachedDir {
    type Target = DirEntries;
    fn deref(&self) -> &Self::Target { &self.entries }
}
impl DerefMut for CachedDir {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.entries }
}

#[derive(Debug, PartialEq)]
pub enum ScanState {
    Ready,
    Scanning,
    Failed,
}

const RESERVED_BIT_COUNT_FOR_ENTRY_KIND: usize = 3;

const ENTRY_KIND_MASK:                   u16   = 0b1110_0000_0000_0000;
const ENTRY_NAME_LEN_MASK:               u16   = !ENTRY_KIND_MASK;
const ENTRY_NAME_LEN_SHIFT:              usize = size_of_val(&ENTRY_KIND_MASK)*8 - RESERVED_BIT_COUNT_FOR_ENTRY_KIND;

const _: () = assert!(ENTRY_KIND_MASK.count_ones()      == RESERVED_BIT_COUNT_FOR_ENTRY_KIND as u32);
const _: () = assert!(ENTRY_NAME_LEN_MASK.count_zeros() == RESERVED_BIT_COUNT_FOR_ENTRY_KIND as u32);

#[derive(Debug, Clone, Copy)]
pub struct DirEntryData {
    pub name_start:    u32,
    pub path_start:    u32,
    pub kind_name_len: u16,
    pub path_len:      u16,
}

impl DirEntryData {
    #[inline]
    pub const fn new(name_start: u32, name_len: u16, path_start: u32, path_len: u16, kind: EntryKind) -> Self {
        Self {
            name_start,
            path_start,
            kind_name_len: (name_len & ENTRY_NAME_LEN_MASK) | ((kind as u16) << ENTRY_NAME_LEN_SHIFT),
            path_len,
        }
    }

    #[inline]
    pub const fn kind(&self) -> EntryKind {
        match self.kind_name_len >> ENTRY_NAME_LEN_SHIFT {
            0 => EntryKind::File,
            1 => EntryKind::Dir,
            _ => EntryKind::File,  // Reserved
        }
    }

    #[inline]
    pub const fn name_len(&self) -> usize {
        (self.kind_name_len & ENTRY_NAME_LEN_MASK) as usize
    }

    #[inline]
    pub const fn path_len(&self) -> usize {
        self.path_len as usize
    }
}

#[derive(Debug, Default)]
pub struct DirEntries {
    pub generation: u64,

    pub blob:     String,

    pub entries:  Vec<DirEntryData>,
}

impl DirEntries {
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = DirEntry<'_>> {
        self.entries.iter().map(|entry| {
            DirEntry {
                name: &self.blob[entry.name_start as usize..][..entry.name_len()],
                path: &self.blob[entry.path_start as usize..][..entry.path_len()],
                kind: entry.kind()
            }
        })
    }

    #[inline] pub fn len(&self)      -> usize { self.entries.len() }
    #[inline] pub fn is_empty(&self) -> bool  { self.len() == 0 }

    #[inline]
    fn append_chunk(&mut self, mut chunk: DirEntries) {
        let blob_offset = self.blob.len() as u32;
        self.blob.push_str(&chunk.blob);

        let mut chunk_entries = core::mem::take(&mut chunk.entries);
        for entry in chunk_entries.iter_mut() {
            entry.name_start = entry.name_start + blob_offset;
            entry.path_start = entry.path_start + blob_offset;
        }

        self.entries.extend(chunk_entries);
    }
}

pub struct DirEntry<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub kind: EntryKind,
}

struct ScanRequest {
    path:       Arc<Path>,

    generation: u64,

    is_recursive:      bool,
    is_user_initiated: bool,
}

#[derive(Debug)]
struct ScanChunk {
    path:       Arc<Path>,
    entries:    DirEntries,
    mtime:      SystemTime,
    inode:      u64,
    generation: u64,
    is_done:    bool,

    error:      Option<std::io::Error>,
}

pub struct Director {
    pub entries: FastHashMap<Arc<Path>, CachedDir>,
    receiver:    Receiver<ScanChunk>,
    queue:       Queue,
}

impl Director {
    pub fn new() -> Self {
        let queue: Queue = Default::default();
        let queue_worker = queue.clone();

        let (res_tx, res_rx) = crossbeam_channel::unbounded();

        std::thread::spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                background_thread_code(res_tx, queue_worker);
            }));

            eprintln!("========================================================================================");
            eprintln!("WORKER THREAD PANICKED: {}", result.is_err());
            eprintln!("========================================================================================");
        });

        Self {
            entries:  FastHashMap::default(),
            receiver: res_rx,
            queue,
        }
    }

    /// Drain completed chunks - call once per frame.
    /// Returns true if anything changed (caller should rebuild filtered list).
    pub fn poll(&mut self, path_for_checking: &Path) -> bool {
        let mut any_new = false;

        while let Ok(chunk) = self.receiver.try_recv() {
            let entry = self.entries.entry(chunk.path.clone()).or_insert_with(|| CachedDir {
                entries:    DirEntries { generation: chunk.generation, ..Default::default() },
                scanned_at: SystemTime::now(),
                mtime:      chunk.mtime,
                inode:      chunk.inode,
                state:      ScanState::Scanning,
            });

            //
            // Discard chunks from cancelled/superseded scans
            //
            if chunk.generation < entry.generation { continue }

            if let Some(e) = chunk.error {
                eprintln!("scan error for {:?}: {}", chunk.path, e);
                entry.state = ScanState::Failed;
                any_new = true;
                continue;
            }

            //
            // First chunk for this generation - clear old entries
            //
            if entry.entries.generation != chunk.generation {
                entry.entries = DirEntries { generation: chunk.generation, ..Default::default() };
            }

            entry.entries.append_chunk(chunk.entries);

            if chunk.is_done {
                entry.mtime      = chunk.mtime;
                entry.inode      = chunk.inode;
                entry.scanned_at = SystemTime::now();
                entry.state      = ScanState::Ready;
            }

            any_new |= chunk.path.canonicalize().is_ok_and(|canon1| {
                path_for_checking.canonicalize().is_ok_and(|canon2| canon1 == canon2)
            });
        }

        any_new
    }

    /// Get entries for a path. Returns cached data if valid, kicks scan if stale/missing.
    /// Never blocks - returns None if not yet ready.
    #[inline]
    pub fn get(&mut self, path: &Path) -> Option<&DirEntries> {
        let needs_scan = match self.entries.get(path) {
            None         => true,
            Some(cached) => {
                cached.state != ScanState::Scanning
                    && cached.scanned_at.elapsed().unwrap_or_default().as_secs_f32() > 1.0
            }
        };

        if needs_scan {
            self.kick_scan(path, false, false, true);
        }

        self.entries.get(path)
            .filter(|c| c.state == ScanState::Ready)
            .map(|c| &c.entries)
    }

    pub fn kick_scan(
        &mut self,
        path: impl Into<Arc<Path>>,
        is_recursive: bool, is_high_priority: bool, is_user_initiated: bool
    ) {
        let path = path.into();

        let generation = self.entries.get(&path)
            .map(|c| c.generation + 1)
            .unwrap_or(0);

        self.entries.entry(path.clone()).or_insert_with(|| CachedDir {
            entries:    DirEntries { generation, ..Default::default() },
            scanned_at: SystemTime::UNIX_EPOCH,
            mtime:      SystemTime::UNIX_EPOCH,
            inode:      0,
            state:      ScanState::Scanning,
        }).state = ScanState::Scanning;

        let (lock, cvar) = &*self.queue;
        let mut q = lock.lock().unwrap();

        if is_high_priority {
            q.clear();
        }
        q.push(ScanRequest { path, generation, is_recursive, is_user_initiated });

        cvar.notify_one();
    }
}

fn do_scan(
    path:           Arc<Path>,

    recursive:      bool,
    user_initiated: bool,

    generation:     u64,

    tx:             &Sender<ScanChunk>,
) {
    let meta = match std::fs::metadata(&path) {
        Err(e) => {
            _ = tx.send(ScanChunk {
                path,
                entries:    DirEntries::default(),
                mtime:      SystemTime::UNIX_EPOCH,
                inode:      0,
                generation,
                is_done:    true,
                error:      Some(e),
            });

            return;
        }

        Ok(m) => m,
    };

    #[cfg(unix)]
    let inode = { use std::os::unix::fs::MetadataExt; meta.ino() };
    #[cfg(not(unix))]
    let inode = 0u64;

    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let mut stack = SmallVec::<[_; 4]>::with_capacity(1);
    stack.push((path.clone(), 0));

    while let Some((dir, depth)) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };

        let mut chunk        = DirEntries::default();
        let mut entry_count  = 0usize;

        for entry in read_dir.flatten() {
            let Ok(ft) = entry.file_type()                              else { continue };

            let p = entry.path();
            let Some(path_str) = p.to_str()                             else { continue };
            let Some(name_str) = p.file_name().and_then(|n| n.to_str()) else { continue };

            if should_skip_dir(name_str)                                     { continue };

            entry_count += 1;
            if !user_initiated && entry_count > MAX_ENTRIES_PER_DIR_IF_ITS_NOT_A_USER_INITIATED_SCAN {
                //
                // Directory is huge - emit what we have and don't recurse into it.
                // (Only if the search isn't user initiated)
                //

                break;
            }

            let kind = if ft.is_dir() { EntryKind::Dir } else { EntryKind::File };

            let name_start = chunk.blob.len() as u32;
            chunk.blob.push_str(name_str);
            let name_len = chunk.blob.len() as u32 - name_start;

            let path_start = chunk.blob.len() as u32;
            chunk.blob.push_str(path_str);
            let path_len = chunk.blob.len() as u32 - path_start;

            chunk.entries.push(DirEntryData::new(name_start, name_len as _, path_start, path_len as _, kind));

            //
            // Queue subdirs for recursion if not too deep and not skipped
            //
            if recursive && kind == EntryKind::Dir {
                stack.push((p.into_boxed_path().into(), depth + 1));
            }

            if chunk.len() >= CHUNK_SIZE {
                _ = tx.send(ScanChunk {
                    path:    path.clone(),
                    entries: std::mem::take(&mut chunk),
                    mtime,
                    inode,
                    generation,
                    is_done:    false,
                    error:   None,
                });
            }
        }

        let is_done = stack.is_empty();
        if !chunk.is_empty() || is_done {
            _ = tx.send(ScanChunk {
                path:    path.clone(),
                entries: chunk,
                mtime,
                inode,
                generation,
                is_done,
                error:   None,
            });
        }
    }
}

fn background_thread_code(res_tx: Sender<ScanChunk>, queue: Queue) {
    loop {
        let req = {
            let (lock, cvar) = &*queue;
            let mut q = lock.lock().unwrap();
            while q.is_empty() {
                q = cvar.wait(q).unwrap();
            }

            //
            // Always take the newest request, discard the rest
            //

            let n = q.len() - 1;
            q.drain(..n).for_each(drop);
            q.pop().unwrap()
        };

        do_scan(req.path, req.is_recursive, req.is_user_initiated, req.generation, &res_tx);
    }
}
