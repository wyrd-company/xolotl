//! Account kinds: every upstream credential xolotl holds is an account of one
//! kind. A kind says where its accounts serve requests and how an operator
//! adds one. The admin API (`GET /admin/account-kinds`) and the web UI's
//! **Add account** screen read this registry.
//!
//! - **Pool accounts** serve a fixed provider name (`codex`, `claude`, `grok`,
//!   `opencode-go`) together with the other accounts of their kind
//!   ([`crate::subscription`]).
//! - **Provider accounts** are named by the operator. The account name is the
//!   provider name that clients address (`<name>/<model>`, `/<name>/...`), and
//!   each provider has one account ([`crate::gateway`]).
//!
//! To add a kind: add a variant to [`AccountKind`] and its row in
//! [`AccountKind::info`], then serve it. A pool kind needs a
//! [`crate::subscription::SubProvider`]; a provider kind needs a
//! [`crate::config::ProviderKind`]. A new way to add accounts is a new
//! [`AddMethod`], with its admin route and its web UI form.

use serde::{Deserialize, Serialize};

use crate::config::ProviderKind;
use crate::subscription::SubProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccountKind {
    Codex,
    Claude,
    Grok,
    OpencodeGo,
    Vercel,
    Mistral,
    Anthropic,
    Openai,
    OpenaiCompatible,
}

/// Where an account serves requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// One of many accounts in the pool of a fixed provider name.
    Pool,
    /// The account is a provider of its own, under the name the operator gives it.
    Provider,
}

/// How an operator adds an account of a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AddMethod {
    /// OAuth device code: `POST /admin/auth/<kind>/start` with `device: true`.
    DeviceLogin,
    /// OAuth in a browser; the operator pastes back a code or URL:
    /// `POST /admin/auth/<kind>/start`, then `/admin/auth/flows/<id>/complete`.
    BrowserLogin,
    /// A login file from the provider's own CLI: `POST /admin/accounts/import`.
    Import,
    /// An API key: `POST /admin/accounts` with `kind` and `api_key`.
    ApiKey,
    /// A named provider: `POST /admin/accounts` with `kind`, `name` and the
    /// provider fields.
    Provider,
}

/// What the admin API and the web UI show for a kind.
#[derive(Debug, Clone, Serialize)]
pub struct KindInfo {
    pub kind: AccountKind,
    pub label: &'static str,
    pub description: &'static str,
    pub placement: Placement,
    /// The provider name of a pool kind. Provider kinds take the account name.
    pub provider: Option<&'static str>,
    /// In the order the web UI offers them.
    pub add: &'static [AddMethod],
    /// Provider kinds: the base URL used when none is given. `None` means the
    /// account must give one.
    pub default_base_url: Option<&'static str>,
}

impl AccountKind {
    /// In the order the web UI lists them.
    pub const ALL: [AccountKind; 9] = [
        AccountKind::Codex,
        AccountKind::Claude,
        AccountKind::Grok,
        AccountKind::OpencodeGo,
        AccountKind::Anthropic,
        AccountKind::Openai,
        AccountKind::Vercel,
        AccountKind::Mistral,
        AccountKind::OpenaiCompatible,
    ];

    pub fn info(self) -> KindInfo {
        use AccountKind::*;
        use AddMethod::*;
        let pool = |provider: SubProvider, label, description, add| KindInfo {
            kind: self,
            label,
            description,
            placement: Placement::Pool,
            provider: Some(provider.as_str()),
            add,
            default_base_url: None,
        };
        let provider = |label, description, default_base_url| KindInfo {
            kind: self,
            label,
            description,
            placement: Placement::Provider,
            provider: None,
            add: &[Provider],
            default_base_url,
        };
        match self {
            Codex => {
                pool(SubProvider::Codex, "Codex (ChatGPT)", "ChatGPT subscription, used by Codex.", &[DeviceLogin, BrowserLogin, Import])
            }
            Claude => pool(SubProvider::Claude, "Claude (Pro / Max)", "Claude subscription, used by Claude Code.", &[BrowserLogin, Import]),
            Grok => pool(SubProvider::Grok, "Grok Build (xAI)", "Grok Build subscription.", &[DeviceLogin]),
            OpencodeGo => pool(SubProvider::OpencodeGo, "OpenCode Go", "API key from the OpenCode console.", &[ApiKey]),
            Anthropic => provider(
                "Anthropic API",
                "Claude Console API key: personal or service account. For a key not scoped to one workspace, add the anthropic-workspace-id header.",
                Some(ANTHROPIC_BASE_URL),
            ),
            Openai => provider(
                "OpenAI API",
                "OpenAI Platform API key: project or service account. Add OpenAI-Project or OpenAI-Organization headers to pick one.",
                Some(OPENAI_BASE_URL),
            ),
            Vercel => provider("Vercel AI Gateway", "Vercel AI Gateway API key.", Some(VERCEL_BASE_URL)),
            Mistral => provider("Mistral", "Mistral API key.", Some(MISTRAL_BASE_URL)),
            OpenaiCompatible => provider("OpenAI-compatible", "Any OpenAI-compatible server, such as llama.cpp.", None),
        }
    }

