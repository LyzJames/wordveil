//! Bounded, encrypted ANN partitions. SQLCipher vectors are the F32 baseline.
//! At most 10,000 vectors per partition and 512 MiB / 12 live accelerators.
//! Searches merge one global top-k, regardless of the number of partitions.
use crate::{Error, Result, index::{DenseIndex, IndexPrecision, SnapshotIdentity}, store::Vault, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};
use serde_json::json;
use std::collections::{BTreeSet, VecDeque};

pub const PARTITION_ROWS: usize = 10_000;
pub const CACHE_BYTES: usize = 512 * 1024 * 1024;
pub const CACHE_PARTITIONS: usize = 12;
const LOAD_RESERVE: usize = 48 * 1024 * 1024;
pub fn summary(vault:&Vault)->serde_json::Value {
    let mut value=vault.dense.as_ref().map(PartitionCache::summary).unwrap_or_else(||PartitionCache::default().summary());
    value["stored_partitions"]=json!(vault.db().query_row("SELECT COUNT(*) FROM index_partitions WHERE vector_count>0",[],|r|r.get::<_,i64>(0)).unwrap_or(0));
    value["vectors_per_partition"]=json!(PARTITION_ROWS);value["precision"]=json!("F32");value
}

#[derive(Default)]
pub struct PartitionCache { entries: VecDeque<(String,DenseIndex)>, pub loads:u64, pub evictions:u64 }
impl PartitionCache {
    pub fn bytes(&self)->usize {self.entries.iter().map(|(_,i)|i.memory_bytes()).sum()}
    pub fn len(&self)->usize {self.entries.len()}
    fn reserve(&mut self, additional:usize) {
        while !self.entries.is_empty() && (self.entries.len()>=CACHE_PARTITIONS || self.bytes().saturating_add(additional)>CACHE_BYTES) {
            self.entries.pop_front();self.evictions+=1;
        }
    }
    fn take(&mut self,id:&str)->Option<DenseIndex> {
        let pos=self.entries.iter().position(|(p,_)|p==id)?;
        self.entries.remove(pos).map(|(_,index)|index)
    }
    fn put(&mut self,id:String,index:DenseIndex)->Result<()> {
        if index.len()>PARTITION_ROWS || index.memory_bytes()>CACHE_BYTES {return Err(Error::Budget);}
        self.reserve(index.memory_bytes());self.entries.push_back((id,index));Ok(())
    }
    pub fn summary(&self)->serde_json::Value {json!({"partitions":self.len(),"estimated_bytes":self.bytes(),"max_partitions":CACHE_PARTITIONS,"max_bytes":CACHE_BYTES,"loads":self.loads,"evictions":self.evictions})}
}
fn identity(vault:&Vault,id:&str,space:&str,generation:i64)->SnapshotIdentity {
    SnapshotIdentity{scope:format!("{}|partition:{id}",vault.id),generation:generation as u64,embedding_space_id:space.into(),precision:IndexPrecision::F32}
}
pub fn vector(bytes:&[u8])->Result<Vec<f32>> {
    if bytes.len()!=4096 {return Err(Error::Integrity);}
    let values:Vec<f32>=bytes.chunks_exact(4).map(|b|f32::from_le_bytes([b[0],b[1],b[2],b[3]])).collect();
    let norm=values.iter().map(|v|v*v).sum::<f32>().sqrt();
    if values.iter().any(|v|!v.is_finite()) || (norm-1.0).abs()>0.002 {return Err(Error::Integrity);}
    Ok(values)
}
fn check(cancel:&std::sync::atomic::AtomicBool)->Result<()> {if cancel.load(std::sync::atomic::Ordering::Acquire){Err(Error::Cancelled)}else{Ok(())}}
fn build(vault:&Vault,id:&str,space:&str,generation:i64,cancel:&std::sync::atomic::AtomicBool)->Result<DenseIndex> {
    let count:i64=vault.db().query_row("SELECT COUNT(*) FROM chunk_vectors v JOIN chunks c ON c.rowid=v.chunk_rowid JOIN objects o ON o.id=c.id WHERE c.partition_id=? AND v.space=? AND o.active=1",params![id,space],|r|r.get(0))?;
    if count<0 || count as usize>PARTITION_ROWS {return Err(Error::Budget);}
    let index=DenseIndex::new(identity(vault,id,space,generation),(count as usize).max(16))?;
    // Stream the encrypted baseline: never collect all vault vectors in RAM.
    let mut stmt=vault.db().prepare("SELECT v.chunk_rowid,v.vector FROM chunk_vectors v JOIN chunks c ON c.rowid=v.chunk_rowid JOIN objects o ON o.id=c.id WHERE c.partition_id=? AND v.space=? AND o.active=1 ORDER BY c.rowid")?;
    let mut rows=stmt.query(params![id,space])?;
    while let Some(row)=rows.next()? {check(cancel)?;index.insert(row.get::<_,i64>(0)? as u64,&vector(&row.get::<_,Vec<u8>>(1)?)?)?;}
    Ok(index)
}
fn save(vault:&mut Vault,id:&str,space:&str,generation:i64,index:&DenseIndex)->Result<()> {
    let old:Option<String>=vault.db().query_row("SELECT snapshot_blob FROM index_partitions WHERE partition_id=?",[id],|r|r.get(0)).optional()?.flatten();
    let blob=vault.put_blob("index","index_partition",&index.encrypted_snapshot(&vault.keys.index)?)?;
    vault.db().execute("UPDATE index_partitions SET generation=?,space=?,snapshot_blob=?,vector_count=?,dirty=0 WHERE partition_id=?",params![generation,space,blob,index.len() as i64,id])?;
    if let Some(old)=old {if old!=blob {vault.delete_blob(&old)?;}}
    Ok(())
}
/// Publish a small encrypted manifest only after dirty partitions are saved.
/// The global generation is unchanged during batching and advances on publish.
pub fn refresh(vault:&mut Vault,space:&str,generation:i64,cancel:&std::sync::atomic::AtomicBool)->Result<()> {
    vault.dense=None;
    let partitions:Vec<String>={let mut s=vault.db().prepare("SELECT partition_id FROM index_partitions WHERE dirty=1 OR space<>? ORDER BY partition_id")?;
        s.query_map([space],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?};
    for id in partitions {check(cancel)?;let index=build(vault,&id,space,generation,cancel)?;check(cancel)?;save(vault,&id,space,generation,&index)?;}
    let manifest:Vec<serde_json::Value>={let mut s=vault.db().prepare("SELECT partition_id,generation,space,snapshot_blob,vector_count FROM index_partitions WHERE vector_count>0 ORDER BY partition_id")?;
        s.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"generation":r.get::<_,i64>(1)?,"space":r.get::<_,String>(2)?,"blob":r.get::<_,Option<String>>(3)?,"vectors":r.get::<_,i64>(4)?})))?.collect::<std::result::Result<_,_>>()?};
    let total:i64=manifest.iter().map(|p|p["vectors"].as_i64().unwrap_or(0)).sum();
    let previous:Option<String>=vault.db().query_row("SELECT snapshot_blob FROM index_state WHERE id=1",[],|r|r.get(0)).optional()?.flatten();
    let blob=vault.put_blob("index","index_manifest",&serde_json::to_vec(&json!({"schema":"lvjin.partition-manifest/1","space":space,"generation":generation,"precision":"F32","partitions":manifest}))?)?;
    vault.db().execute("INSERT INTO index_state(id,generation,space,snapshot_blob,vector_count,dirty,updated_at) VALUES(1,?,?,?,?,0,?) ON CONFLICT(id) DO UPDATE SET generation=excluded.generation,space=excluded.space,snapshot_blob=excluded.snapshot_blob,vector_count=excluded.vector_count,dirty=0,updated_at=excluded.updated_at",params![generation,space,blob,total,now_rfc3339()])?;
    if let Some(previous)=previous {if previous!=blob {vault.delete_blob(&previous)?;}}
    vault.dense=Some(PartitionCache::default());Ok(())
}
pub fn ensure(vault:&mut Vault,space:&str,cancel:&std::sync::atomic::AtomicBool)->Result<()> {
    let state:Option<(i64,String,i64)>=vault.db().query_row("SELECT generation,space,dirty FROM index_state WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    match state {Some((_,ref stored,0)) if stored==space=>{if vault.dense.is_none(){vault.dense=Some(PartitionCache::default());}Ok(())},
        _=>refresh(vault,space,state.map(|s|s.0).unwrap_or(0)+1,cancel)}
}
fn load(vault:&mut Vault,id:&str,space:&str,cancel:&std::sync::atomic::AtomicBool)->Result<DenseIndex> {
    let (generation,stored,blob,dirty):(i64,String,Option<String>,i64)=vault.db().query_row("SELECT generation,space,snapshot_blob,dirty FROM index_partitions WHERE partition_id=?",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if dirty!=0 || stored!=space {return Err(Error::Conflict);}
    if let Some(blob)=blob {if let Ok(bytes)=vault.get_blob(&blob) {if let Ok(index)=DenseIndex::reopen(identity(vault,id,space,generation),&vault.keys.index,&bytes) {
        if index.len()<=PARTITION_ROWS {return Ok(index);}
    }}}
    // A bad accelerator never erases the database baseline or changes source
    // versions. Rebuild this partition only, then replace its ciphertext.
    let index=build(vault,id,space,generation,cancel)?;check(cancel)?;save(vault,id,space,generation,&index)?;Ok(index)
}
pub fn search(vault:&mut Vault,space:&str,query:&[f32],allowed:&BTreeSet<u64>,partitions:&[String],take:usize,cancel:&std::sync::atomic::AtomicBool)->Result<Vec<(u64,f32)>> {
    if take==0 || take>25 {return Err(Error::Contract);}
    let mut best=vec![];
    // Visit resident partitions first. Scanning more partitions than fit in
    // LRU order would evict every resident before using it on the next scan.
    // Per-partition top-k and the stable global merge are order-independent.
    let resident:BTreeSet<String>=vault.dense.as_ref().ok_or(Error::Integrity)?.entries.iter().map(|(id,_)|id.clone()).collect();
    let mut ordered=partitions.iter().collect::<Vec<_>>();
    ordered.sort_by(|a,b|resident.contains(*b).cmp(&resident.contains(*a)).then(a.cmp(b)));
    for id in ordered {
        if cancel.load(std::sync::atomic::Ordering::Acquire){return Err(Error::Cancelled);}
        let cached=vault.dense.as_mut().ok_or(Error::Integrity)?.take(id);
        let index=match cached {Some(index)=>index,None=>{
            vault.dense.as_mut().ok_or(Error::Integrity)?.reserve(LOAD_RESERVE);
            let index=load(vault,id,space,cancel)?;vault.dense.as_mut().ok_or(Error::Integrity)?.loads+=1;index
        }};
        best.extend(index.search_within(index.identity(),query,take,|key|allowed.contains(&key))?);
        best.sort_by(|a,b|a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));best.truncate(take);
        vault.dense.as_mut().ok_or(Error::Integrity)?.put(id.clone(),index)?;
    }
    Ok(best)
}
