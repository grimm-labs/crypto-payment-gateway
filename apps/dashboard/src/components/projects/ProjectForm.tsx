import { Check } from "lucide-react"

import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  PROJECT_COLOR_CLASS,
  PROJECT_COLORS,
  PROJECT_KEY_TAG_MAX_LEN,
  PROJECT_NAME_MAX_CHARS,
} from "@/lib/projects"
import type { ProjectColor } from "@/lib/types"
import { cn } from "@/lib/utils"

export interface ProjectFormValues {
  name: string
  color: ProjectColor
  /** Empty = the global API key tag. */
  key_tag: string
}

export function ProjectFormFields({
  values,
  onChange,
  autoFocus,
  idPrefix = "project",
}: {
  values: ProjectFormValues
  onChange: (values: ProjectFormValues) => void
  usedColors: ProjectColor[]
  autoFocus?: boolean
  idPrefix?: string
}) {
  const preview = `${values.key_tag ? `${values.key_tag}_` : ""}xxxxxxxx_${"·".repeat(12)}`
  return (
    <>
      <div className="space-y-2">
        <Label htmlFor={`${idPrefix}-name`}>Name</Label>
        <Input
          id={`${idPrefix}-name`}
          placeholder="e.g. Web shop"
          maxLength={PROJECT_NAME_MAX_CHARS}
          autoFocus={autoFocus}
          value={values.name}
          onChange={(event) => onChange({ ...values, name: event.target.value })}
        />
      </div>
      <div className="space-y-2">
        <Label htmlFor={`${idPrefix}-tag`}>API key tag (optional)</Label>
        <div className="flex items-center gap-2">
          <Input
            id={`${idPrefix}-tag`}
            className="w-28"
            placeholder="global"
            maxLength={PROJECT_KEY_TAG_MAX_LEN}
            value={values.key_tag}
            onChange={(event) =>
              onChange({ ...values, key_tag: event.target.value.replace(/[^a-zA-Z0-9]/g, "") })
            }
          />
          <code className="text-xs text-muted-foreground">{preview}</code>
        </div>
        <p className="text-xs text-muted-foreground">
          Up to 5 alphanumeric characters prepended to the project&apos;s new API keys;
          empty uses the global tag from Settings.
        </p>
      </div>
    </>
  )
}

export function ProjectColorPicker({
  value,
  onChange,
  usedColors,
}: {
  value: ProjectColor
  onChange: (color: ProjectColor) => void
  usedColors: ProjectColor[]
}) {
  return (
    <div className="space-y-2">
      <Label>Colour</Label>
      <div className="flex flex-wrap gap-2">
        {PROJECT_COLORS.map((color) => {
          const used = usedColors.includes(color)
          return (
            <button
              key={color}
              type="button"
              aria-label={`${color}${used ? " (already used)" : ""}`}
              aria-pressed={value === color}
              title={used ? `${color} — already used by another project` : color}
              onClick={() => onChange(color)}
              className={cn(
                "flex size-7 items-center justify-center rounded-full ring-offset-2 ring-offset-background transition",
                PROJECT_COLOR_CLASS[color],
                value === color ? "ring-2 ring-foreground" : "hover:ring-2 hover:ring-muted-foreground/50",
                used && value !== color && "opacity-50",
              )}
            >
              {value === color && <Check className="size-4 text-white" />}
            </button>
          )
        })}
      </div>
    </div>
  )
}
