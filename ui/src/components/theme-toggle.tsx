import { Monitor, Moon, Sun } from "lucide-react";

import { setTheme, useTheme, type Theme } from "@/lib/theme";
import { cn } from "@/lib/utils";

const OPTIONS: { theme: Theme; label: string; Icon: typeof Sun }[] = [
  { theme: "system", label: "System theme", Icon: Monitor },
  { theme: "light", label: "Light theme", Icon: Sun },
  { theme: "dark", label: "Dark theme", Icon: Moon },
];

/** System / light / dark switch for the (always dark) sidebar. */
export function ThemeToggle() {
  const theme = useTheme();
  return (
    <div role="group" aria-label="Theme" className="bg-sidebar-accent/60 flex w-fit gap-0.5 rounded-md p-0.5">
      {OPTIONS.map(({ theme: t, label, Icon }) => (
        <button
          key={t}
          type="button"
          title={label}
          aria-label={label}
          aria-pressed={theme === t}
          onClick={() => setTheme(t)}
          className={cn(
            "focus-visible:ring-ring/50 rounded px-2 py-1.5 transition-colors outline-none hover:text-white focus-visible:ring-[3px]",
            theme === t && "bg-sidebar-accent text-white",
          )}
        >
          <Icon className="size-4" aria-hidden />
        </button>
      ))}
    </div>
  );
}
