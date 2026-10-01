use crate::{
    native::{self, DIRECTORY, Dir, Entry, FILE, Info, Name},
    progress::{Progress, Reporter},
    store::{Node, Store},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Instant,
};

const CONTROL: &str = "DCDATA";
const DATABASE: &str = "state.sqlite3";
const PAYLOAD: &str = "data";
const LOCK: &str = ".dircrypt.lock";
const CHUNK: i64 = 512;
const BUCKET_SIZE: usize = 1024;
const BUCKET_THRESHOLD: usize = 4096;

pub struct Options {
    pub obfuscate: bool,
    pub suffix: String,
    pub jobs: usize,
    pub quiet: bool,
    pub cancelled: Arc<AtomicBool>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            obfuscate: false,
            suffix: String::new(),
            jobs: 0,
            quiet: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}
#[derive(Default, Serialize, Debug)]
pub struct Outcome {
    pub action: String,
    pub files: u64,
    pub directories: u64,
    pub links: u64,
    pub seconds: f64,
    pub planning_seconds: f64,
    pub validation_seconds: f64,
    pub file_phase_seconds: f64,
    pub directory_phase_seconds: f64,
    pub identity_worker_seconds: f64,
    pub rename_worker_seconds: f64,
    pub header_worker_seconds: f64,
    pub close_worker_seconds: f64,
    pub renamed_entries: u64,
    pub jobs: usize,
    pub filesystem: String,
}
#[derive(Clone, Copy)]
struct Binding {
    ids: bool,
    portable: bool,
    time_slop: u64,
    cache_limit: usize,
}
#[derive(Default)]
struct Batch {
    completed: u64,
    checks: f64,
    renames: f64,
    headers: f64,
    closes: f64,
    moved: u64,
}
type Graph = Arc<HashMap<i64, Node>>;
type Inventory = HashMap<Vec<u8>, Info>;
type BucketCache = HashMap<u32, Option<(Arc<Dir>, Inventory)>>;

#[derive(Clone, Copy)]
struct Phase<'a> {
    root: &'a Arc<Dir>,
    data: &'a Arc<Dir>,
    graph: &'a Graph,
    binding: Binding,
    options: &'a Options,
    progress: &'a Progress,
}

