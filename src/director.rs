use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use wgpu::naga::FastHashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug)]
pub struct CachedDir {
    pub entries:      DirEntries,
    pub scanned_at:   SystemTime,    // When we last scanned
    pub mtime:        SystemTime,    // Dir mtime at scan time
    pub inode:        u64,
    pub state:        ScanState,
    pub generation:   u64,
}

#[derive(Debug, PartialEq)]
pub enum ScanState {
    Ready,
    Scanning,
    Failed,
}

// Flat storage same as before
#[derive(Debug, Default)]
pub struct DirEntries {
    pub blob:         String,
    pub name_starts:  Vec<u32>,
    pub name_lens:    Vec<u32>,
    pub path_starts:  Vec<u32>,
    pub path_lens:    Vec<u32>,
    pub kinds:        Vec<EntryKind>,
    pub inodes:       Vec<u64>,       // For invalidation
}

impl DirEntries {
    pub fn iter(&self) -> impl Iterator<Item = DirEntry<'_>> {
        (0..self.name_starts.len()).map(|i| DirEntry {
            name:  &self.blob[self.name_starts[i] as usize..][..self.name_lens[i] as usize],
            path:  &self.blob[self.path_starts[i] as usize..][..self.path_lens[i] as usize],
            kind:  self.kinds[i],
            inode: self.inodes[i],
        })
    }
}

pub struct DirEntry<'a> {
    pub name:  &'a str,
    pub path:  &'a str,
    pub kind:  EntryKind,
    pub inode: u64,
}

struct ScanRequest {
    pub path:       Arc<Path>,
    pub generation: u64,
    pub recursive:  bool,
}

struct ScanResult {
    pub path:       Arc<Path>,
    pub entries:    DirEntries,
    pub mtime:      SystemTime,
    pub inode:      u64,
    pub generation: u64,
    pub error:      Option<std::io::Error>,
}

pub struct Director {
    // Keyed by canonical path
    pub entries: FastHashMap<Arc<Path>, CachedDir>,

    // Background scan results
    receiver: std::sync::mpsc::Receiver<ScanResult>,
    sender:   std::sync::mpsc::SyncSender<ScanRequest>,
}

impl Director {
    pub fn new() -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::sync_channel::<ScanRequest>(8);
        let (res_tx, res_rx) = std::sync::mpsc::sync_channel::<ScanResult>(8);

        // Worker thread - processes scan requests
        std::thread::spawn(move || {
            while let Ok(req) = req_rx.recv() {
                let result = do_scan(req.path, req.recursive, req.generation);
                _ = res_tx.try_send(result);
            }
        });

