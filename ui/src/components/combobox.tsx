import { useId, useState } from "react";

import { Input } from "@/components/ui/input";
import { highlighted, suggestions } from "@/lib/suggest";
import { cn } from "@/lib/utils";

/**
 * A text field that suggests matching options as you type. Any text is a
 * valid value, so a target missing from the options can still be entered.
 */
export function Combobox({
  value,
  onChange,
  options,
  placeholder,
  className,
  "aria-label": label,
  autoFocus,
}: {
  value: string;
  onChange: (v: string) => void;
  options: string[];
  placeholder?: string;
  className?: string;
  "aria-label"?: string;
  autoFocus?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const id = useId();
  const shown = suggestions(options, value);
  // The list changes under a kept index: after a pick, or new options.
  const at = highlighted(active, shown);
  const pick = (o: string) => {
    onChange(o);
    setActive(0);
    setOpen(false);
  };

  return (
    <div className={cn("relative", className)}>
      <Input
        role="combobox"
        aria-label={label}
        aria-expanded={open && shown.length > 0}
        aria-controls={id}
        aria-autocomplete="list"
        autoFocus={autoFocus}
        className="h-8 w-full font-mono text-xs"
        placeholder={placeholder}
        value={value}
        onFocus={() => {
          setActive(0);
          setOpen(true);
        }}
        // Let a click on an option land before the list closes.
        onBlur={() => setTimeout(() => setOpen(false), 120)}
        onChange={(e) => {
          onChange(e.target.value);
          setActive(0);
          setOpen(true);
        }}
        onKeyDown={(e) => {
          if (!open || shown.length === 0) return;
          if (e.key === "ArrowDown") setActive(Math.min(at + 1, shown.length - 1));
          else if (e.key === "ArrowUp") setActive(Math.max(at - 1, 0));
          else if (e.key === "Enter") pick(shown[at]);
          else if (e.key === "Escape") setOpen(false);
          else return;
          e.preventDefault();
        }}
      />
      {open && shown.length > 0 && (
        <ul id={id} role="listbox" className="bg-popover absolute z-20 mt-1 max-h-64 w-full overflow-auto rounded-md border py-1 shadow-md">
          {shown.map((o, i) => (
            <li
              key={o}
              role="option"
              aria-selected={i === at}
              className={cn("cursor-pointer px-2 py-1 font-mono text-xs", i === at && "bg-accent")}
              onMouseEnter={() => setActive(i)}
              onMouseDown={(e) => {
                e.preventDefault();
                pick(o);
              }}
            >
              {o}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
