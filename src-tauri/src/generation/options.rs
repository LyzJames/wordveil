//! Explicit effort policy. Unknown models are never silently used as a cheap
//! helper with a vendor default. Model profiles are pinned to verified docs.
use super::{Profile,Provider,Protocol};
use crate::{Error,Result};
use serde::{Deserialize,Serialize};
use serde_json::{Value,json};

#[derive(Clone,Copy,Default,Serialize,Deserialize,PartialEq,Eq)]
#[serde(rename_all="snake_case")]
pub enum Effort { #[default] Auto, Low, Medium, High }

pub fn minimum(profile:&Profile)->Result<&'static str> {
    let m=profile.model_id.as_str();
    match profile.provider {
        Provider::Deepseek if matches!(m,"deepseek-chat"|"deepseek-reasoner"|"deepseek-flash"|"deepseek-v4-pro")=>Ok("none"),
        Provider::Openai if m.starts_with("gpt-6.1-sol")||m.starts_with("gpt-6-astra")=>Ok("low"),
        Provider::Openai if ["gpt-6-sol","gpt-6-luna","gpt-5.6","gpt-5.5","gpt-5.4"].iter().any(|prefix|m.starts_with(prefix))=>Ok("none"),
        Provider::Openai if m.starts_with("o3")||m.starts_with("o4-mini")=>Ok("low"),
        Provider::Openai if m.starts_with("gpt-4.1")||m.starts_with("gpt-4o")=>Ok("off"),
        Provider::Anthropic if ["claude-haiku-5-5","claude-sonnet-5-5","claude-sonnet-5","claude-opus-5","claude-opus-4-8","claude-opus-4-7","claude-opus-4-6","claude-sonnet-4-6"].iter().any(|prefix|m.starts_with(prefix))=>Ok("low"),
        Provider::Anthropic if m.starts_with("claude-haiku-4-5")||m.starts_with("claude-3-5-haiku")=>Ok("off"),
        // A loopback/compatible endpoint alone does not prove that the model
        // has no reasoning mode. Unknown local helpers require a known profile.
        Provider::Local=>Err(Error::NeedsInput("helper_reasoning_profile")),
        _=>Err(Error::NeedsInput("helper_reasoning_profile")),
    }
}

pub fn apply(body:&mut Value,profile:&Profile,effort:Effort,helper:bool)->Result<String> {
    let minimum=if helper {Some(minimum(profile)?)}else{None};
    let selected=minimum.unwrap_or(match effort {Effort::Auto=>"auto",Effort::Low=>"low",Effort::Medium=>"medium",Effort::High=>"high"});
    if selected=="auto" {
        if profile.provider==Provider::Deepseek {if let Some(object)=body.as_object_mut(){object.remove("thinking");}}
        return Ok("provider_default".into());
    }
    match profile.provider {
        Provider::Deepseek=>{
            let disabled=matches!(selected,"none"|"off");
            match profile.protocol {
                Protocol::Responses=>{body["reasoning"]=json!({"effort":if disabled{"none"}else if selected=="medium"{"high"}else{selected}});},
                _=>{body["thinking"]=json!({"type":if disabled{"disabled"}else{"enabled"}});if !disabled {
                    let e=if selected=="medium"{"high"}else{selected};
                    if profile.protocol==Protocol::AnthropicMessages{body["output_config"]=json!({"effort":e});}else{body["reasoning_effort"]=json!(e);}
                }}
            }
        },
        Provider::Openai if selected!="off"=>{
            // For a manually selected explicit effort, reject known impossible
            // levels rather than sending a request that silently falls back.
            let supported=minimum.or_else(||self::minimum(profile).ok()).ok_or(Error::NeedsInput("reasoning_profile"))?;
            if supported=="off" {return Err(Error::NeedsInput("reasoning_profile"));}
            if profile.protocol==Protocol::Responses {body["reasoning"]=json!({"effort":selected});}else{body["reasoning_effort"]=json!(selected);}
        },
        Provider::Anthropic=>{
            if selected=="off" {body["thinking"]=json!({"type":"disabled"});}
            else {
                let _=self::minimum(profile)?;
                body["output_config"]=json!({"effort":selected});
                if helper {
                    // Sonnet 5.5 uses between_tools as its lowest thinking mode.
                    body["thinking"]=if profile.model_id.starts_with("claude-sonnet-5-5"){json!({"type":"between_tools"})}else if profile.model_id.starts_with("claude-opus-5-5"){json!({"type":"adaptive"})}else{json!({"type":"disabled"})};
                }
            }
        },
        Provider::Local=>{},
        Provider::Custom=>return Err(Error::NeedsInput("reasoning_profile")),
        _=>{},
    }
    Ok(selected.into())
}
