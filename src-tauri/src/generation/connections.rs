//! Connection profiles, capability records and the request audit trail.
//!
//! Profiles live in the encrypted vault database; keys live in DPAPI files
//! (see `credentials`). Capability results are bound to the tested
//! protocol/endpoint/model/adapter combination and become stale as soon as any
//! of them changes (design 8.4). Request records hold counts, codes and the IDs
//! of the materials sent, never the text itself.
use super::{
    ADAPTER_VERSION, Profile, Protocol, Provider,
    transport::{Endpoint, ProxyMode},
};
use crate::{Error, Result, store::Vault, vault::now_rfc3339};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub const CAPABILITIES: &[&str] = &["text", "streaming", "tool_calling", "strict_schema", "json_object", "vision", "audio_input", "native_video", "joint_media"];
pub const STATUSES: &[&str] = &["supported", "unsupported", "unknown", "unavailable", "policy_blocked"];

/// Draft fields entered by the user in the protected settings view.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftInput {
    pub connection_id: Option<String>,
    pub name: String,
    pub provider: String,
    pub protocol: String,
    pub base_url: String,
    pub model_id: String,
    pub timeout_s: u64,
    pub max_output_tokens: u32,
    pub proxy: String,
}

impl DraftInput {
    pub fn validate(&self) -> Result<Profile> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            return Err(Error::NeedsInput("name"));
        }
        let provider = Provider::parse(&self.provider).ok_or(Error::Contract)?;
        let protocol = Protocol::parse(&self.protocol).ok_or(Error::Contract)?;
        let allowed = match provider {
            Provider::Deepseek | Provider::Local | Provider::Custom => matches!(protocol, Protocol::ChatCompletions | Protocol::Responses | Protocol::AnthropicMessages),
            Provider::Openai => matches!(protocol, Protocol::ChatCompletions | Protocol::Responses),
            Provider::Anthropic => protocol == Protocol::AnthropicMessages,
        };
        if !allowed {
            return Err(Error::NeedsInput("protocol"));
        }
        let endpoint = Endpoint::parse(&self.base_url)?;
        let model = self.model_id.trim();
        if model.is_empty() || model.len() > 200 || !model.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::NeedsInput("model_id"));
        }
        if !(5..=600).contains(&self.timeout_s) {
            return Err(Error::NeedsInput("timeout_s"));
        }
        if !(256..=65_536).contains(&self.max_output_tokens) {
            return Err(Error::NeedsInput("max_output_tokens"));
        }
        let proxy = match self.proxy.as_str() {
            "system" => ProxyMode::System,
            "none" => ProxyMode::None,
            _ => return Err(Error::Contract),
        };
        Ok(Profile { provider, protocol, endpoint, model_id: model.into(), timeout_s: self.timeout_s, max_output_tokens: self.max_output_tokens, proxy })
    }
}

pub struct Connection {
    pub id: String,
    pub name: String,
    pub profile: Profile,
    pub credential: Option<String>,
    pub status: String,
}

fn proxy_text(mode: &ProxyMode) -> &'static str {
    match mode {
        ProxyMode::System => "system",
        ProxyMode::None => "none",
    }
}

/// Register a draft. The key (already sealed by the caller) is referenced by
/// its credential ID; the draft holds no secret.
pub fn create_draft(vault: &Vault, input: &DraftInput, credential: Option<&str>) -> Result<String> {
    let profile = input.validate()?;
    if let Some(existing) = &input.connection_id {
        vault.expect_kind(existing, &["ConnectionRef"])?;
    }
    let id = vault.register("ConnectionDraftRef", "0.3.0", input.connection_id.as_deref())?;
    let stored = json!({"connection_id": input.connection_id, "name": input.name.trim(), "provider": profile.provider.as_str(), "protocol": profile.protocol.as_str(),
        "base_url": profile.endpoint.base_url(), "model_id": profile.model_id, "timeout_s": profile.timeout_s, "max_output_tokens": profile.max_output_tokens, "proxy": proxy_text(&profile.proxy)});
    vault.db().execute("INSERT INTO connection_drafts(id,connection_id,profile,credential,created_at) VALUES(?,?,?,?,?)", params![id, input.connection_id, stored.to_string(), credential, now_rfc3339()])?;
    Ok(id)
}

pub struct Draft {
    pub id: String,
    pub connection_id: Option<String>,
    pub input: DraftInput,
    pub credential: Option<String>,
}

