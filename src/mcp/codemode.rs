//! Code mode: run a client's JavaScript in a QuickJS sandbox where every
//! allowed MCP server is a global object whose methods call its tools, and
//! describe those objects as TypeScript declarations for `search`.
//!
//! The sandbox has no file system, network, timers or modules; the only way
//! out is a tool call, which goes through [`ToolHost`] (and so through the
//! caller's policy). Each execution gets a fresh runtime with a heap limit,
//! a wall-clock deadline and a cap on tool calls.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rquickjs::function::Async;
use rquickjs::{AsyncContext, AsyncRuntime, CaughtError, Ctx, Function, Promise};
use serde::Serialize;
use serde_json::{Value, json};

/// Where tool calls from the sandbox go.
#[async_trait::async_trait]
pub trait ToolHost: Send + Sync {
    /// Call `tool` (the property name the code used) on `server`. `Ok` is
    /// the value the JavaScript call resolves to; `Err` is thrown as an Error.
    async fn call(&self, server: String, tool: String, arguments: Value) -> Result<Value, String>;
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub max_calls: usize,
    /// Cap on the combined size of logged lines.
    pub max_log_bytes: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CallSummary {
    /// `server/tool`.
    pub tool: String,
    pub ok: bool,
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct Outcome {
    /// The value the code returned, or why it failed.
    pub result: Result<Value, String>,
    pub logs: Vec<String>,
    pub calls: Vec<CallSummary>,
}

const PRELUDE: &str = r#"(() => {
  const call = globalThis.__xolotl_call;
  const log = globalThis.__xolotl_log;
  delete globalThis.__xolotl_call;
  delete globalThis.__xolotl_log;
  const fmt = (v) => {
    if (typeof v === "string") return v;
    if (v instanceof Error) return String(v);
    try { const s = JSON.stringify(v); return s === undefined ? String(v) : s; } catch (_) { return String(v); }
  };
  const out = (level) => (...args) => log(level, args.map(fmt).join(" "));
  globalThis.console = Object.freeze({ log: out("log"), info: out("info"), warn: out("warn"), error: out("error"), debug: out("debug") });
  const namespace = (server) => new Proxy(Object.freeze({}), {
    get(_, tool) {
      if (typeof tool !== "string" || tool === "then" || tool === "toJSON") return undefined;
      return async (args) => {
        const reply = JSON.parse(await call(server, tool, JSON.stringify(args === undefined ? {} : args)));
        if ("error" in reply) throw new Error(reply.error);
        return reply.value;
      };
    },
  });
  for (const [ident, server] of __NAMESPACES__) {
    Object.defineProperty(globalThis, ident, { value: namespace(server), enumerable: true });
  }
})();
"#;

/// The program to evaluate: the prelude, then the client's code as the body
/// of an async function (or, if it is one, an async function expression),
/// resolving to its JSON-encoded return value.
fn program(code: &str, namespaces: &[(String, String)]) -> String {
    let pairs: Vec<[&str; 2]> = namespaces.iter().map(|(i, s)| [i.as_str(), s.as_str()]).collect();
    let prelude = PRELUDE.replace("__NAMESPACES__", &serde_json::to_string(&pairs).expect("strings serialize"));
    let code = code.trim();
    let is_function = ["async function", "async (", "async()"].iter().any(|p| code.starts_with(p));
    let call = if is_function { format!("({code})()") } else { format!("(async () => {{\n{code}\n}})()") };
    format!("{prelude}\n{call}.then((v) => JSON.stringify(v === undefined ? null : v))")
}

/// Run `code` with one global per `(identifier, server)` pair.
pub async fn execute(code: &str, namespaces: Vec<(String, String)>, host: Arc<dyn ToolHost>, limits: Limits) -> Outcome {
    let source = program(code, &namespaces);
    let handle = tokio::runtime::Handle::current();
    let logs = Arc::new(Mutex::new(Logs { lines: Vec::new(), bytes: 0, max: limits.max_log_bytes }));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (l, c) = (logs.clone(), calls.clone());
    // QuickJS runtimes aren't Send: run on a blocking thread with its own
    // single-threaded runtime. Tool calls are spawned back onto the main one,
    // where the upstream connections live.
    let joined = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().map_err(|e| e.to_string())?;
        rt.block_on(run(source, handle, host, limits, l, c))
    })
    .await;
    let result = match joined {
        Ok(r) => r,
        Err(e) => Err(format!("execution failed: {e}")),
    };
    let logs = std::mem::take(&mut logs.lock().lines);
    let calls = std::mem::take(&mut *calls.lock());
    Outcome { result, logs, calls }
}

