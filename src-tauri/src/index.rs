//! M0 storage feasibility boundary. Only trusted core code selects scope,
//! generation, embedding semantics and index precision; no SQL/path enters IPC.
use crate::{Error,Result,crypto::SecretKey,ledger::Ledger};
use jieba_rs::Jieba;
use rusqlite::{params,TransactionBehavior};
use serde::{Deserialize,Serialize};
use sha2::{Digest,Sha256};
use usearch::{Index,IndexOptions,MetricKind,ScalarKind};
use zeroize::Zeroizing;

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingProfile {
    pub artifact_sha256:String,
    pub processor_sha256:String,
    pub instruction_version:String,
    pub pooling_version:String,
    pub numerical_profile:String,
    pub truncation_tokens:u32,
}
impl EmbeddingProfile {
    pub fn space_id(&self)->Result<String> {
        for hash in [&self.artifact_sha256,&self.processor_sha256] {
            if hash.len()!=64 || !hash.bytes().all(|b|b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {return Err(Error::Integrity);}
        }
        if self.instruction_version.is_empty() || self.pooling_version.is_empty() || self.numerical_profile.is_empty() || self.truncation_tokens==0 {return Err(Error::Integrity);}
        // Dimension, normalization and metric are fixed application invariants.
        let semantics=serde_json::to_vec(&(self,1024,"fp32-l2-normalized","cosine"))?;
        Ok(format!("embedding-space-v1-{:x}",Sha256::digest(semantics)))
    }
}

#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub enum IndexPrecision {F32,F16}
impl IndexPrecision {
    fn scalar(self)->ScalarKind {match self {Self::F32=>ScalarKind::F32,Self::F16=>ScalarKind::F16}}
}

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotIdentity {
    pub scope:String,
    pub generation:u64,
    pub embedding_space_id:String,
    pub precision:IndexPrecision,
}

pub struct DenseIndex {identity:SnapshotIdentity,index:Index}
impl DenseIndex {
    pub fn new(identity:SnapshotIdentity,capacity:usize)->Result<Self> {
        if identity.scope.is_empty() || identity.generation==0 || !identity.embedding_space_id.starts_with("embedding-space-v1-") || capacity>1_000_000 {return Err(Error::Contract);}
        let options=IndexOptions {dimensions:1024,metric:MetricKind::Cos,quantization:identity.precision.scalar(),connectivity:16,expansion_add:128,expansion_search:128,multi:false};
        let index=Index::new(&options).map_err(|_|Error::Integrity)?;
        index.reserve(capacity.max(1)).map_err(|_|Error::Integrity)?;
        Ok(Self{identity,index})
    }
    fn vector(vector:&[f32])->Result<()> {
        if vector.len()!=1024 || vector.iter().any(|v|!v.is_finite()) {return Err(Error::Contract);}
        let norm=vector.iter().map(|v|f64::from(*v).powi(2)).sum::<f64>();
        if (norm-1.0).abs()>0.002 {return Err(Error::Contract);}
        Ok(())
    }
    pub fn insert(&self,key:u64,vector:&[f32])->Result<()> {
        Self::vector(vector)?;
        if self.index.contains(key) {return Err(Error::Conflict);}
        if self.index.size()>=1_000_000 {return Err(Error::Contract);}
        if self.index.size()>=self.index.capacity() {self.index.reserve(self.index.capacity().saturating_mul(2).min(1_000_000)).map_err(|_|Error::Integrity)?;}
        self.index.add(key,vector).map_err(|_|Error::Integrity)
    }
    pub fn search(&self,identity:&SnapshotIdentity,query:&[f32],count:usize)->Result<Vec<(u64,f32)>> {
        if identity!=&self.identity {return Err(Error::Denied);}
        Self::vector(query)?;
        if count==0 || count>25 {return Err(Error::Contract);}
        let found=self.index.search(query,count).map_err(|_|Error::Integrity)?;
        Ok(found.keys.into_iter().zip(found.distances).collect())
    }
    pub fn remove(&self,key:u64)->Result<bool> {Ok(self.index.remove(key).map_err(|_|Error::Integrity)?!=0)}
    /// No filename or mmap API is exposed. Only ciphertext can leave this type.
    pub fn encrypted_snapshot(&self,key:&SecretKey)->Result<Vec<u8>> {
        let size=self.index.serialized_length();
        let mut buffer=Zeroizing::new(vec![0u8;size]);
        self.index.save_to_buffer(&mut buffer).map_err(|_|Error::Integrity)?;
        key.seal(&buffer,&serde_json::to_vec(&self.identity)?)
    }
    pub fn reopen(identity:SnapshotIdentity,key:&SecretKey,envelope:&[u8])->Result<Self> {
        let buffer=Zeroizing::new(key.open(envelope,&serde_json::to_vec(&identity)?)?);
        let index=Index::restore_from_buffer(&buffer).map_err(|_|Error::Integrity)?;
        if index.dimensions()!=1024 || index.metric_kind()!=MetricKind::Cos || index.scalar_kind()!=identity.precision.scalar() || index.multi() {return Err(Error::Integrity);}
        Ok(Self{identity,index})
    }
}

/// FTS5 lives inside the same SQLCipher connection, including token text and
/// its shadow tables. Jieba's embedded MIT dictionary is deterministic offline.
pub struct KeywordIndex {jieba:Jieba}
impl KeywordIndex {
    pub fn initialize(ledger:&Ledger)->Result<Self> {
        ledger.db.execute_batch("CREATE TABLE IF NOT EXISTS indexed_sources(id INTEGER PRIMARY KEY,scope TEXT NOT NULL,generation INTEGER NOT NULL,version TEXT NOT NULL); CREATE VIRTUAL TABLE IF NOT EXISTS m0_keywords USING fts5(tokens,tokenize='unicode61'); INSERT INTO m0_keywords(m0_keywords,rank) VALUES('secure-delete',1);")?;
        Ok(Self{jieba:Jieba::new()})
    }
    fn tokens(&self,text:&str)->Vec<String> {
        self.jieba.cut_for_search(text,false).into_iter().map(|token|token.word.trim().to_lowercase()).filter(|token|!token.is_empty() && token.chars().any(char::is_alphanumeric)).collect()
    }
    pub fn insert(&self,ledger:&mut Ledger,id:i64,scope:&str,generation:i64,version:&str,text:&str)->Result<()> {
        if scope.is_empty() || generation<=0 || version.is_empty() || text.len()>1_000_000 {return Err(Error::Contract);}
        let tokens=self.tokens(text).join(" ");
        let tx=ledger.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO indexed_sources VALUES(?,?,?,?)",params![id,scope,generation,version])?;
        tx.execute("INSERT INTO m0_keywords(rowid,tokens) VALUES(?,?)",params![id,tokens])?;
        tx.commit()?;Ok(())
    }
    pub fn search(&self,ledger:&Ledger,scope:&str,generation:i64,query:&str)->Result<Vec<(i64,String,f64)>> {
        if query.len()>4096 || scope.is_empty() || generation<=0 {return Err(Error::Contract);}
        let tokens=self.tokens(query);
        if tokens.is_empty() {return Ok(vec![]);}
        // Literal quoted tokens prevent a user's query from becoming FTS syntax.
        let expression=tokens.iter().map(|word|format!("\"{}\"",word.replace('"',"\"\""))).collect::<Vec<_>>().join(" OR ");
        let mut statement=ledger.db.prepare("SELECT s.id,s.version,bm25(m0_keywords) AS score FROM m0_keywords JOIN indexed_sources s ON s.id=m0_keywords.rowid WHERE m0_keywords MATCH ? AND s.scope=? AND s.generation=? ORDER BY score,s.id LIMIT 25")?;
        let rows=statement.query_map(params![expression,scope,generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>,_>>()?)
    }
    pub fn remove(&self,ledger:&mut Ledger,id:i64,scope:&str)->Result<()> {
        let tx=ledger.db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM indexed_sources WHERE id=? AND scope=?)",params![id,scope],|r|r.get(0))?;
        if !valid {return Err(Error::Denied);}
        tx.execute("DELETE FROM m0_keywords WHERE rowid=?",[id])?;
        tx.execute("DELETE FROM indexed_sources WHERE id=?",[id])?;
        tx.commit()?;Ok(())
    }
}