pub fn execute(path: &Path, mut options: Options) -> Result<Outcome> {
    let started = Instant::now();
    ensure!(
        options.jobs <= 16,
        "At most 16 directory workers are supported"
    );
    let reporter = Reporter::new(options.quiet);
    let progress = reporter.progress();
    progress.phase("Opening target directory", 0);
    let path = fs::canonicalize(path).context("Resolving target directory")?;
    let root = Dir::root(&path)?;
    let filesystem = root.filesystem()?;
    native::check_volume(&filesystem)?;
    if options.jobs == 0 {
        options.jobs = if native::is_fat(&filesystem) { 1 } else { 2 };
    }
    let _lock = native::lock_directory(&path.join(LOCK))?;
    let restoring = match root.open_item(&Name::text(CONTROL), false) {
        Ok(_) => true,
        Err(error) if native::is_missing(&error) => false,
        Err(error) => return Err(error),
    };
    // Never reinterpret a previous implementation's unfinished plan as user data.
    if !restoring {
        ensure!(
            options
                .suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "Suffix may contain only ASCII letters, digits, dots, underscores and hyphens"
        );
        Name::text(&format!("~00000000000000{}", options.suffix)).validate()?;
        ensure!(
            !path.join("DCMETA.txt").try_exists()?,
            "Legacy DCMETA.txt found; restore with the old version first"
        );
    }
    let control = root.control_dir(&Name::text(CONTROL), !restoring)?;
    progress.phase(
        if restoring {
            "Opening recovery database"
        } else {
            "Creating recovery plan"
        },
        0,
    );
    let database_path = control.database_path(&path.join(CONTROL), DATABASE);
    if restoring && !database_path.try_exists()? {
        // Only empty initialization/finalization remnants can be removed without a plan.
        let entries = control.entries()?;
        ensure!(
            entries
                .iter()
                .all(|e| e.name == Name::text(PAYLOAD) && e.info.kind() == DIRECTORY),
            "DCDATA is not a new-format archive; its files have been preserved"
        );
        for entry in entries {
            control.open_dir(&entry.name)?.remove_empty()?;
        }
        control.remove_empty()?;
        return Ok(Outcome {
            action: "restored empty initialization".into(),
            seconds: started.elapsed().as_secs_f64(),
            filesystem,
            ..Default::default()
        });
    }
    if restoring {
        let handle = control.open_item(&Name::text(DATABASE), false)?;
        let info = handle.info()?;
        ensure!(
            info.kind() == FILE && info.links == 1,
            "Recovery database must be a regular file without shared hard links"
        );
    }
    let mut store = if restoring {
        Store::open(&database_path, true)?
    } else {
        Store::create(
            &database_path,
            root.info.clone(),
            filesystem.clone(),
            options.obfuscate,
        )?
    };
    let mut outcome = Outcome {
        action: if restoring { "restored" } else { "mapped" }.into(),
        filesystem: filesystem.clone(),
        ..Default::default()
    };
    if restoring && store.run.phase == "planning" {
        finish(store, &control)?;
        outcome.seconds = started.elapsed().as_secs_f64();
        return Ok(outcome);
    }
    if restoring && store.run.phase == "restored" {
        progress.phase("Finishing recovery cleanup", 0);
        store.validate(&progress)?;
        (outcome.files, outcome.directories, outcome.links) = store.counts()?;
        finish(store, &control)?;
        outcome.seconds = started.elapsed().as_secs_f64();
        return Ok(outcome);
    }
    let data = if restoring {
        control.open_dir(&Name::text(PAYLOAD))?
    } else {
        control.create_dir(&Name::text(PAYLOAD))?
    };
    if !restoring {
        progress.phase("Scanning directories and preparing recovery plan", 0);
        let timer = Instant::now();
        plan(&mut store, &root, &options, &progress)?;
        outcome.planning_seconds = timer.elapsed().as_secs_f64();
    }
    progress.phase(
        "Validating recovery records",
        store.run.entries.max(0) as u64,
    );
    let timer = Instant::now();
    store.validate(&progress)?;
    outcome.validation_seconds = timer.elapsed().as_secs_f64();
    let (files, dirs, links) = store.counts()?;
    outcome.files = files;
    outcome.directories = dirs;
    outcome.links = links;
    let graph: Graph = Arc::new(
        store
            .directories()?
            .into_iter()
            .map(|n| (n.id, n))
            .collect(),
    );
    let same_root = store.run.platform == std::env::consts::OS
        && filesystem == store.run.filesystem
        && native::root_matches(&root.info, &store.run.root, &filesystem);
    let portable = !same_root;
    ensure!(
        !portable || ["mapped", "restoring", "restored"].contains(&store.run.phase.as_str()),
        "An incomplete archive was copied or its root identity changed; preserve it and recover the original tree"
    );
    let binding = Binding {
        ids: !portable
            && native::stable_ids(&filesystem)
            && native::stable_ids(&store.run.filesystem),
        portable,
        cache_limit: native::cache_limit(options.jobs),
        time_slop: timestamp_resolution(&filesystem)
            .max(timestamp_resolution(&store.run.filesystem)),
    };
    if portable && !options.quiet {
        eprintln!(
            "[INFO] Copied archive: validating recorded names, sizes and timestamps; original file IDs are unavailable."
        );
    }
    outcome.jobs = options.jobs;
    store.phase(if restoring { "restoring" } else { "mapping" })?;
    control.sync()?;
    root.sync()?;
    fault("prepared");
    let phase = Phase {
        root: &root,
        data: &data,
        graph: &graph,
        binding,
        options: &options,
        progress: &progress,
    };
    if restoring {
        progress.phase("Restoring directory names", dirs);
        let timer = Instant::now();
        directory_phase(&mut store, phase, true, &mut outcome)?;
        outcome.directory_phase_seconds = timer.elapsed().as_secs_f64();
    }
    progress.phase(
        if restoring {
            "Restoring files and verifying untouched entries"
        } else if options.obfuscate {
            "Mapping filenames and headers"
        } else {
            "Mapping filenames (contents unchanged)"
        },
        files + links,
    );
    let timer = Instant::now();
    file_phase(&mut store, phase, restoring, &mut outcome)?;
    outcome.file_phase_seconds = timer.elapsed().as_secs_f64();
    if !restoring {
        progress.phase("Mapping directory names", dirs);
        let timer = Instant::now();
        directory_phase(&mut store, phase, false, &mut outcome)?;
        outcome.directory_phase_seconds = timer.elapsed().as_secs_f64();
        store.phase("mapped")?;
    } else {
        progress.phase("Removing empty storage directories", 0);
        clean_buckets(&store, &root, &data, &graph, binding)?;
        store.phase("restored")?;
        drop(data);
        finish(store, &control)?;
    }
    outcome.seconds = started.elapsed().as_secs_f64();
    Ok(outcome)
}

