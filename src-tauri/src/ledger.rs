use crate::{Error, Result, crypto::SecretKey};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use zeroize::Zeroize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    New,
    AlreadyDispatched,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counters {
    pub generation: u32,
    pub native_media: u32,
    pub answer_versions: u32,
    pub grounding: u32,
    pub retrieval: u32,
}

pub struct Ledger {
    pub(crate) db: Connection,
}

impl Ledger {
    pub fn open(path: &Path, key: &SecretKey) -> Result<Self> {
        #[cfg(feature="test-harness")] eprintln!("ledger diagnostic: connection");
        let db = Connection::open(path)?;
        let mut key_hex = key
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let mut pragma = format!("PRAGMA key=\"x'{key_hex}'\";");
        let keyed = db.execute_batch(&pragma);
        #[cfg(feature="test-harness")] eprintln!("ledger diagnostic: keyed");
        pragma.zeroize();
        key_hex.zeroize();
        keyed?;
        #[cfg(feature="test-harness")] eprintln!("ledger diagnostic: key buffer cleared");
        let cipher: String = db.query_row("PRAGMA cipher_version", [], |row| row.get(0))?;
        if cipher.is_empty() {
            return Err(Error::Integrity);
        }
        // Ordinary SQL may not write shadow tables (FTS5 data), the schema or
        // raw pages (SQLite security guidance). This also mitigates the FTS5
        // flaws CVE-2026-11822/11824 until the bundled SQLCipher is 4.17+.
        db.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        #[cfg(feature="test-harness")] eprintln!("ledger diagnostic: schema");
        let schema = "PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA temp_store=MEMORY; PRAGMA secure_delete=ON; PRAGMA cipher_memory_security=ON;
            CREATE TABLE IF NOT EXISTS roots(id TEXT PRIMARY KEY, scope TEXT NOT NULL, revision INTEGER NOT NULL, cancelled INTEGER NOT NULL DEFAULT 0, generation INTEGER NOT NULL DEFAULT 0, native_media INTEGER NOT NULL DEFAULT 0, answer_versions INTEGER NOT NULL DEFAULT 0, grounding INTEGER NOT NULL DEFAULT 0, retrieval INTEGER NOT NULL DEFAULT 0, finalizer TEXT, finalized INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS requests(root TEXT NOT NULL REFERENCES roots(id), request_id TEXT NOT NULL, invocation TEXT NOT NULL, native INTEGER NOT NULL, state TEXT NOT NULL, answer_version INTEGER, PRIMARY KEY(root,request_id));
            CREATE TABLE IF NOT EXISTS grants(root TEXT NOT NULL REFERENCES roots(id), capability TEXT NOT NULL, PRIMARY KEY(root,capability));
            CREATE TABLE IF NOT EXISTS objects(id TEXT PRIMARY KEY, scope TEXT NOT NULL, kind TEXT NOT NULL, version TEXT NOT NULL, active INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS invocations(id TEXT PRIMARY KEY, root TEXT NOT NULL REFERENCES roots(id), parent TEXT, caused_by TEXT, skill TEXT NOT NULL, actor TEXT NOT NULL, role TEXT, agent TEXT, revision INTEGER NOT NULL, depth INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS agents(id TEXT PRIMARY KEY,root TEXT NOT NULL REFERENCES roots(id),owner TEXT NOT NULL,role TEXT NOT NULL,generation INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS audit(seq INTEGER PRIMARY KEY, root TEXT NOT NULL, invocation TEXT, code TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS grounding_versions(root TEXT NOT NULL, version INTEGER NOT NULL, used INTEGER NOT NULL, PRIMARY KEY(root,version));
            CREATE TABLE IF NOT EXISTS answer_drafts(root TEXT NOT NULL REFERENCES roots(id),draft TEXT NOT NULL REFERENCES objects(id),version INTEGER NOT NULL,PRIMARY KEY(root,draft),UNIQUE(root,version));
            CREATE TABLE IF NOT EXISTS local_requests(root TEXT NOT NULL REFERENCES roots(id),request_id TEXT NOT NULL,invocation TEXT NOT NULL,kind TEXT NOT NULL,draft TEXT NOT NULL,PRIMARY KEY(root,request_id));";
        for statement in schema.split(';').filter(|part| !part.trim().is_empty()) {
            #[cfg(feature="test-harness")] eprintln!("ledger diagnostic SQL: {}",statement.trim().chars().take(65).collect::<String>());
            db.execute_batch(statement)?;
        }
        // Every ledger database carries the current (0.2.0) vault schema.
        crate::store::migrate(&db)?;
        Ok(Self { db })
    }

    pub(crate) fn create_root(
        &mut self,
        root: &str,
        scope: &str,
        capabilities: &[String],
        action: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO roots(id,scope,revision,action,created_at) VALUES(?,?,1,?,?)",
            params![root, scope, action, crate::vault::now_rfc3339()],
        )?;
        for capability in capabilities {
            tx.execute(
                "INSERT INTO grants(root,capability) VALUES(?,?)",
                params![root, capability],
            )?;
        }
        tx.execute(
            "INSERT INTO audit(root,code) VALUES(?,?)",
            params![root, format!("root.created.{action}")],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn check(&self, root: &str, revision: i64) -> Result<String> {
        let row = self
            .db
            .query_row(
                "SELECT scope,revision,cancelled FROM roots WHERE id=?",
                [root],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        match row {
            Some((scope, current, false)) if current == revision => Ok(scope),
            Some(_) => Err(Error::Cancelled),
            None => Err(Error::Denied),
        }
    }

    /// Call only after local validation, immediately before sending. Commit before I/O.
    /// A crash after dispatch stays charged and is never replayed blindly.
    pub(crate) fn dispatch_generation(
        &mut self,
        root: &str,
        revision: i64,
        invocation: &str,
        request: &str,
        native: bool,
        answer: bool,
        agent: Option<&str>,
    ) -> Result<Dispatch> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current, cancelled, generation, media, versions): (i64, bool, u32, u32, u32) = tx.query_row(
            "SELECT revision,cancelled,generation,native_media,answer_versions FROM roots WHERE id=?",
            [root],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,row.get(4)?)),
        )?;
        if cancelled || current != revision {
            return Err(Error::Cancelled);
        }
        if answer {
            // Ownership can transfer between the early contract check and dispatch.
            // Recheck it under the same write transaction that reserves the attempt.
            let finalizer: Option<String> = tx.query_row(
                "SELECT finalizer FROM roots WHERE id=? AND finalized=0", [root], |r|r.get(0)
            ).optional()?.flatten();
            let finalizer=finalizer.ok_or(Error::Denied)?;
            let mut cursor=Some(invocation.to_owned());
            let mut owned=false;
            for _ in 0..4 {
                let Some(id)=cursor else {break;};
                if id==finalizer {owned=true;break;}
                cursor=tx.query_row("SELECT parent FROM invocations WHERE id=? AND root=?",params![id,root],|r|r.get(0)).optional()?.flatten();
            }
            if !owned {return Err(Error::Denied);}
        }
        let existing: Option<(String, bool)> = tx
            .query_row(
                "SELECT invocation,native FROM requests WHERE root=? AND request_id=?",
                params![root, request],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((old_invocation, old_native)) = existing {
            return if old_native == native && old_invocation == invocation {
                Ok(Dispatch::AlreadyDispatched)
            } else {
                Err(Error::Conflict)
            };
        }
        if generation >= 7 || (native && media >= 2) || (answer && versions>=3) {
            return Err(Error::Budget);
        }
        if let Some(agent) = agent {
            let used: u32 = tx.query_row(
                "SELECT generation FROM agents WHERE id=? AND root=?",
                params![agent, root],
                |row| row.get(0),
            )?;
            if used >= 2 {
                return Err(Error::Budget);
            }
            tx.execute(
                "UPDATE agents SET generation=generation+1 WHERE id=?",
                [agent],
            )?;
        }
        tx.execute(
            "INSERT INTO requests(root,request_id,invocation,native,state,answer_version) VALUES(?,?,?,?,'dispatched',?)",
            params![root, request, invocation, native,if answer {Some(versions+1)} else {None}],
        )?;
        tx.execute(
            "UPDATE roots SET generation=generation+1,native_media=native_media+?,answer_versions=answer_versions+? WHERE id=?",
            params![u32::from(native),u32::from(answer),root],
        )?;
        tx.execute(
            "INSERT INTO audit(root,code) VALUES(?,'generation.dispatched')",
            [root],
        )?;
        tx.commit()?;
        Ok(Dispatch::New)
    }

    pub(crate) fn bind_answer(&mut self,root:&str,revision:i64,invocation:&str,request:&str,draft:&str)->Result<u32> {
        let tx=self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (scope,current,cancelled):(String,i64,bool)=tx.query_row("SELECT scope,revision,cancelled FROM roots WHERE id=?",[root],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        if current!=revision || cancelled {return Err(Error::Cancelled);}
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE id=? AND scope=? AND kind='DraftRef' AND active=1)",params![draft,scope],|r|r.get(0))?;
        if !valid {return Err(Error::Denied);}
        let version:Option<u32>=tx.query_row("SELECT answer_version FROM requests WHERE root=? AND request_id=? AND invocation=?",params![root,request,invocation],|r|r.get(0)).optional()?.flatten();
        let version=version.ok_or(Error::Denied)?;
        if let Some((previous,number))=tx.query_row("SELECT draft,version FROM answer_drafts WHERE root=? AND (version=? OR draft=?)",params![root,version,draft],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u32>(1)?))).optional()? {
            return if previous==draft && number==version {Ok(version)} else {Err(Error::Conflict)};
        }
        tx.execute("INSERT INTO answer_drafts VALUES(?,?,?)",params![root,draft,version])?;
        tx.commit()?;Ok(version)
    }

    pub(crate) fn dispatch_local(&mut self,root:&str,revision:i64,invocation:&str,request:&str,kind:&str,draft:&str)->Result<Dispatch> {
        let tx=self.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (scope,current,cancelled,retrieval,grounding):(String,i64,bool,u32,u32)=tx.query_row("SELECT scope,revision,cancelled,retrieval,grounding FROM roots WHERE id=?",[root],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        if current!=revision || cancelled {return Err(Error::Cancelled);}
        if let Some((old_invocation,old_kind,old_draft))=tx.query_row("SELECT invocation,kind,draft FROM local_requests WHERE root=? AND request_id=?",params![root,request],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()? {
            return if old_invocation==invocation && old_kind==kind && old_draft==draft {Ok(Dispatch::AlreadyDispatched)} else {Err(Error::Conflict)};
        }
        match kind {
            "retrieval"=>{
                if retrieval>=3 {return Err(Error::Budget);}
                tx.execute("UPDATE roots SET retrieval=retrieval+1 WHERE id=?",[root])?;
            },
            "grounding"=>{
                if grounding>=36 {return Err(Error::Budget);}
                let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE id=? AND scope=? AND kind='DraftRef' AND active=1)",params![draft,scope],|r|r.get(0))?;
                if !valid {return Err(Error::Denied);}
                let version:u32=tx.query_row("SELECT version FROM answer_drafts WHERE root=? AND draft=?",params![root,draft],|r|r.get(0)).optional()?.ok_or(Error::Denied)?;
                let used:u32=tx.query_row("SELECT used FROM grounding_versions WHERE root=? AND version=?",params![root,version],|r|r.get(0)).optional()?.unwrap_or(0);
                if used>=12 {return Err(Error::Budget);}
                tx.execute("INSERT INTO grounding_versions VALUES(?,?,1) ON CONFLICT(root,version) DO UPDATE SET used=used+1",params![root,version])?;
                tx.execute("UPDATE roots SET grounding=grounding+1 WHERE id=?",[root])?;
            },
            _=>return Err(Error::Denied),
        }
        tx.execute("INSERT INTO local_requests VALUES(?,?,?,?,?)",params![root,request,invocation,kind,draft])?;
        tx.commit()?;Ok(Dispatch::New)
    }

    pub fn counters(&self, root: &str) -> Result<Counters> {
        Ok(self.db.query_row("SELECT generation,native_media,answer_versions,grounding,retrieval FROM roots WHERE id=?", [root], |row| Ok(Counters { generation:row.get(0)?,native_media:row.get(1)?,answer_versions:row.get(2)?,grounding:row.get(3)?,retrieval:row.get(4)? }))?)
    }

    pub(crate) fn stop(&mut self, root: &str) -> Result<()> {
        self.db.execute(
            "UPDATE roots SET cancelled=1,revision=revision+1 WHERE id=?",
            [root],
        )?;
        Ok(())
    }

    pub(crate) fn revoke(&mut self, root: &str, capability: &str) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM grants WHERE root=? AND capability=?",
            params![root, capability],
        )?;
        tx.execute("UPDATE roots SET revision=revision+1 WHERE id=?", [root])?;
        tx.commit()?;
        Ok(())
    }
}