    /// The pool a pool kind's accounts join.
    pub fn sub_provider(self) -> Option<SubProvider> {
        match self {
            AccountKind::Codex => Some(SubProvider::Codex),
            AccountKind::Claude => Some(SubProvider::Claude),
            AccountKind::Grok => Some(SubProvider::Grok),
            AccountKind::OpencodeGo => Some(SubProvider::OpencodeGo),
            _ => None,
        }
    }

    /// The gateway kind that serves a provider kind's account.
    pub fn provider_kind(self) -> Option<ProviderKind> {
        match self {
            AccountKind::Anthropic => Some(ProviderKind::Anthropic),
            AccountKind::Openai => Some(ProviderKind::Openai),
            AccountKind::Vercel => Some(ProviderKind::Vercel),
            AccountKind::Mistral => Some(ProviderKind::Mistral),
            AccountKind::OpenaiCompatible => Some(ProviderKind::OpenaiCompatible),
            _ => None,
        }
    }

    /// The kind's name, as the admin API and the config file spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::Codex => "codex",
            AccountKind::Claude => "claude",
            AccountKind::Grok => "grok",
            AccountKind::OpencodeGo => "opencode-go",
            AccountKind::Anthropic => "anthropic",
            AccountKind::Openai => "openai",
            AccountKind::Vercel => "vercel",
            AccountKind::Mistral => "mistral",
            AccountKind::OpenaiCompatible => "openai-compatible",
        }
    }

    pub fn can_add(self, method: AddMethod) -> bool {
        self.info().add.contains(&method)
    }
}

impl From<ProviderKind> for AccountKind {
    fn from(k: ProviderKind) -> Self {
        match k {
            ProviderKind::Anthropic => AccountKind::Anthropic,
            ProviderKind::Openai => AccountKind::Openai,
            ProviderKind::Vercel => AccountKind::Vercel,
            ProviderKind::Mistral => AccountKind::Mistral,
            ProviderKind::OpenaiCompatible => AccountKind::OpenaiCompatible,
        }
    }
}

impl From<SubProvider> for AccountKind {
    fn from(p: SubProvider) -> Self {
        match p {
            SubProvider::Codex => AccountKind::Codex,
            SubProvider::Claude => AccountKind::Claude,
            SubProvider::Grok => AccountKind::Grok,
            SubProvider::OpencodeGo => AccountKind::OpencodeGo,
        }
    }
}

pub const VERCEL_BASE_URL: &str = "https://ai-gateway.vercel.sh";
pub const MISTRAL_BASE_URL: &str = "https://api.mistral.ai";
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com";

pub fn kinds() -> Vec<KindInfo> {
    AccountKind::ALL.iter().map(|k| k.info()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_one_placement() {
        for k in AccountKind::ALL {
            let info = k.info();
            assert!(!info.add.is_empty(), "{k:?} can be added");
            match info.placement {
                Placement::Pool => {
                    assert_eq!(k.sub_provider().map(SubProvider::as_str), info.provider);
                    assert!(k.provider_kind().is_none());
                }
                Placement::Provider => {
                    assert!(k.sub_provider().is_none() && info.provider.is_none());
                    assert_eq!(k.provider_kind().map(AccountKind::from), Some(k));
                    assert_eq!(info.add, &[AddMethod::Provider]);
                }
            }
        }
    }

    #[test]
    fn kinds_use_the_config_names() {
        for k in AccountKind::ALL {
            let name = serde_json::to_value(k).unwrap();
            assert_eq!(name, k.as_str());
            if let Some(p) = k.sub_provider() {
                assert_eq!(name, p.as_str());
            }
            if let Some(p) = k.provider_kind() {
                assert_eq!(name, serde_json::to_value(p).unwrap());
            }
        }
    }
}
