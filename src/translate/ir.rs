//! The format-neutral shape every translation passes through: a request,
//! a complete response, and the stream events a response is made of.

use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Request {
    /// System / developer instructions, in order.
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    pub tool_choice: Option<ToolChoice>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stop: Vec<String>,
    pub stream: bool,
    /// Reasoning effort in OpenAI's vocabulary: `none`, `minimal`, `low`,
    /// `medium`, `high`, `xhigh`.
    pub effort: Option<String>,
    pub parallel_tool_calls: Option<bool>,
    pub response_format: Option<ResponseFormat>,
}

impl Request {
    /// Append parts to the conversation, merging with the previous message
    /// when it has the same role.
    pub fn push(&mut self, role: Role, parts: Vec<Part>) {
        if parts.is_empty() {
            return;
        }
        match self.messages.last_mut() {
            Some(last) if last.role == role => last.parts.extend(parts),
            _ => self.messages.push(Message { role, parts }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema { name: String, schema: Value, strict: Option<bool> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Image(Image),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Image {
    Url(String),
    Base64 { media_type: String, data: String },
}

impl Image {
    /// A URL, splitting `data:` URLs into media type and payload.
    pub fn from_url(url: &str) -> Image {
        if let Some(rest) = url.strip_prefix("data:")
            && let Some((meta, data)) = rest.split_once(',')
            && let Some(media_type) = meta.strip_suffix(";base64")
        {
            return Image::Base64 { media_type: media_type.to_string(), data: data.to_string() };
        }
        Image::Url(url.to_string())
    }

    pub fn to_url(&self) -> String {
        match self {
            Image::Url(u) => u.clone(),
            Image::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reasoning {
    pub text: String,
    pub opaque: Option<Opaque>,
}

/// Provider state attached to reasoning so a later turn can replay it. Only
/// the provider that issued it accepts it back.
#[derive(Debug, Clone, PartialEq)]
pub enum Opaque {
    /// An Anthropic `thinking` block signature.
    AnthropicSignature(String),
    /// An Anthropic `redacted_thinking` block's data.
    AnthropicRedacted(String),
    /// An OpenAI reasoning item's `encrypted_content`.
    OpenaiEncrypted(String),
}

/// How opaque reasoning state travels through a client format that has no
/// slot for it: as a tagged Anthropic signature or OpenAI encrypted content.
const TAG_ANTHROPIC: &str = "xolotl.anthropic.";
const TAG_ANTHROPIC_REDACTED: &str = "xolotl.anthropic-redacted.";
const TAG_OPENAI: &str = "xolotl.openai.";

impl Opaque {
    /// As an Anthropic signature: native signatures as is, others tagged.
    pub fn to_signature(&self) -> String {
        match self {
            Opaque::AnthropicSignature(s) => s.clone(),
            Opaque::AnthropicRedacted(d) => format!("{TAG_ANTHROPIC_REDACTED}{d}"),
            Opaque::OpenaiEncrypted(e) => format!("{TAG_OPENAI}{e}"),
        }
    }

    /// A signature a client sent back.
    pub fn from_signature(s: &str) -> Option<Opaque> {
        if s.is_empty() {
            None
        } else if let Some(e) = s.strip_prefix(TAG_OPENAI) {
            Some(Opaque::OpenaiEncrypted(e.to_string()))
        } else if let Some(d) = s.strip_prefix(TAG_ANTHROPIC_REDACTED) {
            Some(Opaque::AnthropicRedacted(d.to_string()))
        } else {
            Some(Opaque::AnthropicSignature(s.to_string()))
        }
    }

    /// As OpenAI encrypted content: native content as is, others tagged.
    pub fn to_encrypted(&self) -> String {
        match self {
            Opaque::OpenaiEncrypted(e) => e.clone(),
            Opaque::AnthropicSignature(s) => format!("{TAG_ANTHROPIC}{s}"),
            Opaque::AnthropicRedacted(d) => format!("{TAG_ANTHROPIC_REDACTED}{d}"),
        }
    }

    /// Encrypted content a client sent back.
    pub fn from_encrypted(e: &str) -> Option<Opaque> {
        if e.is_empty() {
            None
        } else if let Some(s) = e.strip_prefix(TAG_ANTHROPIC) {
            Some(Opaque::AnthropicSignature(s.to_string()))
        } else if let Some(d) = e.strip_prefix(TAG_ANTHROPIC_REDACTED) {
            Some(Opaque::AnthropicRedacted(d.to_string()))
        } else {
            Some(Opaque::OpenaiEncrypted(e.to_string()))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    /// Text and image parts only.
    pub content: Vec<Part>,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn images(&self) -> impl Iterator<Item = &Image> {
        self.content.iter().filter_map(|p| match p {
            Part::Image(i) => Some(i),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: Option<String>,
    /// JSON schema of the arguments.
    pub parameters: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool(String),
}

// ------------------------------------------------------------- responses

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Every input token, cached or not.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StopReason {
    #[default]
    End,
    MaxTokens,
    ToolUse,
    StopSequence,
    ContentFilter,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(String),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub blocks: Vec<Block>,
    pub stop: StopReason,
    pub usage: Usage,
}

impl Response {
    pub fn has_tool_calls(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::ToolCall(_)))
    }

    /// The response as the stream that would have produced it.
    pub fn events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        for b in &self.blocks {
            match b {
                Block::Text(t) => {
                    out.push(Event::Start(Start::Text));
                    out.push(Event::Delta(t.clone()));
                }
                Block::Reasoning(r) => {
                    out.push(Event::Start(Start::Reasoning));
                    out.push(Event::Delta(r.text.clone()));
                    if let Some(o) = &r.opaque {
                        out.push(Event::Opaque(o.clone()));
                    }
                }
                Block::ToolCall(c) => {
                    out.push(Event::Start(Start::ToolCall { id: c.id.clone(), name: c.name.clone() }));
                    out.push(Event::Delta(c.arguments.clone()));
                }
            }
            out.push(Event::Stop);
        }
        out.retain(|e| !matches!(e, Event::Delta(d) if d.is_empty()));
        out.push(Event::Finish { stop: self.stop, usage: self.usage });
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    Text,
    Reasoning,
    ToolCall { id: String, name: String },
}

/// One step of a response. Blocks never overlap: every `Start` is closed by
/// a `Stop` before the next `Start`, and `Delta` / `Opaque` belong to the open
/// block. A stream ends with exactly one `Finish` or `Error`.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Start(Start),
    /// Text, reasoning text, or a fragment of a tool call's JSON arguments.
    Delta(String),
    Opaque(Opaque),
    Stop,
    Finish {
        stop: StopReason,
        usage: Usage,
    },
    Error {
        kind: String,
        message: String,
    },
}

/// Builds well-formed events for a decoder: opening a block closes the
/// previous one, and the stream ends exactly once.
#[derive(Default)]
pub struct Emitter {
    open: bool,
    finished: bool,
    out: Vec<Event>,
}

impl Emitter {
    pub fn start(&mut self, s: Start) {
        if self.finished {
            return;
        }
        self.stop();
        self.out.push(Event::Start(s));
        self.open = true;
    }

    pub fn delta(&mut self, d: &str) {
        if self.open && !d.is_empty() {
            self.out.push(Event::Delta(d.to_string()));
        }
    }

    pub fn opaque(&mut self, o: Opaque) {
        if self.open {
            self.out.push(Event::Opaque(o));
        }
    }

    pub fn stop(&mut self) {
        if self.open {
            self.out.push(Event::Stop);
            self.open = false;
        }
    }

    pub fn finish(&mut self, stop: StopReason, usage: Usage) {
        if !self.finished {
            self.stop();
            self.out.push(Event::Finish { stop, usage });
            self.finished = true;
        }
    }

    pub fn error(&mut self, kind: &str, message: &str) {
        if !self.finished {
            self.stop();
            self.out.push(Event::Error { kind: kind.to_string(), message: message.to_string() });
            self.finished = true;
        }
    }

    pub fn finished(&self) -> bool {
        self.finished
    }

    /// The upstream stream ended: a response that never completed is an error.
    pub fn end(&mut self) {
        self.error("api_error", "upstream stream ended before the response completed");
    }

    pub fn take(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.out)
    }
}

/// Assembles a complete response from events.
#[derive(Default)]
pub struct Collector {
    resp: Response,
    error: Option<(String, String)>,
}

impl Collector {
    pub fn push(&mut self, e: Event) {
        match e {
            Event::Start(Start::Text) => self.resp.blocks.push(Block::Text(String::new())),
            Event::Start(Start::Reasoning) => self.resp.blocks.push(Block::Reasoning(Reasoning { text: String::new(), opaque: None })),
            Event::Start(Start::ToolCall { id, name }) => {
                self.resp.blocks.push(Block::ToolCall(ToolCall { id, name, arguments: String::new() }))
            }
            Event::Delta(d) => match self.resp.blocks.last_mut() {
                Some(Block::Text(t)) => t.push_str(&d),
                Some(Block::Reasoning(r)) => r.text.push_str(&d),
                Some(Block::ToolCall(c)) => c.arguments.push_str(&d),
                None => {}
            },
            Event::Opaque(o) => {
                if let Some(Block::Reasoning(r)) = self.resp.blocks.last_mut() {
                    r.opaque = Some(o);
                }
            }
            Event::Stop => {}
            Event::Finish { stop, usage } => {
                self.resp.stop = stop;
                self.resp.usage = usage;
            }
            Event::Error { kind, message } => self.error = Some((kind, message)),
        }
    }

    /// The response, or the error the stream ended with.
    pub fn finish(self) -> Result<Response, (String, String)> {
        match self.error {
            Some(e) => Err(e),
            None => Ok(self.resp),
        }
    }
}

/// Parse JSON-encoded tool arguments into an object; anything else becomes
/// an empty object.
pub fn arguments_object(args: &str) -> Value {
    match serde_json::from_str::<Value>(args) {
        Ok(v @ Value::Object(_)) => v,
        _ => Value::Object(Default::default()),
    }
}

/// Effort level for an Anthropic thinking budget (CLIProxyAPI's thresholds).
pub fn effort_for_budget(budget: u64) -> &'static str {
    match budget {
        0 => "none",
        1..=1024 => "low",
        1025..=8192 => "medium",
        8193..=24576 => "high",
        _ => "xhigh",
    }
}

/// Anthropic thinking budget for an effort level.
pub fn budget_for_effort(effort: &str) -> Option<u64> {
    match effort {
        "minimal" | "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" | "max" => Some(32768),
        _ => None,
    }
}