fn plan(store: &mut Store, root: &Arc<Dir>, options: &Options, progress: &Progress) -> Result<()> {
    struct Frame {
        parent: i64,
        dir: Arc<Dir>,
        entries: std::vec::IntoIter<Entry>,
        ordinal: usize,
        partition: bool,
    }
    let frame = |parent: i64, dir: Arc<Dir>| -> Result<Frame> {
        let mut entries = dir.entries()?;
        if parent == 0 {
            entries.retain(|e| {
                ![CONTROL, LOCK]
                    .iter()
                    .any(|n| Name::text(n).key() == e.name.key())
            });
        }
        let partition = entries.len() >= BUCKET_THRESHOLD
            && (native::is_fat(&store.run.filesystem) || parent == 0);
        if partition {
            let names: HashSet<_> = entries.iter().map(|e| e.name.key()).collect();
            for bucket in 1..=entries.len().div_ceil(BUCKET_SIZE) as u32 {
                ensure!(
                    !names.contains(&bucket_name(&store.run.prefix, bucket).key()),
                    "Storage subdirectory name conflicts with an existing entry"
                );
            }
        }
        Ok(Frame {
            parent,
            dir,
            entries: entries.into_iter(),
            ordinal: 0,
            partition,
        })
    };
    store.conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut stack = vec![frame(0, root.clone())?];
    let mut id = 0i64;
    let mut manifest = blake3::Hasher::new();
    while let Some(top) = stack.last_mut() {
        cancelled(options)?;
        let Some(entry) = top.entries.next() else {
            stack.pop();
            continue;
        };
        id += 1;
        ensure!(id <= u32::MAX as i64, "Archive has too many entries");
        let kind = entry.info.kind();
        let mut node = Node {
            id,
            parent: top.parent,
            kind,
            original: entry.name,
            mapped: Name::text(&format!(
                "~{}{:08x}{}",
                store.run.prefix,
                id,
                if kind == DIRECTORY {
                    ""
                } else {
                    &options.suffix
                }
            )),
            info: entry.info,
            header: None,
            bucket: if top.partition {
                (top.ordinal / BUCKET_SIZE + 1) as u32
            } else {
                0
            },
        };
        top.ordinal += 1;
        if options.obfuscate && kind == FILE && node.info.size > 16 {
            let mut handle = top.dir.open_item(&node.original, true)?;
            let actual = handle.info()?;
            verify(
                &actual,
                &node,
                false,
                Binding {
                    ids: native::stable_ids(&store.run.filesystem),
                    portable: false,
                    time_slop: timestamp_resolution(&store.run.filesystem),
                    cache_limit: native::cache_limit(options.jobs),
                },
            )?;
            ensure!(
                actual.links == 1,
                "Header obfuscation refuses shared hard links: {:?}",
                node.original.display()
            );
            node.header = Some(handle.header()?);
            node.info = actual;
        }
        store.insert(&node)?;
        manifest.update(&node.digest());
        if id % 256 == 0 {
            progress.advance(256);
            progress.current(&node.original.display());
        }
        if kind == DIRECTORY {
            let child = top.dir.open_dir(&node.original)?;
            verify(
                &child.info,
                &node,
                false,
                Binding {
                    ids: native::stable_ids(&store.run.filesystem),
                    portable: false,
                    time_slop: timestamp_resolution(&store.run.filesystem),
                    cache_limit: native::cache_limit(options.jobs),
                },
            )?;
            stack.push(frame(node.id, child)?);
        }
    }
    progress.advance(id as u64 % 256);
    store.conn.execute_batch("COMMIT")?;
    store.seal(id, manifest.finalize().as_bytes())?;
    fault("plan");
    Ok(())
}

