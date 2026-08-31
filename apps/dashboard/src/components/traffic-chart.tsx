import { useMemo } from "react"
import { Bar, BarChart, CartesianGrid, XAxis, YAxis } from "recharts"

import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from "@/components/ui/chart"
import { Skeleton } from "@/components/ui/skeleton"
import type { TrafficBucket } from "@/lib/types"

const chartConfig = {
  ok: { label: "OK (2xx/3xx)", color: "var(--chart-2)" },
  client_error: { label: "4xx", color: "oklch(0.77 0.16 70)" },
  server_error: { label: "5xx", color: "var(--destructive)" },
} satisfies ChartConfig

function formatTime(iso: string): string {
  const date = new Date(iso)
  if (Number.isNaN(date.getTime())) return ""
  return date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
}

/** Stacked requests-per-bucket bars, one colour per HTTP status class. */
export function TrafficChart({
  data,
  bucketSeconds,
  loading,
}: {
  data: TrafficBucket[] | undefined
  bucketSeconds: number
  loading: boolean
}) {
  const chartData = useMemo(
    () =>
      (data ?? []).map((point) => ({
        ...point,
        label: formatTime(point.bucket),
      })),
    [data],
  )

  if (loading && chartData.length === 0) {
    return <Skeleton className="h-[260px] w-full" />
  }

  return (
    <ChartContainer config={chartConfig} className="aspect-auto h-[260px] w-full">
      <BarChart data={chartData} barCategoryGap={2}>
        <CartesianGrid vertical={false} />
        <XAxis
          dataKey="label"
          tickLine={false}
          axisLine={false}
          tickMargin={8}
          minTickGap={40}
        />
        <YAxis
          allowDecimals={false}
          tickLine={false}
          axisLine={false}
          width={36}
        />
        <ChartTooltip
          cursor={{ fillOpacity: 0.08 }}
          content={
            <ChartTooltipContent
              labelFormatter={(_, payload) => {
                const bucket = payload?.[0]?.payload?.bucket as string | undefined
                if (!bucket) return ""
                const start = new Date(bucket)
                const end = new Date(start.getTime() + bucketSeconds * 1000)
                return `${formatTime(start.toISOString())} – ${formatTime(end.toISOString())}`
              }}
              indicator="dot"
            />
          }
        />
        <ChartLegend content={<ChartLegendContent />} />
        <Bar dataKey="ok" stackId="status" fill="var(--color-ok)" />
        <Bar dataKey="client_error" stackId="status" fill="var(--color-client_error)" />
        <Bar
          dataKey="server_error"
          stackId="status"
          fill="var(--color-server_error)"
          radius={[3, 3, 0, 0]}
        />
      </BarChart>
    </ChartContainer>
  )
}
