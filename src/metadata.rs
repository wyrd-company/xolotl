// ---
// relationships:
//   implements: ../docs/design/model-metadata.md
// ---
//! Validated projection of LiteLLM's registry. Unknown fields remain upstream-owned.
use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTRY_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Metadata {
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_cost_per_token: Option<f64>,
    pub output_cost_per_token: Option<f64>,
    pub cache_read_input_token_cost: Option<f64>,
    pub cache_creation_input_token_cost: Option<f64>,
}

impl Metadata {
    pub fn validate(&self) -> Result<()> {
        for n in [self.max_input_tokens, self.max_output_tokens].into_iter().flatten() {
            ensure!(n > 0 && n <= i64::MAX as u64, "token limit must be a positive signed integer");
        }
        for n in
            [self.input_cost_per_token, self.output_cost_per_token, self.cache_read_input_token_cost, self.cache_creation_input_token_cost]
                .into_iter()
                .flatten()
        {
            ensure!(n.is_finite() && n >= 0.0, "price must be finite and nonnegative");
        }
        Ok(())
    }

    pub fn overlay(&mut self, other: &Self) {
        macro_rules! copy { ($($field:ident),*) => { $(if other.$field.is_some() { self.$field = other.$field; })* }; }
        copy!(
            max_input_tokens,
            max_output_tokens,
            input_cost_per_token,
            output_cost_per_token,
            cache_read_input_token_cost,
            cache_creation_input_token_cost
        );
    }

