//! Local text protection. Detectors propose entities; only this module assigns
//! scoped tokens and constructs outgoing text. Neither model output nor YAML
//! can disable credential protection or authorize network activity.
use crate::{Error, Result};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const POLICY_VERSION: &str = "shouyu.text-protection/1";
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_ENTITIES: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Person, Organization, Address, Phone, Email, Identifier, Account, Secret, Sensitive }

impl Kind {
    fn tag(self) -> &'static str {
        match self { Self::Person=>"PERSON", Self::Organization=>"ORG", Self::Address=>"ADDR", Self::Phone=>"PHONE", Self::Email=>"EMAIL", Self::Identifier=>"ID", Self::Account=>"ACCOUNT", Self::Secret=>"SECRET", Self::Sensitive=>"PRIVATE" }
    }
    pub fn from_model_type(value: &str) -> Option<Self> {
        Some(match value.to_lowercase().trim() {
            "人名"|"姓名"|"person"|"person name" => Self::Person,
            "组织"|"机构"|"组织名称"|"organization"|"organisation" => Self::Organization,
            "地址"|"地点"|"address"|"location" => Self::Address,
            "电话"|"电话号码"|"手机号"|"phone"|"phone number" => Self::Phone,
            "邮箱"|"电子邮件"|"email"|"email address" => Self::Email,
            "身份证"|"身份证号码"|"证件号码"|"id number"|"identifier" => Self::Identifier,
            "银行卡号"|"银行账号"|"账号"|"account"|"bank card number" => Self::Account,
            "密码"|"密钥"|"password"|"secret"|"api key" => Self::Secret,
            _ => return None,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateTerm {
    pub value: String,
    pub kind: Kind,
    #[serde(default)] pub aliases: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub pattern: String,
    pub kind: Kind,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)] pub private_terms: Vec<PrivateTerm>,
    #[serde(default)] pub rules: Vec<Rule>,
    #[serde(default)] pub preview_every_request: bool,
}

impl Config {
    pub fn from_yaml(input: &str) -> Result<Self> {
        if input.len()>32*1024 || input.lines().count()>512 || input.contains('\0') { return Err(Error::Contract); }
        // This deliberately restricted configuration format has no YAML graph,
        // tag, directive or multi-document features. Scan tokens outside quoted
        // scalars before invoking the parser, so aliases cannot expand first.
        let mut quote=None;
        let mut escaped=false;
        for line in input.lines() {
            if line.len()>2048 || line.starts_with('\t') || line.chars().take_while(|c|*c==' ').count()>8 { return Err(Error::Contract); }
            if quote.is_none() && matches!(line.trim(),"---"|"...") { return Err(Error::Contract); }
            for c in line.chars() {
                if escaped { escaped=false; continue; }
                match quote {
                    Some('"') if c=='\\' => escaped=true,
                    Some(q) if c==q => quote=None,
                    Some(_) => {},
                    None if c=='#' => break,
                    None if c=='\'' || c=='"' => quote=Some(c),
                    None if matches!(c,'&'|'*'|'!'|'%') => return Err(Error::Contract),
                    _ => {},
                }
            }
            if quote.is_some() { return Err(Error::Contract); }
        }
        let mut options=serde_saphyr::Options::default();
        options.with_snippet=false;
        options.strict_indentation=true;
        options.strict_booleans=true;
        options.reject_unsupported_tags=true;
        options.duplicate_keys=serde_saphyr::DuplicateKeyPolicy::Error;
        options.merge_keys=serde_saphyr::MergeKeyPolicy::Error;
        let mut budget=serde_saphyr::Budget::default();
        budget.max_reader_input_bytes=Some(32*1024);budget.max_depth=12;
        budget.max_documents=1;budget.max_nodes=4096;budget.max_events=8192;
        budget.max_total_scalar_bytes=32*1024;budget.max_aliases=0;budget.max_anchors=0;
        budget.max_merge_keys=0;options.budget=Some(budget);
        let config:Self=serde_saphyr::from_str_with_options(input,options).map_err(|_|Error::Contract)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.private_terms.len()>128 || self.rules.len()>32 { return Err(Error::Contract); }
        let mut seen=BTreeSet::new();
        for term in &self.private_terms {
            if term.value.trim().is_empty() || term.value.len()>1024 || term.aliases.len()>8 || term.value.contains('\0') || !seen.insert(term.value.as_str()) { return Err(Error::Contract); }
            for alias in &term.aliases {
                if alias.trim().is_empty() || alias.len()>1024 || alias.contains('\0') || !seen.insert(alias.as_str()) { return Err(Error::Contract); }
            }
        }
        let mut ids=BTreeSet::new();
        for rule in &self.rules {
            if !rule.id.starts_with("custom_") || rule.id.len()>60 || !rule.id.bytes().all(|c|c.is_ascii_lowercase()||c.is_ascii_digit()||c==b'_') || !ids.insert(&rule.id) { return Err(Error::Contract); }
            let pattern=bounded_regex(&rule.pattern)?;
            if pattern.is_match("") { return Err(Error::Contract); }
        }
        Ok(())
    }
}

pub fn bounded_regex(pattern: &str) -> Result<Regex> {
    if pattern.is_empty() || pattern.len()>512 { return Err(Error::Contract); }
    RegexBuilder::new(pattern).size_limit(256*1024).dfa_size_limit(512*1024).nest_limit(32).build().map_err(|_|Error::Contract)
}

#[derive(Clone)]
pub struct Candidate {
    pub value: String,
    pub kind: Kind,
}

/// The detector sees strong-rule tokens, never their secrets. Some NER output
/// identifies part of such a token (e.g. PERSON) as a new entity. Ignore only
/// occurrences wholly inside verified core marker ranges; mixed ranges and
/// invented values still fail closed.
pub fn detector_candidate(value:&str,view:&str,original:&str)->Result<bool> {
    if value.trim().is_empty()||!view.contains(value){return Err(Error::Worker("privacy_entity_not_in_source".into()));}
    let markers=Regex::new(r"⟦(?:SY:[A-F0-9]{16}:(?:PERSON|ORG|ADDR|PHONE|EMAIL|ID|ACCOUNT|SECRET|PRIVATE):[A-F0-9]{10}|DLP:(?:PERSON|ORG|ADDR|PHONE|EMAIL|ID|ACCOUNT|SECRET|PRIVATE))⟧").map_err(|_|Error::Integrity)?;
    let ranges=markers.find_iter(view).map(|m|(m.start(),m.end())).collect::<Vec<_>>();
    let only_marker=view.match_indices(value).all(|(start,text)|ranges.iter().any(|(from,to)|start>=*from&&start+text.len()<=*to));
    if only_marker{return Ok(false);}
    if original.contains(value){Ok(true)}else{Err(Error::Worker("privacy_entity_not_in_source".into()))}
}

/// Contains raw values and is serializable only for the encrypted vault. It
/// intentionally has no Debug implementation and is never a frontend DTO.
#[derive(Clone, Serialize, Deserialize)]
pub struct EntityMap {
    scope: String,
    entries: Vec<Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry { original:String, token:String, kind:Kind, restore:bool }

pub struct ProtectedText {
    pub wire: String,
    pub replacements: usize,
    pub requires_preview: bool,
}

#[derive(Clone)]
struct Span { start:usize, end:usize, kind:Kind, canonical:String, strong:bool }

impl Default for EntityMap {
    fn default()->Self { Self::new() }
}

impl EntityMap {
    pub fn new()->Self { Self {scope:uuid::Uuid::new_v4().simple().to_string()[..16].to_uppercase(),entries:vec![]} }
    pub fn len(&self)->usize { self.entries.len() }
    pub fn is_empty(&self)->bool { self.entries.is_empty() }

    fn token(&mut self, original:&str, kind:Kind)->Result<String> {
        if let Some(entry)=self.entries.iter().find(|e|e.original==original && e.kind==kind) { return Ok(entry.token.clone()); }
        if self.entries.len()>=MAX_ENTITIES { return Err(Error::Budget); }
        let token=format!("⟦SY:{}:{}:{}⟧",self.scope,kind.tag(),uuid::Uuid::new_v4().simple().to_string()[..10].to_uppercase());
        self.entries.push(Entry {original:original.into(),token:token.clone(),kind,restore:kind!=Kind::Secret});
        Ok(token)
    }

    /// Exact, non-recursive replacement. Original values are appended directly
    /// and are never scanned as tokens again. Unknown tokens stay visibly raw.
    pub fn restore(&self, text:&str)->String {
        let mut out=String::with_capacity(text.len());
        let mut remaining=text;
        while let Some(start)=remaining.find("⟦SY:") {
            out.push_str(&remaining[..start]);
            let tail=&remaining[start..];
            let Some(end)=tail.find('⟧').map(|i|i+'⟧'.len_utf8()) else {out.push_str(tail);return out;};
            let token=&tail[..end];
            if let Some(entry)=self.entries.iter().find(|e|e.token==token) {
                out.push_str(if entry.restore {&entry.original} else {"[凭证已隐藏]"});
            } else {out.push_str(token);}
            remaining=&tail[end..];
        }
        out.push_str(remaining);out
    }

    /// Only known non-credential entities may become local literal terms.
    pub fn local_literal(&self, token:&str)->Result<&str> {
        self.entries.iter().find(|e|e.token==token && e.restore).map(|e|e.original.as_str()).ok_or(Error::Denied)
    }

    pub fn protect(&mut self, original:&str, candidates:&[Candidate], config:&Config)->Result<ProtectedText> {
        if original.len()>MAX_TEXT_BYTES || original.contains('\0') || candidates.len()>MAX_ENTITIES { return Err(Error::Budget); }
        config.validate()?;
        // User-supplied tokens cannot impersonate tokens assigned by the core.
        let text=original.replace("⟦SY:","⟦LITERAL_SY:");
        let mut spans=rule_spans(&text,config)?;
        // Once this conversation/recipient has identified an entity, retain
        // protection even if a later detector call misses the same raw value.
        for entry in &self.entries {
            for (start,value) in text.match_indices(&entry.original) {spans.push(Span{start,end:start+value.len(),kind:entry.kind,canonical:entry.original.clone(),strong:true});}
        }
        let mut preview=config.preview_every_request;
        for candidate in candidates {
            if candidate.value.trim().is_empty() || candidate.value.len()>2048 { return Err(Error::Contract); }
            if !text.contains(&candidate.value) { return Err(Error::Worker("privacy_entity_not_in_source".into())); }
            for (start,value) in text.match_indices(&candidate.value) {
                spans.push(Span {start,end:start+value.len(),kind:candidate.kind,canonical:candidate.value.clone(),strong:false});
                if spans.len()>MAX_ENTITIES*4 { return Err(Error::Budget); }
            }
        }
        spans.sort_by_key(|s|(s.start,std::cmp::Reverse(s.end),std::cmp::Reverse(s.strong)));
        let mut merged:Vec<Span>=vec![];
        for span in spans {
            if let Some(last)=merged.last_mut().filter(|last|span.start<last.end) {
                if span.end<=last.end {
                    if span.kind==Kind::Secret {last.kind=Kind::Secret;last.canonical=text[last.start..last.end].into();last.strong=true;}
                    else if span.start==last.start && span.end==last.end && span.strong && !last.strong {*last=span;}
                    continue;
                }
                last.end=last.end.max(span.end);
                last.kind=if last.kind==Kind::Secret || span.kind==Kind::Secret {Kind::Secret} else {Kind::Sensitive};
                last.canonical=text[last.start..last.end].into();preview=true;
            } else {merged.push(span);}
        }
        if merged.len()>MAX_ENTITIES { return Err(Error::Budget); }
        let mut out=String::with_capacity(text.len());let mut pos=0;
        for span in &merged {
            out.push_str(&text[pos..span.start]);
            out.push_str(&self.token(&span.canonical,span.kind)?);
            pos=span.end;
            preview|=span.kind==Kind::Secret;
        }
        out.push_str(&text[pos..]);
        Ok(ProtectedText {wire:out,replacements:merged.len(),requires_preview:preview})
    }

    /// The local model does not need credential values or registered terms.
    /// A fresh temporary scope keeps this view independent of outbound IDs.
    pub fn detector_view(text:&str,config:&Config)->Result<String> {
        // Random high-entropy outbound IDs look like account numbers to NER.
        // The local detector needs only a functional type tag. Escape any user
        // tag first so an input cannot impersonate a core-generated range.
        let text=text.replace("⟦DLP:","⟦LITERAL_DLP:");
        let mut temporary=Self::new();let protected=temporary.protect(&text,&[],config)?;
        let mut view=protected.wire;
        for entry in &temporary.entries {view=view.replace(&entry.token,&format!("⟦DLP:{}⟧",entry.kind.tag()));}
        Ok(view)
    }
}

fn rule_spans(text:&str,config:&Config)->Result<Vec<Span>> {
    let mut spans=vec![];
    let builtins:[(&str,Kind);10]=[
        (r"\b(?:sk-[A-Za-z0-9_-]{16,}|AKIA[A-Z0-9]{16}|gh[pousr]_[A-Za-z0-9_]{20,}|github_pat_[A-Za-z0-9_]{20,})\b",Kind::Secret),
        (r#"(?i)(?:password|passwd|api[_-]?key|access[_-]?token|secret|密码|密钥)\s*[:=：]\s*["']?(?P<secret>[^\s"',，;；&{}<>]{1,})"#,Kind::Secret),
        (r"(?s)-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----.*?-----END (?:RSA |EC |OPENSSH )?PRIVATE KEY-----",Kind::Secret),
        (r"\beyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b",Kind::Secret),
        // Narrow identity contexts complement measured NER misses. Match only
        // the name, retaining the surrounding verb, dates, numbers and syntax.
        (r"(?:^|[\s，。；:：])(?P<person>[王李张刘陈杨赵黄周吴徐孙胡朱高林何郭马罗梁宋郑谢韩唐冯董程曹袁邓许沈曾彭吕苏卢蒋蔡丁魏薛叶余潘杜夏钟田任姜范方石姚谭金陆孔白崔康毛秦江史顾侯邵孟龙段雷钱汤尹黎][\p{Han}]{1,2})(?:住在|任职于|的(?:电话|邮箱|手机|住址|身份证)|女士|先生|说[:：，,])",Kind::Person),
        (r"(?:姓名|联系人|收件人|申请人)[\s:：=]+(?P<person>[\p{Han}]{2,4})",Kind::Person),
        (r"[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?\.[A-Za-z]{2,63}",Kind::Email),
        (r"(?:\+?86[- ]?)?1[3-9][0-9]{9}",Kind::Phone),
        (r"\b[1-9][0-9]{5}(?:18|19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9Xx]\b",Kind::Identifier),
        (r"(?:[A-Za-z]:\\Users\\|/Users/|/home/)[^\s\\/]+",Kind::Address),
    ];
    for (pattern,kind) in builtins {
        let regex=bounded_regex(pattern)?;
        for matched in regex.captures_iter(text).take(MAX_ENTITIES+1) {let value=matched.name("secret").or_else(||matched.name("person")).or_else(||matched.get(0)).ok_or(Error::Integrity)?;spans.push(Span {start:value.start(),end:value.end(),kind,canonical:value.as_str().into(),strong:true});}
        if spans.len()>MAX_ENTITIES*2 { return Err(Error::Budget); }
    }
    for term in &config.private_terms {
        for value in std::iter::once(&term.value).chain(&term.aliases) {
            for (start,matched) in text.match_indices(value) {spans.push(Span {start,end:start+matched.len(),kind:term.kind,canonical:term.value.clone(),strong:true});}
        }
    }
    for rule in &config.rules {
        for value in bounded_regex(&rule.pattern)?.find_iter(text).take(MAX_ENTITIES+1) {spans.push(Span {start:value.start(),end:value.end(),kind:rule.kind,canonical:value.as_str().into(),strong:true});}
    }
    if spans.len()>MAX_ENTITIES*4 { return Err(Error::Budget); }
    Ok(spans)
}
