import { useState, type FormEvent } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { post, type Session } from "@/lib/api";

export function LoginPage({ passwordSet, error, onSignedIn }: { passwordSet: boolean; error?: string; onSignedIn: (csrf: string) => void }) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | undefined>(error);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setFailure(undefined);
    try {
      const s = await post<Session>("/admin/session", { password });
      onSignedIn(s.csrf_token!);
    } catch (err) {
      setFailure((err as Error).message);
      setPassword("");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="bg-sidebar flex min-h-screen items-center justify-center p-4">
      <form onSubmit={submit} className="bg-card flex w-full max-w-sm flex-col gap-5 rounded-xl p-8 shadow-lg">
        <div className="flex flex-col gap-1.5">
          <div className="font-mono text-xl font-medium">xolotl</div>
          <h1 className="text-xl font-semibold">Sign in</h1>
          <p className="text-muted-foreground text-sm leading-relaxed">
            Enter the admin password. It is checked against the hash stored in OpenBao in the <span className="font-mono">admin</span>{" "}
            secret.
          </p>
        </div>
        {!passwordSet && (
          <p className="bg-warning-muted text-warning rounded-md px-3 py-2 text-sm">
            No password is set yet. Run <span className="font-mono">xolotl ui set-password</span> first.
          </p>
        )}
        <div className="flex flex-col gap-2">
          <Label htmlFor="password">Password</Label>
          <Input
            id="password"
            type="password"
            autoComplete="current-password"
            autoFocus
            required
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            aria-invalid={failure ? true : undefined}
            aria-describedby={failure ? "login-error" : undefined}
          />
        </div>
        {failure && (
          <p id="login-error" role="alert" className="text-destructive text-sm">
            {failure}
          </p>
        )}
        <Button type="submit" size="lg" disabled={busy || !password}>
          {busy ? "Signing in…" : "Sign in"}
        </Button>
        <p className="text-muted-foreground text-xs leading-relaxed">
          Sessions last 12 hours. Five failed attempts lock this address out for 15 minutes.
        </p>
      </form>
    </div>
  );
}