pub fn read_draft(vault: &Vault, id: &str) -> Result<Draft> {
    vault.expect_kind(id, &["ConnectionDraftRef"])?;
    let (connection_id, profile, credential, consumed): (Option<String>, String, Option<String>, bool) =
        vault.db().query_row("SELECT connection_id,profile,credential,consumed FROM connection_drafts WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?.ok_or(Error::Denied)?;
    if consumed {
        return Err(Error::Denied);
    }
    let input: DraftInput = serde_json::from_str(&profile)?;
    Ok(Draft { id: id.into(), connection_id, input, credential })
}

pub fn consume_draft(vault: &Vault, id: &str) -> Result<()> {
    let changed = vault.db().execute("UPDATE connection_drafts SET consumed=1 WHERE id=? AND consumed=0", [id])?;
    if changed != 1 {
        return Err(Error::Denied);
    }
    Ok(())
}

/// Create or update the connection described by a draft. Returns the
/// connection ID and the credential that the update replaced (to be removed).
pub fn apply_draft(vault: &Vault, draft: &Draft) -> Result<(String, Option<String>)> {
    let profile = draft.input.validate()?;
    let now = now_rfc3339();
    let first = !vault.db().query_row("SELECT EXISTS(SELECT 1 FROM connections WHERE status='active')", [], |r| r.get::<_, bool>(0))?;
    let (id, replaced) = match &draft.connection_id {
        Some(id) => {
            let (status, previous): (String, Option<String>) = vault.db().query_row("SELECT status,credential FROM connections WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
            if status != "active" {
                return Err(Error::Denied);
            }
            let credential = draft.credential.clone().or(previous.clone());
            vault.db().execute(
                "UPDATE connections SET name=?,provider=?,protocol=?,base_url=?,model_id=?,timeout_s=?,max_output_tokens=?,proxy=?,credential=?,updated_at=? WHERE id=?",
                params![draft.input.name.trim(), profile.provider.as_str(), profile.protocol.as_str(), profile.endpoint.base_url(), profile.model_id, profile.timeout_s as i64, profile.max_output_tokens as i64, proxy_text(&profile.proxy), credential, now, id],
            )?;
            let replaced = if draft.credential.is_some() { previous.filter(|p| Some(p) != draft.credential.as_ref()) } else { None };
            if replaced.is_some() {vault.db().execute("DELETE FROM capabilities WHERE connection_id=?",[id])?;}
            (id.clone(), replaced)
        }
        None => {
            let id = vault.register("ConnectionRef", "0.3.0", None)?;
            vault.db().execute(
                "INSERT INTO connections(id,name,provider,protocol,base_url,model_id,timeout_s,max_output_tokens,proxy,credential,status,is_default,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,'active',?,?,?)",
                params![id, draft.input.name.trim(), profile.provider.as_str(), profile.protocol.as_str(), profile.endpoint.base_url(), profile.model_id, profile.timeout_s as i64, profile.max_output_tokens as i64, proxy_text(&profile.proxy), draft.credential, first, now, now],
            )?;
            (id, None)
        }
    };
    Ok((id, replaced))
}

pub fn load(vault: &Vault, id: &str) -> Result<Connection> {
    vault.expect_kind(id, &["ConnectionRef"])?;
    let row: Option<(String, String, String, String, String, i64, i64, String, Option<String>, String)> = vault
        .db()
        .query_row("SELECT name,provider,protocol,base_url,model_id,timeout_s,max_output_tokens,proxy,credential,status FROM connections WHERE id=?", [id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
        })
        .optional()?;
    let (name, provider, protocol, base_url, model_id, timeout_s, max_output_tokens, proxy, credential, status) = row.ok_or(Error::Denied)?;
    let profile = Profile {
        provider: Provider::parse(&provider).ok_or(Error::Integrity)?,
        protocol: Protocol::parse(&protocol).ok_or(Error::Integrity)?,
        endpoint: Endpoint::parse(&base_url).map_err(|_| Error::Integrity)?,
        model_id,
        timeout_s: timeout_s as u64,
        max_output_tokens: max_output_tokens as u32,
        proxy: if proxy == "none" { ProxyMode::None } else { ProxyMode::System },
    };
    Ok(Connection { id: id.into(), name, profile, credential, status })
}

pub fn default_connection(vault: &Vault) -> Result<Option<String>> {
    Ok(vault.db().query_row("SELECT id FROM connections WHERE status='active' ORDER BY is_default DESC, updated_at DESC LIMIT 1", [], |r| r.get(0)).optional()?)
}

pub fn set_default(vault: &Vault, id: &str) -> Result<()> {
    let tx = vault.db().unchecked_transaction()?;
    tx.execute("UPDATE connections SET is_default=0", [])?;
    let changed = tx.execute("UPDATE connections SET is_default=1 WHERE id=? AND status='active'", [id])?;
    if changed != 1 {
        return Err(Error::Denied);
    }
    tx.commit()?;
    Ok(())
}

/// Revoke: the key file is removed by the caller; the profile stays for audit.
pub fn revoke(vault: &Vault, id: &str) -> Result<Option<String>> {
    let credential: Option<String> = vault.db().query_row("SELECT credential FROM connections WHERE id=? AND status='active'", [id], |r| r.get(0)).optional()?.ok_or(Error::Denied)?;
    vault.db().execute("UPDATE connections SET status='revoked',credential=NULL,is_default=0,revoked_at=?,updated_at=? WHERE id=?", params![now_rfc3339(), now_rfc3339(), id])?;
    vault.db().execute("DELETE FROM capabilities WHERE connection_id=?", [id])?;
    vault.db().execute("UPDATE egress_grants SET revoked_at=? WHERE connection_id=? AND revoked_at IS NULL", params![now_rfc3339(), id])?;
    Ok(credential)
}

pub fn record_capability(vault: &Vault, connection: &str, binding: &str, capability: &str, status: &str, evidence: &str, limits: &Value) -> Result<()> {
    if !CAPABILITIES.contains(&capability) || !STATUSES.contains(&status) {
        return Err(Error::Contract);
    }
    vault.db().execute(
        "INSERT INTO capabilities(connection_id,capability,binding,status,evidence,limits,tested_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(connection_id,capability) DO UPDATE SET binding=excluded.binding,status=excluded.status,evidence=excluded.evidence,limits=excluded.limits,tested_at=excluded.tested_at",
        params![connection, capability, binding, status, evidence, limits.to_string(), now_rfc3339()],
    )?;
    Ok(())
}

/// Capability status for routing; anything not tested for the current binding
/// is `unknown` (never assumed from the model name).
pub fn capability(vault: &Vault, connection: &Connection, capability: &str) -> Result<String> {
    let row: Option<(String, String)> = vault.db().query_row("SELECT binding,status FROM capabilities WHERE connection_id=? AND capability=?", params![connection.id, capability], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(match row {
        Some((binding, status)) if binding == connection.profile.binding() => status,
        _ => "unknown".into(),
    })
}

pub fn summary(vault: &Vault, has_key: &dyn Fn(&str) -> bool) -> Result<Vec<Value>> {
    let mut statement = vault.db().prepare("SELECT id FROM connections WHERE status='active' ORDER BY is_default DESC, created_at")?;
    let ids: Vec<String> = statement.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    let mut out = vec![];
    for id in ids {
        let connection = load(vault, &id)?;
        let binding = connection.profile.binding();
        let mut caps = serde_json::Map::new();
        let mut statement = vault.db().prepare("SELECT capability,binding,status,evidence,limits,tested_at FROM capabilities WHERE connection_id=?")?;
        let rows = statement.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?)))?;
        for row in rows {
            let (capability, bound, status, evidence, limits, tested) = row?;
            caps.insert(capability, json!({"status": status, "stale": bound != binding, "evidence": evidence, "limits": serde_json::from_str::<Value>(&limits).unwrap_or(Value::Null), "tested_at": tested}));
        }
        let is_default: bool = vault.db().query_row("SELECT is_default FROM connections WHERE id=?", [&id], |r| r.get(0))?;
        out.push(json!({
            "id": id, "name": connection.name, "provider": connection.profile.provider.as_str(), "protocol": connection.profile.protocol.as_str(),
            "base_url": connection.profile.endpoint.base_url(), "host": connection.profile.endpoint.display_host(), "loopback": connection.profile.endpoint.is_loopback(),
            "model_id": connection.profile.model_id, "timeout_s": connection.profile.timeout_s, "max_output_tokens": connection.profile.max_output_tokens,
            "proxy": proxy_text(&connection.profile.proxy), "has_key": connection.credential.as_deref().is_some_and(|c| has_key(c)), "is_default": is_default,
            "adapter_version": ADAPTER_VERSION, "capabilities": Value::Object(caps),
        }));
    }
    Ok(out)
}

/// Audit row for one real request: codes, counts and material IDs only.
pub fn begin_request(vault: &Vault, root: &str, invocation: &str, connection: &Connection, purpose: &str, materials: &Value) -> Result<String> {
    let id = format!("req_{}", uuid::Uuid::new_v4().simple());
    vault.db().execute(
        "INSERT INTO generation_requests(id,root,invocation,connection_id,purpose,host,model_id,status,materials,started_at) VALUES(?,?,?,?,?,?,?,'sent',?,?)",
        params![id, root, invocation, connection.id, purpose, connection.profile.endpoint.display_host(), connection.profile.model_id, materials.to_string(), now_rfc3339()],
    )?;
    Ok(id)
}

pub fn finish_request(vault: &Vault, id: &str, status: &str, error: Option<&str>, input_tokens: u64, output_tokens: u64) -> Result<()> {
    vault.db().execute(
        "UPDATE generation_requests SET status=?,error=?,input_tokens=?,output_tokens=?,finished_at=? WHERE id=?",
        params![status, error, input_tokens as i64, output_tokens as i64, now_rfc3339(), id],
    )?;
    Ok(())
}