struct Resolver {
    root: Arc<Dir>,
    graph: Graph,
    cache: HashMap<i64, Arc<Dir>>,
    binding: Binding,
}
impl Resolver {
    fn new(root: Arc<Dir>, graph: Graph, binding: Binding) -> Self {
        Self {
            cache: HashMap::from([(0, root.clone())]),
            root,
            graph,
            binding,
        }
    }
    fn get(&mut self, id: i64) -> Result<Arc<Dir>> {
        if let Some(dir) = self.cache.get(&id) {
            return Ok(dir.clone());
        }
        let mut chain = Vec::new();
        let mut cursor = id;
        while !self.cache.contains_key(&cursor) {
            let node = self
                .graph
                .get(&cursor)
                .context("Missing parent directory")?;
            chain.push(cursor);
            cursor = node.parent;
        }
        let mut parent = self.cache[&cursor].clone();
        for id in chain.into_iter().rev() {
            let node = &self.graph[&id];
            let child = parent.open_dir(&node.original)?;
            verify(&child.info, node, false, self.binding)?;
            if self.cache.len() >= self.binding.cache_limit {
                self.cache.clear();
                self.cache.insert(0, self.root.clone());
            }
            self.cache.insert(id, child.clone());
            parent = child;
        }
        Ok(parent)
    }
    fn release_subtree(&mut self, id: i64) {
        // NTFS can reject a directory move while descendant directory handles
        // remain open, even with delete sharing. Keep only unrelated/ancestor handles.
        self.cache.retain(|key, _| {
            let mut cursor = *key;
            while cursor >= id && cursor != 0 {
                if cursor == id {
                    return false;
                }
                cursor = self.graph[&cursor].parent;
            }
            true
        });
    }
}

fn bucket_name(prefix: &str, bucket: u32) -> Name {
    Name::text(&format!("~b{prefix}{bucket:06x}"))
}
fn bucket(base: &Arc<Dir>, number: u32, prefix: &str, create: bool) -> Result<Arc<Dir>> {
    if number == 0 {
        return Ok(base.clone());
    }
    let name = bucket_name(prefix, number);
    match base.open_dir(&name) {
        Ok(dir) => Ok(dir),
        Err(error) if create && native::is_missing(&error) => base.create_dir(&name),
        Err(error) => Err(error),
    }
}
fn verify(info: &Info, node: &Node, mapped: bool, binding: Binding) -> Result<()> {
    ensure!(
        info.kind() == node.kind,
        "Entry type changed for {:?}",
        node.original.display()
    );
    if node.kind == FILE {
        ensure!(
            info.size == node.info.size,
            "File size changed for {:?}",
            node.original.display()
        );
    }
    if binding.ids {
        ensure!(
            info.id != 0 && info.id == node.info.id && info.volume == node.info.volume,
            "File identity changed for {:?}",
            node.original.display()
        );
    } else if !binding.portable {
        ensure!(
            info.born == node.info.born,
            "Creation time changed for {:?}",
            node.original.display()
        );
    }
    if node.kind == FILE && (!mapped || node.header.is_none()) {
        ensure!(
            info.modified.abs_diff(node.info.modified) <= binding.time_slop,
            "Modification time changed for {:?} (difference {:.3}s)",
            node.original.display(),
            info.modified.abs_diff(node.info.modified) as f64 / 10_000_000.0
        );
    }
    Ok(())
}
fn timestamp_resolution(filesystem: &str) -> u64 {
    // FILETIME ticks are 100 ns. Windows SetFileTime/copy operations were measured
    // rounding UP to two seconds on exFAT too, despite its 10 ms on-disk fields.
    if ["exFAT", "FAT", "FAT32"]
        .iter()
        .any(|fs| filesystem.eq_ignore_ascii_case(fs))
    {
        20_000_000
    } else {
        0
    }
}
fn inventory(dir: &Dir) -> Result<HashMap<Vec<u8>, Info>> {
    Ok(dir
        .entries()?
        .into_iter()
        .map(|e| (e.name.key(), e.info))
        .collect())
}

