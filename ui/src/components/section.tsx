import { useState, type ReactNode } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";

import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { cn } from "@/lib/utils";

const stored = (id: string) => localStorage.getItem(`section:${id}`);

/**
 * A card whose body folds away under its title. `id` keeps the open state
 * across visits; `summary` shows next to the title while it is folded.
 */
export function Section({
  id,
  title,
  description,
  summary,
  defaultOpen = true,
  className,
  children,
}: {
  id: string;
  title: ReactNode;
  description?: ReactNode;
  summary?: ReactNode;
  defaultOpen?: boolean;
  className?: string;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(() => (stored(id) ?? (defaultOpen ? "1" : "0")) === "1");
  const flip = () =>
    setOpen((o) => {
      localStorage.setItem(`section:${id}`, o ? "0" : "1");
      return !o;
    });
  const Icon = open ? ChevronDown : ChevronRight;

  return (
    <Card className={cn(open ? className : "gap-0")}>
      <CardHeader>
        <CardTitle className="text-base">
          <button type="button" className="flex w-full items-center gap-2 text-left" aria-expanded={open} onClick={flip}>
            <Icon className="text-muted-foreground size-4 shrink-0" />
            <span>{title}</span>
            {!open && summary && <span className="text-muted-foreground truncate text-sm font-normal">{summary}</span>}
          </button>
        </CardTitle>
        {open && description && <CardDescription className="pl-6">{description}</CardDescription>}
      </CardHeader>
      {open && children}
    </Card>
  );
}

export { CardContent as SectionContent };
