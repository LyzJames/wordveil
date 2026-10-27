//! Read-only, vault-scoped retrieval. The core fixes scope and budgets before
//! an API planner can propose queries. No filesystem paths or shell enter it.
use crate::{Error,Result,index::{fts_query,search_tokens},privacy::bounded_regex,store::Vault};
use rusqlite::params_from_iter;
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::{BTreeMap,BTreeSet},sync::atomic::{AtomicBool,Ordering}};

pub const MAX_TOOL_CALLS:usize=8;
pub const MAX_SCAN:usize=20_000;
pub const MAX_SCAN_BYTES:usize=32*1024*1024;

#[derive(Clone,Copy,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="snake_case")]
pub enum Strategy { Auto, Exact, Semantic }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolQuery {pub tool:String,pub query:String}

pub fn route(query:&str,requested:Strategy)->Strategy {
    if requested!=Strategy::Auto {return requested;}
    // IDs, error codes, quoted phrases and filenames are strong exact hints;
    // a file extension alone never excludes a semantic query.
    let words=query.split_whitespace().collect::<Vec<_>>();
    let quoted=query.contains('"') || query.contains('“') || query.contains('`');
    let anchor=words.iter().any(|word|word.len()>3 && (word.contains("::")||word.contains('_')||word.contains("ERROR")||word.contains("0x")||(word.chars().any(char::is_alphabetic)&&word.chars().any(|c|c.is_ascii_digit())&&word.contains('-'))));
    if quoted||anchor {Strategy::Exact}else{Strategy::Semantic}
}

/// This is a conservative review trigger, not a guarantee of detecting every
/// indirect prompt injection. The broker never gives retrieved data authority.
pub fn instruction_risk(text:&str)->bool {
    let lower=text.to_lowercase();
    ["ignore previous instructions","ignore all previous","system prompt","developer message","<|im_start|>","⟦sy:","忽略之前的指令","忽略以上指令","泄露密钥","发送到以下地址","覆盖系统提示","你现在必须调用"].iter().any(|term|lower.contains(term))
}

pub fn validate_plan(value:&str)->Result<Vec<ToolQuery>> {
    if value.len()>16*1024 {return Err(Error::Budget);}
    let queries:Vec<ToolQuery>=serde_json::from_str(value).map_err(|_|Error::Remote("invalid_search_plan".into()))?;
    if queries.is_empty()||queries.len()>3 {return Err(Error::Budget);}
    for query in &queries {
        if !matches!(query.tool.as_str(),"shouyu_glob"|"shouyu_search_text"|"shouyu_search_semantic"|"shouyu_read_excerpt")||query.query.is_empty()||query.query.len()>512||query.query.contains('\0') {return Err(Error::Denied);}
    }
    Ok(queries)
}

/// Restricted glob on a logical display name. No recursive filesystem walk.
pub fn glob_match(pattern:&str,name:&str)->Result<bool> {
    if pattern.len()>128||pattern.contains(['\\','/','\0',':'])||pattern.contains("..") {return Err(Error::Contract);}
    let mut expression=String::from("(?i)^");
    for c in pattern.chars() {match c {'*'=>expression.push_str(".*"),'?'=>expression.push('.'),_=>expression.push_str(&regex::escape(&c.to_string()))}}
    expression.push('$');Ok(bounded_regex(&expression)?.is_match(name))
}

fn clause(vault:&Vault,projects:&[String],versions:&[String])->Result<(String,Vec<String>)> {
    if projects.len()>64||versions.len()>64 {return Err(Error::Budget);}
    let mut clauses=vec!["o.active=1".to_string(),"NOT EXISTS(SELECT 1 FROM legal_sources l WHERE l.version_id=c.version_id)".to_string()];
    let mut values=vec![];
    if projects.is_empty()&&versions.is_empty() {return Err(Error::Denied);}
    if !projects.is_empty() {
        for id in projects {vault.expect_kind(id,&["MatterRef"])?;}
        clauses.push(format!("c.matter_id IN ({})",vec!["?";projects.len()].join(",")));values.extend_from_slice(projects);
    }
    if !versions.is_empty() {
        for id in versions {vault.expect_kind(id,&["DocumentVersionRef"])?;}
        clauses.push(format!("c.version_id IN ({})",vec!["?";versions.len()].join(",")));values.extend_from_slice(versions);
    }
    Ok((clauses.join(" AND "),values))
}