fn directory_phase(
    store: &mut Store,
    phase: Phase<'_>,
    restoring: bool,
    outcome: &mut Outcome,
) -> Result<()> {
    let Phase {
        root,
        data,
        graph,
        binding,
        options,
        progress,
    } = phase;
    let mut directories: Vec<_> = graph.values().collect();
    directories.sort_by_key(|n| n.id);
    if !restoring {
        directories.reverse();
    }
    let mut resolver = Resolver::new(root.clone(), graph.clone(), binding);
    for node in directories {
        cancelled(options)?;
        progress.current(&node.original.display());
        resolver.release_subtree(node.id);
        let parent = resolver.get(node.parent)?;
        let base = if node.parent == 0 {
            data.clone()
        } else {
            parent.clone()
        };
        let target = bucket(&base, node.bucket, &store.run.prefix, !restoring);
        if restoring {
            let source = match target {
                Ok(dir) => Some(dir),
                Err(e) if native::is_missing(&e) => None,
                Err(e) => return Err(e),
            };
            let original = match parent.open_dir(&node.original) {
                Ok(dir) => Some(dir),
                Err(e) if native::is_missing(&e) => None,
                Err(e) => return Err(e),
            };
            let mapped = source
                .as_ref()
                .map(|dir| dir.open_dir(&node.mapped))
                .transpose();
            let mapped = match mapped {
                Ok(v) => v,
                Err(e) if native::is_missing(&e) => None,
                Err(e) => return Err(e),
            };
            match (original, mapped) {
                (Some(_), Some(_)) => bail!(
                    "Both directory names exist for {:?}; neither will be overwritten",
                    node.original.display()
                ),
                (Some(dir), None) => {
                    verify(&dir.info, node, false, binding)?;
                }
                (None, Some(dir)) => {
                    verify(&dir.info, node, true, binding)?;
                    source
                        .unwrap()
                        .rename_child(&node.mapped, &parent, &node.original)?;
                    outcome.renamed_entries += 1;
                    fault("rename");
                }
                (None, None) => bail!("Missing directory {:?}", node.original.display()),
            }
        } else {
            let handle = parent.open_item(&node.original, false)?;
            verify(&handle.info()?, node, false, binding)?;
            handle.rename(target?.as_ref(), &node.mapped)?;
            outcome.renamed_entries += 1;
            fault("rename");
        }
        progress.advance(1);
    }
    Ok(())
}

