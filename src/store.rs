use crate::native::{DIRECTORY, FILE, Info, Name, database_guard, validate_database_sidecars};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const APPLICATION_ID: i64 = 0x44435232;
const VERSION: i64 = 2;
const COLUMNS: &str = "id,parent,kind,original,mapped,metadata,header,bucket,digest";

#[derive(Clone, Debug)]
pub struct Node {
    pub id: i64,
    pub parent: i64,
    pub kind: i64,
    pub original: Name,
    pub mapped: Name,
    pub info: Info,
    pub header: Option<[u8; 16]>,
    pub bucket: u32,
}
impl Node {
    pub fn digest(&self) -> Vec<u8> {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.id.to_le_bytes());
        hash.update(&self.parent.to_le_bytes());
        hash.update(&self.kind.to_le_bytes());
        hash.update(&self.bucket.to_le_bytes());
        for bytes in [
            self.original.bytes(),
            self.mapped.bytes(),
            encode_info(&self.info),
            self.header.map(|v| v.to_vec()).unwrap_or_default(),
        ] {
            hash.update(&(bytes.len() as u32).to_le_bytes());
            hash.update(&bytes);
        }
        hash.finalize().as_bytes().to_vec()
    }
    fn validate(&self, digest: &[u8]) -> Result<()> {
        ensure!(
            self.info.modified_subtick < 100,
            "Invalid sub-tick timestamp"
        );
        self.original.validate()?;
        self.mapped.validate()?;
        ensure!(
            self.id > 0 && self.parent >= 0 && self.parent < self.id,
            "Invalid directory graph"
        );
        ensure!(self.kind == self.info.kind(), "Inconsistent entry type");
        ensure!(
            self.header.is_none() || (self.kind == FILE && self.info.size > 16),
            "Invalid header backup"
        );
        ensure!(
            self.digest() == digest,
            "Recovery record {} failed its checksum; no file changes permitted",
            self.id
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct Run {
    pub phase: String,
    pub prefix: String,
    pub filesystem: String,
    pub platform: String,
    pub header: bool,
    pub root: Info,
    pub entries: i64,
    pub manifest: Vec<u8>,
}
impl Run {
    fn digest(&self) -> Vec<u8> {
        let mut hash = blake3::Hasher::new();
        for bytes in [
            self.phase.as_bytes(),
            self.prefix.as_bytes(),
            self.filesystem.as_bytes(),
            self.platform.as_bytes(),
            &encode_info(&self.root),
        ] {
            hash.update(&(bytes.len() as u32).to_le_bytes());
            hash.update(bytes);
        }
        hash.update(&[u8::from(self.header)]);
        hash.update(&self.entries.to_le_bytes());
        hash.update(&self.manifest);
        hash.finalize().as_bytes().to_vec()
    }
}

pub struct Store {
    pub conn: Connection,
    pub path: PathBuf,
    pub run: Run,
    _guard: File,
}
impl Store {
    pub fn create(path: &Path, root: Info, filesystem: String, header: bool) -> Result<Self> {
        let guard = database_guard(path, true)?;
        validate_database_sidecars(path)?;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&conn, true)?;
        conn.execute_batch("BEGIN IMMEDIATE;
            PRAGMA application_id=1145262642;
            PRAGMA user_version=2;
            CREATE TABLE run(singleton INTEGER PRIMARY KEY CHECK(singleton=1),phase TEXT NOT NULL,
                prefix TEXT NOT NULL,filesystem TEXT NOT NULL,platform TEXT NOT NULL,header INTEGER NOT NULL,root BLOB NOT NULL,
                entries INTEGER NOT NULL,manifest BLOB NOT NULL,digest BLOB NOT NULL);
            CREATE TABLE nodes(id INTEGER PRIMARY KEY,parent INTEGER NOT NULL CHECK(parent>=0 AND parent<id),
                kind INTEGER NOT NULL,original BLOB NOT NULL,mapped BLOB NOT NULL,
                old_key BLOB NOT NULL,new_key BLOB NOT NULL,metadata BLOB NOT NULL,header BLOB,
                bucket INTEGER NOT NULL,digest BLOB NOT NULL,
                UNIQUE(parent,old_key),UNIQUE(parent,new_key));
            CREATE INDEX children ON nodes(parent,id);")?;
        let mut nonce = [0u8; 3];
        getrandom::fill(&mut nonce)
            .map_err(|e| anyhow::anyhow!("Random generation failed: {e}"))?;
        let run = Run {
            phase: "planning".into(),
            prefix: nonce.iter().map(|b| format!("{b:02x}")).collect(),
            filesystem,
            platform: std::env::consts::OS.into(),
            header,
            root,
            entries: 0,
            manifest: blake3::hash(&[]).as_bytes().to_vec(),
        };
        conn.execute(
            "INSERT INTO run VALUES(1,?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                run.phase,
                run.prefix,
                run.filesystem,
                run.platform,
                run.header,
                encode_info(&run.root),
                run.entries,
                run.manifest,
                run.digest()
            ],
        )?;
        conn.execute_batch("COMMIT")?;
        Ok(Self {
            conn,
            path: path.into(),
            run,
            _guard: guard,
        })
    }
    pub fn open(path: &Path, writable: bool) -> Result<Self> {
        let mut guard = database_guard(path, false)?;
        validate_database_sidecars(path)?;
        let mut header = [0u8; 100];
        guard
            .read_exact(&mut header)
            .context("Incomplete recovery database header; preserve DCDATA")?;
        ensure!(
            &header[..16] == b"SQLite format 3\0"
                && u32::from_be_bytes(header[68..72].try_into()?) == APPLICATION_ID as u32
                && u32::from_be_bytes(header[60..64].try_into()?) == VERSION as u32,
            "Unrecognized recovery database; no file changes permitted"
        );
        let conn = Connection::open_with_flags(
            path,
            if writable {
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
            } else {
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
            },
        )
        .context("Opening recovery database (old dircrypt formats are not supported)")?;
        configure(&conn, writable)?;
        let app: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(
            app == APPLICATION_ID && version == VERSION,
            "Unrecognized recovery format; use the version that created this archive"
        );
        let (run,digest) = conn.query_row(
            "SELECT phase,prefix,filesystem,header,root,entries,manifest,digest,platform FROM run WHERE singleton=1", [],
            |r| Ok((Run { phase:r.get(0)?,prefix:r.get(1)?,filesystem:r.get(2)?,header:r.get(3)?,
                root:decode_info(&r.get::<_,Vec<u8>>(4)?).map_err(|e|rusqlite::Error::FromSqlConversionFailure(4,rusqlite::types::Type::Blob,e.into()))?,
                entries:r.get(5)?,manifest:r.get(6)?,platform:r.get(8)? },r.get::<_,Vec<u8>>(7)?)))?;
        ensure!(
            run.prefix.len() == 6 && run.prefix.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid archive prefix"
        );
        ensure!(
            ["planning", "mapping", "mapped", "restoring", "restored"]
                .contains(&run.phase.as_str()),
            "Invalid recovery phase"
        );
        ensure!(
            run.digest() == digest,
            "Recovery control record failed its checksum"
        );
        Ok(Self {
            conn,
            path: path.into(),
            run,
            _guard: guard,
        })
    }
    pub fn phase(&mut self, value: &str) -> Result<()> {
        let mut run = self.run.clone();
        run.phase = value.into();
        self.conn.execute(
            "UPDATE run SET phase=?1,digest=?2 WHERE singleton=1",
            params![run.phase, run.digest()],
        )?;
        self.run = run;
        Ok(())
    }
    pub fn seal(&mut self, entries: i64, manifest: &[u8]) -> Result<()> {
        let mut run = self.run.clone();
        run.entries = entries;
        run.manifest = manifest.to_vec();
        self.conn.execute(
            "UPDATE run SET entries=?1,manifest=?2,digest=?3 WHERE singleton=1",
            params![run.entries, run.manifest, run.digest()],
        )?;
        self.run = run;
        Ok(())
    }
    pub fn insert(&self, node: &Node) -> Result<()> {
        self.conn.prepare_cached("INSERT INTO nodes(id,parent,kind,original,mapped,old_key,new_key,metadata,header,bucket,digest)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)")?.execute(params![node.id,node.parent,node.kind,
                node.original.bytes(),node.mapped.bytes(),node.original.key(),node.mapped.key(),encode_info(&node.info),
                node.header.map(|h|h.to_vec()),node.bucket,node.digest()])?;
        Ok(())
    }
    pub fn directories(&self) -> Result<Vec<Node>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM nodes WHERE kind={DIRECTORY} ORDER BY id"),
            [],
        )
    }
    pub fn leaves(&self, parent: i64, after: i64, limit: i64) -> Result<Vec<Node>> {
        self.query(&format!("SELECT {COLUMNS} FROM nodes WHERE parent=?1 AND kind!={DIRECTORY} AND id>?2 ORDER BY id LIMIT ?3"), params![parent,after,limit])
    }
    fn query<P: rusqlite::Params>(&self, sql: &str, params: P) -> Result<Vec<Node>> {
        let mut statement = self.conn.prepare_cached(sql)?;
        let mut rows = statement.query(params)?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(decode_node(row)?);
        }
        Ok(result)
    }
    pub fn counts(&self) -> Result<(u64, u64, u64)> {
        Ok(self.conn.query_row("SELECT coalesce(sum(kind=0),0),coalesce(sum(kind=1),0),coalesce(sum(kind=2),0) FROM nodes", [], |r| Ok((r.get::<_,i64>(0)? as u64,r.get::<_,i64>(1)? as u64,r.get::<_,i64>(2)? as u64)))?)
    }
    pub fn validate(&self, progress: &crate::progress::Progress) -> Result<()> {
        let check: String = self
            .conn
            .pragma_query_value(None, "quick_check", |r| r.get(0))?;
        ensure!(
            check == "ok",
            "Recovery database integrity check failed: {check}"
        );
        let unsupported: i64 = self.conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type IN('trigger','view')",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            unsupported == 0,
            "Unexpected executable schema objects in recovery database"
        );
        let broken: i64 = self.conn.query_row("SELECT count(*) FROM nodes n LEFT JOIN nodes p ON n.parent=p.id WHERE n.parent!=0 AND (p.id IS NULL OR p.kind!=1)", [], |r|r.get(0))?;
        ensure!(broken == 0, "Broken recovery directory graph");
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM nodes ORDER BY id"))?;
        let mut rows = statement.query([])?;
        let mut manifest = blake3::Hasher::new();
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let node = decode_node(row)?;
            manifest.update(&node.digest());
            count += 1;
            if count % 512 == 0 {
                progress.advance(512);
            }
        }
        progress.advance(count as u64 % 512);
        ensure!(
            count == self.run.entries
                && manifest.finalize().as_bytes().as_slice() == self.run.manifest,
            "Recovery manifest is incomplete or changed; recovery records retained"
        );
        let collisions: i64 = self.conn.query_row("SELECT count(*) FROM nodes n JOIN nodes o ON n.parent=o.parent AND n.new_key=o.old_key", [], |r|r.get(0))?;
        ensure!(
            collisions == 0,
            "Generated name conflicts with an original name; retry with a new archive prefix"
        );
        Ok(())
    }
}

