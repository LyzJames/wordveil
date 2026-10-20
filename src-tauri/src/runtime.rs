use crate::{
    Error, Result,
    ledger::{Dispatch, Ledger},
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputField {
    pub name: String,
    pub r#type: String,
    pub required: bool,
    pub rule: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub description: String,
    pub actors: Vec<String>,
    pub direct_capabilities: Vec<String>,
    /// Capabilities that only specific operations need (design catalog
    /// `conditional_capabilities`). They widen the ceiling, never the grants:
    /// the core grants them only to explicitly confirmed or protected roots.
    #[serde(default)]
    pub conditional_capabilities: Vec<String>,
    pub children: Vec<String>,
    pub handler_binding: String,
    pub input_fields: Vec<InputField>,
    pub version: String,
    pub schema_version: String,
    pub implementation_status: String,
    pub input_schema: String,
    pub output_schema: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub id: String,
    pub title: String,
    pub generation_skill: String,
    pub generation_purpose: String,
    pub allowed_skills: Vec<String>,
    pub version: String,
    #[serde(default)]
    pub tool_input_constraints: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct SkillCatalog {
    skills: Vec<Skill>,
}
#[derive(Deserialize)]
struct RoleCatalog {
    roles: Vec<Role>,
}
#[derive(Deserialize)]
struct LockedFile {
    path: String,
    sha256: String,
    bytes: u64,
}
#[derive(Deserialize)]
struct ResourceLock {
    files: Vec<LockedFile>,
    release_complete: bool,
}

pub struct Registry {
    pub skills: BTreeMap<String, Skill>,
    pub roles: BTreeMap<String, Role>,
    schemas: BTreeMap<String, jsonschema::Validator>,
    outputs: BTreeMap<String, jsonschema::Validator>,
    /// Locked system prompts and role instructions: id → (text, sha256).
    prompts: BTreeMap<String, (String, String)>,
    /// Model-facing output schemas (labels instead of references) per purpose.
    wire: BTreeMap<String, Value>,
    /// Goal, acceptance and stop texts of the fixed flows' role tasks.
    goals: BTreeMap<String, Goal>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub goal: String,
    pub acceptance: String,
    pub stop: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptEntry {
    id: String,
    file: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptCatalog {
    #[allow(dead_code)]
    schema_version: String,
    #[allow(dead_code)]
    version: String,
    prompts: Vec<PromptEntry>,
    goals: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalFile {
    #[allow(dead_code)]
    schema_version: String,
    goals: BTreeMap<String, Goal>,
}

/// Purposes whose model output is parsed against a locked wire schema.
pub const WIRE_PURPOSES: &[&str] = &["search_plan", "draft_answer", "task_plan", "role_analysis", "role_check", "context_brief", "direct_media"];

impl Registry {
    /// M0 prototype loading is explicit. Normal installation must require complete=true.
    pub fn load(resources: &Path, require_complete: bool) -> Result<Self> {
        let resources = resources.canonicalize()?;
        let lock_bytes=std::fs::read(resources.join("resource-lock.json"))?;
        if Sha256::digest(&lock_bytes)!=Sha256::digest(include_bytes!("../../resources/resource-lock.json")) {
            return Err(Error::Integrity);
        }
        let lock: ResourceLock = serde_json::from_slice(&lock_bytes)?;
        if require_complete && !lock.release_complete {
            return Err(Error::Integrity);
        }
        for file in lock.files {
            let path = resources.join(&file.path).canonicalize()?;
            if !path.starts_with(&resources) {
                return Err(Error::Integrity);
            }
            let data = std::fs::read(path)?;
            let digest = format!("{:x}", Sha256::digest(&data));
            if data.len() as u64 != file.bytes || digest != file.sha256 {
                return Err(Error::Integrity);
            }
        }
        let catalog: SkillCatalog =
            serde_json::from_slice(&std::fs::read(resources.join("skills/catalog.json"))?)?;
        let roles: RoleCatalog =
            serde_json::from_slice(&std::fs::read(resources.join("agents/catalog.json"))?)?;
        let mut schemas = BTreeMap::new();
        let mut outputs = BTreeMap::new();
        let mut skills = BTreeMap::new();
        for skill in catalog.skills {
            let value: Value = serde_json::from_slice(&std::fs::read(
                resources
                    .join("skills")
                    .join(&skill.id)
                    .join(&skill.input_schema),
            )?)?;
            let schema = jsonschema::validator_for(&value).map_err(|_| Error::Integrity)?;
            schemas.insert(skill.id.clone(), schema);
            let output:Value=serde_json::from_slice(&std::fs::read(resources.join("skills").join(&skill.id).join(&skill.output_schema))?)?;
            outputs.insert(skill.id.clone(),jsonschema::validator_for(&output).map_err(|_|Error::Integrity)?);
            if skills.insert(skill.id.clone(), skill).is_some() {
                return Err(Error::Integrity);
            }
        }
        for purpose in [
            "search_plan",
            "task_plan",
            "draft_answer",
            "role_analysis",
            "role_check",
            "context_brief",
            "direct_media",
        ] {
            let value: Value = serde_json::from_slice(&std::fs::read(
                resources
                    .join("contracts/generation")
                    .join(format!("{purpose}.input.schema.json")),
            )?)?;
            schemas.insert(
                format!("purpose:{purpose}"),
                jsonschema::validator_for(&value).map_err(|_| Error::Integrity)?,
            );
            let output:Value=serde_json::from_slice(&std::fs::read(resources.join("contracts/generation").join(format!("{purpose}.output.schema.json")))?)?;
            outputs.insert(format!("purpose:{purpose}"),jsonschema::validator_for(&output).map_err(|_|Error::Integrity)?);
        }
        let roles: BTreeMap<_, _> = roles
            .roles
            .into_iter()
            .map(|role| (role.id.clone(), role))
            .collect();
        if skills.len() != 30 || roles.len() != 7 {
            return Err(Error::Integrity);
        }
        let mut wire = BTreeMap::new();
        for purpose in WIRE_PURPOSES {
            let value: Value = serde_json::from_slice(&std::fs::read(resources.join("contracts/generation").join(format!("{purpose}.wire.schema.json")))?)?;
            jsonschema::validator_for(&value).map_err(|_| Error::Integrity)?;
            wire.insert((*purpose).to_owned(), value);
        }
        let catalog: PromptCatalog = serde_json::from_slice(&std::fs::read(resources.join("prompts/catalog.json"))?)?;
        let mut prompts = BTreeMap::new();
        for entry in catalog.prompts {
            let text = std::fs::read_to_string(resources.join("prompts").join(&entry.file))?;
            if format!("{:x}", Sha256::digest(text.as_bytes())) != entry.sha256 || prompts.insert(entry.id, (text, entry.sha256)).is_some() {
                return Err(Error::Integrity);
            }
        }
        let goals: GoalFile = serde_json::from_slice(&std::fs::read(resources.join("prompts").join(&catalog.goals))?)?;
        let registry = Self {
            skills,
            roles,
            schemas,
            outputs,
            prompts,
            wire,
            goals: goals.goals,
        };
        for skill in registry.skills.values() {
            registry.depth(&skill.id, &mut BTreeSet::new())?;
        }
        Ok(registry)
    }

    /// A locked prompt and its content hash (recorded with every request).
    pub fn prompt(&self, id: &str) -> Result<(&str, &str)> {
        self.prompts.get(id).map(|(text, sha)| (text.as_str(), sha.as_str())).ok_or(Error::Integrity)
    }

    pub fn wire(&self, purpose: &str) -> Result<&Value> {
        self.wire.get(purpose).ok_or(Error::Integrity)
    }

    pub fn goal(&self, id: &str) -> Result<&Goal> {
        self.goals.get(id).ok_or(Error::Denied)
    }

    fn depth(&self, id: &str, ancestors: &mut BTreeSet<String>) -> Result<usize> {
        if !ancestors.insert(id.into()) {
            return Err(Error::Integrity);
        }
        let skill = self.skills.get(id).ok_or(Error::Integrity)?;
        let mut depth = 1;
        for child in &skill.children {
            depth = depth.max(1 + self.depth(child, ancestors)?);
        }
        ancestors.remove(id);
        if depth > 4 {
            return Err(Error::Integrity);
        }
        Ok(depth)
    }

    fn validate(&self, skill: &str, purpose: Option<&str>, input: &Value) -> Result<()> {
        let key = purpose
            .map(|value| format!("purpose:{value}"))
            .unwrap_or_else(|| skill.into());
        if !self
            .schemas
            .get(&key)
            .ok_or(Error::Contract)?
            .is_valid(input)
        {
            return Err(Error::Contract);
        }
        // Cross-field checks are repeated at the trusted boundary; Python is untrusted.
        if skill == "knowledge-search" {
            let queries = input["queries"].as_array().ok_or(Error::Contract)?;
            let ids: BTreeSet<_> = queries
                .iter()
                .map(|query| query["query_id"].as_str())
                .collect();
            if queries.first().map(|query| query["query_id"].as_str()) != Some(Some("q0"))
                || ids.len() != queries.len()
            {
                return Err(Error::Contract);
            }
        }
        if skill == "contract-review"
            && input["document_versions"]
                .as_array()
                .is_none_or(Vec::is_empty)
            && input["media_refs"].as_array().is_none_or(Vec::is_empty)
        {
            return Err(Error::Contract);
        }
        if purpose == Some("draft_answer") || (skill == "answer-compose" && purpose.is_none()) {
            if input.get("draft_ref").is_some_and(|v| !v.is_null())
                != input
                    .get("validation_report_ref")
                    .is_some_and(|v| !v.is_null())
            {
                return Err(Error::Contract);
            }
        }
        if skill == "media-observe" {
            let comparison = input.get("comparison_ref").is_some_and(|v| !v.is_null());
            if (input["observation_mode"] == "targeted_compare") != comparison {
                return Err(Error::Contract);
            }
        }
        Ok(())
    }

    fn capability_closure(&self, id: &str) -> BTreeSet<String> {
        let mut result = BTreeSet::new();
        if let Some(skill) = self.skills.get(id) {
            result.extend(skill.direct_capabilities.iter().cloned());
            result.extend(skill.conditional_capabilities.iter().cloned());
            for child in &skill.children {
                result.extend(self.capability_closure(child));
            }
        }
        result
    }
}

/// Cannot be deserialized, constructed by a worker, or passed in a business payload.
pub struct UserAction {
    scope: String,
    capabilities: Vec<String>,
    action: String,
}

impl UserAction {
    /// Only trusted native code paths (UI command handlers, confirmed jobs)
    /// inside the core crate construct actions. `action` records how the root
    /// was authorised: ui, confirm (explicit user confirmation), session (read
    /// views), protected (passphrase/backup flows) or system (confirmed jobs).
    pub(crate) fn trusted(scope: &str, capabilities: &[&str], action: &str) -> Self {
        UserAction {
            scope: scope.into(),
            capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
            action: action.into(),
        }
    }
}
pub struct RootHandle {
    id: String,
    revision: i64,
}
impl RootHandle {
    pub fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Clone)]
pub struct Invocation {
    id: String,
    root: String,
    revision: i64,
    skill: String,
    actor: String,
    parent: Option<String>,
    caused_by: Option<String>,
    depth: u32,
    ceiling: BTreeSet<String>,
    purpose: Option<String>,
    agent: Option<String>,
}
impl Invocation {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn root_id(&self) -> &str {
        &self.root
    }
    pub fn skill(&self) -> &str {
        &self.skill
    }
    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent.as_deref()
    }
    pub fn caused_by_id(&self) -> Option<&str> { self.caused_by.as_deref() }
    /// The registered role instance this invocation acts for, if any.
    pub fn agent_id(&self) -> Option<&str> {
        self.agent.as_deref()
    }
    pub fn actor(&self) -> &str {
        &self.actor
    }
    pub fn purpose(&self) -> Option<&str> {
        self.purpose.as_deref()
    }
}
#[derive(Clone)]
pub struct AgentHandle {
    id: String,
    root: String,
    owner: String,
    role: String,
}
impl AgentHandle {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn role(&self) -> &str {
        &self.role
    }
    pub fn owner_id(&self) -> &str {
        &self.owner
    }
}

/// Purpose bound by the core to a skill invoked outside a role.
fn default_purpose(skill: &str) -> Option<&'static str> {
    match skill {
        "query-plan" => Some("search_plan"),
        "answer-compose" => Some("draft_answer"),
        "media-observe" => Some("direct_media"),
        _ => None,
    }
}

pub struct Runtime {
    pub registry: Registry,
    pub ledger: Ledger,
}

impl Runtime {
    /// The application calls this with an action captured by trusted native UI code.
    pub fn begin_root(&mut self, action: UserAction) -> Result<RootHandle> {
        let root = RootHandle {
            id: Uuid::new_v4().to_string(),
            revision: 1,
        };
        self.ledger
            .create_root(&root.id, &action.scope, &action.capabilities, &action.action)?;
        Ok(root)
    }

    pub fn resume_root(&self, action: UserAction, identity: &str) -> Result<RootHandle> {
        let (scope, revision): (String, i64) = self.ledger.db.query_row(
            "SELECT scope,revision FROM roots WHERE id=? AND cancelled=0",
            [identity],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if scope != action.scope {
            return Err(Error::Denied);
        }
        Ok(RootHandle {
            id: identity.into(),
            revision,
        })
    }

    pub fn invoke_ui(
        &mut self,
        root: &RootHandle,
        skill: &str,
        input: &Value,
    ) -> Result<Invocation> {
        self.ledger.check(&root.id, root.revision)?;
        let registered = self.registry.skills.get(skill).ok_or(Error::Denied)?;
        if !registered.actors.iter().any(|actor| actor == "ui") {
            return Err(Error::Denied);
        }
        let purpose = default_purpose(skill);
        self.registry.validate(skill, purpose, input)?;
        self.validate_refs(&root.id, root.revision, skill, purpose, input)?;
        let invocation = Invocation {
            id: Uuid::new_v4().to_string(),
            root: root.id.clone(),
            revision: root.revision,
            skill: skill.into(),
            actor: "ui".into(),
            parent: None,
            caused_by: None,
            depth: 1,
            ceiling: self.registry.capability_closure(skill),
            purpose: purpose.map(str::to_owned),
            agent: None,
        };
        self.save(&invocation)?;
        // The skill a user starts owns the delivered result; a written analysis
        // hands that ownership to the lvjin-retrieval child its flow opens.
        if matches!(
            skill,
            "lvjin-retrieval"
                | "answer-compose"
                | "contract-review"
                | "loan-analysis"
                | "budget-analysis"
                | "image-review"
                | "audio-review"
                | "video-review"
        ) {
            self.ledger.db.execute(
                "UPDATE roots SET finalizer=COALESCE(finalizer,?) WHERE id=?",
                params![invocation.id, root.id],
            )?;
        }
        Ok(invocation)
    }

    pub fn invoke_child(
        &mut self,
        parent: &Invocation,
        skill: &str,
        input: &Value,
    ) -> Result<Invocation> {
        self.child(parent, skill, input, None, None)
    }

    fn child(
        &mut self,
        parent: &Invocation,
        skill: &str,
        input: &Value,
        role: Option<&Role>,
        agent: Option<&str>,
    ) -> Result<Invocation> {
        self.ledger.check(&parent.root, parent.revision)?;
        let inherited_role = if role.is_none() {
            if let Some(agent) = &parent.agent {
                let role_id: String = self.ledger.db.query_row(
                    "SELECT role FROM agents WHERE id=? AND root=?",
                    params![agent, parent.root],
                    |row| row.get(0),
                )?;
                Some(
                    self.registry
                        .roles
                        .get(&role_id)
                        .ok_or(Error::Denied)?
                        .clone(),
                )
            } else {
                None
            }
        } else {
            None
        };
        let role = role.or(inherited_role.as_ref());
        let owner = self
            .registry
            .skills
            .get(&parent.skill)
            .ok_or(Error::Denied)?;
        if parent.depth >= 4 || !owner.children.iter().any(|id| id == skill) {
            return Err(Error::Denied);
        }
        let registered = self.registry.skills.get(skill).ok_or(Error::Denied)?;
        let actor = if role.is_some() || parent.actor == "agent" {
            "agent"
        } else {
            parent.actor.as_str()
        };
        if !registered.actors.iter().any(|allowed| allowed == actor) {
            return Err(Error::Denied);
        }
        if let Some(role) = role {
            if !role.allowed_skills.iter().any(|allowed| allowed == skill) {
                return Err(Error::Denied);
            }
            if let Some(constraints) = role.tool_input_constraints.get(skill) {
                for (field, expected) in constraints {
                    if input[field].as_str() != Some(expected) {
                        return Err(Error::Denied);
                    }
                }
            }
        }
        let purpose = default_purpose(skill);
        self.registry.validate(skill, purpose, input)?;
        self.validate_refs(&parent.root, parent.revision, skill, purpose, input)?;
        let ceiling = self
            .registry
            .capability_closure(skill)
            .intersection(&parent.ceiling)
            .cloned()
            .collect();
        let invocation = Invocation {
            id: Uuid::new_v4().to_string(),
            root: parent.root.clone(),
            revision: parent.revision,
            skill: skill.into(),
            actor: actor.into(),
            parent: Some(parent.id.clone()),
            caused_by: parent.caused_by.clone(),
            depth: parent.depth + 1,
            ceiling,
            purpose: purpose.map(str::to_owned),
            agent: agent.map(str::to_owned).or_else(|| parent.agent.clone()),
        };
        self.save(&invocation)?;
        Ok(invocation)
    }

    pub fn register_role(&mut self, owner: &Invocation, role_id: &str) -> Result<AgentHandle> {
        self.ledger.check(&owner.root, owner.revision)?;
        if owner.agent.is_some() || !self.registry.roles.contains_key(role_id) {
            return Err(Error::Denied);
        }
        let count: u32 = self.ledger.db.query_row(
            "SELECT COUNT(*) FROM agents WHERE root=?",
            [&owner.root],
            |row| row.get(0),
        )?;
        if count >= 6 {
            return Err(Error::Budget);
        }
        let handle = AgentHandle {
            id: Uuid::new_v4().to_string(),
            root: owner.root.clone(),
            owner: owner.id.clone(),
            role: role_id.into(),
        };
        self.ledger.db.execute(
            "INSERT INTO agents(id,root,owner,role) VALUES(?,?,?,?)",
            params![handle.id, handle.root, handle.owner, handle.role],
        )?;
        Ok(handle)
    }

    pub fn invoke_role_tool(
        &mut self,
        owner: &Invocation,
        agent: &AgentHandle,
        skill: &str,
        input: &Value,
    ) -> Result<Invocation> {
        if agent.owner != owner.id || agent.root != owner.root {
            return Err(Error::Denied);
        }
        let role = self
            .registry
            .roles
            .get(&agent.role)
            .ok_or(Error::Denied)?
            .clone();
        self.child(owner, skill, input, Some(&role), Some(&agent.id))
    }

    pub fn invoke_role_generation(
        &mut self,
        owner: &Invocation,
        agent: &AgentHandle,
        input: &Value,
    ) -> Result<Invocation> {
        self.ledger.check(&owner.root, owner.revision)?;
        if agent.owner != owner.id || agent.root != owner.root {
            return Err(Error::Denied);
        }
        let role = self.registry.roles.get(&agent.role).ok_or(Error::Denied)?;
        if owner.depth >= 4
            || !self.registry.skills[&owner.skill]
                .children
                .contains(&role.generation_skill)
        {
            return Err(Error::Denied);
        }
        self.registry.validate(
            &role.generation_skill,
            Some(&role.generation_purpose),
            input,
        )?;
        self.validate_refs(
            &owner.root,
            owner.revision,
            &role.generation_skill,
            Some(&role.generation_purpose),
            input,
        )?;
        let invocation = Invocation {
            id: Uuid::new_v4().to_string(),
            root: owner.root.clone(),
            revision: owner.revision,
            skill: role.generation_skill.clone(),
            actor: "agent".into(),
            parent: Some(owner.id.clone()),
            caused_by: None,
            depth: owner.depth + 1,
            ceiling: self
                .registry
                .capability_closure(&role.generation_skill)
                .intersection(&owner.ceiling)
                .cloned()
                .collect(),
            purpose: Some(role.generation_purpose.clone()),
            agent: Some(agent.id.clone()),
        };
        self.save(&invocation)?;
        Ok(invocation)
    }

    /// Core context maintenance (design 8): a lossy brief the core itself
    /// requests under `owner`, never a role. It counts like every request.
    pub fn invoke_maintenance(&mut self, owner: &Invocation, input: &Value) -> Result<Invocation> {
        self.ledger.check(&owner.root, owner.revision)?;
        if owner.depth >= 4 || !self.registry.skills.get(&owner.skill).ok_or(Error::Denied)?.children.iter().any(|c| c == "answer-compose") {
            return Err(Error::Denied);
        }
        self.registry.validate("answer-compose", Some("context_brief"), input)?;
        self.validate_refs(&owner.root, owner.revision, "answer-compose", Some("context_brief"), input)?;
        let invocation = Invocation {
            id: Uuid::new_v4().to_string(),
            root: owner.root.clone(),
            revision: owner.revision,
            skill: "answer-compose".into(),
            actor: "system".into(),
            parent: Some(owner.id.clone()),
            caused_by: None,
            depth: owner.depth + 1,
            ceiling: self.registry.capability_closure("answer-compose").intersection(&owner.ceiling).cloned().collect(),
            purpose: Some("context_brief".into()),
            agent: None,
        };
        self.save(&invocation)?;
        Ok(invocation)
    }

    /// A validated suggestion keeps its real workflow parent; the generation
    /// invocation is a separate causal link and never grants authority.
    pub fn invoke_role_suggested_tool(&mut self,owner:&Invocation,agent:&AgentHandle,cause:&Invocation,skill:&str,input:&Value)->Result<Invocation> {
        self.ledger.check(&owner.root,owner.revision)?;
        if cause.root!=owner.root || cause.parent.as_deref()!=Some(&owner.id) || cause.agent.as_deref()!=Some(&agent.id) || cause.purpose.is_none() { return Err(Error::Denied); }
        let dispatched:bool=self.ledger.db.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE root=? AND invocation=?)",params![owner.root,cause.id],|row|row.get(0))?;
        if !dispatched { return Err(Error::Denied); }
        let mut invocation=self.invoke_role_tool(owner,agent,skill,input)?;
        invocation.caused_by=Some(cause.id.clone());
        self.ledger.db.execute("UPDATE invocations SET caused_by=? WHERE id=?",params![cause.id,invocation.id])?;
        Ok(invocation)
    }

    pub fn authorize(&self, invocation: &Invocation, capability: &str) -> Result<()> {
        self.ledger.check(&invocation.root, invocation.revision)?;
        if !invocation.ceiling.contains(capability) {
            return Err(Error::Denied);
        }
        let granted: bool = self.ledger.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM grants WHERE root=? AND capability=?)",
            params![invocation.root, capability],
            |row| row.get(0),
        )?;
        if !granted {
            return Err(Error::Denied);
        }
        Ok(())
    }

    pub fn dispatch_generation(
        &mut self,
        invocation: &Invocation,
        request_id: &str,
        native: bool,
    ) -> Result<Dispatch> {
        self.authorize(invocation, "generation.request")?;
        if invocation.purpose.is_none()
            || native != (invocation.purpose.as_deref() == Some("direct_media"))
        {
            return Err(Error::Denied);
        }
        if invocation.purpose.as_deref()==Some("draft_answer") {
            let finalizer:Option<String>=self.ledger.db.query_row("SELECT finalizer FROM roots WHERE id=? AND finalized=0",[&invocation.root],|r|r.get(0)).optional()?.flatten();
            let finalizer=finalizer.ok_or(Error::Denied)?;
            let mut current=Some(invocation.id.clone());let mut owned=false;
            for _ in 0..4 {
                let Some(id)=current else {break;};
                if id==finalizer {owned=true;break;}
                current=self.ledger.db.query_row("SELECT parent FROM invocations WHERE id=? AND root=?",params![id,invocation.root],|r|r.get(0)).optional()?.flatten();
            }
            if !owned {return Err(Error::Denied);}
        }
        self.ledger.dispatch_generation(
            &invocation.root,
            invocation.revision,
            &invocation.id,
            request_id,
            native,
            invocation.purpose.as_deref()==Some("draft_answer"),
            invocation.agent.as_deref(),
        )
    }

    /// Untrusted generator payloads remain data. A valid plan never dispatches
    /// tools or grants capabilities; every proposed action needs its own checks.
    pub fn validate_generation_output(&self,invocation:&Invocation,output:&Value)->Result<()> {
        self.ledger.check(&invocation.root,invocation.revision)?;
        let purpose=invocation.purpose.as_deref().ok_or(Error::Denied)?;
        let dispatched:bool=self.ledger.db.query_row("SELECT EXISTS(SELECT 1 FROM requests WHERE root=? AND invocation=?)",params![invocation.root,invocation.id],|r|r.get(0))?;
        if !dispatched {return Err(Error::Denied);}
        if !self.registry.outputs.get(&format!("purpose:{purpose}")).ok_or(Error::Integrity)?.is_valid(output) {return Err(Error::Contract);}
        if purpose=="task_plan" {
            let nodes=output["task_graph"].as_array().ok_or(Error::Contract)?;
            let mut remaining:BTreeMap<&str,BTreeSet<&str>>=BTreeMap::new();
            for node in nodes {
                let id=node["node_id"].as_str().ok_or(Error::Contract)?;
                let deps=node["depends_on"].as_array().ok_or(Error::Contract)?.iter().map(|v|v.as_str().ok_or(Error::Contract)).collect::<Result<BTreeSet<_>>>()?;
                if remaining.insert(id,deps).is_some() {return Err(Error::Contract);}
            }
            let mut complete=BTreeSet::new();
            while !remaining.is_empty() {
                let ready=remaining.iter().find(|(_,deps)|deps.is_subset(&complete)).map(|(id,_)|*id).ok_or(Error::Contract)?;
                remaining.remove(ready);complete.insert(ready);
            }
        }
        Ok(())
    }

    /// Common result envelopes are constructed by the core, never copied from
    /// worker authority fields. This also checks their exact closed wire schema.
    pub fn validate_result_envelope(&self,invocation:&Invocation,output:&Value)->Result<()> {
        self.ledger.check(&invocation.root,invocation.revision)?;
        if !self.registry.outputs.get(&invocation.skill).ok_or(Error::Integrity)?.is_valid(output) {return Err(Error::Contract);}
        let skill=self.registry.skills.get(&invocation.skill).ok_or(Error::Integrity)?;
        if output["root_task_id"]!=invocation.root || output["invocation_id"]!=invocation.id || output["skill_id"]!=invocation.skill || output["skill_version"]!=skill.version {return Err(Error::Denied);}
        Ok(())
    }

    fn check_object_kind(&self,invocation:&Invocation,id:&str,expected:&str)->Result<()> {
        let scope=self.ledger.check(&invocation.root,invocation.revision)?;
        let valid:bool=self.ledger.db.query_row("SELECT EXISTS(SELECT 1 FROM objects WHERE id=? AND scope=? AND kind=? AND active=1)",params![id,scope,expected],|r|r.get(0))?;
        if valid {Ok(())} else {Err(Error::Denied)}
    }

    pub fn bind_answer_draft(&mut self,invocation:&Invocation,request:&str,draft:&str)->Result<u32> {
        if invocation.purpose.as_deref()!=Some("draft_answer") {return Err(Error::Denied);}
        self.check_object_kind(invocation,draft,"DraftRef")?;
        self.ledger.bind_answer(&invocation.root,invocation.revision,&invocation.id,request,draft)
    }

    pub fn dispatch_retrieval_round(&mut self,invocation:&Invocation,request:&str)->Result<Dispatch> {
        if invocation.skill!="knowledge-search" {return Err(Error::Denied);}
        self.authorize(invocation,"vault.read")?;
        self.ledger.dispatch_local(&invocation.root,invocation.revision,&invocation.id,request,"retrieval","")
    }

    pub fn dispatch_grounding_unit(&mut self,invocation:&Invocation,request:&str,draft:&str)->Result<Dispatch> {
        if invocation.skill!="answer-grounding" {return Err(Error::Denied);}
        self.authorize(invocation,"local.grounding")?;
        self.check_object_kind(invocation,draft,"DraftRef")?;
        self.ledger.dispatch_local(&invocation.root,invocation.revision,&invocation.id,request,"grounding",draft)
    }

    pub fn transfer_finalizer(&mut self, parent: &Invocation, child: &Invocation) -> Result<()> {
        self.ledger.check(&parent.root, parent.revision)?;
        if child.root != parent.root
            || child.parent.as_deref() != Some(&parent.id)
            || child.skill != "lvjin-retrieval"
        {
            return Err(Error::Denied);
        }
        let changed = self.ledger.db.execute(
            "UPDATE roots SET finalizer=? WHERE id=? AND finalizer=? AND finalized=0",
            params![child.id, parent.root, parent.id],
        )?;
        if changed != 1 {
            return Err(Error::Denied);
        }
        Ok(())
    }

    pub fn finalize(&mut self, owner: &Invocation) -> Result<()> {
        self.ledger.check(&owner.root, owner.revision)?;
        let changed = self.ledger.db.execute(
            "UPDATE roots SET finalized=1 WHERE id=? AND finalizer=? AND finalized=0",
            params![owner.root, owner.id],
        )?;
        if changed != 1 {
            return Err(Error::Denied);
        }
        Ok(())
    }

    pub fn cancel(&mut self, root: &RootHandle) -> Result<()> {
        self.ledger.stop(&root.id)
    }

    pub fn cancel_root(&mut self, root: &str) -> Result<()> {
        self.ledger.stop(root)
    }

    pub(crate) fn root_action(&self, root: &str) -> Result<String> {
        Ok(self.ledger.db.query_row("SELECT action FROM roots WHERE id=?", [root], |r| r.get(0))?)
    }

    /// Rebuild a live invocation of this vault from the ledger (a continued
    /// flow run). The root must be uncancelled at the recorded revision; the
    /// capability ceiling is recomputed from the registry along the parents.
    pub fn reload_invocation(&self, id: &str) -> Result<Invocation> {
        let row: Option<(String, Option<String>, Option<String>, String, String, Option<String>, i64, u32)> = self.ledger.db.query_row(
            "SELECT root,parent,caused_by,skill,actor,agent,revision,depth FROM invocations WHERE id=?", [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
        ).optional()?;
        let (root, parent, caused_by, skill, actor, agent, revision, depth) = row.ok_or(Error::Denied)?;
        self.ledger.check(&root, revision)?;
        let closure = self.registry.capability_closure(&skill);
        let ceiling = match &parent {
            Some(parent) => {
                let parent = self.reload_invocation(parent)?;
                if parent.root != root { return Err(Error::Integrity); }
                closure.intersection(&parent.ceiling).cloned().collect()
            }
            None => closure,
        };
        let purpose = match &agent {
            Some(agent) => {
                let role: String = self.ledger.db.query_row("SELECT role FROM agents WHERE id=? AND root=?", params![agent, root], |r| r.get(0))?;
                let role = self.registry.roles.get(&role).ok_or(Error::Integrity)?;
                if role.generation_skill == skill { Some(role.generation_purpose.clone()) } else { default_purpose(&skill).map(str::to_owned) }
            }
            None => default_purpose(&skill).map(str::to_owned),
        };
        Ok(Invocation { id: id.into(), root, revision, skill, actor, parent, caused_by, depth, ceiling, purpose, agent })
    }

    /// A registered role instance of `root`.
    pub fn agent(&self, root: &str, id: &str) -> Result<AgentHandle> {
        let (owner, role): (String, String) = self.ledger.db.query_row("SELECT owner,role FROM agents WHERE id=? AND root=?", params![id, root], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or(Error::Denied)?;
        Ok(AgentHandle { id: id.into(), root: root.into(), owner, role })
    }

    pub fn handle(&self, root: &str) -> Result<RootHandle> {
        let revision: i64 = self.ledger.db.query_row("SELECT revision FROM roots WHERE id=? AND cancelled=0", [root], |r| r.get(0))?;
        Ok(RootHandle { id: root.into(), revision })
    }

    /// Core-built result envelope. Authority fields come from the invocation,
    /// never from a handler or worker; the closed output schema is enforced.
    pub fn envelope(&self, invocation: &Invocation, outcome: &EnvelopeParts) -> Result<Value> {
        self.ledger.db.execute(
            "INSERT INTO audit(root,invocation,code) VALUES(?,?,?)",
            params![invocation.root, invocation.id, format!("skill.{}.{}", invocation.skill, outcome.status)],
        )?;
        let audit = self.ledger.db.last_insert_rowid();
        let skill = self.registry.skills.get(&invocation.skill).ok_or(Error::Integrity)?;
        let value = serde_json::json!({
            "status": outcome.status,
            "root_task_id": invocation.root,
            "invocation_id": invocation.id,
            "skill_id": invocation.skill,
            "skill_version": skill.version,
            "data_ref": outcome.data_ref,
            "evidence_refs": outcome.evidence_refs,
            "warnings": outcome.warnings.iter().map(|(code, message)| serde_json::json!({"code": code, "message": message})).collect::<Vec<_>>(),
            "coverage": {"processed_refs": outcome.processed, "omitted_refs": outcome.omitted, "limitations": outcome.limitations},
            "usage": {"generation_attempts": 0, "native_media_attempts": 0, "grounding_units": 0},
            "audit_id": format!("audit_{audit}"),
        });
        self.validate_result_envelope(invocation, &value)?;
        Ok(value)
    }
    pub fn revoke(&mut self, root: &RootHandle, capability: &str) -> Result<()> {
        self.ledger.revoke(&root.id, capability)
    }

    fn save(&self, invocation: &Invocation) -> Result<()> {
        let role:Option<String>=match &invocation.agent { Some(agent)=>Some(self.ledger.db.query_row("SELECT role FROM agents WHERE id=? AND root=?",params![agent,invocation.root],|row|row.get(0))?),None=>None };
        self.ledger.db.execute("INSERT INTO invocations(id,root,parent,caused_by,skill,actor,role,agent,revision,depth) VALUES(?,?,?,?,?,?,?,?,?,?)",params![invocation.id,invocation.root,invocation.parent,invocation.caused_by,invocation.skill,invocation.actor,role,invocation.agent,invocation.revision,invocation.depth])?;
        Ok(())
    }

    fn validate_refs(
        &self,
        root: &str,
        revision: i64,
        skill_id: &str,
        purpose: Option<&str>,
        input: &Value,
    ) -> Result<()> {
        let scope = self.ledger.check(root, revision)?;
        let fields = &self
            .registry
            .skills
            .get(skill_id)
            .ok_or(Error::Denied)?
            .input_fields;
        for (name, value) in input.as_object().ok_or(Error::Contract)? {
            let expected = fields
                .iter()
                .find(|field| field.name == *name)
                .map(|field| {
                    field
                        .r#type
                        .trim_end_matches("[]")
                        .trim_end_matches("|null")
                        .to_string()
                });
            // Purpose-specific refs have names fixed by compiled schema, not free JSON.
            let expected = expected.or_else(|| match name.as_str() {
                "context_packet_ref" => Some("ContextPacketRef".into()),
                "catalog_ref" => Some("CatalogRef".into()),
                "goal_ref" => Some("GoalRef".into()),
                "stop_constraints_ref" => Some("StopConstraintsRef".into()),
                "progress_refs" => Some("ProgressRef".into()),
                "original_record_refs" => Some("OriginalRecordRef".into()),
                _ => None,
            });
            if let Some(expected) = expected.filter(|kind| kind.ends_with("Ref")) {
                let values = value
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![value.clone()]);
                for reference in values.into_iter().filter(|value| !value.is_null()) {
                    let identity = reference.as_str().ok_or(Error::Contract)?;
                    let record: Option<(String, String)> = self
                        .ledger
                        .db
                        .query_row(
                            "SELECT scope,kind FROM objects WHERE id=? AND active=1",
                            [identity],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    let (object_scope, kind) = record.ok_or(Error::Denied)?;
                    let alternatives: &[&str] = match expected.as_str() {
                        "ImportSourceRef" => &[
                            "SelectionRef",
                            "DownloadedArtifactRef",
                            "OfflinePackageArtifactRef",
                        ],
                        "ImportTargetRef" => &["MatterRef", "PublicKnowledgeCollectionRef"],
                        "MediaOrSelectionRef" => &["MediaVersionRef", "SelectionRef"],
                        "ImageOrPageRef" => &["ImageRef", "PageRef"],
                        "EvidenceOrNodeRef" => &["EvidenceRef", "NodeRef", "DocumentVersionRef", "FactRef", "FinancialInputRef", "MatterRef"],
                        "KnownConstraintRef" => &["KnownConstraintRef", "FactRef"],
                        "ResultRef" => RESULT_KINDS,
                        "ComponentOrTaskRef" => &["ComponentRef", "TaskRef"],
                        // A targeted re-look names the observation whose field
                        // comparison (direct reading vs local extraction) it checks.
                        "ExtractionAndObservationRef" => &["MediaObservationRef"],
                        _ => &[expected.as_str()],
                    };
                    if object_scope != scope || !alternatives.contains(&kind.as_str()) {
                        return Err(Error::Denied);
                    }
                }
            }
        }
        let _ = purpose;
        Ok(())
    }
}

/// Typed result objects that report-export may reference as ResultRef.
pub const RESULT_KINDS: &[&str] = &["ResultRef", "ImportResultRef", "OcrResultRef", "IndexResultRef", "CandidateSetRef", "EvidenceBundleRef", "EvidenceReadRef", "GraphResultRef", "CalculationRef", "MatterSnapshotRef", "ChangeResultRef", "MediaResultRef", "TranscriptResultRef", "ReviewResultRef", "RetrievalResultRef", "DiagnosticsResultRef", "MaintenanceResultRef", "SearchPlanRef", "DraftRef", "ValidationReportRef", "ConnectionResultRef", "TaskPlanRef", "RoleAnalysisRef", "ChallengeReportRef", "ContextBriefRef", "MediaObservationRef"];

/// Handler outcome before the core seals it into the closed SkillResult schema.
#[derive(Clone, Debug, Default)]
pub struct EnvelopeParts {
    pub status: String,
    pub data_ref: Option<String>,
    pub evidence_refs: Vec<String>,
    pub warnings: Vec<(String, String)>,
    pub processed: Vec<String>,
    pub omitted: Vec<String>,
    pub limitations: Vec<String>,
}

#[cfg(feature = "m0-harness")]
pub mod harness {
    use super::*;
    /// Compiled only for synthetic tests. Not in production builds or worker IPC.
    pub fn ui_action(scope: &str, capabilities: &[&str]) -> UserAction {
        UserAction::trusted(scope, capabilities, "ui")
    }
    pub fn action(scope: &str, capabilities: &[&str], kind: &str) -> UserAction {
        UserAction::trusted(scope, capabilities, kind)
    }
    pub fn register_object(runtime: &Runtime, scope: &str, kind: &str) -> Result<String> {
        let id = format!("ref_{}", Uuid::new_v4().simple());
        runtime.ledger.db.execute(
            "INSERT INTO objects(id,scope,kind,version,active) VALUES(?,?,?,'synthetic-v1',1)",
            params![id, scope, kind],
        )?;
        Ok(id)
    }
}
