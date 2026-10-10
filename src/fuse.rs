//! Read-only FUSE projection of the in-process [`OrFs`] namespace.
//!
//! This module exposes the *logical* object namespace normally — logical files
//! read back byte-exact through [`OrFs::read`] — while concealing the manifested
//! operational store. Specifically:
//!
//! 1. The concealed namespace (`<root>/.ekbloom/`, see [`crate::filesystem`]) is
//!    never visible: it does not appear in `readdir` and `lookup` fails for it.
//! 2. A single virtual control file, `.ekbloom-physical.json`, sits at the mount
//!    root. It reports the administrative *physical* accounting
//!    ([`OrFs::inspect_physical`]) and is explicitly an accounting view, not
//!    logical content. It is not the concealed namespace and is not a logical
//!    file; it exists so the manifested excess can be inspected without exposing
//!    the store that produces it.
//!
//! The filesystem is strictly read-only: writes (and any attempt to open a file
//! for writing) fail with `EROFS`.
//!
//! # Mounting
//!
//! Mounting requires `/dev/fuse` and `CAP_SYS_ADMIN` (or the setuid `fusermount3`
//! helper), so it is opt-in and never performed implicitly. The whole module is
//! behind the `fuse` cargo feature and is not built by default.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use crate::error::{EkbloomError, Result};
use crate::filesystem::OrFs;
use crate::ReadRequest;

/// FUSE root inode. Matches [`fuser::FUSE_ROOT_ID`].
const ROOT_INO: u64 = 1;

/// Fixed inode for the physical-accounting control file.
const CONTROL_INO: u64 = 2;

/// Name of the virtual control file at the mount root.
pub const CONTROL_FILE_NAME: &str = ".ekbloom-physical.json";

/// Attribute/entry cache lifetime.
const TTL: Duration = Duration::from_secs(1);

// Minimal Linux errno values used to answer FUSE requests. These are the values
// the kernel expects in a reply, not host-side failures.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;

// Open-flag access mask and read-only value (`fcntl.h`).
const O_ACCMODE: i32 = 0o3;
const O_RDONLY: i32 = 0o0;

/// A mounted, read-only OrFS projection.
///
/// Dropping the handle unmounts the filesystem and joins the serving thread.
pub struct OrFsMount {
    session: Option<BackgroundSession>,
    mountpoint: PathBuf,
}

impl OrFsMount {
    /// The directory the filesystem is (or was) mounted on.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for OrFsMount {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            // `join` unmounts and then joins the serving thread.
            session.join();
        }
    }
}

/// Mount `fs` read-only at `mountpoint`.
///
/// When `foreground` is `false` the FUSE session runs on a background thread and
/// the returned [`OrFsMount`] unmounts on drop. When `foreground` is `true` the
/// session runs in the calling thread until the filesystem is unmounted (for
/// example with `fusermount3 -u`); the returned handle is then already unmounted.
pub fn mount(fs: OrFs, mountpoint: &Path, foreground: bool) -> Result<OrFsMount> {
    let view = OrFsView::new(fs)?;
    let options = [
        MountOption::RO,
        MountOption::FSName("ekbloom-orfs".to_string()),
        MountOption::NoAtime,
    ];
    if foreground {
        fuser::mount2(view, mountpoint, &options)?;
        Ok(OrFsMount {
            session: None,
            mountpoint: mountpoint.to_path_buf(),
        })
    } else {
        let session = fuser::spawn_mount2(view, mountpoint, &options)?;
        Ok(OrFsMount {
            session: Some(session),
            mountpoint: mountpoint.to_path_buf(),
        })
    }
}

/// One logical file: its logical path and revealed length.
struct FileEntry {
    path: String,
    len: u64,
}

/// One directory child: the name it appears under, its inode and its kind.
struct ChildEntry {
    name: String,
    ino: u64,
    kind: FileType,
}

/// The logical namespace, resolved once at mount time into an inode tree.
///
/// The tree is derived purely from the *logical* view of [`OrFs`]
/// ([`OrFs::list`] and [`OrFs::stat`]), so the concealed namespace can never
/// enter it: `list` already filters it, and `stat` only resolves registered
/// logical objects.
struct OrFsView {
    fs: OrFs,
    /// Directory inode -> logical directory path (`""` for the root).
    dirs: HashMap<u64, String>,
    /// Logical directory path -> directory inode.
    dir_ino: HashMap<String, u64>,
    /// File inode -> logical path and revealed length.
    files: HashMap<u64, FileEntry>,
    /// Directory inode -> children (in name order).
    children: HashMap<u64, Vec<ChildEntry>>,
    /// Stable attribute timestamps for the lifetime of the mount.
    mount_time: SystemTime,
    /// Next inode to hand out.
    next_ino: u64,
}

