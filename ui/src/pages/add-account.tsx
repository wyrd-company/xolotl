import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router";
import { toast } from "sonner";

import { PageHeader, QueryError } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import {
  get,
  post,
  type Account,
  type AccountKind,
  type AccountKindInfo,
  type FlowStart,
  type FlowStatus,
  type ProviderAccount,
} from "@/lib/api";
import { useAction } from "@/lib/hooks";
import { PROVIDER_QUERIES } from "@/pages/provider-accounts";

/** The forms of the kinds that log in with OAuth. Each takes its own flow. */
const LOGIN_FORMS: Partial<Record<AccountKind, () => ReactNode>> = {
  codex: () => <CodexLogin />,
  claude: () => <ClaudeLogin />,
  grok: () => <GrokLogin />,
};

/** Kinds are listed by the server. A pool kind gets its login or key form; provider kinds share one form. */
export function AddAccountPage() {
  const kinds = useQuery({ queryKey: ["account-kinds"], queryFn: () => get<AccountKindInfo[]>("/admin/account-kinds"), staleTime: Infinity });
  const all = kinds.data ?? [];
  const pools = all.filter((k) => k.placement === "pool");
  const providers = all.filter((k) => k.placement === "provider");
  const imports = pools.filter((k) => k.add.includes("import"));
  return (
    <>
      <Link to="/accounts" className="text-primary text-sm">
        Back to accounts
      </Link>
      <PageHeader
        title="Add an account"
        description="Credentials go straight to OpenBao. Logging in to an account that already exists replaces its tokens instead of adding a duplicate."
      />
      <QueryError error={kinds.error} />
      <section aria-labelledby="add-pools" className="flex flex-col gap-3">
        <h2 id="add-pools" className="font-medium">
          Subscriptions
        </h2>
        <p className="text-muted-foreground text-sm">Accounts of a pool share its provider name. Add several to spread load and limits.</p>
        <div className="grid gap-4 lg:grid-cols-2">
          {pools.map((k) => (
            <PoolForm key={k.kind} info={k} />
          ))}
          {imports.length > 0 && <ImportLogin kinds={imports} />}
        </div>
      </section>
      {providers.length > 0 && (
        <section aria-labelledby="add-providers" className="flex flex-col gap-3">
          <h2 id="add-providers" className="font-medium">
            Providers
          </h2>
          <p className="text-muted-foreground text-sm">
            A provider account is a provider of its own. Clients address it by the name you give it.
          </p>
          <div className="grid gap-4 lg:grid-cols-2">
            <ProviderAccountForm kinds={providers} />
          </div>
        </section>
      )}
    </>
  );
}

function PoolForm({ info }: { info: AccountKindInfo }) {
  const login = LOGIN_FORMS[info.kind];
  if (login) return <>{login()}</>;
  if (info.add.includes("api-key")) return <ApiKeyAccount info={info} />;
  return null;
}

export function AccountLogin({ provider }: { provider: "codex" | "claude" | "grok" }) {
  return (
    <div>
      <h3 className="mb-2 font-medium">Log in again</h3>
      {provider === "codex" ? <CodexLogin /> : provider === "claude" ? <ClaudeLogin /> : <GrokLogin />}
    </div>
  );
}

function Section({ title, description, children }: { title: string; description: ReactNode; children: ReactNode }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <CardDescription className="leading-relaxed">{description}</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-col gap-4">{children}</CardContent>
    </Card>
  );
}

function useLoggedIn() {
  const qc = useQueryClient();
  return (email?: string | null) => {
    toast.success(email ? `Logged in ${email}` : "Account added");
    qc.invalidateQueries({ queryKey: ["accounts"] });
    qc.invalidateQueries({ queryKey: ["status"] });
    qc.invalidateQueries({ queryKey: ["catalog"] });
    qc.invalidateQueries({ queryKey: ["catalog-routes"] });
    qc.invalidateQueries({ queryKey: ["catalog-status"] });
    qc.invalidateQueries({ queryKey: ["policy-view"] });
  };
}

function ExternalLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noreferrer noopener" className="text-primary break-all">
      {children}
    </a>
  );
}