struct Logs {
    lines: Vec<String>,
    bytes: usize,
    max: usize,
}

impl Logs {
    fn push(&mut self, line: String) {
        if self.bytes > self.max {
            return;
        }
        self.bytes += line.len();
        if self.bytes > self.max {
            self.lines.push("[further logs dropped: output limit reached]".into());
        } else {
            self.lines.push(line);
        }
    }
}

async fn run(
    source: String,
    handle: tokio::runtime::Handle,
    host: Arc<dyn ToolHost>,
    limits: Limits,
    logs: Arc<Mutex<Logs>>,
    calls: Arc<Mutex<Vec<CallSummary>>>,
) -> Result<Value, String> {
    let deadline = Instant::now() + limits.timeout;
    let timeout_msg = format!("execution timed out after {}s", limits.timeout.as_secs());
    let runtime = AsyncRuntime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(limits.memory_bytes).await;
    runtime.set_max_stack_size(512 * 1024).await;
    // Stops JavaScript that keeps running past the deadline (busy loops).
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline))).await;
    let context = AsyncContext::full(&runtime).await.map_err(|e| e.to_string())?;

    let max_calls = limits.max_calls;
    let msg = timeout_msg.clone();
    let evaluated = context.async_with(async move |ctx| {
        let call = Function::new(
            ctx.clone(),
            Async(move |server: String, tool: String, args: String| {
                let (host, handle, calls, msg) = (host.clone(), handle.clone(), calls.clone(), msg.clone());
                async move {
                    let reply = dispatch(&calls, max_calls, deadline, &msg, server, tool, args, host, handle).await;
                    reply.to_string()
                }
            }),
        )
        .map_err(|e| e.to_string())?;
        let log = Function::new(ctx.clone(), move |level: String, line: String| {
            let line = if level == "log" { line } else { format!("[{level}] {line}") };
            logs.lock().push(line);
        })
        .map_err(|e| e.to_string())?;
        ctx.globals().set("__xolotl_call", call).map_err(|e| e.to_string())?;
        ctx.globals().set("__xolotl_log", log).map_err(|e| e.to_string())?;
        let promise: Promise = ctx.eval(source).map_err(|e| js_error(&ctx, e))?;
        let json: String = promise.into_future().await.map_err(|e| js_error(&ctx, e))?;
        serde_json::from_str::<Value>(&json).map_err(|e| e.to_string())
    });
    // The deadline normally ends execution from inside (tool calls fail, the
    // interrupt handler stops busy code); this catches code that awaits
    // something that never settles.
    let grace = deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
    let result = match tokio::time::timeout(grace, evaluated).await {
        Ok(r) => r,
        Err(_) => Err(timeout_msg.clone()),
    };
    if Instant::now() >= deadline && result.as_ref().is_err_and(|e| e.contains("interrupted")) {
        return Err(timeout_msg);
    }
    // Out of memory, QuickJS may fail to allocate the error object and throw
    // `null` instead; by now the memory is freed, so the two can't be told apart.
    if result.as_ref().is_err_and(|e| e == "Uncaught: null") {
        return Err(format!("Uncaught: null (the code threw null, or ran out of memory: the limit is {} MiB)", limits.memory_bytes >> 20));
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn dispatch(
    calls: &Mutex<Vec<CallSummary>>,
    max_calls: usize,
    deadline: Instant,
    timeout_msg: &str,
    server: String,
    tool: String,
    args: String,
    host: Arc<dyn ToolHost>,
    handle: tokio::runtime::Handle,
) -> Value {
    if calls.lock().len() >= max_calls {
        return json!({"error": format!("tool call limit reached ({max_calls} per execution)")});
    }
    let arguments = match serde_json::from_str::<Value>(&args) {
        Ok(v @ Value::Object(_)) => v,
        _ => return json!({"error": "tool arguments must be an object"}),
    };
    let label = format!("{server}/{tool}");
    let started = Instant::now();
    let remaining = deadline.saturating_duration_since(started);
    let result = match tokio::time::timeout(remaining, handle.spawn(async move { host.call(server, tool, arguments).await })).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("tool call failed: {e}")),
        Err(_) => Err(timeout_msg.to_string()),
    };
    let ms = started.elapsed().as_millis() as u64;
    let summary = CallSummary { tool: label, ok: result.is_ok(), ms, error: result.as_ref().err().cloned() };
    calls.lock().push(summary);
    match result {
        Ok(value) => json!({"value": value}),
        Err(error) => json!({"error": error}),
    }
}

