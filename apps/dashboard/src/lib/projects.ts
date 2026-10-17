import type { ProjectColor } from "@/lib/types"

/** Mirrors `domain::ProjectColor::ALL` — palette order. */
export const PROJECT_COLORS: ProjectColor[] = [
  "blue",
  "emerald",
  "amber",
  "rose",
  "violet",
  "cyan",
  "orange",
  "fuchsia",
]

/** Background class of each palette colour (tokens in index.css). */
export const PROJECT_COLOR_CLASS: Record<ProjectColor, string> = {
  blue: "bg-project-blue",
  emerald: "bg-project-emerald",
  amber: "bg-project-amber",
  rose: "bg-project-rose",
  violet: "bg-project-violet",
  cyan: "bg-project-cyan",
  orange: "bg-project-orange",
  fuchsia: "bg-project-fuchsia",
}

export const PROJECT_NAME_MAX_CHARS = 60
export const PROJECT_KEY_TAG_MAX_LEN = 5

/** The first palette colour no project uses yet; once all are taken, the
 *  least-used one, palette order breaking ties (mirrors `domain::pick_color`). */
export function nextProjectColor(usedColors: ProjectColor[]): ProjectColor {
  let best: ProjectColor = PROJECT_COLORS[0]
  let bestCount = Number.POSITIVE_INFINITY
  for (const color of PROJECT_COLORS) {
    const count = usedColors.filter((c) => c === color).length
    if (count < bestCount) {
      best = color
      bestCount = count
    }
  }
  return best
}