pub fn scope(vault:&Vault,projects:&[String],versions:&[String])->Result<(BTreeSet<u64>,Vec<String>)> {
    let (clause,values)=clause(vault,projects,versions)?;
    let mut statement=vault.db().prepare(&format!("SELECT c.rowid,c.partition_id FROM chunks c JOIN objects o ON o.id=c.id WHERE {clause}"))?;
    let mut allowed=BTreeSet::new();let mut partitions=BTreeSet::new();
    let mut rows=statement.query(params_from_iter(values))?;
    while let Some(row)=rows.next()? {allowed.insert(row.get::<_,i64>(0)? as u64);partitions.insert(row.get::<_,String>(1)?);}
    Ok((allowed,partitions.into_iter().collect()))
}

pub fn lexical(vault:&Vault,projects:&[String],versions:&[String],query:&str,exact:bool,cancel:&AtomicBool)->Result<Value> {
    if query.len()>2048||query.is_empty(){return Err(Error::Contract);}
    let (clause,values)=clause(vault,projects,versions)?;
    let mut ids=vec![];let mut scanned=0;let mut bytes=0;let mut truncated=false;
    let _cancel=crate::index::QueryCancellation::new(vault.db(),cancel);
    if exact {
        // Explicit re: is bounded, non-backtracking Rust regex; other input is
        // a literal. Never interpolate a query into SQL or invoke grep.exe.
        let literal=query.trim_matches(['"','“','”','`']);
        let regex=literal.strip_prefix("re:").map(bounded_regex).transpose()?;
        let mut statement=vault.db().prepare(&format!("SELECT c.rowid,c.text FROM chunks c JOIN objects o ON o.id=c.id WHERE {clause} ORDER BY c.rowid LIMIT {}",MAX_SCAN+1))?;
        let mut rows=statement.query(params_from_iter(values))?;
        while let Some(row)=rows.next()? {
            if cancel.load(Ordering::Acquire){return Err(Error::Cancelled);}
            let text:String=row.get(1)?;scanned+=1;bytes+=text.len();
            if scanned>MAX_SCAN||bytes>MAX_SCAN_BYTES {truncated=true;break;}
            if regex.as_ref().map_or_else(||text.to_lowercase().contains(&literal.to_lowercase()),|r|r.is_match(&text)) {
                ids.push(row.get::<_,i64>(0)?);
                if ids.len()>=20 {truncated=true;break;}
            }
        }
    } else if let Some(expression)=fts_query(&search_tokens(query)) {
        let mut parameters=vec![expression];parameters.extend(values);
        let mut statement=vault.db().prepare(&format!("SELECT c.rowid FROM chunk_fts CROSS JOIN chunks c ON c.rowid=chunk_fts.rowid JOIN objects o ON o.id=c.id WHERE chunk_fts MATCH ? AND {clause} ORDER BY bm25(chunk_fts),c.rowid LIMIT 20"))?;
        ids=statement.query_map(params_from_iter(parameters),|row|row.get(0))?.collect::<rusqlite::Result<_>>()?;
    }
    if cancel.load(Ordering::Acquire){return Err(Error::Cancelled);}
    Ok(json!({"items":excerpts(vault,&ids)?,"scanned_chunks":scanned,"truncated":truncated,"strategy":if exact{"exact"}else{"bm25"}}))
}

pub fn excerpts(vault:&Vault,ids:&[i64])->Result<Vec<Value>> {
    ids.iter().take(20).map(|id| {
        let (reference,version,text,name,spans):(String,String,String,String,String)=vault.db().query_row("SELECT c.id,c.version_id,c.text,d.display_name,c.spans FROM chunks c JOIN objects o ON o.id=c.id JOIN document_versions v ON v.id=c.version_id JOIN documents d ON d.id=v.document_id WHERE c.rowid=? AND o.active=1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
        Ok(json!({"source_id":reference,"version_id":version,"name":name,"text":text.chars().take(2200).collect::<String>(),"locators":serde_json::from_str::<Value>(&spans)?,"trust":"data_only"}))
    }).collect()
}

pub fn merge(lexical:&[Value],semantic:&[Value])->Vec<Value> {
    let mut ranked:BTreeMap<String,(f64,Value)>=BTreeMap::new();
    for list in [lexical,semantic] {for (rank,item) in list.iter().enumerate() {if let Some(id)=item["source_id"].as_str(){let entry=ranked.entry(id.into()).or_insert((0.,item.clone()));entry.0+=1./(60.+rank as f64);}}}
    let mut ranked=ranked.into_values().collect::<Vec<_>>();ranked.sort_by(|a,b|b.0.total_cmp(&a.0));ranked.into_iter().take(8).map(|(_,v)|v).collect()
}
