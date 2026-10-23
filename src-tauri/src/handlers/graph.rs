//! Source-backed two-hop traversal. A graph path is a route to original
//! material, not a finding about legal applicability or truth.
use super::*;
use crate::retrieval_scope::{self,Policy};
use rusqlite::{OptionalExtension,params,params_from_iter};
use std::collections::{BTreeSet,VecDeque};

const MAX_NODES:usize=128;
const MAX_EDGES:usize=128;
const MAX_SCANNED:usize=512;
const FANOUT:usize=32;

/// Resolve a proof from the live source instead of trusting stored excerpts.
/// Legacy user relationships remain visible but cannot add retrieval hits.
pub(crate) fn proof(vault:&Vault,value:&Value,matters:&[String],versions:&[String],policy:&Policy)->Result<Option<Value>> {
    if let Some(id)=value["evidence_id"].as_str() {
        if !retrieval_scope::node_allowed(vault,id,matters,versions,policy)?{return Ok(None);}
        let row:Option<(String,String,String)>=vault.db().query_row("SELECT c.version_id,c.spans,v.content_sha256 FROM chunks c JOIN document_versions v ON v.id=c.version_id JOIN objects o ON o.id=c.id WHERE c.id=? AND o.active=1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((version,spans,hash))=row {return Ok(Some(enrich_proof(vault,json!({"evidence_id":id,"version_id":version,"content_sha256":hash,"spans":serde_json::from_str::<Value>(&spans)?}))?));}
    }
    if let Some(block)=value["block"].as_i64() {
        let row:Option<(String,String,String)>=vault.db().query_row("SELECT b.version_id,b.text,b.locator FROM blocks b WHERE b.rowid=?",[block],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((version,text,locator))=row {
            if !retrieval_scope::version_allowed(vault,&version,matters,versions,policy)? {return Ok(None);}
            let start=value["start"].as_u64().unwrap_or(0);let end=value["end"].as_u64().unwrap_or(text.chars().count() as u64);
            if start>=end || end>text.chars().count() as u64{return Ok(None);}
            let hash:String=vault.db().query_row("SELECT content_sha256 FROM document_versions WHERE id=?",[&version],|r|r.get(0))?;
            return Ok(Some(enrich_proof(vault,json!({"version_id":version,"content_sha256":hash,"block":block,"start":start,"end":end,"locator":serde_json::from_str::<Value>(&locator)?}))?));
        }
    }
    Ok(None)
}
fn enrich_proof(vault:&Vault,mut proof:Value)->Result<Value> {
    let version=proof["version_id"].as_str().ok_or(Error::Integrity)?;
    let (name,number):(String,i64)=vault.db().query_row("SELECT d.display_name,v.version_no FROM document_versions v JOIN documents d ON d.id=v.document_id WHERE v.id=?",[version],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let legal:Option<String>=vault.db().query_row("SELECT metadata FROM legal_sources WHERE version_id=?",[version],|r|r.get(0)).optional()?;
    let mut locators=vec![];
    if proof["locator"].is_object(){locators.push(proof["locator"].clone());}
    for span in proof["spans"].as_array().cloned().unwrap_or_default().iter().take(8) {
        if let Some(block)=span["block"].as_i64(){if let Some(locator)=vault.db().query_row("SELECT locator FROM blocks WHERE rowid=? AND version_id=?",params![block,version],|r|r.get::<_,String>(0)).optional()?{locators.push(serde_json::from_str::<Value>(&locator)?);}}
    }
    proof["document"]=json!({"display_name":name,"version_no":number});proof["locators"]=json!(locators);proof["legal_source"]=legal.map(|s|serde_json::from_str::<Value>(&s)).transpose()?.unwrap_or(Value::Null);
    Ok(proof)
}

fn same_root(vault:&Vault,id:&str,kind:&str,root:&str)->Result<Value> {
    let owner:Option<String>=vault.db().query_row("SELECT i.root FROM results r JOIN invocations i ON i.id=r.invocation WHERE r.id=? AND r.kind=?",params![id,kind],|r|r.get(0)).optional()?;
    if owner.as_deref()!=Some(root){return Err(Error::Denied);}
    Ok(vault.get_result(id)?.2)
}

pub fn run(ctx:&Ctx,invocation:&Invocation,input:&Value)->Result<EnvelopeParts> {
    ctx.authorize(invocation,"graph.read")?;
    let original=optional_text(input,"candidate_set_id");let snapshot=optional_text(input,"snapshot_id");
    if original.is_some()!=snapshot.is_some(){return Err(Error::Contract);}
    let browse=original.is_none();
    let mut seeds=refs_of(input,"seed_ids")?;
    if seeds.len()>32 || (!browse&&!seeds.is_empty()){return Err(Error::Contract);}
    let types=refs_of(input,"relation_types")?;
    let hops=input["max_hops"].as_u64().filter(|h|*h<=2).ok_or(Error::Contract)? as usize;
    let mut base=json!({});
    let mut expected_generation=None;
    let (matters,versions,policy,budget)=if let (Some(set),Some(snap))=(&original,&snapshot) {
        let cached;
        {
            let vault=ctx.lock()?;
            cached=vault.db().query_row("SELECT result_ref FROM retrieval_expansions WHERE candidate_set=? AND root=?",params![set,invocation.root_id()],|r|r.get::<_,String>(0)).optional()?;
            base=same_root(&vault,set,"CandidateSetRef",invocation.root_id())?;
            same_root(&vault,snap,"RetrievalSnapshotRef",invocation.root_id())?;
            if base["snapshot_id"]!=json!(snap) || base["graph_enabled"]!=json!(true){return Err(Error::Denied);}
            seeds=base["candidates"].as_array().cloned().unwrap_or_default().iter().take(8).filter_map(|c|c["evidence_id"].as_str().map(str::to_owned)).collect();
        }
        let (scope,generation)=super::indexing::load_scope(ctx,&[],snap)?;
        {let vault=ctx.lock()?;super::indexing::stable(&vault,generation)?;}
        expected_generation=Some(generation);
        // Replays share the expansion receipt, but still revalidate the
        // supplied snapshot and current index before reusing its result.
        if let Some(result)=cached {ctx.lock()?.get_result(&result)?;return Ok(completed(Some(result)));}
        let slots=base["candidate_slots"].as_u64().ok_or(Error::Integrity)? as usize;
        let budget=(base["graph_budget"].as_u64().unwrap_or(0) as usize).min(40).min(200usize.saturating_sub(slots));
        (scope.matters,scope.versions,scope.policy,budget)
    } else {
        let vault=ctx.lock()?;let mut matters=vec![];
        for seed in &seeds {let Some((matter,_))=retrieval_scope::node_matter(&vault,seed)? else{return Err(Error::Denied)};if !matters.contains(&matter){matters.push(matter);}}
        // A user browsing an explicitly selected old version may inspect its
        // version links; only an actual retrieval scope supplies candidates.
        let mut versions=vec![];let mut stmt=vault.db().prepare("SELECT v.id,d.matter_id FROM document_versions v JOIN documents d ON d.id=v.document_id JOIN objects o ON o.id=v.id WHERE o.active=1")?;
        for row in stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {let (version,matter)=row?;if matters.contains(&matter){versions.push(version);}}
        (matters,versions,Policy::default(),0)
    };
    let allowed_node=|vault:&Vault,node:&str|->Result<bool>{
        if browse {if let Some((matter,None))=retrieval_scope::node_matter(vault,node)? {return Ok(matters.contains(&matter));}}
        retrieval_scope::node_allowed(vault,node,&matters,&versions,&policy)
    };
    let mut frontier=VecDeque::new();let mut seen=BTreeSet::new();
    {let vault=ctx.lock()?;for seed in &seeds {if !allowed_node(&vault,seed)?{return Err(Error::Denied);}if seen.insert(seed.clone()){frontier.push_back((seed.clone(),Vec::<Value>::new()));}
        if let Some((_,Some(version)))=retrieval_scope::node_matter(&vault,seed)? {if seen.insert(version.clone()){frontier.push_back((version,vec![]));}}
    }}
    let mut node_ids=seen.clone();
    let mut edges=vec![];let mut edge_ids=BTreeSet::new();let mut paths=vec![];
    let mut new_hits=vec![];let mut hit_ids:BTreeSet<String>=base["candidates"].as_array().cloned().unwrap_or_default().iter().filter_map(|c|c["evidence_id"].as_str().map(str::to_owned)).collect();
    let mut scanned=0;let mut truncated=false;
    while let Some((node,path))=frontier.pop_front() {
        ctx.check()?;
        if path.len()>=hops {continue;}
        if edges.len()>=MAX_EDGES || node_ids.len()>=MAX_NODES || scanned>=MAX_SCANNED {truncated=true;break;}
        let rows:Vec<Value>={
            let vault=ctx.lock()?;
            let sql=format!("SELECT r.id,r.matter_id,r.src,r.dst,r.relation_type,r.evidence,r.status,r.origin,r.valid_from,r.valid_to,r.jurisdiction,r.extraction_method FROM relations r JOIN objects o ON o.id=r.id WHERE o.active=1 AND r.status='confirmed' AND (r.src=? OR r.dst=?) AND r.matter_id IN ({}) AND (r.valid_from IS NULL OR r.valid_from<=?) AND (r.valid_to IS NULL OR r.valid_to>?) ORDER BY r.id LIMIT {}",vec!["?";matters.len().max(1)].join(","),FANOUT+1);
            let mut values=vec![node.clone(),node.clone()];values.extend(if matters.is_empty(){vec![String::new()]}else{matters.clone()});values.extend([policy.date()?,policy.date()?]);
            let mut s=vault.db().prepare(&sql)?;
            let mut rows=s.query_map(params_from_iter(values.iter()),|r|Ok(json!({"id":r.get::<_,String>(0)?,"matter_id":r.get::<_,String>(1)?,"src":r.get::<_,String>(2)?,"dst":r.get::<_,String>(3)?,"relation_type":r.get::<_,String>(4)?,"evidence":r.get::<_,Option<String>>(5)?.and_then(|s|serde_json::from_str::<Value>(&s).ok()),"status":r.get::<_,String>(6)?,"origin":r.get::<_,String>(7)?,"valid_from":r.get::<_,Option<String>>(8)?,"valid_to":r.get::<_,Option<String>>(9)?,"jurisdiction":r.get::<_,Option<String>>(10)?,"extraction_method":r.get::<_,String>(11)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
            if !browse && (types.is_empty()||types.iter().any(|t|t=="cites")) {rows.extend(explicit_references(&vault,&node,&base,&matters,&versions,&policy)?);}
            rows
        };
        if rows.len()>FANOUT{truncated=true;}
        for mut edge in rows.into_iter().take(FANOUT) {
            ctx.check()?;scanned+=1;
            if scanned>MAX_SCANNED || edges.len()>=MAX_EDGES || node_ids.len()>=MAX_NODES {truncated=true;break;}
            let id=edge["id"].as_str().ok_or(Error::Integrity)?.to_owned();
            let src=edge["src"].as_str().ok_or(Error::Integrity)?.to_owned();let dst=edge["dst"].as_str().ok_or(Error::Integrity)?.to_owned();
            let forward=node==src || edge["traversal_anchor"].as_str()==Some(node.as_str());
            if !forward&&node!=dst{continue;}
            if !matters.iter().any(|m|edge["matter_id"]==json!(m)) || (!types.is_empty()&&!types.iter().any(|t|edge["relation_type"]==json!(t))){continue;}
            let date=policy.date()?;
            if edge["valid_from"].as_str().is_some_and(|d|!retrieval_scope::canonical_date(d)||d>date.as_str()) || edge["valid_to"].as_str().is_some_and(|d|!retrieval_scope::canonical_date(d)||d<=date.as_str()) {continue;}
            if let Some(region)=&policy.region {if edge["jurisdiction"].as_str().is_some_and(|r|!retrieval_scope::region_applies(r,region)){continue;}}
            let vault=ctx.lock()?;
            if !allowed_node(&vault,&src)? || !allowed_node(&vault,&dst)?{continue;}
            let extra=[src.clone(),dst.clone()].into_iter().filter(|id|!node_ids.contains(id)).collect::<BTreeSet<_>>();
            if node_ids.len()+extra.len()>MAX_NODES{truncated=true;continue;}node_ids.extend(extra);
            let mut source=proof(&vault,&edge["evidence"],&matters,&versions,&policy)?;
            let owner_version=retrieval_scope::node_matter(&vault,&src)?.and_then(|(_,v)|v);
            if source.as_ref().is_some_and(|p|p["version_id"].as_str()!=owner_version.as_deref()){source=None;}
            edge["proof"]=source.clone().unwrap_or(json!({"type":"user_record_without_original","retrieval_eligible":false}));
            edge["retrieval_eligible"]=json!(source.is_some());
            let other=if forward{dst.clone()}else{src.clone()};
            let step=json!({"edge_id":id,"src":src,"dst":dst,"relation_type":edge["relation_type"],"traversal":if forward{"forward"}else{"reverse"},"proof":edge["proof"],"status":edge["status"],"origin":edge["origin"],"jurisdiction":edge["jurisdiction"],"valid_from":edge["valid_from"],"valid_to":edge["valid_to"],"target_location":edge["target_location"]});
            if edge_ids.insert(id){edges.push(edge);}
            // Unproven legacy links are displayed, but cannot bridge to a
            // second relation or introduce passages into the answer context.
            if !browse && source.is_none(){continue;}
            if !seen.insert(other.clone()){continue;}
            let mut next=path.clone();next.push(step);
            paths.push(json!({"target":other,"hops":next.len(),"steps":next}));
            if !browse && new_hits.len()<budget {
                let kind=vault.object_kind(&other)?;
                let candidates:Vec<(String,String,String)>=if kind=="EvidenceRef" {
                    vault.db().query_row("SELECT id,version_id,matter_id FROM chunks WHERE id=?",[&other],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.into_iter().collect()
                }else if kind=="DocumentVersionRef" {
                    // The relationship names the version, not an arbitrary
                    // article. Locate up to three query-relevant passages in it.
                    passages_for_version(&vault,&other,&base,3)?
                }else{vec![]};
                for (hit,version,matter) in candidates {if new_hits.len()>=budget{break;}if hit_ids.insert(hit.clone()) {
                    new_hits.push(json!({"evidence_id":hit,"version_id":version,"matter_id":matter,"rrf":0.0,"ranks":[],"graph_paths":[{"target":other,"hops":next.len(),"steps":next}],"retrieval_method":"graph_source_path"}));
                }}
            }
            let anchor=retrieval_scope::node_matter(&vault,&other)?.and_then(|(_,v)|v);
            drop(vault);frontier.push_back((other,next.clone()));
            if let Some(version)=anchor {if node_ids.contains(&version)||node_ids.len()<MAX_NODES {node_ids.insert(version.clone());if seen.insert(version.clone()){frontier.push_back((version,next));}}else{truncated=true;}}
        }
    }
    let mut merged_ref=None;
    if let Some(set)=&original {
        let raw=base["candidate_slots"].as_u64().ok_or(Error::Integrity)? as usize;
        if raw+new_hits.len()>200{return Err(Error::Integrity);}
        let mut items=base["candidates"].as_array().cloned().unwrap_or_default();
        // Keep ordinary recall prominent, and let at most every third place
        // enter through a graph route before the same 20-pair reranker.
        let mut graph_hits=new_hits.clone().into_iter();let mut merged=vec![];
        for (i,item) in items.drain(..).enumerate(){merged.push(item);if i%2==1 {if let Some(hit)=graph_hits.next(){merged.push(hit);}}}
        merged.extend(graph_hits);merged.truncate(200);
        base["candidates"]=json!(merged);base["candidate_slots"]=json!(raw+new_hits.len());base["graph"]=json!({"enabled":true,"added":new_hits.len(),"paths":paths,"truncated":truncated,"scanned_edges":scanned,"seed_count":seeds.len()});base["original_candidate_set"]=json!(set);
        merged_ref=Some(ctx.lock()?.put_result(invocation.skill(),invocation.id(),"CandidateSetRef",&base)?);
    }
    let nodes:Vec<Value>={let vault=ctx.lock()?;node_ids.iter().map(|id|Ok(json!({"id":id,"kind":vault.object_kind(id)?}))).collect::<Result<_>>()?};
    let payload=json!({"seeds":seeds,"max_hops":hops,"relation_types":types,"nodes":nodes,"edges":edges,"paths":paths,"new_candidates":new_hits,"candidate_set":merged_ref,"truncated":truncated,"scanned_edges":scanned,"limits":{"nodes":MAX_NODES,"edges":MAX_EDGES,"scanned":MAX_SCANNED,"fanout":FANOUT},"policy":policy});
    let vault=ctx.lock()?;ctx.check()?;
    if let Some(generation)=expected_generation {super::indexing::stable(&vault,generation)?;}
    let result=vault.put_result(invocation.skill(),invocation.id(),"GraphResultRef",&payload)?;
    if let Some(set)=original {vault.db().execute("INSERT INTO retrieval_expansions(candidate_set,root,result_ref) VALUES(?,?,?)",params![set,invocation.root_id(),result])?;}
    let mut parts=completed(Some(result));
    if truncated {warn(&mut parts,"graph_truncated","关系路径已达到本轮节点、边或扇出限制；保留常规检索结果。");}
    if paths.is_empty(){parts.limitations.push("没有可用的关系路径；继续使用普通混合检索。".into());}
    Ok(parts)
}

fn optional_text(value:&Value,key:&str)->Option<String>{value[key].as_str().map(str::to_owned)}
fn passages_for_version(vault:&Vault,version:&str,base:&Value,take:usize)->Result<Vec<(String,String,String)>> {
    let text=base["queries"][0]["query"].as_str().unwrap_or("");
    let Some(expression)=crate::index::fts_query(&crate::index::search_tokens(text)) else{return Ok(vec![])};
    let mut s=vault.db().prepare("SELECT c.id,c.version_id,c.matter_id FROM chunk_fts CROSS JOIN chunks c ON c.rowid=chunk_fts.rowid CROSS JOIN objects o ON o.id=c.id WHERE chunk_fts MATCH ? AND c.version_id=? AND o.active=1 ORDER BY bm25(chunk_fts),c.rowid LIMIT ?")?;
    Ok(s.query_map(params![expression,version,take as i64],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<std::result::Result<_,_>>()?)
}

/// Literal 《official title》 links are re-derived from current original
/// chunks. These are read-only source references, never model-inferred facts
/// or persisted unconfirmed graph writes. Article candidates stay separately
/// labelled as query locations within the referenced version.
fn explicit_references(vault:&Vault,node:&str,base:&Value,matters:&[String],versions:&[String],policy:&Policy)->Result<Vec<Value>> {
    let kind=vault.object_kind(node)?;
    let sources=if kind=="EvidenceRef"{vec![node.to_owned()]}else if kind=="DocumentVersionRef"{passages_for_version(vault,node,base,3)?.into_iter().map(|r|r.0).collect()}else{return Ok(vec![])};
    let targets:Vec<(String,Value)>={let mut s=vault.db().prepare("SELECT version_id,metadata FROM legal_sources ORDER BY version_id LIMIT 256")?;
        s.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.map(|r|{let (id,m)=r?;Ok((id,serde_json::from_str::<Value>(&m)?))}).collect::<Result<_>>()?};
    let mut out=vec![];
    for source in sources {
        let (text,source_version,matter):(String,String,String)=vault.db().query_row("SELECT body,version_id,matter_id FROM chunks WHERE id=?",[&source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        for (target,metadata) in &targets {
            if target==&source_version||!retrieval_scope::version_allowed(vault,target,matters,versions,policy)?{continue;}
            let Some(title)=metadata["title"].as_str() else{continue};
            let short=title.strip_prefix("中华人民共和国").unwrap_or(title);
            if !text.contains(&format!("《{title}》"))&&!text.contains(&format!("《{short}》")){continue;}
            let edge_id=format!("source_{}",&crate::store::sha256_hex(format!("{source}|{target}|{}",metadata["sha256"]).as_bytes())[..32]);
            out.push(json!({"id":edge_id,"matter_id":matter,"src":source,"dst":target,"traversal_anchor":node,"relation_type":"cites","evidence":{"evidence_id":source},"status":"source_recorded","origin":"explicit_source_reference","extraction_method":"literal_official_title","valid_from":null,"valid_to":null,"jurisdiction":metadata["region"],"target_location":"query_located_within_cited_version"}));
            if out.len()>=FANOUT{return Ok(out);}
        }
    }
    Ok(out)
}
