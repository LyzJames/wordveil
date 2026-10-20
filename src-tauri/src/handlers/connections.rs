//! connection-settings (protected UI flow): save a connection from a draft,
//! run the synthetic capability self-test against the exact endpoint, or revoke
//! it. Keys never pass through this handler's input; drafts reference a
//! credential the core sealed with DPAPI. The self-test has its own bounded
//! budget and is not a business generation call (contract section 5).
use super::*;
use crate::{
    generation::{connections, credentials::CredentialStore, probe},
    vault::now_rfc3339,
};
use serde_json::{Value, json};

fn store(ctx: &Ctx) -> Result<CredentialStore> {
    let vault = ctx.lock()?;
    CredentialStore::for_vault(&ctx.services.layout.data_root, &vault.id, &vault.keys.credential)
}

pub fn settings(ctx: &Ctx, invocation: &Invocation, input: &Value) -> Result<EnvelopeParts> {
    ctx.authorize(invocation, "settings.connection")?;
    let operation = text_of(input, "operation")?;
    let draft_id = text_of(input, "connection_draft_id")?;
    let draft = connections::read_draft(&*ctx.lock()?, &draft_id)?;
    let payload = match operation.as_str() {
        "save" => {
            ctx.authorize(invocation, "credential.write")?;
            let credentials = store(ctx)?;
            if let Some(credential) = &draft.credential {
                if !credentials.exists(credential) {
                    return Err(Error::NeedsInput("api_key"));
                }
            }
            let (connection, replaced) = {
                let vault = ctx.lock()?;
                let applied = connections::apply_draft(&vault, &draft)?;
                connections::consume_draft(&vault, &draft_id)?;
                applied
            };
            if let Some(old) = replaced {
                credentials.remove(&old)?;
            }
            ctx.services.log.event("connection.saved", &[("key", crate::logging::Field::Flag(draft.credential.is_some()))]);
            json!({"operation": "save", "connection_id": connection, "saved_at": now_rfc3339()})
        }
        "probe" => {
            ctx.authorize(invocation, "network.probe")?;
            let connection_id = draft.connection_id.clone().ok_or(Error::Contract)?;
            let connection = {
                let vault = ctx.lock()?;
                connections::consume_draft(&vault, &draft_id)?;
                connections::load(&vault, &connection_id)?
            };
            if connection.status != "active" {
                return Err(Error::Denied);
            }
            let key = match &connection.credential {
                Some(id) => Some(store(ctx)?.get(id)?),
                None if connection.profile.endpoint.is_loopback() => None,
                None => return Err(Error::NeedsInput("api_key")),
            };
            let binding = connection.profile.binding();
            let started = std::time::Instant::now();
            let mut sent = 0u32;
            let results = probe::run(&connection.profile, key.as_ref(), ctx.cancel, |result| {
                // Record each finished check right away (audit + capability row).
                if let Ok(vault) = ctx.lock() {
                    if result.sent {
                        sent += 1;
                        if let Ok(audit) = connections::begin_request(&vault, invocation.root_id(), invocation.id(), &connection, "capability_probe", &json!({"synthetic": true, "capability": result.capability})) {
                            let status = if matches!(result.status, "supported" | "unknown" | "unsupported") { "completed" } else { "failed" };
                            let _ = connections::finish_request(&vault, &audit, status, None, result.input_tokens, result.output_tokens);
                        }
                    }
                    let _ = connections::record_capability(&vault, &connection.id, &binding, result.capability, result.status, &result.evidence, &result.limits);
                }
            });
            ctx.check()?;
            if sent > probe::MAX_PROBE_REQUESTS {
                return Err(Error::Budget);
            }
            ctx.services.log.event(
                "connection.probed",
                &[("requests", crate::logging::Field::Count(i64::from(sent))), ("ms", crate::logging::Field::Millis(started.elapsed().as_millis() as u64)),
                  ("supported", crate::logging::Field::Count(results.iter().filter(|r| r.status == "supported").count() as i64))],
            );
            json!({"operation": "probe", "connection_id": connection.id, "binding": binding, "requests": sent, "results": results, "probed_at": now_rfc3339()})
        }
        "revoke" => {
            ctx.authorize(invocation, "credential.write")?;
            let connection_id = draft.connection_id.clone().ok_or(Error::Contract)?;
            let credential = {
                let vault = ctx.lock()?;
                connections::consume_draft(&vault, &draft_id)?;
                connections::revoke(&vault, &connection_id)?
            };
            if let Some(credential) = credential {
                store(ctx)?.remove(&credential)?;
            }
            ctx.services.log.event("connection.revoked", &[]);
            json!({"operation": "revoke", "connection_id": connection_id, "revoked_at": now_rfc3339(),
                   "note": "已删除本机保存的 Key，之后不会再向该服务发送请求；已经发送给服务方的内容无法从远端撤回。"})
        }
        _ => return Err(Error::Contract),
    };
    let result = ctx.lock()?.put_result(invocation.skill(), invocation.id(), "ConnectionResultRef", &payload)?;
    Ok(completed(Some(result)))
}
