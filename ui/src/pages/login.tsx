import { useState } from "react";

import { Button } from "@/components/ui/button";

// Reasons the callback sends back in `?signin=`; the details are in the server log.
const SIGN_IN_ERRORS: Record<string, string> = {
  denied: "The identity provider refused the sign-in.",
  expired: "That sign-in expired or was started in another browser. Sign in again.",
  failed: "The sign-in could not be verified. The server log has the details.",
  forbidden: "Your account is not in the admin group.",
  unavailable: "The identity provider could not be reached. Try again, or use the CLI.",
};

/** The reason a sign-in just failed, taken from the URL once. */
function takeSignInError(): string | undefined {
  const params = new URLSearchParams(window.location.search);
  const reason = params.get("signin");
  if (reason === null) return undefined;
  window.history.replaceState(null, "", window.location.pathname + window.location.hash);
  return SIGN_IN_ERRORS[reason] ?? SIGN_IN_ERRORS.failed;
}

export function LoginPage({ configured, error }: { configured: boolean; error?: string }) {
  const [failure] = useState<string | undefined>(() => takeSignInError() ?? error);

  return (
    <div className="bg-sidebar flex min-h-screen items-center justify-center p-4">
      <div className="bg-card flex w-full max-w-sm flex-col gap-5 rounded-xl p-8 shadow-lg">
        <div className="flex flex-col gap-1.5">
          <div className="font-mono text-xl font-medium">xolotl</div>
          <h1 className="text-xl font-semibold">Sign in</h1>
          <p className="text-muted-foreground text-sm leading-relaxed">
            Sign in with your identity provider. Only members of the admin group get in.
          </p>
        </div>
        {!configured && (
          <p className="bg-warning-muted text-warning rounded-md px-3 py-2 text-sm">
            Sign-in is not configured. Add <span className="font-mono">[ui.oidc]</span> to the config file.
          </p>
        )}
        {failure && (
          <p role="alert" className="text-destructive text-sm">
            {failure}
          </p>
        )}
        {configured && (
          <Button asChild size="lg">
            <a href="/admin/session/login" className="no-underline">
              Sign in
            </a>
          </Button>
        )}
        <p className="text-muted-foreground text-xs leading-relaxed">Sessions last 12 hours.</p>
      </div>
    </div>
  );
}
