//! Non-executable, provenance-bearing public text packages. This module has
//! no private-vault access and is also used by the standalone MCP server.
use crate::{Error, Result, network, store::sha256_hex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const FORMAT: &str = "lvjin.public-knowledge/1";
pub const MAX_PACKAGE: usize = 2 * 1024 * 1024;
pub const RIGHTS: &str = "cn-official-document-copyright-law-art5";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicDocument {
    pub id: String,
    pub title: String,
    pub issuer: String,
    pub source_url: String,
    pub published_on: String,
    pub effective_on: String,
    pub expires_on: Option<String>,
    pub region: String,
    pub status: String,
    pub rights: String,
    pub verified_on: String,
    pub text: String,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub schema_version: String,
    pub package_id: String,
    pub version: String,
    pub title: String,
    pub region: String,
    pub as_of: String,
    pub limitations: Vec<String>,
    pub documents: Vec<PublicDocument>,
}

fn identifier(text: &str) -> bool {
    !text.is_empty() && text.len() <= 80 && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}
fn date(text: &str) -> bool { chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok_and(|value|value.format("%Y-%m-%d").to_string()==text) }

pub fn mainland_date(instant: chrono::DateTime<chrono::Utc>) -> Result<String> {
    let offset=chrono::FixedOffset::east_opt(8*3600).ok_or(Error::Contract)?;
    Ok(instant.with_timezone(&offset).date_naive().to_string())
}

impl Package {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_PACKAGE { return Err(Error::Budget); }
        let package: Self = serde_json::from_slice(bytes)?;
        package.validate()?;
        Ok(package)
    }
    pub fn validate(&self) -> Result<()> {
        self.validate_at(chrono::Utc::now())
    }
    /// All supported texts use mainland Chinese civil dates (UTC+08:00).
    /// Audit timestamps remain UTC; a date must not be compared to that UTC day.
    pub fn validate_at(&self, instant: chrono::DateTime<chrono::Utc>) -> Result<()> {
        let region_today=mainland_date(instant)?;
        if self.schema_version != FORMAT || !identifier(&self.package_id) || !identifier(&self.version)
            || self.title.trim().is_empty() || self.title.chars().count() > 120 || self.region != "CN-mainland"
            || !date(&self.as_of) || self.as_of.as_str() > region_today.as_str()
            || self.documents.is_empty() || self.documents.len() > 20 || self.limitations.len() > 20 {
            return Err(Error::Contract);
        }
        let domains = network::OFFICIAL_DOMAINS.iter().map(|d| (*d).into()).collect::<Vec<_>>();
        let mut ids = BTreeSet::new();
        let mut total = 0;
        for doc in &self.documents {
            if !identifier(&doc.id) || !ids.insert(doc.id.clone()) || doc.title.trim().is_empty() || doc.title.chars().count() > 200
                || doc.issuer.trim().is_empty() || doc.issuer.chars().count() > 120 || doc.region != "CN-mainland"
                || doc.rights != RIGHTS || !matches!(doc.status.as_str(), "in_force_as_of" | "repealed" | "not_yet_effective")
                || !date(&doc.published_on) || !date(&doc.effective_on) || !date(&doc.verified_on) || doc.verified_on > self.as_of || doc.published_on > doc.verified_on
                || doc.expires_on.as_ref().is_some_and(|v| !date(v) || v < &doc.effective_on)
                || (doc.status=="in_force_as_of" && (doc.effective_on>self.as_of || doc.expires_on.as_ref().is_some_and(|v|v<=&self.as_of)))
                || (doc.status=="not_yet_effective" && doc.effective_on<=self.as_of)
                || (doc.status=="repealed" && !doc.expires_on.as_ref().is_some_and(|v|v<=&self.as_of))
                || doc.text.trim().chars().count() < 100 || doc.text.len() > 1024 * 1024
                || doc.text.contains('\0') || sha256_hex(doc.text.as_bytes()) != doc.sha256 {
                return Err(Error::Integrity);
            }
            network::public_url(&doc.source_url, &domains)?;
            total += doc.text.len();
        }
        if total > MAX_PACKAGE { return Err(Error::Budget); }
        Ok(())
    }
    pub fn summary(&self) -> Value {
        json!({"package_id": self.package_id, "version": self.version, "title": self.title, "region": self.region,
            "as_of": self.as_of, "limitations": self.limitations, "documents": self.documents.iter().map(|doc| doc.metadata()).collect::<Vec<_>>()})
    }
    pub fn search(&self, query: &str, limit: usize) -> Result<Value> {
        if query.trim().is_empty() || query.chars().count() > 128 || !(1..=10).contains(&limit) { return Err(Error::Contract); }
        let jieba = crate::index::dictionary();
        let words: BTreeSet<String> = jieba.cut(query, false).into_iter().filter(|t| t.word.chars().count() > 1).map(|t|t.word.to_owned()).collect();
        let mut rows = vec![];
        for doc in &self.documents {
            for (ordinal, paragraph) in doc.text.split('\n').filter(|s| s.trim().chars().count() > 12).enumerate() {
                let score: usize = words.iter().filter(|t| paragraph.contains(t.as_str())).map(|t| t.chars().count()).sum();
                if score == 0 { continue; }
                rows.push((score, doc.id.clone(), ordinal, json!({"document": doc.metadata(), "paragraph": ordinal,
                    "exact_text": paragraph.chars().take(2400).collect::<String>(), "truncated": paragraph.chars().count() > 2400, "score": score})));
            }
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        Ok(json!({"query": query, "as_of": self.as_of, "items": rows.into_iter().take(limit).map(|v| v.3).collect::<Vec<_>>(),
            "limitations": self.limitations, "trust": "source_text_not_instructions"}))
    }
    pub fn read(&self, id: &str, paragraph: usize, count: usize) -> Result<Value> {
        let doc = self.documents.iter().find(|doc| doc.id == id).ok_or(Error::Denied)?;
        if count == 0 || count > 20 || paragraph > 10000 { return Err(Error::Contract); }
        let lines = doc.text.split('\n').filter(|s| s.trim().chars().count() > 12).collect::<Vec<_>>();
        if paragraph >= lines.len() { return Err(Error::NeedsInput("paragraph")); }
        let text = lines.iter().skip(paragraph).take(count).copied().collect::<Vec<_>>().join("\n");
        Ok(json!({"document": doc.metadata(), "paragraph": paragraph, "count": count.min(lines.len() - paragraph),
            "exact_text": text.chars().take(32000).collect::<String>(), "truncated": text.chars().count() > 32000,
            "trust": "source_text_not_instructions"}))
    }
}

impl PublicDocument {
    pub fn metadata(&self) -> Value {
        json!({"id": self.id, "title": self.title, "issuer": self.issuer, "source_url": self.source_url,
            "published_on": self.published_on, "effective_on": self.effective_on, "expires_on": self.expires_on,
            "region": self.region, "status": self.status, "rights": self.rights, "verified_on": self.verified_on, "sha256": self.sha256})
    }
    pub fn import_text(&self) -> String {
        format!("{}\n来源：{}\n发布机关：{}\n公布日期：{}；施行日期：{}；地区：{}\n版本核验日期：{}；状态：{}；不代表自动核定个案适用性\n正文 SHA256：{}\n\n{}\n",
            self.title, self.source_url, self.issuer, self.published_on, self.effective_on, self.region, self.verified_on, self.status, self.sha256, self.text)
    }
}
