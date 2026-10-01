# Engine invariants

The Rust engine runs on Windows, macOS and Linux with no Python runtime or legacy metadata adapter. Recovery format 2 is shared across platforms.

## Durable plan, physical reconciliation

`Store` creates an exclusive new SQLite database inside a verified `DCDATA` directory. Existing databases are checked for the application ID and schema version before SQLite opens them for writing. On Windows, non-reparse file handles pin the database and control directory names. Linux addresses SQLite through the open control directory under `/proc/self/fd`; macOS uses its verified pathname. Unix no-follow opens reject database and lock symlinks and shared hard links. All connections close before cleanup. Unix locks are advisory: concurrent external changes, including renaming the root/control directory, are outside the supported operating model.

The `run` row holds the phase, namespace prefix, source OS, filesystem identity, entry count and manifest checksum. `nodes` holds parent IDs, tagged original/mapped name components (UTF-8 for Unicode; raw native encoding otherwise), file identity, optional 16-byte backup and bucket number. Parents must precede children, so the graph cannot contain cycles. A `(parent,id)` index supports keyset pagination without repeatedly sorting or scanning a wide directory. Checksums cover immutable records and the complete ordered manifest. No absolute paths are stored. Unsupported native encodings are rejected before the recovery mutation phases. Identity timestamps use FILETIME-compatible ticks plus a sub-tick remainder to preserve Unix nanoseconds.

The complete plan commits before `planning → mapping`. No per-entry completion updates are necessary: node records remain immutable, and recovery examines both physical locations. SQLite uses DELETE journaling and EXTRA synchronization. Connections are thread-local; the coordinator owns phase changes. A transaction protects the plan, not the filesystem operations.

| Phase | Filesystem invariant | Next invocation |
|---|---|---|
| planning | User entries have not been modified | Remove empty initialization storage |
| mapping | Some leaves and/or directories may be mapped | Restore directories, then reconcile leaves |
| mapped | Entire planned tree has been mapped | Restore automatically |
| restoring | Original and mapped names may coexist for different entries | Reconcile each entry again |
| restored | All planned entries are back; empty storage may remain | Finish cleanup |

Mapping leaves before directories keeps parents addressable by their original names. Mapping directories in descending ID order is a postorder traversal. Restoration processes directories in ascending ID order, re-establishing ancestors before descendants. Cached descendant directory handles are released before moving their ancestor: NTFS can reject that move even with delete sharing.

For each restored leaf, exactly one recorded name must exist. Two names mean a conflict; neither means a missing entry. An original-only file is verified and left alone. A mapped-only file is opened by handle, checked, optionally repaired from its header backup, and renamed with replacement disabled. This works when a process exits immediately after any rename, without a committed completion marker.

## Native filesystem boundary

The absolute target is resolved once. Child operations use `NtCreateFile` with `RootDirectory` and exactly one validated component. Reparse points are opened as objects; they are never followed inside the tree. `NtSetInformationFile(FileRenameInformation)` renames the verified source handle into a verified destination handle, with replacement disabled. Persistent Windows rename-destination handles request only traverse/attribute access to avoid sharing conflicts on older Windows kernels. Temporary enumeration and deletion handles reopen the same directory object with an empty NT relative name and close before child renames. Windows directory enumeration retrieves names and identity information in 64 KiB batches, avoiding one metadata open per file during name-only planning.

Linux/macOS use `openat`, `fstatat(AT_SYMLINK_NOFOLLOW)`, `fdopendir`/`readdir` and directory descriptors. Scans open a fresh directory stream (not a shared duplicated offset). Linux name-only handles use `O_PATH`; macOS uses `O_EVTONLY | O_SYMLINK`. Linux `renameat2(RENAME_NOREPLACE)` and macOS `renameatx_np(RENAME_EXCL)` provide atomic destination exclusion; there is no check-then-overwriting-rename fallback. Source identity is checked immediately before rename, but Unix has no Windows-style rename-by-handle guarantee against a hostile concurrent source replacement. Header operations use a no-follow data handle and an advisory exclusive lock.

Special files and cross-device child mounts fail during planning. Supported Unix local filesystems use device/inode identity; FAT-family identity is weaker. The root identity and source OS distinguish a copied archive from an original tree. Unix directory fsync persists the recovery directory entries before user-file mutation; ordinary renames still do not force a synchronous device flush per file.

File IDs are checked on an original NTFS, APFS/HFS+ or supported Linux native tree. FAT-family IDs are not used as durable identity because moves may change them; creation time (where available), type, size and write time provide weaker checks; Linux POSIX stat does not expose FAT birth times. A complete copied archive likewise uses weaker checks because its original IDs cannot survive the copy. Observed FAT timestamp rounding is accounted for. The system does not claim detection of all same-size edits with indistinguishable timestamps or hostile metadata manipulation.

Header mode is explicitly opt-in. Shared hard links are rejected before modifying any user entry. A Windows write handle denies other writers while open; Unix relies on cooperative locking. Encoding first renames the file and then writes deterministic obfuscated bytes derived from the saved header; decoding writes original bytes, flushes, then renames. Recovery overwrites a partially written header idempotently. Files of 16 bytes or less and all reparse points are unchanged internally.

## Work and memory bounds

Files are fetched from SQLite in batches of 512. Work is assigned per directory, with a bounded result channel. A worker caches at most 256 source directory handles (further reduced on Unix according to `RLIMIT_NOFILE` and worker count) and closes each leaf handle after processing it. Bucket handles and directory inventories live only while processing that parent. The directory graph stays in memory, as does the largest individual directory enumeration; this is not a constant-memory streaming implementation for arbitrarily wide directories. Default workers: FAT family 1, other supported local filesystems 2. There is no unbounded per-file task queue.

Progress uses a separate reporting thread and per-64-file updates, independent of filesystem stalls. SQLite validation reports every 512 records. Timings ending in `worker_seconds` accumulate time across workers and cannot be added directly to wall-clock phase durations.

No function recursively deletes user data. Finalization deletes empty storage, closes SQLite, deletes its database, then removes the empty control directory. A dedicated `restored` phase allows retry after payload deletion but before database deletion. Unknown extra files cause an error with recovery data retained whenever it still exists.

## Durability boundary

The tested failure model is a terminated process, cooperative cancellation, ordinary I/O failure, sharing conflicts and detectable metadata corruption. Sudden power loss, defective media, concurrent third-party edits and loss of the recovery database are outside a guarantee of lossless recovery. Per-file flushes are used when restoring headers; name-only mapping does not force a device flush for every rename. This avoids turning 130,000 metadata operations into 130,000 synchronous device flushes.

References: [SQLite atomic commit](https://www.sqlite.org/atomiccommit.html), [SQLite synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous), [Windows file times](https://learn.microsoft.com/en-us/windows/win32/sysinfo/file-times), [exFAT timestamp fields](https://learn.microsoft.com/en-us/windows/win32/fileio/exfat-specification#749-10msincrement-fields).


## Release gates

The Actions matrix tests native Windows AMD64, macOS ARM64 and Linux AMD64/musl binaries. Debug builds exercise real process crashes; each release binary also creates and restores fresh archives. A second matrix restores all six archives (three origins, two modes) on each platform. Archives use representable Unicode names and preserved timestamps. A failed matrix blocks publication. Assets upload into a draft before it becomes a prerelease; checksums are verified again in the publisher. Action revisions and the Rust toolchain are pinned.
