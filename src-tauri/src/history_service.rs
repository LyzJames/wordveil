//! Read saved top-level results in the currently unlocked SQLCipher vault.
//! A history read never creates a root, starts a flow, or contacts a provider.
use super::*;
use rusqlite::named_params;
use serde::Deserialize;

fn page_size() -> usize {
    40
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    #[serde(default)]
    pub search: String,
    pub matter: Option<String>,
    pub from: Option<String>,
    pub through: Option<String>,
    pub before: Option<i64>,
    #[serde(default = "page_size")]
    pub limit: usize,
}
impl Default for Query {
    fn default() -> Self {
        Self {
            search: String::new(),
            matter: None,
            from: None,
            through: None,
            before: None,
            limit: page_size(),
        }
    }
}

impl CoreService {
    pub fn qa_history(&self, query: &Query) -> Result<Value> {
        let handle = self.vault()?;
        if query.search.chars().count() > 2000
            || !(1..=100).contains(&query.limit)
            || query.before.is_some_and(|id| id <= 0)
            || [&query.from, &query.through].iter().any(|date| {
                date.as_ref()
                    .is_some_and(|d| !crate::retrieval_scope::canonical_date(d))
            })
            || matches!((&query.from,&query.through),(Some(a),Some(b)) if a>b)
        {
            return Err(Error::Contract);
        }
        let vault = self.lock_vault(&handle)?;
        if let Some(matter) = &query.matter {
            vault.expect_kind(matter, &["MatterRef", "PublicKnowledgeCollectionRef"])?;
        }
        let mut statement = vault.db().prepare(
            "SELECT r.rowid,r.id,r.skill,r.payload,r.created_at,q.text,
                (SELECT g.purpose FROM egress_grants g WHERE g.root=i.root LIMIT 1),
                (SELECT s.matters FROM egress_grants g JOIN scopes s ON s.id=g.scope_id WHERE g.root=i.root LIMIT 1)
             FROM results r JOIN objects o ON o.id=r.id AND o.active=1
             JOIN invocations i ON i.id=r.invocation AND i.depth=1
             JOIN questions q ON q.id=json_extract(r.payload,'$.question_id')
             JOIN objects qo ON qo.id=q.id AND qo.active=1
             WHERE r.kind IN ('RetrievalResultRef','ReviewResultRef')
               AND r.skill IN ('lvjin-retrieval','contract-review','loan-analysis','budget-analysis','image-review','audio-review','video-review')
               AND (:before IS NULL OR r.rowid<:before)
               AND (:search='' OR instr(lower(q.text),lower(:search))>0)
               AND (:from IS NULL OR date(r.created_at,'+8 hours')>=:from)
               AND (:through IS NULL OR date(r.created_at,'+8 hours')<=:through)
               AND (:matter IS NULL OR json_extract(r.payload,'$.matter_id')=:matter
                    OR EXISTS(SELECT 1 FROM json_each(json_extract(r.payload,'$.scope.matters')) WHERE value=:matter)
                    OR EXISTS(SELECT 1 FROM egress_grants g JOIN scopes s ON s.id=g.scope_id,json_each(s.matters) m WHERE g.root=i.root AND m.value=:matter))
               AND NOT EXISTS(SELECT 1 FROM comparison_roots c WHERE c.root=i.root)
               AND NOT EXISTS(SELECT 1 FROM egress_grants g WHERE g.question_id=q.id AND (g.root IS NULL OR g.root<>i.root))
               AND NOT EXISTS(SELECT 1 FROM results newer JOIN invocations ni ON ni.id=newer.invocation
                    JOIN objects no ON no.id=newer.id AND no.active=1
                    WHERE ni.root=i.root AND ni.depth=1 AND newer.rowid>r.rowid AND newer.kind IN ('RetrievalResultRef','ReviewResultRef'))
             ORDER BY r.rowid DESC LIMIT :limit")?;
        let rows=statement.query_map(named_params! {
            ":before":query.before, ":search":query.search.trim(), ":from":query.from,
            ":through":query.through, ":matter":query.matter, ":limit":(query.limit+1) as i64,
        },|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,
                   row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,Option<String>>(6)?,row.get::<_,Option<String>>(7)?)))?
            .collect::<std::result::Result<Vec<_>,_>>()?;
        let more = rows.len() > query.limit;
        let mut items = Vec::new();
        let mut cursor = None;
        for (rowid, id, skill, payload, created, question, purpose, scope) in
            rows.into_iter().take(query.limit)
        {
            let payload: Value = serde_json::from_str(&payload)?;
            let kind = match skill.as_str() {
                "contract-review" => "contract",
                "loan-analysis" => "loan",
                "budget-analysis" => "budget",
                "image-review" => "image",
                "audio-review" => "audio",
                "video-review" => "video",
                _ if purpose.as_deref() == Some("deep_answer") => "deep",
                _ if payload["requested_output"] == "evidence_only" => "evidence",
                _ => "answer",
            };
            let mut matters: Vec<String> =
                serde_json::from_value(payload["scope"]["matters"].clone()).unwrap_or_default();
            if let Some(matter) = payload["matter_id"].as_str() {
                matters.push(matter.to_owned());
            }
            if let Some(scope) = scope {
                matters.extend(serde_json::from_str::<Vec<String>>(&scope)?);
            }
            matters.sort();
            matters.dedup();
            items.push(json!({"id":id,"question":question,"kind":kind,"matters":matters,"created_at":created}));
            cursor = Some(rowid);
        }
        drop(statement);
        drop(vault);
        self.session_cancel(&handle)?;
        Ok(json!({"items":items,"next_cursor":if more {cursor} else {None}}))
    }
}