impl OrFsView {
    /// Resolve the whole logical tree, assigning inodes.
    fn new(fs: OrFs) -> Result<OrFsView> {
        let mut view = OrFsView {
            fs,
            dirs: HashMap::new(),
            dir_ino: HashMap::new(),
            files: HashMap::new(),
            children: HashMap::new(),
            mount_time: SystemTime::now(),
            next_ino: CONTROL_INO + 1,
        };
        view.dirs.insert(ROOT_INO, String::new());
        view.dir_ino.insert(String::new(), ROOT_INO);
        view.walk(ROOT_INO, "")?;
        Ok(view)
    }

    /// Recursively resolve `dir_path`, assigning inodes to its children.
    fn walk(&mut self, dir_ino: u64, dir_path: &str) -> Result<()> {
        let mut kids: Vec<ChildEntry> = Vec::new();
        if dir_path.is_empty() {
            kids.push(ChildEntry {
                name: CONTROL_FILE_NAME.to_string(),
                ino: CONTROL_INO,
                kind: FileType::RegularFile,
            });
        }
        for name in self.fs.list(dir_path)? {
            let child_path = if dir_path.is_empty() {
                name.clone()
            } else {
                format!("{dir_path}/{name}")
            };
            let ino = self.next_ino;
            self.next_ino = self.next_ino.checked_add(1).ok_or(EkbloomError::Overflow {
                what: "fuse inode counter",
            })?;
            match self.fs.stat(&child_path) {
                Ok(meta) => {
                    self.files.insert(
                        ino,
                        FileEntry {
                            path: child_path,
                            len: meta.logical_len,
                        },
                    );
                    kids.push(ChildEntry {
                        name,
                        ino,
                        kind: FileType::RegularFile,
                    });
                }
                Err(_) => {
                    self.dirs.insert(ino, child_path.clone());
                    self.dir_ino.insert(child_path.clone(), ino);
                    kids.push(ChildEntry {
                        name,
                        ino,
                        kind: FileType::Directory,
                    });
                    self.walk(ino, &child_path)?;
                }
            }
        }
        self.children.insert(dir_ino, kids);
        Ok(())
    }

    /// The parent inode of `ino` (the root is its own parent).
    fn parent_ino(&self, ino: u64) -> u64 {
        if ino == ROOT_INO {
            return ROOT_INO;
        }
        let path = self
            .dirs
            .get(&ino)
            .map(String::as_str)
            .or_else(|| self.files.get(&ino).map(|f| f.path.as_str()));
        match path.and_then(|p| p.rsplit_once('/')) {
            Some((parent, _)) => self.dir_ino.get(parent).copied().unwrap_or(ROOT_INO),
            None => ROOT_INO,
        }
    }

