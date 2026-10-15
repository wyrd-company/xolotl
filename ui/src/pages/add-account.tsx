import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router";
import { toast } from "sonner";

import { PageHeader } from "@/components/layout";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { get, post, type Account, type FlowStart, type FlowStatus } from "@/lib/api";
import { useAction } from "@/lib/hooks";

export function AddAccountPage() {
  return (
    <>
      <Link to="/accounts" className="text-primary text-sm">
        Back to accounts
      </Link>
      <PageHeader
        title="Add a subscription account"
        description="Credentials go straight to OpenBao. Logging in to an account that already exists replaces its tokens instead of adding a duplicate."
      />
      <div className="grid gap-4 lg:grid-cols-2">
        <CodexLogin />
        <ClaudeLogin />
        <GrokLogin />
        <OpencodeGoKey />
        <ImportLogin />
      </div>
    </>
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

function OpencodeGoKey() {
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const loggedIn = useLoggedIn();
  const add = useAction(
    () => post<Account>("/admin/accounts/import", { provider: "opencode-go", data: { api_key: key.trim() }, label: label.trim() || null }),
    {
      onSuccess: () => {
        loggedIn();
        setKey("");
        setLabel("");
      },
    },
  );
  return (
    <Section title="OpenCode Go" description="API key from the OpenCode console. Add several to spread load and limits.">
      <form
        className="flex flex-col gap-4"
        onSubmit={(e) => {
          e.preventDefault();
          add.mutate(undefined);
        }}
      >
        <div className="flex flex-col gap-2">
          <Label htmlFor="ocg-label">Label</Label>
          <Input id="ocg-label" placeholder="go-main" value={label} onChange={(e) => setLabel(e.target.value)} />
        </div>
        <div className="flex flex-col gap-2">
          <Label htmlFor="ocg-key">API key</Label>
          <Input id="ocg-key" type="password" autoComplete="off" className="font-mono" value={key} onChange={(e) => setKey(e.target.value)} />
        </div>
        <Button type="submit" className="self-start" disabled={!key.trim() || add.isPending}>
          Add key
        </Button>
      </form>
    </Section>
  );
}

function ImportLogin() {
  const [provider, setProvider] = useState<"codex" | "claude">("codex");
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
          Codex <span className="font-mono">~/.codex/auth.json</span> or Claude Code <span className="font-mono">~/.claude/.credentials.json</span>.
          Stop using that login locally afterwards: refresh tokens rotate, and only one side keeps a working one.
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
          <Select value={provider} onValueChange={(v) => setProvider(v as "codex" | "claude")}>
            <SelectTrigger id="import-provider" className="w-48">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="codex">Codex</SelectItem>
              <SelectItem value="claude">Claude</SelectItem>
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