fn js_error(ctx: &Ctx<'_>, error: rquickjs::Error) -> String {
    match CaughtError::from_error(ctx, error) {
        CaughtError::Exception(e) => {
            let name = e.as_object().get::<_, String>("name").unwrap_or_else(|_| "Error".into());
            let mut out = format!("{name}: {}", e.message().unwrap_or_default());
            if let Some(stack) = e.stack().filter(|s| !s.trim().is_empty()) {
                out.push('\n');
                out.push_str(stack.trim_end());
            }
            out
        }
        CaughtError::Value(v) => {
            let text = ctx.json_stringify(v).ok().flatten().and_then(|s| s.to_string().ok());
            format!("Uncaught: {}", text.unwrap_or_else(|| "a non-JSON value".into()))
        }
        CaughtError::Error(e) => e.to_string(),
    }
}

// ------------------------------------------------------------ declarations

/// One server's allowed tools, for [`declarations`].
pub struct Group<'a> {
    pub namespace: &'a str,
    pub server: &'a str,
    pub description: &'a str,
    pub tools: Vec<&'a rmcp::model::Tool>,
}

/// TypeScript declarations of the given servers and tools.
pub fn declarations(groups: &[Group<'_>]) -> String {
    let mut out = String::new();
    for g in groups {
        let title = if g.description.is_empty() { g.server.to_string() } else { format!("{}: {}", g.server, g.description) };
        out.push_str(&format!("/** {} */\ndeclare const {}: {{\n", comment(&title), g.namespace));
        for t in &g.tools {
            let mut doc = t.description.as_deref().unwrap_or("").trim().to_string();
            if let Some(title) = t.title.as_deref().or(t.annotations.as_ref().and_then(|a| a.title.as_deref()))
                && doc.is_empty()
            {
                doc = title.to_string();
            }
            if let Some(a) = &t.annotations {
                let mut hints = Vec::new();
                if a.read_only_hint == Some(true) {
                    hints.push("read-only");
                }
                if a.destructive_hint == Some(true) {
                    hints.push("destructive");
                }
                if !hints.is_empty() {
                    doc = format!("{doc}\n@remarks {}", hints.join(", "));
                }
            }
            if !doc.is_empty() {
                out.push_str(&doc_comment(&doc, 2));
            }
            let input = Value::Object((*t.input_schema).clone());
            let output = t.output_schema.as_ref().map(|s| ts_type(&Value::Object((**s).clone()), 0, 2)).unwrap_or_else(|| "unknown".into());
            out.push_str(&format!("  {}(args: {}): Promise<{}>;\n", key(&t.name), ts_type(&input, 0, 2), output));
        }
        out.push_str("};\n\n");
    }
    out.trim_end().to_string()
}

fn comment(s: &str) -> String {
    s.replace("*/", "* /")
}

fn doc_comment(text: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let text = comment(&crate::mcp::truncate(text, 1500));
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() == 1 {
        return format!("{pad}/** {} */\n", lines[0]);
    }
    let mut out = format!("{pad}/**\n");
    for l in lines {
        out.push_str(&format!("{pad} * {}\n", l.trim_end()).replace(" * \n", " *\n"));
    }
    out.push_str(&format!("{pad} */\n"));
    out
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A property name: bare when it's an identifier, quoted otherwise.
fn key(name: &str) -> String {
    if is_identifier(name) { name.to_string() } else { serde_json::to_string(name).expect("strings serialize") }
}

const MAX_DEPTH: usize = 6;

/// A TypeScript type for a JSON Schema. Unsupported constructs become `unknown`.
fn ts_type(schema: &Value, depth: usize, indent: usize) -> String {
    let Some(s) = schema.as_object() else {
        return if schema == &Value::Bool(false) { "never".into() } else { "unknown".into() };
    };
    if depth > MAX_DEPTH {
        return "unknown".into();
    }
    if let Some(c) = s.get("const") {
        return c.to_string();
    }
    if let Some(Value::Array(values)) = s.get("enum") {
        return union(values.iter().map(Value::to_string).collect());
    }
    for k in ["anyOf", "oneOf"] {
        if let Some(Value::Array(options)) = s.get(k) {
            return union(options.iter().map(|o| ts_type(o, depth + 1, indent)).collect());
        }
    }
    if let Some(Value::Array(parts)) = s.get("allOf") {
        let parts: Vec<String> = parts.iter().map(|o| ts_type(o, depth + 1, indent)).filter(|t| t != "unknown").collect();
        return if parts.is_empty() { "unknown".into() } else { parts.join(" & ") };
    }
    match s.get("type") {
        Some(Value::String(t)) => single_type(t, s, depth, indent),
        Some(Value::Array(types)) => union(types.iter().filter_map(Value::as_str).map(|t| single_type(t, s, depth, indent)).collect()),
        _ if s.contains_key("properties") => object_type(s, depth, indent),
        _ => "unknown".into(),
    }
}

fn single_type(t: &str, s: &serde_json::Map<String, Value>, depth: usize, indent: usize) -> String {
    match t {
        "string" => "string".into(),
        "number" | "integer" => "number".into(),
        "boolean" => "boolean".into(),
        "null" => "null".into(),
        "array" => {
            let item = s.get("items").map(|i| ts_type(i, depth + 1, indent)).unwrap_or_else(|| "unknown".into());
            if item.contains(['|', '&']) { format!("({item})[]") } else { format!("{item}[]") }
        }
        "object" => object_type(s, depth, indent),
        _ => "unknown".into(),
    }
}

fn object_type(s: &serde_json::Map<String, Value>, depth: usize, indent: usize) -> String {
    let props = s.get("properties").and_then(Value::as_object);
    let required: Vec<&str> =
        s.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = props.filter(|p| !p.is_empty()) else {
        return match s.get("additionalProperties") {
            Some(Value::Bool(false)) => "{}".into(),
            Some(v @ Value::Object(_)) => format!("Record<string, {}>", ts_type(v, depth + 1, indent)),
            _ => "Record<string, unknown>".into(),
        };
    };
    let pad = " ".repeat(indent + 2);
    let mut out = String::from("{\n");
    for (name, p) in props {
        if let Some(d) = p.get("description").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()) {
            out.push_str(&doc_comment(d, indent + 2));
        }
        let opt = if required.contains(&name.as_str()) { "" } else { "?" };
        out.push_str(&format!("{pad}{}{opt}: {};\n", key(name), ts_type(p, depth + 1, indent + 2)));
    }
    out.push_str(&" ".repeat(indent));
    out.push('}');
    out
}

fn union(mut parts: Vec<String>) -> String {
    parts.dedup();
    match parts.len() {
        0 => "unknown".into(),
        1 => parts.remove(0),
        _ => parts.join(" | "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl ToolHost for Echo {
        async fn call(&self, server: String, tool: String, arguments: Value) -> Result<Value, String> {
            match tool.as_str() {
                "fail" => Err("boom".into()),
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(Value::Null)
                }
                _ => Ok(json!({"server": server, "tool": tool, "args": arguments})),
            }
        }
    }

    fn limits() -> Limits {
        Limits { timeout: Duration::from_secs(5), memory_bytes: 32 << 20, max_calls: 3, max_log_bytes: 200 }
    }

    async fn run(code: &str) -> Outcome {
        execute(code, vec![("gh_work".into(), "gh-work".into())], Arc::new(Echo), limits()).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn calls_tools_and_returns_json() {
        let o = run("const r = await gh_work.create_issue({title: 'x'}); console.log('made', r.args); return {r, n: 1};").await;
        let v = o.result.unwrap();
        assert_eq!(v["r"], json!({"server": "gh-work", "tool": "create_issue", "args": {"title": "x"}}));
        assert_eq!(v["n"], 1);
        assert_eq!(o.logs, vec![r#"made {"title":"x"}"#]);
        assert_eq!(o.calls.len(), 1);
        assert!(o.calls[0].ok && o.calls[0].tool == "gh-work/create_issue");

        let o = run("async () => (await gh_work['get-item']()).tool").await;
        assert_eq!(o.result.unwrap(), json!("get-item"));
        assert_eq!(run("1 + 1").await.result.unwrap(), Value::Null, "a body without return resolves to null");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn errors_and_limits() {
        let o = run("try { await gh_work.fail({}) } catch (e) { return e.message }").await;
        assert_eq!(o.result.unwrap(), json!("boom"));
        assert!(!o.calls[0].ok);

        let e = run("throw new TypeError('bad')").await.result.unwrap_err();
        assert!(e.starts_with("TypeError: bad"), "{e}");
        assert!(run("return (").await.result.unwrap_err().contains("SyntaxError"));
        assert!(run("await gh_work.x(5)").await.result.unwrap_err().contains("must be an object"));

        let o = run("for (let i = 0; i < 5; i++) { try { await gh_work.t({}) } catch (e) { return [i, e.message] } }").await;
        assert_eq!(o.result.unwrap(), json!([3, "tool call limit reached (3 per execution)"]));

        let o = run("for (let i = 0; i < 100; i++) console.log('line ' + i)").await;
        assert!(o.logs.len() < 40 && o.logs.last().unwrap().contains("dropped"), "{:?}", o.logs);

        for code in ["typeof fetch", "typeof require", "typeof setTimeout", "typeof __xolotl_call"] {
            assert_eq!(run(&format!("return {code}")).await.result.unwrap(), json!("undefined"), "{code}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeouts_and_memory() {
        let quick = Limits { timeout: Duration::from_millis(300), ..limits() };
        let host: Arc<dyn ToolHost> = Arc::new(Echo);
        let ns = || vec![("gh_work".to_string(), "gh-work".to_string())];
        for code in ["while (true) {}", "await gh_work.slow({})", "await new Promise(() => {})"] {
            let started = Instant::now();
            let o = execute(code, ns(), host.clone(), quick.clone()).await;
            assert!(o.result.unwrap_err().contains("timed out"), "{code}");
            assert!(started.elapsed() < Duration::from_secs(3), "{code}");
        }
        let o = execute("const a = []; while (true) a.push('x'.repeat(1000));", ns(), host, limits()).await;
        let e = o.result.unwrap_err();
        assert!(e.to_lowercase().contains("memory"), "{e}");
    }

    #[test]
    fn typescript_declarations() {
        let tool: rmcp::model::Tool = serde_json::from_value(json!({
            "name": "create_issue",
            "description": "Create an issue",
            "annotations": {"destructiveHint": false, "readOnlyHint": false},
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "owner/name"},
                    "labels": {"type": "array", "items": {"enum": ["bug", "docs"]}},
                    "priority": {"type": ["integer", "null"]},
                    "meta": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["repo"]
            },
            "outputSchema": {"type": "object", "properties": {"number": {"type": "integer"}}, "required": ["number"]}
        }))
        .unwrap();
        let odd: rmcp::model::Tool = serde_json::from_value(json!({"name": "get-item", "inputSchema": {"type": "object"}})).unwrap();
        let ts = declarations(&[Group { namespace: "gh_work", server: "gh-work", description: "GitHub (work)", tools: vec![&tool, &odd] }]);
        let expected = r#"/** gh-work: GitHub (work) */
declare const gh_work: {
  /** Create an issue */
  create_issue(args: {
    labels?: ("bug" | "docs")[];
    meta?: Record<string, string>;
    priority?: number | null;
    /** owner/name */
    repo: string;
  }): Promise<{
    number: number;
  }>;
  "get-item"(args: Record<string, unknown>): Promise<unknown>;
};"#;
        assert_eq!(ts, expected);
    }
}