    /// Attributes for `ino`, or `None` if no such inode exists.
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        if ino == ROOT_INO {
            return Some(self.file_attr(ino, FileType::Directory, 0, 0o555, 2));
        }
        if ino == CONTROL_INO {
            let size = self.control_bytes().map_or(0, |b| b.len() as u64);
            return Some(self.file_attr(ino, FileType::RegularFile, size, 0o444, 1));
        }
        if self.dirs.contains_key(&ino) {
            return Some(self.file_attr(ino, FileType::Directory, 0, 0o555, 2));
        }
        if let Some(file) = self.files.get(&ino) {
            return Some(self.file_attr(ino, FileType::RegularFile, file.len, 0o444, 1));
        }
        None
    }

    /// Construct a [`FileAttr`] with stable timestamps.
    fn file_attr(&self, ino: u64, kind: FileType, size: u64, perm: u16, nlink: u32) -> FileAttr {
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: self.mount_time,
            mtime: self.mount_time,
            ctime: self.mount_time,
            crtime: self.mount_time,
            kind,
            perm,
            nlink,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// The directory entries for `ino`: `.`, `..`, then the children.
    fn dir_entries(&self, ino: u64) -> Option<Vec<(u64, FileType, String)>> {
        let kids = self.children.get(&ino)?;
        let mut out = Vec::with_capacity(kids.len() + 2);
        out.push((ino, FileType::Directory, ".".to_string()));
        out.push((self.parent_ino(ino), FileType::Directory, "..".to_string()));
        for kid in kids {
            out.push((kid.ino, kid.kind, kid.name.clone()));
        }
        Some(out)
    }

    /// The administrative physical-accounting JSON for the control file.
    fn control_bytes(&self) -> Option<Vec<u8>> {
        let report = self.fs.inspect_physical().ok()?;
        let mut s = String::new();
        s.push_str("{\n");
        s.push_str("  \"kind\": \"physical-accounting\",\n");
        s.push_str(
            "  \"note\": \"Administrative physical accounting; NOT logical content, \
             and not the concealed namespace.\",\n",
        );
        s.push_str(&format!("  \"total_bytes\": {},\n", report.total_bytes));
        s.push_str(&format!("  \"logical_bytes\": {},\n", report.logical_bytes));
        s.push_str(&format!(
            "  \"concealed_bytes\": {},\n",
            report.concealed_bytes
        ));
        s.push_str("  \"objects\": [\n");
        for (i, obj) in report.objects.iter().enumerate() {
            s.push_str("    {");
            s.push_str(&format!(
                "\"logical_path\": {}, ",
                json_string(&obj.logical_path)
            ));
            s.push_str(&format!("\"visible_bytes\": {}, ", obj.visible_bytes));
            s.push_str(&format!("\"physical_bytes\": {}", obj.physical_bytes));
            s.push('}');
            if i + 1 != report.objects.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("  ]\n}\n");
        Some(s.into_bytes())
    }

    /// Read a logical file range, clamping to the file length. Returns an errno
    /// on failure so it can be handed straight to a FUSE reply.
    fn read_logical(&self, ino: u64, offset: u64, size: u32) -> std::result::Result<Vec<u8>, i32> {
        let file = self.files.get(&ino).ok_or(ENOENT)?;
        if offset >= file.len {
            return Ok(Vec::new());
        }
        let want = u64::from(size).min(file.len - offset);
        self.fs
            .read(
                &file.path,
                ReadRequest {
                    offset,
                    length: want,
                },
            )
            .map_err(|_| EIO)
    }
}

impl Filesystem for OrFsView {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(ENOENT);
            return;
        };
        if parent == ROOT_INO && name == CONTROL_FILE_NAME {
            match self.attr(CONTROL_INO) {
                Some(attr) => reply.entry(&TTL, &attr, 0),
                None => reply.error(ENOENT),
            }
            return;
        }
        let ino = self
            .children
            .get(&parent)
            .and_then(|kids| kids.iter().find(|kid| kid.name == name))
            .map(|kid| kid.ino);
        match ino.and_then(|i| self.attr(i)) {
            Some(attr) => reply.entry(&TTL, &attr, 0),
            None => reply.error(ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(ENOENT),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if ino != CONTROL_INO && !self.files.contains_key(&ino) {
            reply.error(ENOENT);
            return;
        }
        if flags & O_ACCMODE != O_RDONLY {
            reply.error(EROFS);
            return;
        }
        reply.opened(0, 0);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let offset = match u64::try_from(offset) {
            Ok(value) => value,
            Err(_) => {
                reply.error(EINVAL);
                return;
            }
        };
        let result = if ino == CONTROL_INO {
            self.control_bytes()
                .map_or(Err(EIO), |bytes| slice_bytes(&bytes, offset, size))
        } else {
            self.read_logical(ino, offset, size)
        };
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(errno) => reply.error(errno),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        if ino != ROOT_INO && !self.dirs.contains_key(&ino) {
            reply.error(ENOENT);
            return;
        }
        reply.opened(0, 0);
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(entries) = self.dir_entries(ino) else {
            reply.error(ENOENT);
            return;
        };
        let start = usize::try_from(offset).unwrap_or(0);
        for (index, (entry_ino, kind, name)) in entries.iter().enumerate().skip(start) {
            let next = (index as i64) + 1;
            if reply.add(*entry_ino, next, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let Some(entries) = self.dir_entries(ino) else {
            reply.error(ENOENT);
            return;
        };
        let start = usize::try_from(offset).unwrap_or(0);
        for (index, (entry_ino, _kind, name)) in entries.iter().enumerate().skip(start) {
            let Some(attr) = self.attr(*entry_ino) else {
                continue;
            };
            let next = (index as i64) + 1;
            if reply.add(*entry_ino, next, name, &TTL, &attr, 0) {
                break;
            }
        }
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // Administrative view: report the real manifested bytes as the block count
        // but never as free space (bfree == bavail == 0). This mirrors
        // `OrFs::inspect_physical` and does not present physical allocation as
        // capacity.
        let blocks = self
            .fs
            .inspect_physical()
            .map(|report| report.total_bytes / 512)
            .unwrap_or(0);
        let files = (self.files.len() + self.dirs.len()) as u64;
        reply.statfs(blocks, 0, 0, files, 0, 512, 255, 512);
    }

    fn access(&mut self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        // Read-only projection: grant read/execute, deny any write access.
        const W_OK: i32 = 2;
        if self.attr(ino).is_none() {
            reply.error(ENOENT);
            return;
        }
        if mask & W_OK != 0 {
            reply.error(EACCES);
            return;
        }
        reply.ok();
    }

    fn destroy(&mut self) {}
}

/// Slice `bytes` to `[offset, offset + size]`, clamped to the buffer.
fn slice_bytes(bytes: &[u8], offset: u64, size: u32) -> std::result::Result<Vec<u8>, i32> {
    let len = bytes.len() as u64;
    if offset >= len {
        return Ok(Vec::new());
    }
    let want = u64::from(size).min(len - offset);
    let start = usize::try_from(offset).map_err(|_| EIO)?;
    let end = start
        .checked_add(usize::try_from(want).map_err(|_| EIO)?)
        .ok_or(EIO)?;
    Ok(bytes[start..end].to_vec())
}

/// JSON-escape a string into a quoted JSON string literal.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManifestationProfile;

    fn temp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-fuse-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names_of(view: &OrFsView, ino: u64) -> Vec<String> {
        view.dir_entries(ino)
            .unwrap()
            .into_iter()
            .map(|(_, _, name)| name)
            .collect()
    }

    #[test]
    fn tree_hides_concealed_namespace_and_adds_control_file() {
        let root = temp_root("tree");
        let fs = OrFs::open(&root).unwrap();
        fs.create("data/hello.bin", b"hello fuse", ManifestationProfile::Full)
            .unwrap();
        fs.create("top.bin", b"top", ManifestationProfile::Root)
            .unwrap();
        let view = OrFsView::new(fs).unwrap();

        let root_names = names_of(&view, ROOT_INO);
        assert!(root_names.contains(&"data".to_string()));
        assert!(root_names.contains(&"top.bin".to_string()));
        assert!(root_names.contains(&CONTROL_FILE_NAME.to_string()));
        assert!(!root_names.iter().any(|n| n == ".ekbloom"));
        assert!(root_names.iter().all(|n| !n.starts_with(".ekbloom/")));

        let data_ino = view.dir_ino["data"];
        let data_names = names_of(&view, data_ino);
        assert!(data_names.contains(&"hello.bin".to_string()));
        assert!(!data_names.iter().any(|n| n == ".ekbloom"));

        // The concealed namespace is physically present but never enumerated.
        assert!(root.join(".ekbloom").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn logical_reads_are_byte_exact_through_the_tree() {
        let root = temp_root("read");
        let fs = OrFs::open(&root).unwrap();
        let src = b"byte exact logical content for fuse".to_vec();
        fs.create(
            "data/hello.bin",
            &src,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();
        let view = OrFsView::new(fs).unwrap();

        let ino = view
            .children
            .get(&ROOT_INO)
            .unwrap()
            .iter()
            .find(|k| k.name == "data")
            .unwrap()
            .ino;
        let file_ino = view
            .children
            .get(&ino)
            .unwrap()
            .iter()
            .find(|k| k.name == "hello.bin")
            .unwrap()
            .ino;

        let whole = view.read_logical(file_ino, 0, src.len() as u32).unwrap();
        assert_eq!(whole, src);
        let part = view.read_logical(file_ino, 5, 10).unwrap();
        assert_eq!(part, src[5..15].to_vec());
        // Beyond EOF clamps to an empty read rather than erroring.
        let eof = view.read_logical(file_ino, src.len() as u64, 16).unwrap();
        assert!(eof.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn control_file_reports_physical_accounting_not_logical() {
        let root = temp_root("control");
        let fs = OrFs::open(&root).unwrap();
        fs.create("data/full.bin", &[0x5Au8; 1024], ManifestationProfile::Full)
            .unwrap();
        let view = OrFsView::new(fs).unwrap();

        let bytes = view.control_bytes().unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("\"kind\": \"physical-accounting\""));
        assert!(text.contains("\"total_bytes\""));
        assert!(text.contains("\"concealed_bytes\""));
        assert!(text.contains("\"physical_bytes\""));
        assert!(text.contains("data/full.bin"));

        // It is clearly flagged as accounting, not logical content.
        let attr = view.attr(CONTROL_INO).unwrap();
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.size, text.len() as u64);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