        Self {
            entries:  FastHashMap::default(),
            receiver: res_rx,
            sender:   req_tx,
        }
    }

    /// Get entries for a path. Returns cached data if valid, kicks scan if stale/missing.
    /// Never blocks - returns None if not yet ready.
    pub fn get(&mut self, path: &Path) -> Option<&DirEntries> {
        let needs_scan = match self.entries.get(path) {
            None => true,
            Some(cached) => {
                cached.state != ScanState::Scanning && self.is_stale(path, cached)
            }
        };

        if needs_scan {
            self.kick_scan(path.to_path_buf(), false);
        }

        self.entries.get(path)
            .filter(|c| c.state == ScanState::Ready)
            .map(|c| &c.entries)
    }

    /// Force invalidate a specific path (e.g. after a file save)
    pub fn invalidate(&mut self, path: &Path) {
        if let Some(cached) = self.entries.get_mut(path) {
            cached.state = ScanState::Scanning;
            self.kick_scan(path.to_path_buf(), false);
        }
    }

    fn is_stale(&self, path: &Path, cached: &CachedDir) -> bool {
        match std::fs::metadata(path) {
            Err(_) => true, // Path removed

            Ok(meta) => {
                // Inode changed = replaced entirely
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if meta.ino() != cached.inode { return true; }
                }

                // Mtime changed = contents changed
                meta.modified().map(|m| m != cached.mtime).unwrap_or(true)
            }
        }
    }

    /// Call once per frame - integrates completed scans
    pub fn poll(&mut self) {
        while let Ok(result) = self.receiver.try_recv() {
            let entry = self.entries.entry(result.path.clone()).or_insert_with(|| CachedDir { // @Clone
                entries:    DirEntries::default(),
                scanned_at: SystemTime::now(),
                mtime:      result.mtime,
                inode:      result.inode,
                state:      ScanState::Ready,
                generation: result.generation,
            });

            //
            // Only apply if this is newer than what we have
            //
            if result.generation >= entry.generation {
                if let Some(e) = result.error {
                    eprintln!("scan error for {:?}: {}", result.path, e);
                    entry.state = ScanState::Failed;
                } else {
                    entry.entries    = result.entries;
                    entry.mtime      = result.mtime;
                    entry.inode      = result.inode;
                    entry.scanned_at = SystemTime::now();
                    entry.state      = ScanState::Ready;
                    entry.generation = result.generation;
                }
            }
        }
    }

    pub fn kick_scan(&mut self, path: impl Into<Arc<Path>>, recursive: bool) {
        let path = path.into();

        let generation = self.entries.get(&path)
            .map(|c| c.generation + 1)
            .unwrap_or(0);

        //
        // Mark as scanning so we don't kick again next frame
        //
        self.entries.entry(path.clone()).or_insert_with(|| CachedDir {
            entries:    DirEntries::default(),
            scanned_at: SystemTime::UNIX_EPOCH,
            mtime:      SystemTime::UNIX_EPOCH,
            inode:      0,
            state:      ScanState::Scanning,
            generation
        }).state = ScanState::Scanning;

        _ = self.sender.try_send(ScanRequest { path, generation, recursive });
    }
}

fn do_scan(path: impl Into<Arc<Path>>, recursive: bool, generation: u64) -> ScanResult {
    let path = path.into();

    let meta = match std::fs::metadata(&path) {
        Err(e) => return ScanResult {
            path,
            entries: DirEntries::default(),
            mtime: SystemTime::UNIX_EPOCH,
            inode: 0,
            generation,
            error: Some(e),
        },

        Ok(m) => m,
    };

    #[cfg(unix)]
    let inode = { use std::os::unix::fs::MetadataExt; meta.ino() };
    #[cfg(not(unix))]
    let inode = 0u64;

    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

    let mut entries = DirEntries::default();
    let mut stack: Vec<Arc<Path>> = vec![path.clone()]; // @Clone

    while let Some(dir) = stack.pop() { // @Incomplete
        let Ok(read_dir) = std::fs::read_dir(&dir) else { continue };
        for entry in read_dir.flatten() {
            let Ok(ft)   = entry.file_type()                               else { continue };
            let Ok(meta) = entry.metadata()                                else { continue };
            let path     = entry.path();
            let Some(path_str) = path.to_str()                             else { continue };
            let Some(name_str) = path.file_name().and_then(|n| n.to_str()) else { continue };

            if name_str.starts_with('.')                                        { continue }
            if matches!(name_str, "target" | "node_modules" | ".git")           { continue }

            #[cfg(unix)]
            let inode = { use std::os::unix::fs::MetadataExt; meta.ino() };
            #[cfg(not(unix))]
            let inode = 0u64;

            let kind = if ft.is_dir() { EntryKind::Dir } else { EntryKind::File };

            let name_start = entries.blob.len() as u32;
            entries.blob.push_str(name_str);
            let name_len = entries.blob.len() as u32 - name_start;

            let path_start = entries.blob.len() as u32;
            entries.blob.push_str(path_str);
            let path_len = entries.blob.len() as u32 - path_start;

            entries.name_starts.push(name_start);
            entries.name_lens.push(name_len);
            entries.path_starts.push(path_start);
            entries.path_lens.push(path_len);
            entries.kinds.push(kind);
            entries.inodes.push(inode);

            if recursive && kind == EntryKind::Dir {
                stack.push(path.into_boxed_path().into());
            }
        }
    }

    ScanResult { path, entries, mtime, inode, generation, error: None }
}