fn configure(conn: &Connection, write: bool) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA trusted_schema=OFF; PRAGMA cache_size=-8192; PRAGMA temp_store=MEMORY;",
    )?;
    if write {
        conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=EXTRA;")?;
    }
    Ok(())
}

fn decode_node(row: &rusqlite::Row<'_>) -> Result<Node> {
    let header: Option<Vec<u8>> = row.get(6)?;
    let node = Node {
        id: row.get(0)?,
        parent: row.get(1)?,
        kind: row.get(2)?,
        original: Name::from_bytes(&row.get::<_, Vec<u8>>(3)?)?,
        mapped: Name::from_bytes(&row.get::<_, Vec<u8>>(4)?)?,
        info: decode_info(&row.get::<_, Vec<u8>>(5)?)?,
        header: header
            .map(|b| {
                b.try_into()
                    .map_err(|_| anyhow::anyhow!("Invalid saved header length"))
            })
            .transpose()?,
        bucket: row.get(7)?,
    };
    node.validate(&row.get::<_, Vec<u8>>(8)?)?;
    Ok(node)
}
fn encode_info(info: &Info) -> Vec<u8> {
    let mut b = Vec::with_capacity(52);
    b.extend(info.id.to_le_bytes());
    b.extend(info.volume.to_le_bytes());
    b.extend(info.size.to_le_bytes());
    b.extend(info.born.to_le_bytes());
    b.extend(info.modified.to_le_bytes());
    b.extend(info.attributes.to_le_bytes());
    b.extend(info.links.to_le_bytes());
    b.extend(info.modified_subtick.to_le_bytes());
    b
}
fn decode_info(b: &[u8]) -> Result<Info> {
    ensure!(b.len() == 52, "Invalid filesystem identity record");
    Ok(Info {
        id: u64::from_le_bytes(b[0..8].try_into()?),
        volume: u64::from_le_bytes(b[8..16].try_into()?),
        size: u64::from_le_bytes(b[16..24].try_into()?),
        born: u64::from_le_bytes(b[24..32].try_into()?),
        modified: u64::from_le_bytes(b[32..40].try_into()?),
        attributes: u32::from_le_bytes(b[40..44].try_into()?),
        links: u32::from_le_bytes(b[44..48].try_into()?),
        modified_subtick: u32::from_le_bytes(b[48..52].try_into()?),
    })
}