fn file_phase(
    store: &mut Store,
    phase: Phase<'_>,
    restoring: bool,
    outcome: &mut Outcome,
) -> Result<()> {
    let options = phase.options;
    let parents: Vec<i64> = {
        let mut query = store
            .conn
            .prepare("SELECT DISTINCT parent FROM nodes WHERE kind!=1 ORDER BY parent")?;
        query
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let jobs = options.jobs.min(parents.len()).max(1);
    let (sender, receiver) = mpsc::sync_channel::<Result<Batch>>(jobs * 2);
    let path = store.path.clone();
    let prefix = store.run.prefix.clone();
    let mut failure = None;
    thread::scope(|scope| {
        for _ in 0..jobs {
            let sender = sender.clone();
            let next = &next;
            let failed = &failed;
            let parents = &parents;
            let path = &path;
            let prefix = &prefix;
            scope.spawn(move || {
                let worker = FileWorker {
                    phase,
                    parents,
                    next,
                    failed,
                    path,
                    prefix,
                    restoring,
                };
                let result = worker.run(&sender);
                if let Err(error) = result {
                    failed.store(true, Ordering::Relaxed);
                    let _ = sender.send(Err(error));
                }
            });
        }
        drop(sender);
        let mut completed = 0;
        for result in receiver {
            match result {
                Ok(batch) => {
                    outcome.identity_worker_seconds += batch.checks;
                    outcome.rename_worker_seconds += batch.renames;
                    outcome.header_worker_seconds += batch.headers;
                    outcome.close_worker_seconds += batch.closes;
                    outcome.renamed_entries += batch.moved;
                    completed += batch.completed;
                    if completed >= 2048 {
                        completed = 0;
                        fault("checkpoint");
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
    });
    if let Some(error) = failure {
        return Err(error).context("Operation incomplete; keep DCDATA and run again to restore");
    }
    Ok(())
}

struct FileWorker<'a> {
    phase: Phase<'a>,
    parents: &'a [i64],
    next: &'a AtomicUsize,
    failed: &'a AtomicBool,
    path: &'a Path,
    prefix: &'a str,
    restoring: bool,
}
impl FileWorker<'_> {
    fn run(&self, sender: &mpsc::SyncSender<Result<Batch>>) -> Result<()> {
        let Self {
            phase,
            parents,
            next,
            failed,
            path,
            prefix,
            restoring,
        } = *self;
        let Phase {
            root,
            data,
            graph,
            binding,
            options,
            progress,
        } = phase;

        let reader = Store::open(path, false)?;
        let mut resolver = Resolver::new(root.clone(), graph.clone(), binding);
        loop {
            if failed.load(Ordering::Relaxed) {
                break;
            }
            cancelled(options)?;
            let index = next.fetch_add(1, Ordering::Relaxed);
            if index >= parents.len() {
                break;
            }
            let parent_id = parents[index];
            let parent = resolver.get(parent_id)?;
            let base = if parent_id == 0 {
                data.clone()
            } else {
                parent.clone()
            };
            let originals = if restoring {
                inventory(&parent)?
            } else {
                HashMap::new()
            };
            let mut buckets = BucketCache::new();
            let mut after = 0;
            loop {
                if failed.load(Ordering::Relaxed) {
                    break;
                }
                let nodes = reader.leaves(parent_id, after, CHUNK)?;
                if nodes.is_empty() {
                    break;
                }
                progress.current(&nodes[0].original.display());
                let mut batch = Batch::default();
                for node in nodes {
                    if failed.load(Ordering::Relaxed) {
                        break;
                    }
                    cancelled(options)?;
                    after = node.id;
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        buckets.entry(node.bucket)
                    {
                        let value = match bucket(&base, node.bucket, prefix, !restoring) {
                            Ok(dir) => {
                                let entries = if restoring {
                                    inventory(&dir)?
                                } else {
                                    HashMap::new()
                                };
                                Some((dir, entries))
                            }
                            Err(e) if restoring && native::is_missing(&e) => None,
                            Err(e) => return Err(e),
                        };
                        entry.insert(value);
                    }
                    let target = &buckets[&node.bucket];
                    if restoring {
                        let old = originals.get(&node.original.key());
                        let new = target
                            .as_ref()
                            .and_then(|(_, entries)| entries.get(&node.mapped.key()));
                        match (old, new) {
                            (Some(_), Some(_)) => bail!(
                                "Destination already exists for {:?}; nothing will be overwritten",
                                node.original.display()
                            ),
                            (Some(info), None) => verify(info, &node, false, binding)?,
                            (None, Some(_)) => {
                                let timer = Instant::now();
                                let mut handle = target
                                    .as_ref()
                                    .unwrap()
                                    .0
                                    .open_item(&node.mapped, node.header.is_some())?;
                                let info = handle.info()?;
                                verify(&info, &node, true, binding)?;
                                if node.header.is_some() {
                                    ensure!(
                                        info.links == 1,
                                        "Refusing to modify a shared hard link"
                                    );
                                }
                                if let Some(saved) = node.header {
                                    let current = handle.header()?;
                                    ensure!(
                                        current
                                            .iter()
                                            .zip(saved)
                                            .all(|(&byte, original)| byte == original
                                                || byte == original ^ 0x39),
                                        "Header differs from both saved states for {:?}; preserving the changed file",
                                        node.original.display()
                                    );
                                }
                                batch.checks += timer.elapsed().as_secs_f64();
                                if let Some(header) = node.header {
                                    let timer = Instant::now();
                                    handle.write_header(&header, &node.info, true)?;
                                    batch.headers += timer.elapsed().as_secs_f64();
                                    fault("header");
                                }
                                let timer = Instant::now();
                                handle.rename(&parent, &node.original)?;
                                batch.renames += timer.elapsed().as_secs_f64();
                                batch.moved += 1;
                                fault("rename");
                                let closing = Instant::now();
                                drop(handle);
                                batch.closes += closing.elapsed().as_secs_f64();
                            }
                            (None, None) => bail!(
                                "Both recorded locations are missing for {:?}",
                                node.original.display()
                            ),
                        }
                    } else {
                        let timer = Instant::now();
                        let mut handle = parent.open_item(&node.original, node.header.is_some())?;
                        let info = handle.info()?;
                        verify(&info, &node, false, binding)?;
                        if node.header.is_some() {
                            ensure!(info.links == 1, "Refusing to modify a shared hard link");
                        }
                        if let Some(saved) = node.header {
                            ensure!(
                                handle.header()? == saved,
                                "Header changed after planning for {:?}",
                                node.original.display()
                            );
                        }
                        batch.checks += timer.elapsed().as_secs_f64();
                        let timer = Instant::now();
                        handle.rename(&target.as_ref().unwrap().0, &node.mapped)?;
                        batch.renames += timer.elapsed().as_secs_f64();
                        batch.moved += 1;
                        fault("rename");
                        if let Some(header) = node.header {
                            let timer = Instant::now();
                            let encoded = header.map(|byte| byte ^ 0x39);
                            handle.write_header(&encoded, &node.info, false)?;
                            batch.headers += timer.elapsed().as_secs_f64();
                            fault("header");
                        }
                        let closing = Instant::now();
                        drop(handle);
                        batch.closes += closing.elapsed().as_secs_f64();
                    }
                    batch.completed += 1;
                    if batch.completed.is_multiple_of(64) {
                        progress.advance(64);
                        progress.current(&node.original.display());
                    }
                }
                progress.advance(batch.completed % 64);
                sender
                    .send(Ok(batch))
                    .map_err(|_| anyhow::anyhow!("Coordinator stopped"))?;
            }
        }
        Ok(())
    }
}

fn clean_buckets(
    store: &Store,
    root: &Arc<Dir>,
    data: &Arc<Dir>,
    graph: &Graph,
    binding: Binding,
) -> Result<()> {
    let mut query = store.conn.prepare(
        "SELECT DISTINCT parent,bucket FROM nodes WHERE bucket!=0 ORDER BY parent,bucket",
    )?;
    let groups: Vec<(i64, u32)> = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut resolver = Resolver::new(root.clone(), graph.clone(), binding);
    for (parent, bucket_id) in groups {
        let base = if parent == 0 {
            data.clone()
        } else {
            resolver.get(parent)?
        };
        match base.open_dir(&bucket_name(&store.run.prefix, bucket_id)) {
            Ok(dir) => dir.remove_empty().context(
                "Storage directory contains unexpected files; recovery database retained",
            )?,
            Err(e) if native::is_missing(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn finish(store: Store, control: &Arc<Dir>) -> Result<()> {
    ensure!(
        control
            .entries()?
            .iter()
            .all(|e| [DATABASE, PAYLOAD, "state.sqlite3-journal"]
                .iter()
                .any(|n| e.name == Name::text(n))),
        "Unexpected files in DCDATA; recovery records retained"
    );
    match control.open_dir(&Name::text(PAYLOAD)) {
        Ok(dir) => dir
            .remove_empty()
            .context("DCDATA/data is not empty; recovery records retained")?,
        Err(e) if native::is_missing(&e) => {}
        Err(e) => return Err(e),
    }
    fault("cleanup_data");
    drop(store);
    // The database closes before deletion; SQLite removes its own rollback journal.
    control
        .open_item(&Name::text(DATABASE), false)?
        .delete_empty()?;
    fault("cleanup");
    control.remove_empty()?;
    Ok(())
}
fn cancelled(options: &Options) -> Result<()> {
    ensure!(
        !options.cancelled.load(Ordering::Relaxed),
        "Interrupted; keep DCDATA and run again to restore"
    );
    Ok(())
}

#[cfg(debug_assertions)]
pub(crate) fn fault(label: &str) {
    use std::sync::OnceLock;
    static SETTING: OnceLock<(String, usize, AtomicUsize)> = OnceLock::new();
    let (point, after, count) = SETTING.get_or_init(|| {
        (
            std::env::var("DIRCRYPT_TEST_CRASH_POINT").unwrap_or_default(),
            std::env::var("DIRCRYPT_TEST_CRASH_AFTER")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            AtomicUsize::new(0),
        )
    });
    if point == label && count.fetch_add(1, Ordering::Relaxed) + 1 == *after {
        std::process::exit(77);
    }
}
#[cfg(not(debug_assertions))]
pub(crate) fn fault(_: &str) {}