/** Starts a login flow and remembers it until it completes or is reset. */
function useFlow(provider: "codex" | "claude" | "grok") {
  const [flow, setFlow] = useState<FlowStart | null>(null);
  const start = useAction((device: boolean) => post<FlowStart>(`/admin/auth/${provider}/start`, { device }), {
    onSuccess: (f) => setFlow(f),
  });
  return { flow, setFlow, start };
}

function PasteCode({ flow, onDone, placeholder }: { flow: FlowStart; onDone: () => void; placeholder: string }) {
  const [code, setCode] = useState("");
  const loggedIn = useLoggedIn();
  const complete = useAction((c: string) => post<Account>(`/admin/auth/flows/${flow.flow_id}/complete`, { code: c }), {
    onSuccess: (a) => {
      loggedIn(a.email);
      onDone();
    },
  });
  const id = `code-${flow.flow_id}`;
  return (
    <form
      className="flex flex-col gap-3"
      onSubmit={(e: FormEvent) => {
        e.preventDefault();
        complete.mutate(code.trim());
      }}
    >
      <div className="flex flex-col gap-2">
        <Label htmlFor={id}>Authorization code</Label>
        <Input id={id} className="font-mono" placeholder={placeholder} value={code} onChange={(e) => setCode(e.target.value)} />
      </div>
      <div className="flex gap-2">
        <Button type="submit" disabled={!code.trim() || complete.isPending}>
          Finish login
        </Button>
        <Button type="button" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** A device-code flow: shows the code and polls until the server finishes the login. */
function DeviceCode({ flow, name, onDone }: { flow: FlowStart; name: string; onDone: () => void }) {
  const loggedIn = useLoggedIn();
  const status = useQuery({
    queryKey: ["flow", flow.flow_id],
    queryFn: () => get<FlowStatus>(`/admin/auth/flows/${flow.flow_id}`),
    refetchInterval: (q) => (q.state.data?.status === "pending" || !q.state.data ? 3000 : false),
  });

  useEffect(() => {
    const s = status.data;
    if (s?.status === "complete") {
      // Device logins finish outside the admin mutation handlers.
      void post(`/admin/catalog/refresh?providers=${encodeURIComponent(flow.provider)}`).catch((error: Error) => toast.error(error.message));
      loggedIn(s.email);
      onDone();
    } else if (s?.status === "failed") {
      toast.error(`${name} login failed: ${s.error}`);
      onDone();
    }
    // Act once per final status.
  }, [status.data?.status]);

  return (
    <>
      <div className="bg-background flex flex-col gap-2 rounded-md p-4">
        <div className="text-muted-foreground text-sm">Go to</div>
        <ExternalLink href={flow.url}>{flow.url}</ExternalLink>
        <div className="text-muted-foreground text-sm">and enter</div>
        <div className="font-mono text-3xl font-medium tracking-widest select-all">{flow.user_code}</div>
      </div>
      <div role="status" className="text-info text-sm">
        Waiting for authorization… (the code expires after 15 minutes)
      </div>
      <Button variant="ghost" className="self-start" onClick={onDone}>
        Cancel
      </Button>
    </>
  );
}

function CodexLogin() {
  const { flow, setFlow, start } = useFlow("codex");
  return (
    <Section
      title="Codex (ChatGPT)"
      description="Device code: open the page on any device, sign in to ChatGPT and enter the code. This screen updates when you finish."
    >
      {!flow && (
        <div className="flex flex-wrap gap-2">
          <Button disabled={start.isPending} onClick={() => start.mutate(true)}>
            Start device login
          </Button>
          <Button variant="outline" disabled={start.isPending} onClick={() => start.mutate(false)}>
            Use browser sign-in instead
          </Button>
        </div>
      )}
      {flow && !flow.needs_code && <DeviceCode flow={flow} name="Codex" onDone={() => setFlow(null)} />}
      {flow?.needs_code && (
        <>
          <p className="text-sm leading-relaxed">
            {flow.instructions} <ExternalLink href={flow.url}>Open the sign-in page</ExternalLink>
          </p>
          <PasteCode flow={flow} onDone={() => setFlow(null)} placeholder="paste the http://localhost:1455/… URL" />
        </>
      )}
    </Section>
  );
}

function ClaudeLogin() {
  const { flow, setFlow, start } = useFlow("claude");
  return (
    <Section title="Claude (Pro / Max)" description="Authorize on claude.ai, then paste the code it shows (it looks like CODE#STATE).">
      {!flow ? (
        <Button className="self-start" disabled={start.isPending} onClick={() => start.mutate(true)}>
          Start Claude login
        </Button>
      ) : (
        <>
          <Button asChild variant="secondary" className="self-start">
            <a href={flow.url} target="_blank" rel="noreferrer noopener">
              Open claude.ai authorization
            </a>
          </Button>
          <PasteCode flow={flow} onDone={() => setFlow(null)} placeholder="CODE#STATE or the callback URL" />
        </>
      )}
    </Section>
  );
}

function GrokLogin() {
  const { flow, setFlow, start } = useFlow("grok");
  return (
    <Section
      title="Grok Build (xAI)"
      description="Device code: open the page on any device, sign in to your xAI account and confirm the code. This screen updates when you finish."
    >
      {!flow ? (
        <Button className="self-start" disabled={start.isPending} onClick={() => start.mutate(true)}>
          Start device login
        </Button>
      ) : (
        <DeviceCode flow={flow} name="Grok" onDone={() => setFlow(null)} />
      )}
    </Section>
  );
}

interface CatalogRefreshResult {
  provider: string;
  models?: number;
  error?: string;
}

function reportModels(r: { catalog_refresh?: CatalogRefreshResult | null }) {
  const c = r.catalog_refresh;
  if (!c) return;
  if (c.error) toast.error(c.error);
  else toast.success(`${c.provider}: ${c.models} models found`);
}

/** A pool account added with an API key. */
function ApiKeyAccount({ info }: { info: AccountKindInfo }) {
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const loggedIn = useLoggedIn();
  const add = useAction(
    () =>
      post<Account & { catalog_refresh: CatalogRefreshResult }>("/admin/accounts", {
        kind: info.kind,
        api_key: key.trim(),
        label: label.trim() || null,
      }),
    {
      onSuccess: (r) => {
        loggedIn();
        reportModels(r);
        setKey("");
        setLabel("");
      },
    },
  );
  const id = `add-${info.kind}`;
  return (
    <Section title={info.label} description={info.description}>
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          add.mutate(undefined);
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor={`${id}-label`}>Label</Label>
          <Input id={`${id}-label`} placeholder="main" value={label} onChange={(e) => setLabel(e.target.value)} />
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor={`${id}-key`}>API key</Label>
          <Input
            id={`${id}-key`}
            type="password"
            autoComplete="off"
            className="font-mono"
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
        </div>
        <Button type="submit" className="self-start" disabled={!key.trim() || add.isPending}>
          Add key
        </Button>
      </form>
    </Section>
  );
}

/** `Name: value` lines to a header map. */
export function parseHeaders(text: string): Record<string, string> | string {
  const out: Record<string, string> = {};
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const at = line.indexOf(":");
    if (at <= 0) return `"${line.trim()}" is not "Name: value"`;
    out[line.slice(0, at).trim()] = line.slice(at + 1).trim();
  }
  return out;
}

/** A provider account: a kind, the name clients address it by, and its key. */
function ProviderAccountForm({ kinds }: { kinds: AccountKindInfo[] }) {
  const qc = useQueryClient();
  const [kind, setKind] = useState(kinds[0].kind);
  const [name, setName] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [key, setKey] = useState("");
  const [noAuth, setNoAuth] = useState(false);
  const [headers, setHeaders] = useState("");
  const info = kinds.find((k) => k.kind === kind) ?? kinds[0];
  const parsed = parseHeaders(headers);
  const needsUrl = info.default_base_url === null && !baseUrl.trim();
  const add = useAction(
    () =>
      post<ProviderAccount & { catalog_refresh: CatalogRefreshResult | null }>("/admin/accounts", {
        kind,
        name: name.trim(),
        base_url: baseUrl.trim() || null,
        api_key: noAuth ? null : key.trim(),
        no_auth: noAuth,
        headers: typeof parsed === "string" ? {} : parsed,
      }),
    {
      invalidate: PROVIDER_QUERIES,
      onSuccess: (r) => {
        toast.success(`Added ${r.name}`);
        reportModels(r);
        qc.invalidateQueries({ queryKey: ["accounts"] });
        setName("");
        setBaseUrl("");
        setKey("");
        setHeaders("");
      },
    },
  );
  return (
    <Section title="Provider account" description={info.description}>
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          add.mutate(undefined);
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor="provider-kind">Kind</Label>
          <Select value={kind} onValueChange={(v) => setKind(v as AccountKind)}>
            <SelectTrigger id="provider-kind" className="w-64">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {kinds.map((k) => (
                <SelectItem key={k.kind} value={k.kind}>
                  {k.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="provider-name">Name</Label>
          <Input
            id="provider-name"
            className="font-mono"
            placeholder="local"
            aria-describedby="provider-name-help"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <p id="provider-name-help" className="text-muted-foreground text-xs">
            Letters, digits, - and _. Clients use it as the model prefix: <span className="font-mono">{name.trim() || "name"}/model</span>.
          </p>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="provider-url">Base URL{info.default_base_url === null ? "" : " (optional)"}</Label>
          <Input
            id="provider-url"
            className="font-mono"
            placeholder={info.default_base_url ?? "http://llama.example.lan:8080"}
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
          />
        </div>
        <div className="flex items-center gap-2">
          <Checkbox id="provider-no-auth" checked={noAuth} onCheckedChange={(v) => setNoAuth(v === true)} />
          <Label htmlFor="provider-no-auth">The server needs no key</Label>
        </div>
        {!noAuth && (
          <div className="flex flex-col gap-2">
            <Label htmlFor="provider-key">API key</Label>
            <Input
              id="provider-key"
              type="password"
              autoComplete="off"
              className="font-mono"
              value={key}
              onChange={(e) => setKey(e.target.value)}
            />
          </div>
        )}
        <div className="flex flex-col gap-2">
          <Label htmlFor="provider-headers">Extra headers (optional)</Label>
          <Textarea
            id="provider-headers"
            className="font-mono"
            rows={2}
            placeholder="X-Team: blue"
            aria-describedby="provider-headers-help"
            aria-invalid={typeof parsed === "string"}
            value={headers}
            onChange={(e) => setHeaders(e.target.value)}
          />
          <p id="provider-headers-help" className={typeof parsed === "string" ? "text-destructive text-xs" : "text-muted-foreground text-xs"}>
            {typeof parsed === "string" ? parsed : "One Name: value per line, sent upstream with every request. Stored in OpenBao."}
          </p>
        </div>
        <Button
          type="submit"
          className="self-start"
          disabled={!name.trim() || needsUrl || (!noAuth && !key.trim()) || typeof parsed === "string" || add.isPending}
        >
          Add provider
        </Button>
      </form>
    </Section>
  );
}

function ImportLogin({ kinds }: { kinds: AccountKindInfo[] }) {
  const [provider, setProvider] = useState(kinds[0].kind);
  const [file, setFile] = useState<File | null>(null);
  const [inputKey, setInputKey] = useState(0);
  const loggedIn = useLoggedIn();
  const imp = useAction(
    async () => {
      let data: unknown;
      try {
        data = JSON.parse(await file!.text());
      } catch {
        throw new Error("That file is not JSON.");
      }
      return post<Account>("/admin/accounts/import", { provider, data });
    },
    {
      onSuccess: (a) => {
        loggedIn(a.email);
        setFile(null);
        setInputKey((k) => k + 1);
      },
    },
  );
  return (
    <Section
      title="Import a login"
      description={
        <>
          Codex <span className="font-mono">~/.codex/auth.json</span> or Claude Code{" "}
          <span className="font-mono">~/.claude/.credentials.json</span>. Stop using that login locally afterwards: refresh tokens rotate,
          and only one side keeps a working one.
        </>
      }
    >
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          imp.mutate(undefined);
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor="import-provider">Provider</Label>
          <Select value={provider} onValueChange={(v) => setProvider(v as AccountKind)}>
            <SelectTrigger id="import-provider" className="w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {kinds.map((k) => (
                <SelectItem key={k.kind} value={k.kind}>
                  {k.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="import-file">Credential file</Label>
          <Input
            key={inputKey}
            id="import-file"
            type="file"
            accept="application/json,.json"
            onChange={(e) => setFile(e.target.files?.[0] ?? null)}
          />
        </div>
        <Button type="submit" className="self-start" disabled={!file || imp.isPending}>
          Import
        </Button>
      </form>
    </Section>
  );
}
