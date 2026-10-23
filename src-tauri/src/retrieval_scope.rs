//! Trusted retrieval policy. A scope narrows the vault; pinned versions and
//! legal applicability filters are intersections, never alternative grants.
use crate::{Error,Result,store::Vault};
use rusqlite::{OptionalExtension,params};
use serde::{Deserialize,Serialize};
use serde_json::Value;

#[derive(Clone,Debug,Default,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default)] pub graph_enabled:bool,
    #[serde(default)] pub as_of:Option<String>,
    #[serde(default)] pub region:Option<String>,
}
impl Policy {
    pub fn validate(&self)->Result<()> {
        if self.as_of.as_ref().is_some_and(|d|!canonical_date(d)) || self.region.as_ref().is_some_and(|r|r.is_empty()||r.len()>40||r.chars().any(|c|!c.is_alphanumeric()&&c!='-'&&c!='_')) {return Err(Error::Contract);}
        Ok(())
    }
    pub fn date(&self)->Result<String> {Ok(match &self.as_of{Some(d)=>d.clone(),None=>crate::public_knowledge::mainland_date(chrono::Utc::now())?})}
}
pub fn canonical_date(text:&str)->bool {chrono::NaiveDate::parse_from_str(text,"%Y-%m-%d").is_ok_and(|d|d.to_string()==text)}
pub fn region_name(region:Option<&str>)->String {
    static LABELS:std::sync::OnceLock<Vec<(String,String)>>=std::sync::OnceLock::new();
    let labels=LABELS.get_or_init(||serde_json::from_str(include_str!("../../resources/data/regions.json")).unwrap_or_default());
    match region {None=>"未限定地域".into(),Some(region)=>labels.iter().find(|(id,_)|id==region).map(|(_,name)|name.clone()).unwrap_or_else(||format!("资料标注地区 {region}"))}
}
pub fn region_applies(source:&str,requested:&str)->bool {
    if source==requested{return true;}
    const MAINLAND:&[&str]=&["11","12","13","14","15","21","22","23","31","32","33","34","35","36","37","41","42","43","44","45","46","50","51","52","53","54","61","62","63","64","65"];
    let code=|r:&str|r.strip_prefix("CN-").filter(|c|(c.len()==2||c.len()==6)&&c.bytes().all(|b|b.is_ascii_digit())).map(str::to_owned);
    let requested_code=code(requested);
    if source=="CN-mainland" {return requested_code.as_ref().is_some_and(|c|MAINLAND.contains(&&c[..2]));}
    match (code(source),requested_code) {
        (Some(s),Some(r)) if s.len()==2=>r.starts_with(&s),
        (Some(s),Some(r)) if s.len()==6&&r.len()==6=>if s.ends_with("0000"){s[..2]==r[..2]}else if s.ends_with("00"){s[..4]==r[..4]}else{false},
        _=>false,
    }
}
pub fn legal_allowed(metadata:&Value,policy:&Policy)->Result<bool> {
    let date=policy.date()?;
    let Some(start)=metadata["effective_on"].as_str().filter(|d|canonical_date(d)) else{return Ok(false)};
    let end=metadata["expires_on"].as_str();
    if start>date.as_str() || end.is_some_and(|d|!canonical_date(d)||d<=date.as_str()) {return Ok(false);}
    if policy.region.as_ref().is_some_and(|region|!metadata["region"].as_str().is_some_and(|r|region_applies(r,region))){return Ok(false);}
    Ok(true)
}
pub fn version_allowed(vault:&Vault,version:&str,matters:&[String],versions:&[String],policy:&Policy)->Result<bool> {
    let row:Option<(String,String,i64)>=vault.db().query_row("SELECT d.matter_id,v.document_id,v.version_no FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id JOIN matters m ON m.id=d.matter_id WHERE v.id=? AND o.active=1 AND m.status NOT IN ('deleted','incomplete','pending')",[version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((matter,document,number))=row else{return Ok(false)};
    if !matters.contains(&matter) || (!versions.is_empty()&&!versions.iter().any(|v|v==version)){return Ok(false);}
    if versions.is_empty() {
        let newer:bool=vault.db().query_row("SELECT EXISTS(SELECT 1 FROM document_versions v JOIN objects o ON o.id=v.id WHERE v.document_id=? AND v.version_no>? AND o.active=1 AND EXISTS(SELECT 1 FROM chunks c WHERE c.version_id=v.id))",params![document,number],|r|r.get(0))?;
        if newer{return Ok(false);}
    }
    let meta:Option<String>=vault.db().query_row("SELECT metadata FROM legal_sources WHERE version_id=?",[version],|r|r.get(0)).optional()?;
    match meta {Some(meta)=>legal_allowed(&serde_json::from_str(&meta)?,policy),None=>Ok(true)}
}
pub fn node_matter(vault:&Vault,node:&str)->Result<Option<(String,Option<String>)>> {
    let kind=match vault.object_kind(node){Ok(kind)=>kind,Err(Error::Denied)=>return Ok(None),Err(e)=>return Err(e)};
    let (sql,is_version)=match kind.as_str(){
        "EvidenceRef"=>("SELECT matter_id,version_id FROM chunks WHERE id=?",false),
        "DocumentVersionRef"=>("SELECT d.matter_id,v.id FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",true),
        "PageRef"=>("SELECT d.matter_id,p.version_id FROM pages p JOIN document_versions v ON v.id=p.version_id JOIN documents d ON d.id=v.document_id WHERE p.id=?",false),
        "FactRef"=>("SELECT matter_id,source_version FROM facts WHERE id=? AND status='confirmed'",false),
        "FinancialInputRef"=>("SELECT matter_id,source_version FROM cashflows WHERE id=? AND status='confirmed'",false),
        "MatterRef"|"PublicKnowledgeCollectionRef"=>return Ok(Some((node.into(),None))),
        _=>return Ok(None),
    };
    let _=is_version;
    Ok(vault.db().query_row(sql,[node],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
}
pub fn node_allowed(vault:&Vault,node:&str,matters:&[String],versions:&[String],policy:&Policy)->Result<bool> {
    let Some((matter,version))=node_matter(vault,node)? else{return Ok(false)};
    if !matters.contains(&matter){return Ok(false);}
    match version {Some(version)=>version_allowed(vault,&version,matters,versions,policy),None=>Ok(versions.is_empty())}
}
