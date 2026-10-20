//! Unlocked vault storage: SQLCipher schema (M1), object registry and AEAD blobs.
//!
//! Disk objects use random names; original names, hashes and every derived
//! artefact (pages, OCR, text, vectors, snapshots, exports) live encrypted in
//! the database or in LVJ2 blobs bound to (vault, blob id, purpose).
use crate::{
    Error, Result,
    crypto::{open_stream, seal_stream},
    ledger::Ledger,
    runtime::{Registry, Runtime},
    vault::{DB_FILE, OBJECTS_DIR, VaultHeader, VaultKeys, now_rfc3339},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

pub const SCHEMA_VERSION: i64 = 4;

const M1_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
ALTER TABLE roots ADD COLUMN action TEXT NOT NULL DEFAULT 'ui';
ALTER TABLE roots ADD COLUMN created_at TEXT;
ALTER TABLE objects ADD COLUMN created_at TEXT;
ALTER TABLE objects ADD COLUMN parent TEXT;
CREATE INDEX IF NOT EXISTS objects_parent ON objects(parent);
CREATE INDEX IF NOT EXISTS objects_kind ON objects(kind, active);
CREATE TABLE IF NOT EXISTS blobs(id TEXT PRIMARY KEY, owner TEXT NOT NULL, purpose TEXT NOT NULL, file TEXT NOT NULL UNIQUE, bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS blobs_owner ON blobs(owner);
CREATE TABLE IF NOT EXISTS matters(id TEXT PRIMARY KEY REFERENCES objects(id), title TEXT NOT NULL, matter_type TEXT NOT NULL, region TEXT, status TEXT NOT NULL, is_root INTEGER NOT NULL DEFAULT 0, notes TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS documents(id TEXT PRIMARY KEY, matter_id TEXT NOT NULL REFERENCES matters(id), display_name TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS document_versions(id TEXT PRIMARY KEY REFERENCES objects(id), document_id TEXT NOT NULL REFERENCES documents(id), version_no INTEGER NOT NULL, content_sha256 TEXT NOT NULL, bytes INTEGER NOT NULL, media_kind TEXT NOT NULL, original_blob TEXT NOT NULL, status TEXT NOT NULL, page_count INTEGER NOT NULL DEFAULT 0, error_code TEXT, details TEXT, created_at TEXT NOT NULL, UNIQUE(document_id, version_no));
CREATE INDEX IF NOT EXISTS versions_hash ON document_versions(content_sha256);
CREATE TABLE IF NOT EXISTS pages(id TEXT PRIMARY KEY REFERENCES objects(id), version_id TEXT NOT NULL REFERENCES document_versions(id), page_no INTEGER NOT NULL, source TEXT NOT NULL, width REAL, height REAL, image_blob TEXT, derived_blob TEXT, rotation_turns INTEGER NOT NULL DEFAULT 0, uvdoc INTEGER NOT NULL DEFAULT 0, geometry TEXT, ocr_status TEXT NOT NULL DEFAULT 'none', created_at TEXT NOT NULL, UNIQUE(version_id, page_no));
CREATE TABLE IF NOT EXISTS blocks(rowid INTEGER PRIMARY KEY, version_id TEXT NOT NULL REFERENCES document_versions(id), page_id TEXT, ordinal INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, locator TEXT NOT NULL, confidence REAL, source TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS blocks_version ON blocks(version_id, ordinal);
CREATE TABLE IF NOT EXISTS chunks(rowid INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE REFERENCES objects(id), version_id TEXT NOT NULL REFERENCES document_versions(id), matter_id TEXT NOT NULL, ordinal INTEGER NOT NULL, unit INTEGER NOT NULL, unit_title TEXT NOT NULL, text TEXT NOT NULL, body TEXT NOT NULL, token_count INTEGER NOT NULL, spans TEXT NOT NULL, chunker TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS chunks_version ON chunks(version_id, ordinal);
CREATE INDEX IF NOT EXISTS chunks_matter ON chunks(matter_id);
CREATE VIRTUAL TABLE IF NOT EXISTS chunk_fts USING fts5(tokens, tokenize='unicode61');
INSERT INTO chunk_fts(chunk_fts, rank) VALUES('secure-delete', 1);
CREATE TABLE IF NOT EXISTS chunk_vectors(chunk_rowid INTEGER PRIMARY KEY REFERENCES chunks(rowid), space TEXT NOT NULL, vector BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS index_state(id INTEGER PRIMARY KEY CHECK(id = 1), generation INTEGER NOT NULL, space TEXT NOT NULL, snapshot_blob TEXT, vector_count INTEGER NOT NULL, dirty INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS media(id TEXT PRIMARY KEY REFERENCES objects(id), version_id TEXT NOT NULL REFERENCES document_versions(id), probe TEXT NOT NULL, duration_s REAL, has_audio INTEGER NOT NULL, has_video INTEGER NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS audio_segments(id TEXT PRIMARY KEY REFERENCES objects(id), media_id TEXT NOT NULL REFERENCES media(id), seq INTEGER NOT NULL, start_s REAL NOT NULL, end_s REAL NOT NULL, samples INTEGER NOT NULL, pcm_blob TEXT NOT NULL, transcript TEXT, raw_output TEXT, language TEXT, truncated INTEGER, status TEXT NOT NULL, created_at TEXT NOT NULL, UNIQUE(media_id, seq));
CREATE TABLE IF NOT EXISTS frames(id TEXT PRIMARY KEY REFERENCES objects(id), media_id TEXT NOT NULL REFERENCES media(id), seq INTEGER NOT NULL, pts INTEGER, time_s REAL NOT NULL, page_id TEXT, created_at TEXT NOT NULL, UNIQUE(media_id, seq));
CREATE TABLE IF NOT EXISTS facts(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL REFERENCES matters(id), fact_type TEXT NOT NULL, value TEXT NOT NULL, label TEXT, source_version TEXT, source_locator TEXT, status TEXT NOT NULL, origin TEXT NOT NULL, created_at TEXT NOT NULL, confirmed_at TEXT);
CREATE INDEX IF NOT EXISTS facts_matter ON facts(matter_id, status);
CREATE TABLE IF NOT EXISTS accounts(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL REFERENCES matters(id), name TEXT NOT NULL, currency TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cashflows(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL REFERENCES matters(id), account_id TEXT, occurred_on TEXT NOT NULL, amount TEXT NOT NULL, currency TEXT NOT NULL, direction TEXT NOT NULL, kind TEXT NOT NULL, category TEXT, counterparty TEXT, description TEXT, source_version TEXT, source_locator TEXT, status TEXT NOT NULL, dedup_key TEXT, created_at TEXT NOT NULL, confirmed_at TEXT);
CREATE INDEX IF NOT EXISTS cashflows_matter ON cashflows(matter_id, status);
CREATE TABLE IF NOT EXISTS todos(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL REFERENCES matters(id), title TEXT NOT NULL, due_on TEXT, status TEXT NOT NULL, source_fact TEXT, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS relations(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL REFERENCES matters(id), src TEXT NOT NULL, dst TEXT NOT NULL, relation_type TEXT NOT NULL, evidence TEXT, origin TEXT NOT NULL, status TEXT NOT NULL, valid_from TEXT, valid_to TEXT, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS relations_src ON relations(src);
CREATE INDEX IF NOT EXISTS relations_dst ON relations(dst);
CREATE TABLE IF NOT EXISTS assumptions(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL, kind TEXT NOT NULL, value TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS changes(id TEXT PRIMARY KEY REFERENCES objects(id), matter_id TEXT NOT NULL, change TEXT NOT NULL, impact TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, committed_at TEXT);
CREATE TABLE IF NOT EXISTS results(id TEXT PRIMARY KEY REFERENCES objects(id), skill TEXT NOT NULL, invocation TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS selections(id TEXT PRIMARY KEY REFERENCES objects(id), kind TEXT NOT NULL, path TEXT NOT NULL, display_name TEXT NOT NULL, bytes INTEGER, sha256 TEXT, purpose TEXT NOT NULL, created_at TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS exports(id TEXT PRIMARY KEY REFERENCES objects(id), format TEXT NOT NULL, result_refs TEXT NOT NULL, content_blob TEXT NOT NULL, content_sha256 TEXT NOT NULL, status TEXT NOT NULL, created_at TEXT NOT NULL, committed_at TEXT);
CREATE TABLE IF NOT EXISTS commit_log(idempotency_key TEXT PRIMARY KEY, root TEXT NOT NULL, kind TEXT NOT NULL, target TEXT, outcome TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY, root TEXT NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL, status TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS questions(id TEXT PRIMARY KEY REFERENCES objects(id), text TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scopes(id TEXT PRIMARY KEY REFERENCES objects(id), matters TEXT NOT NULL, versions TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS filters(id TEXT PRIMARY KEY REFERENCES objects(id), kind TEXT NOT NULL, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS root_usage(root TEXT NOT NULL, key TEXT NOT NULL, value REAL NOT NULL, PRIMARY KEY(root, key));
CREATE TABLE IF NOT EXISTS ui_requests(id TEXT PRIMARY KEY REFERENCES objects(id), operation TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS ranges(id TEXT PRIMARY KEY REFERENCES objects(id), media_id TEXT NOT NULL, start_s REAL NOT NULL, end_s REAL, created_at TEXT NOT NULL);
"#;

/// M2: user-managed generation connections, capability records, request audit
/// and destination-bound egress grants. Keys are not stored here (DPAPI files).
const M2_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY REFERENCES objects(id), name TEXT NOT NULL, provider TEXT NOT NULL, protocol TEXT NOT NULL, base_url TEXT NOT NULL, model_id TEXT NOT NULL, timeout_s INTEGER NOT NULL, max_output_tokens INTEGER NOT NULL, proxy TEXT NOT NULL, credential TEXT, status TEXT NOT NULL, is_default INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, revoked_at TEXT);
CREATE TABLE IF NOT EXISTS connection_drafts(id TEXT PRIMARY KEY REFERENCES objects(id), connection_id TEXT, profile TEXT NOT NULL, credential TEXT, created_at TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS capabilities(connection_id TEXT NOT NULL, capability TEXT NOT NULL, binding TEXT NOT NULL, status TEXT NOT NULL, evidence TEXT NOT NULL, limits TEXT NOT NULL, tested_at TEXT NOT NULL, PRIMARY KEY(connection_id, capability));
CREATE TABLE IF NOT EXISTS generation_requests(id TEXT PRIMARY KEY, root TEXT NOT NULL, invocation TEXT NOT NULL, connection_id TEXT NOT NULL, purpose TEXT NOT NULL, host TEXT NOT NULL, model_id TEXT NOT NULL, status TEXT NOT NULL, error TEXT, input_tokens INTEGER, output_tokens INTEGER, materials TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT);
CREATE INDEX IF NOT EXISTS generation_requests_root ON generation_requests(root);
CREATE TABLE IF NOT EXISTS egress_grants(id TEXT PRIMARY KEY REFERENCES objects(id), root TEXT, connection_id TEXT NOT NULL, host TEXT NOT NULL, purpose TEXT NOT NULL, scope_id TEXT NOT NULL, question_id TEXT NOT NULL, max_items INTEGER NOT NULL, max_requests INTEGER NOT NULL, preview TEXT NOT NULL, created_at TEXT NOT NULL, approved_at TEXT, revoked_at TEXT);
"#;

/// M2 (second segment): LangGraph flow runs. The orchestrator's checkpoints,
/// channel blobs and pending writes are opaque serialised bytes it alone
/// reads back; receipts make every state-changing call idempotent across a
/// continued run. Context packets, goals and native media payload records
/// support the narrow roles.
const M3_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS flow_runs(id TEXT PRIMARY KEY, root TEXT NOT NULL, owner TEXT NOT NULL, owner_input TEXT NOT NULL, skill TEXT NOT NULL, flow TEXT NOT NULL, flow_version TEXT, inputs TEXT NOT NULL, snapshot_generation INTEGER NOT NULL, status TEXT NOT NULL, reason TEXT, attempts INTEGER NOT NULL DEFAULT 1, requests INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0, output TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS flow_runs_root ON flow_runs(root, status);
CREATE TABLE IF NOT EXISTS flow_owners(run TEXT NOT NULL REFERENCES flow_runs(id), invocation TEXT NOT NULL, input TEXT NOT NULL, PRIMARY KEY(run, invocation));
CREATE TABLE IF NOT EXISTS flow_checkpoints(run TEXT NOT NULL REFERENCES flow_runs(id), ns TEXT NOT NULL, checkpoint_id TEXT NOT NULL, parent_id TEXT, checkpoint TEXT NOT NULL, metadata TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(run, ns, checkpoint_id));
CREATE TABLE IF NOT EXISTS flow_blobs(run TEXT NOT NULL REFERENCES flow_runs(id), ns TEXT NOT NULL, channel TEXT NOT NULL, version TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(run, ns, channel, version));
CREATE TABLE IF NOT EXISTS flow_writes(run TEXT NOT NULL REFERENCES flow_runs(id), ns TEXT NOT NULL, checkpoint_id TEXT NOT NULL, task_id TEXT NOT NULL, idx INTEGER NOT NULL, channel TEXT NOT NULL, value TEXT NOT NULL, task_path TEXT NOT NULL, PRIMARY KEY(run, ns, checkpoint_id, task_id, idx));
CREATE TABLE IF NOT EXISTS flow_receipts(run TEXT NOT NULL REFERENCES flow_runs(id), key TEXT NOT NULL, method TEXT NOT NULL, params_sha256 TEXT NOT NULL, external INTEGER NOT NULL, state TEXT NOT NULL, response TEXT, started_at TEXT NOT NULL, finished_at TEXT, PRIMARY KEY(run, key));
CREATE TABLE IF NOT EXISTS flow_events(run TEXT NOT NULL REFERENCES flow_runs(id), seq INTEGER NOT NULL, node TEXT NOT NULL, status TEXT NOT NULL, at TEXT NOT NULL, PRIMARY KEY(run, seq));
CREATE TABLE IF NOT EXISTS goals(id TEXT PRIMARY KEY REFERENCES objects(id), root TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS context_packets(id TEXT PRIMARY KEY REFERENCES objects(id), root TEXT NOT NULL, agent TEXT NOT NULL, role TEXT NOT NULL, payload TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS native_media(id TEXT PRIMARY KEY REFERENCES objects(id), grant_id TEXT NOT NULL, source TEXT NOT NULL, kind TEXT NOT NULL, mime TEXT NOT NULL, bytes INTEGER NOT NULL, sha256 TEXT NOT NULL, blob TEXT NOT NULL, detail TEXT NOT NULL, created_at TEXT NOT NULL);
"#;

/// M2 fix round: the user's own "依据有误？" notes on a result (they stay in
/// the vault, are never sent and go when the result is removed), and an owner
/// index for blob lookups such as cached page thumbnails.
const M4_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS feedback(id TEXT PRIMARY KEY REFERENCES objects(id), result_ref TEXT NOT NULL, claim_id TEXT, issue TEXT NOT NULL, note TEXT, created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS feedback_result ON feedback(result_ref);
CREATE INDEX IF NOT EXISTS blobs_owner ON blobs(owner, purpose);
"#;

/// An unlocked vault. Dropping it closes SQLCipher and zeroizes keys.
pub struct Vault {
    pub id: String,
    pub dir: PathBuf,
    pub header: VaultHeader,
    pub(crate) keys: VaultKeys,
    pub runtime: Runtime,
    /// In-memory ANN accelerator; the SQLCipher vectors remain the baseline.
    pub(crate) dense: Option<crate::index::DenseIndex>,
}

pub fn new_ref() -> String {
    format!("ref_{}", uuid::Uuid::new_v4().simple())
}

impl Vault {
    pub fn open(dir: &Path, header: VaultHeader, keys: VaultKeys, registry: Registry) -> Result<Self> {
        let ledger = Ledger::open(&dir.join(DB_FILE), &keys.db)?;
        let vault = Self { id: header.vault_id.clone(), dir: dir.to_path_buf(), header, keys, runtime: Runtime { registry, ledger }, dense: None };
        vault.collect_orphan_files()?;
        Ok(vault)
    }

    pub fn db(&self) -> &Connection {
        &self.runtime.ledger.db
    }

    /// Register a core-issued object reference in this vault's scope.
    pub fn register(&self, kind: &str, version: &str, parent: Option<&str>) -> Result<String> {
        let id = new_ref();
        self.db().execute(
            "INSERT INTO objects(id,scope,kind,version,active,created_at,parent) VALUES(?,?,?,?,1,?,?)",
            params![id, self.id, kind, version, now_rfc3339(), parent],
        )?;
        Ok(id)
    }

    pub fn object_kind(&self, id: &str) -> Result<String> {
        self.db()
            .query_row("SELECT kind FROM objects WHERE id=? AND scope=? AND active=1", params![id, self.id], |r| r.get(0))
            .optional()?
            .ok_or(Error::Denied)
    }

    pub fn expect_kind(&self, id: &str, kinds: &[&str]) -> Result<()> {
        let kind = self.object_kind(id)?;
        if kinds.contains(&kind.as_str()) { Ok(()) } else { Err(Error::Denied) }
    }

    fn blob_path(&self, file: &str) -> PathBuf {
        self.dir.join(OBJECTS_DIR).join(&file[..2]).join(format!("{file}.lvj"))
    }

    fn blob_context(&self, blob: &str, purpose: &str) -> Vec<u8> {
        format!("lvjin-blob/2|{}|{blob}|{purpose}", self.id).into_bytes()
    }

    /// Encrypt bytes into a new blob owned by `owner`.
    pub fn put_blob(&self, owner: &str, purpose: &str, plaintext: &[u8]) -> Result<String> {
        self.put_blob_reader(owner, purpose, plaintext, plaintext.len() as u64)
    }

    /// Stream an arbitrary-size source into a new encrypted blob.
    pub fn put_blob_reader<R: Read>(&self, owner: &str, purpose: &str, reader: R, length: u64) -> Result<String> {
        let blob = format!("blob_{}", uuid::Uuid::new_v4().simple());
        let file = format!("{:x}", uuid::Uuid::new_v4().simple());
        let path = self.blob_path(&file);
        std::fs::create_dir_all(path.parent().ok_or(Error::Integrity)?)?;
        let temporary = path.with_extension("tmp");
        let mut hasher = HashingReader { inner: reader, digest: Sha256::new() };
        {
            let mut writer = BufWriter::new(File::create(&temporary)?);
            let sealed = seal_stream(&self.keys.objects, &self.blob_context(&blob, purpose), &mut hasher, length, &mut writer);
            // The ciphertext reaches the device before the database row that
            // refers to it can be committed.
            let finished = sealed.and_then(|_| writer.into_inner().map_err(|e| Error::Io(e.into_error()))).and_then(|file| file.sync_all().map_err(Error::from));
            if let Err(error) = finished {
                let _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
        }
        crate::durable::replace(&temporary, &path)?;
        let digest = format!("{:x}", hasher.digest.finalize());
        self.db().execute(
            "INSERT INTO blobs(id,owner,purpose,file,bytes,sha256,created_at) VALUES(?,?,?,?,?,?,?)",
            params![blob, owner, purpose, file, length as i64, digest, now_rfc3339()],
        )?;
        Ok(blob)
    }

    fn blob_record(&self, blob: &str) -> Result<(String, String, i64, String)> {
        self.db()
            .query_row("SELECT file,purpose,bytes,sha256 FROM blobs WHERE id=?", [blob], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?
            .ok_or(Error::Denied)
    }

    pub fn blob_sha256(&self, blob: &str) -> Result<String> {
        Ok(self.blob_record(blob)?.3)
    }

    /// Decrypt a blob into memory. Authentication failure is an integrity fault.
    pub fn get_blob(&self, blob: &str) -> Result<Zeroizing<Vec<u8>>> {
        let (file, purpose, bytes, _) = self.blob_record(blob)?;
        let mut output = Zeroizing::new(Vec::with_capacity(bytes.max(0) as usize));
        let reader = BufReader::new(File::open(self.blob_path(&file))?);
        open_stream(&self.keys.objects, &self.blob_context(blob, &purpose), reader, &mut *output).map_err(|_| Error::Integrity)?;
        Ok(output)
    }

    /// Decrypt a blob into a writer (for example a protected worker scratch file).
    pub fn copy_blob_to<W: Write>(&self, blob: &str, writer: W) -> Result<u64> {
        let (file, purpose, _, _) = self.blob_record(blob)?;
        let reader = BufReader::new(File::open(self.blob_path(&file))?);
        open_stream(&self.keys.objects, &self.blob_context(blob, &purpose), reader, writer).map_err(|_| Error::Integrity)
    }

    /// Delete the row first so a crash leaves at most an orphan ciphertext file,
    /// which `collect_orphan_files` removes on the next unlock.
    pub fn delete_blob(&self, blob: &str) -> Result<()> {
        if let Some(file) = self.db().query_row("SELECT file FROM blobs WHERE id=?", [blob], |r| r.get::<_, String>(0)).optional()? {
            self.db().execute("DELETE FROM blobs WHERE id=?", [blob])?;
            let _ = std::fs::remove_file(self.blob_path(&file));
        }
        Ok(())
    }

    pub fn blob_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut statement = self.db().prepare("SELECT id,file FROM blobs ORDER BY id")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut output = vec![];
        for row in rows {
            let (id, file) = row?;
            output.push((id, self.blob_path(&file)));
        }
        Ok(output)
    }

    fn collect_orphan_files(&self) -> Result<usize> {
        let root = self.dir.join(OBJECTS_DIR);
        if !root.is_dir() {
            std::fs::create_dir_all(&root)?;
            return Ok(0);
        }
        let known: std::collections::BTreeSet<String> = {
            let mut statement = self.db().prepare("SELECT file FROM blobs")?;
            let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut removed = 0;
        for shard in std::fs::read_dir(&root)? {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                let stem = name.trim_end_matches(".lvj").trim_end_matches(".tmp");
                if name.ends_with(".tmp") || !known.contains(stem) {
                    let _ = std::fs::remove_file(entry.path());
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Typed result payloads (encrypted in SQLCipher) referenced by SkillResult.data_ref.
    pub fn put_result(&self, skill: &str, invocation: &str, kind: &str, payload: &Value) -> Result<String> {
        let id = self.register(kind, "m1", Some(invocation))?;
        self.db().execute(
            "INSERT INTO results(id,skill,invocation,kind,payload,created_at) VALUES(?,?,?,?,?,?)",
            params![id, skill, invocation, kind, serde_json::to_string(payload)?, now_rfc3339()],
        )?;
        Ok(id)
    }

    pub fn get_result(&self, id: &str) -> Result<(String, String, Value)> {
        let (skill, kind, payload): (String, String, String) = self
            .db()
            .query_row(
                "SELECT r.skill,r.kind,r.payload FROM results r JOIN objects o ON o.id=r.id WHERE r.id=? AND o.active=1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(Error::Denied)?;
        Ok((skill, kind, serde_json::from_str(&payload)?))
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db().query_row("SELECT value FROM settings WHERE key=?", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])?;
        Ok(())
    }

    /// Root matter created with the vault; matter creation is a change on it.
    pub fn root_matter(&self) -> Result<String> {
        if let Some(id) = self.db().query_row("SELECT id FROM matters WHERE is_root=1", [], |r| r.get(0)).optional()? {
            return Ok(id);
        }
        let id = self.register("MatterRef", "m1", None)?;
        let now = now_rfc3339();
        self.db().execute(
            "INSERT INTO matters(id,title,matter_type,region,status,is_root,notes,created_at,updated_at) VALUES(?,?,?,?,?,1,NULL,?,?)",
            params![id, "我的私密空间", "vault", Option::<String>::None, "active", now, now],
        )?;
        Ok(id)
    }

    /// Idempotent side effects: returns the recorded outcome if the key exists.
    pub fn commit_once(&self, key: &str, root: &str, kind: &str, target: Option<&str>, outcome: &Value) -> Result<Option<Value>> {
        if let Some(existing) = self.db().query_row("SELECT outcome FROM commit_log WHERE idempotency_key=?", [key], |r| r.get::<_, String>(0)).optional()? {
            return Ok(Some(serde_json::from_str(&existing)?));
        }
        self.db().execute(
            "INSERT INTO commit_log(idempotency_key,root,kind,target,outcome,created_at) VALUES(?,?,?,?,?,?)",
            params![key, root, kind, target, serde_json::to_string(outcome)?, now_rfc3339()],
        )?;
        Ok(None)
    }

    pub fn temp_store_is_memory(&self) -> Result<bool> {
        let mode: i64 = self.db().query_row("PRAGMA temp_store", [], |r| r.get(0))?;
        Ok(mode == 2)
    }
}

struct HashingReader<R> {
    inner: R,
    digest: Sha256,
}
impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.digest.update(&buffer[..read]);
        Ok(read)
    }
}

pub(crate) fn migrate(db: &Connection) -> Result<()> {
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(Error::Integrity);
    }
    if version < 1 {
        let applied = db.execute_batch(&format!("BEGIN IMMEDIATE;{M1_SCHEMA}INSERT INTO meta(key,value) VALUES('schema','lvjin.vault-db/m1-1');PRAGMA user_version=1;COMMIT;"));
        if let Err(error) = applied {
            let _ = db.execute_batch("ROLLBACK;");
            return Err(error.into());
        }
    }
    if version < 2 {
        // Additive only: M1 data stays as it is; M2 tables start empty.
        let applied = db.execute_batch(&format!("BEGIN IMMEDIATE;{M2_SCHEMA}INSERT INTO meta(key,value) VALUES('schema','lvjin.vault-db/m2-2') ON CONFLICT(key) DO UPDATE SET value=excluded.value;PRAGMA user_version=2;COMMIT;"));
        if let Err(error) = applied {
            let _ = db.execute_batch("ROLLBACK;");
            return Err(error.into());
        }
    }
    if version < 3 {
        let applied = db.execute_batch(&format!("BEGIN IMMEDIATE;{M3_SCHEMA}INSERT INTO meta(key,value) VALUES('schema','lvjin.vault-db/m2-3') ON CONFLICT(key) DO UPDATE SET value=excluded.value;PRAGMA user_version=3;COMMIT;"));
        if let Err(error) = applied {
            let _ = db.execute_batch("ROLLBACK;");
            return Err(error.into());
        }
    }
    if version < 4 {
        let applied = db.execute_batch(&format!("BEGIN IMMEDIATE;{M4_SCHEMA}INSERT INTO meta(key,value) VALUES('schema','lvjin.vault-db/m2-4') ON CONFLICT(key) DO UPDATE SET value=excluded.value;PRAGMA user_version=4;COMMIT;"));
        if let Err(error) = applied {
            let _ = db.execute_batch("ROLLBACK;");
            return Err(error.into());
        }
    }
    Ok(())
}

pub fn sha256_hex(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