    /// Input includes cache reads/writes. Missing required prices yield unknown, not zero.
    pub fn cost(&self, usage: crate::proxy::TokenUsage) -> Option<f64> {
        let input = usage.input?.max(0);
        let output = usage.output?.max(0);
        let read = usage.cached.unwrap_or(0).clamp(0, input);
        let write = usage.cache_write.unwrap_or(0).clamp(0, input - read);
        let rate = |tokens: i64, price: Option<f64>| if tokens == 0 { Some(0.0) } else { price.map(|p| p * tokens as f64) };
        Some(
            rate(input - read - write, self.input_cost_per_token)?
                + rate(output, self.output_cost_per_token)?
                + rate(read, self.cache_read_input_token_cost)?
                + rate(write, self.cache_creation_input_token_cost)?,
        )
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Override {
    pub litellm_key: Option<String>,
    #[serde(flatten)]
    pub values: Metadata,
    pub tools: Option<bool>,
    pub images: Option<bool>,
    pub reasoning_levels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Registry {
    pub fetched_at: Option<i64>,
    pub entries: BTreeMap<String, Metadata>,
    pub error: Option<String>,
}

impl Registry {
    /// Schema projection follows LiteLLM's current model-catalog/src/schema.rs:
    /// object entries, provider string, integer limits, nonnegative numeric prices.
    pub fn parse(value: Value, at: i64) -> Result<Self> {
        let obj = value.as_object().ok_or_else(|| anyhow::anyhow!("registry must be an object"))?;
        let mut entries = BTreeMap::new();
        for (key, value) in obj {
            if key == "sample_spec" || key == "fallback_generalizations" {
                continue;
            }
            ensure!(value.is_object(), "registry entry must be an object: {key}");
            ensure!(value.get("litellm_provider").is_some_and(Value::is_string), "missing provider: {key}");
            let mut m: Metadata = serde_json::from_value(value.clone())?;
            // LiteLLM uses zero for non-generative limits. It is not a usable window.
            if m.max_input_tokens == Some(0) {
                m.max_input_tokens = None;
            }
            if m.max_output_tokens == Some(0) {
                m.max_output_tokens = None;
            }
            m.validate()?;
            entries.insert(key.clone(), m);
        }
        ensure!(!entries.is_empty(), "registry contains no models");
        Ok(Self { fetched_at: Some(at), entries, error: None })
    }

    pub fn lookup(&self, provider: &str, model: &str, override_: Option<&Override>) -> Resolved {
        let family = match provider {
            "codex" => "openai",
            "claude" => "anthropic",
            "grok" => "xai",
            "opencode-go" if model.starts_with("qwen") => "dashscope",
            _ => provider,
        };
        let candidates = [format!("{provider}/{model}"), format!("{family}/{model}"), model.to_string()];
        let found = if let Some(key) = override_.and_then(|o| o.litellm_key.as_ref()) {
            self.entries.get_key_value(key)
        } else {
            candidates.iter().find_map(|k| self.entries.get_key_value(k))
        };
        let (mut values, mut source) = found.map(|(k, m)| (m.clone(), format!("LiteLLM:{k}"))).unwrap_or_default();
        if let Some(o) = override_ {
            values.overlay(&o.values);
            if !source.is_empty() {
                source.push_str(" + ");
            }
            source.push_str(&format!("override:{provider}/{model}"));
        }
        Resolved {
            values,
            source: (!source.is_empty()).then_some(source),
            fetched_at: found.and(self.fetched_at),
            age_secs: found.and(self.fetched_at).map(|t| (crate::db::now() - t).max(0)),
            error: self.error.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Resolved {
    #[serde(flatten)]
    pub values: Metadata,
    pub source: Option<String>,
    pub fetched_at: Option<i64>,
    pub age_secs: Option<i64>,
    pub error: Option<String>,
}

impl Resolved {
    pub fn description(&self, provider: &str) -> String {
        let m = &self.values;
        let window = m.max_input_tokens.map(|n| format!("{n} context")).unwrap_or_else(|| "unknown context".into());
        let output = m.max_output_tokens.map(|n| format!("{n} max output")).unwrap_or_else(|| "unknown max output".into());
        let price = match (m.input_cost_per_token, m.output_cost_per_token) {
            (Some(i), Some(o)) => format!("${:.4} / ${:.4} per Mtok (API-equivalent)", i * 1e6, o * 1e6),
            _ => "unknown API-equivalent price".into(),
        };
        format!("via {provider} · {window} · {output} · {price}")
    }
}

/// A text/JSON byte heuristic, not a tokenizer or an admission check. Includes
/// tools and system prompts; byte count deliberately errs high on non-ASCII text.
pub fn estimate_tokens(body: &Value) -> u64 {
    ["system", "messages", "tools", "tool_choice"]
        .iter()
        .filter_map(|k| body.get(*k))
        .map(|v| v.to_string().len() as u64)
        .sum::<u64>()
        .div_ceil(3)
        .max(1)
}

/// Shared by HTTP and streaming translation errors.
pub fn too_long(kind: &str, message: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    let m = message.to_ascii_lowercase();
    ["context_length_exceeded", "max_tokens_exceeded", "prompt_too_long", "input_too_long"].contains(&k.as_str())
        || [
            "prompt is too long",
            "prompt too long",
            "maximum context length",
            "context length exceeded",
            "exceeds the context window",
            "exceeds the model's maximum context",
            "input exceeds",
        ]
        .iter()
        .any(|s| m.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry() -> Value {
        json!({"litellm_provider":"sample", "max_input_tokens":1000000, "max_output_tokens":8000, "input_cost_per_token":0.000002, "output_cost_per_token":0.000008})
    }

    #[test]
    fn registry_guards_isolate_each_invalid_field() {
        assert!(Registry::parse(json!({"sample-model":entry()}), 10).is_ok());
        for (field, invalid) in [
            ("max_input_tokens", json!(-1)),
            ("max_input_tokens", json!(1.5)),
            ("max_output_tokens", json!("8000")),
            ("input_cost_per_token", json!(-0.1)),
            ("output_cost_per_token", json!("free")),
            ("cache_read_input_token_cost", json!(-1)),
            ("cache_creation_input_token_cost", json!(-1)),
            ("litellm_provider", json!(42)),
        ] {
            let mut e = entry();
            e[field] = invalid;
            assert!(Registry::parse(json!({"sample-model":e}), 10).is_err(), "{field}");
        }
        assert!(Registry::parse(json!([]), 10).is_err());
        assert!(Registry::parse(json!({}), 10).is_err());
        assert!(Registry::parse(json!({"sample-model":[]}), 10).is_err());
        let mut e = entry();
        e["max_input_tokens"] = json!(0);
        assert_eq!(Registry::parse(json!({"sample-model":e}), 10).unwrap().entries["sample-model"].max_input_tokens, None);
    }

    #[test]
    fn provider_mapping_and_explicit_override_are_deterministic() {
        let r = Registry::parse(
            json!({"gpt-sample":entry(), "dashscope/qwen-sample":entry(), "xai/grok-sample":entry(), "anthropic/claude-sample":entry()}),
            10,
        )
        .unwrap();
        for (p, m, key) in [
            ("codex", "gpt-sample", "gpt-sample"),
            ("opencode-go", "qwen-sample", "dashscope/qwen-sample"),
            ("grok", "grok-sample", "xai/grok-sample"),
            ("claude", "claude-sample", "anthropic/claude-sample"),
        ] {
            assert_eq!(r.lookup(p, m, None).source.as_deref(), Some(format!("LiteLLM:{key}").as_str()));
        }
        assert!(r.lookup("sample", "missing", None).values.max_input_tokens.is_none());
        let o = Override {
            litellm_key: Some("gpt-sample".into()),
            values: Metadata { max_input_tokens: Some(128000), ..Default::default() },
            ..Default::default()
        };
        let m = r.lookup("sample", "missing", Some(&o));
        assert_eq!(m.values.max_input_tokens, Some(128000));
        assert_eq!(m.values.max_output_tokens, Some(8000));
        assert!(m.source.unwrap().contains("override:sample/missing"));
        let missing = Override { litellm_key: Some("absent".into()), ..Default::default() };
        assert!(r.lookup("codex", "gpt-sample", Some(&missing)).values.max_input_tokens.is_none());
    }

    #[test]
    fn costs_split_anthropic_cache_and_preserve_unknown() {
        let prices = Metadata {
            input_cost_per_token: Some(2.0),
            output_cost_per_token: Some(8.0),
            cache_read_input_token_cost: Some(0.2),
            cache_creation_input_token_cost: Some(2.5),
            ..Default::default()
        };
        let mut u = crate::proxy::TokenUsage::default();
        u.observe(&json!({"usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"output_tokens":5}}));
        assert_eq!(u.input, Some(60));
        assert_eq!(prices.cost(u), Some(139.0));
        assert_eq!(Metadata::default().cost(u), None);
        assert_eq!(prices.cost(Default::default()), None);
    }

    #[test]
    fn estimates_include_tools_and_utf8_and_errors_are_specific() {
        let base = json!({"messages":[{"role":"user","content":"hello"}]});
        let mut tools = base.clone();
        tools["tools"] = json!([{"name":"sample","input_schema":{"description":"long instructions".repeat(100)}}]);
        assert!(estimate_tokens(&tools) > estimate_tokens(&base));
        assert!(estimate_tokens(&json!({"messages":[{"content":"界".repeat(100)}]})) >= 100);
        assert!(too_long("context_length_exceeded", ""));
        assert!(too_long("invalid_request_error", "maximum context length exceeded"));
        assert!(!too_long("rate_limit_error", "too many requests"));
    }

    #[test]
    fn metadata_override_config_parses_and_validates() {
        let cfg: crate::config::Config=toml::from_str("[catalog.metadata.\"sample/model-a\"]\nmax_input_tokens=128000\ninput_cost_per_token=0.0\ntools=true\nimages=false\nreasoning_levels=[\"low\",\"high\"]").unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.catalog.metadata["sample/model-a"].values.max_input_tokens, Some(128000));
        let mut bad = cfg.clone();
        bad.catalog.metadata.get_mut("sample/model-a").unwrap().values.max_input_tokens = Some(0);
        assert!(bad.validate().is_err());
    }
}
