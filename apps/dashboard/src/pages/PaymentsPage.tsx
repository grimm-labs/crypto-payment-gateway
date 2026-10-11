import { useEffect, useState } from "react"
import { zodResolver } from "@hookform/resolvers/zod"
import { Plus } from "lucide-react"
import { useForm } from "react-hook-form"
import { Link, useSearchParams } from "react-router-dom"
import { toast } from "sonner"
import { z } from "zod"

import { CopyButton } from "@/components/shared/CopyButton"
import { Pagination } from "@/components/shared/Pagination"
import { QueryError } from "@/components/shared/QueryError"
import { StatusBadge } from "@/components/shared/StatusBadge"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Skeleton } from "@/components/ui/skeleton"
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table"
import { useCreatePayment, usePayments } from "@/hooks/queries"
import { toastError } from "@/lib/errors"
import {
  formatAmount,
  formatDate,
  isBtcNetwork,
  networkLabel,
  networkToken,
  satsToBtc,
  shortHash,
} from "@/lib/format"
import type { DepositAddress } from "@/lib/types"

const PAGE_SIZE = 20
const ALL = "all"

export function PaymentsPage() {
  const [searchParams, setSearchParams] = useSearchParams()
  const status = searchParams.get("status") ?? ALL
  const network = searchParams.get("network") ?? ALL
  const offset = Number(searchParams.get("offset") ?? 0)

  const payments = usePayments({
    status: status === ALL ? undefined : status,
    network: network === ALL ? undefined : network,
    limit: PAGE_SIZE,
    offset,
  })

  function updateParams(updates: Record<string, string | undefined>) {
    const next = new URLSearchParams(searchParams)
    for (const [key, value] of Object.entries(updates)) {
      if (value === undefined || value === ALL) next.delete(key)
      else next.set(key, value)
    }
    setSearchParams(next)
  }

  return (
    <div className="space-y-6 p-4 lg:p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Payments</h1>
          <p className="text-sm text-muted-foreground">
            Deposit addresses generated for expected payments.
          </p>
        </div>
        <CreatePaymentDialog />
      </div>

      <div className="flex flex-wrap gap-3">
        <Select value={status} onValueChange={(value) => updateParams({ status: value, offset: undefined })}>
          <SelectTrigger className="w-36">
            <SelectValue placeholder="Status" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All statuses</SelectItem>
            <SelectItem value="pending">Pending</SelectItem>
            <SelectItem value="paid">Paid</SelectItem>
            <SelectItem value="expired">Expired</SelectItem>
          </SelectContent>
        </Select>
        <Select value={network} onValueChange={(value) => updateParams({ network: value, offset: undefined })}>
          <SelectTrigger className="w-36">
            <SelectValue placeholder="Network" />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ALL}>All networks</SelectItem>
            <SelectItem value="tron">Tron</SelectItem>
            <SelectItem value="ethereum">Ethereum</SelectItem>
            <SelectItem value="solana">Solana</SelectItem>
            <SelectItem value="bitcoin">Bitcoin</SelectItem>
            <SelectItem value="lightning">Lightning</SelectItem>
          </SelectContent>
        </Select>
      </div>

      {payments.isError ? (
        <QueryError error={payments.error} />
      ) : payments.isLoading ? (
        <Skeleton className="h-64 w-full" />
      ) : (
        <>
          <div className="rounded-lg border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Address</TableHead>
                  <TableHead>Network</TableHead>
                  <TableHead>Expected</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead>Expires</TableHead>
                  <TableHead>Created</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {payments.data?.data.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={6} className="h-24 text-center text-muted-foreground">
                      No payments found.
                    </TableCell>
                  </TableRow>
                )}
                {payments.data?.data.map((payment) => (
                  <TableRow key={payment.id}>
                    <TableCell>
                      <Link
                        to={`/payments/${payment.id}`}
                        className="font-mono text-sm underline-offset-4 hover:underline"
                      >
                        {shortHash(payment.address)}
                      </Link>
                    </TableCell>
                    <TableCell>{networkLabel(payment.network)}</TableCell>
                    <TableCell className="tabular-nums">
                      {formatAmount(payment.expected_amount, payment.token)}
                    </TableCell>
                    <TableCell>
                      <StatusBadge status={payment.status} />
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {formatDate(payment.expires_at)}
                    </TableCell>
                    <TableCell className="text-muted-foreground">
                      {formatDate(payment.created_at)}
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>
          {payments.data && (
            <Pagination
              total={payments.data.total}
              limit={PAGE_SIZE}
              offset={offset}
              onOffsetChange={(next) => updateParams({ offset: next ? String(next) : undefined })}
            />
          )}
        </>
      )}
    </div>
  )
}

/** Total bitcoin supply, in sats. */
const MAX_SATS = "2100000000000000"
const DESCRIPTION_MAX_CHARS = 128
/** API bounds for an explicit expiry, relative to now. */
const EXPIRY_MIN_MS = 60_000
const EXPIRY_MAX_MS = 1440 * 60_000

function isValidSats(value: string): boolean {
  if (!/^[1-9]\d*$/.test(value)) return false
  // Integer strings without leading zeros compare by length, then lexically.
  return value.length < MAX_SATS.length || (value.length === MAX_SATS.length && value <= MAX_SATS)
}

const createSchema = z
  .object({
    network: z.enum(["tron", "ethereum", "solana", "bitcoin", "lightning"]),
    // USDT (decimal) on tron/ethereum/solana, sats (integer) on bitcoin/lightning.
    expected_amount: z.string().trim(),
    description: z.string().trim().optional(),
    expires_at: z.string().optional(),
  metadata: z
    .string()
    .trim()
    .optional()
    .refine(
      (value) => {
        if (!value) return true
        try {
          const parsed: unknown = JSON.parse(value)
          return typeof parsed === "object" && parsed !== null
        } catch {
          return false
        }
      },
      { message: "Must be a valid JSON object" },
    ),
  })
  .superRefine((values, ctx) => {
    if (isBtcNetwork(values.network)) {
      if (!isValidSats(values.expected_amount)) {
        ctx.addIssue({
          code: z.ZodIssueCode.custom,
          path: ["expected_amount"],
          message: "Whole number of sats, at least 1",
        })
      }
    } else if (
      !/^\d+(\.\d{1,6})?$/.test(values.expected_amount) ||
      // String check, no float parsing: some non-zero digit.
      !/[1-9]/.test(values.expected_amount)
    ) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ["expected_amount"],
        message: "Positive amount, up to 6 decimals",
      })
    }
    if (values.expires_at) {
      const delta = new Date(values.expires_at).getTime() - Date.now()
      if (Number.isNaN(delta) || delta < EXPIRY_MIN_MS || delta > EXPIRY_MAX_MS) {
        ctx.addIssue({
          code: z.ZodIssueCode.custom,
          path: ["expires_at"],
          message: "Between 1 minute and 24 hours from now",
        })
      }
    }
    if ((values.description?.length ?? 0) > DESCRIPTION_MAX_CHARS) {
      ctx.addIssue({
        code: z.ZodIssueCode.custom,
        path: ["description"],
        message: `At most ${DESCRIPTION_MAX_CHARS} characters`,
      })
    }
  })

type CreateValues = z.infer<typeof createSchema>

function CreatePaymentDialog() {
  const [searchParams, setSearchParams] = useSearchParams()
  const [open, setOpen] = useState(() => searchParams.has("create"))
  const [created, setCreated] = useState<DepositAddress | null>(null)
  const createPayment = useCreatePayment()
  const form = useForm<CreateValues>({
    resolver: zodResolver(createSchema),
    defaultValues: { network: "tron", expected_amount: "" },
  })
  const selectedNetwork = form.watch("network")
  const isBtc = isBtcNetwork(selectedNetwork)
  const btcHint = isBtc ? satsToBtc(form.watch("expected_amount") ?? "") : null

  // The sidebar "New payment" button navigates to /payments?create=1.
  useEffect(() => {
    if (searchParams.has("create")) setOpen(true)
  }, [searchParams])

  async function onSubmit(values: CreateValues) {
    try {
      const btc = isBtcNetwork(values.network)
      const payment = await createPayment.mutateAsync({
        network: values.network,
        token: networkToken(values.network),
        ...(btc
          ? { expected_amount_sats: values.expected_amount }
          : { expected_amount: values.expected_amount }),
        description:
          values.network === "lightning" && values.description ? values.description : undefined,
        expires_at: values.expires_at
          ? new Date(values.expires_at).toISOString()
          : undefined,
        metadata: values.metadata ? JSON.parse(values.metadata) : undefined,
      })
      setCreated(payment)
      toast.success("Deposit address created")
    } catch (error) {
      toastError(error, "Creation failed")
    }
  }

  function handleOpenChange(next: boolean) {
    setOpen(next)
    if (!next) {
      setCreated(null)
      form.reset({
        network: "tron",
        expected_amount: "",
        description: "",
        expires_at: "",
        metadata: "",
      })
      if (searchParams.has("create")) {
        const params = new URLSearchParams(searchParams)
        params.delete("create")
        setSearchParams(params, { replace: true })
      }
    }
  }

  const { errors, isSubmitting } = form.formState

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogTrigger asChild>
        <Button>
          <Plus className="size-4" /> New payment
        </Button>
      </DialogTrigger>
      <DialogContent className="sm:max-w-md">
        {created ? (
          <>
            <DialogHeader>
              <DialogTitle>
                {created.network === "lightning" ? "Invoice ready" : "Address ready"}
              </DialogTitle>
              <DialogDescription>
                {created.network === "lightning"
                  ? "Share this Lightning payment request with the payer."
                  : `Share this ${networkLabel(created.network)} address with the payer.`}
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="flex items-center justify-between gap-2 rounded-lg border bg-muted/40 p-3">
                <code className="break-all text-sm">{created.address}</code>
                <CopyButton value={created.address} />
              </div>
              <p className="text-sm text-muted-foreground">
                Expecting <span className="font-medium">{formatAmount(created.expected_amount, created.token)}</span>{" "}
                before {formatDate(created.expires_at)}.
              </p>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => handleOpenChange(false)}>
                Close
              </Button>
              <Button asChild>
                <Link to={`/payments/${created.id}`}>View payment</Link>
              </Button>
            </DialogFooter>
          </>
        ) : (
          <>
            <DialogHeader>
              <DialogTitle>New payment</DialogTitle>
              <DialogDescription>
                Generates a unique deposit address — or a Lightning invoice — for the
                expected amount.
              </DialogDescription>
            </DialogHeader>
            <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
              <div className="space-y-2">
                <Label>Network</Label>
                <Select
                  value={selectedNetwork}
                  onValueChange={(value) => {
                    const next = value as CreateValues["network"]
                    // The amount unit changes (USDT ↔ sats): don't carry it over.
                    if (isBtcNetwork(next) !== isBtc) form.setValue("expected_amount", "")
                    form.setValue("network", next)
                  }}
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="tron">Tron (TRC-20)</SelectItem>
                    <SelectItem value="ethereum">Ethereum (ERC-20)</SelectItem>
                    <SelectItem value="solana">Solana (SPL)</SelectItem>
                    <SelectItem value="bitcoin">Bitcoin (on-chain)</SelectItem>
                    <SelectItem value="lightning">Lightning (BOLT11)</SelectItem>
                  </SelectContent>
                </Select>
                {isBtc && (
                  <p className="text-xs text-muted-foreground">Token: BTC</p>
                )}
              </div>
              <div className="space-y-2">
                <Label htmlFor="expected_amount">
                  Expected amount ({isBtc ? "sats" : "USDT"})
                </Label>
                <Input
                  id="expected_amount"
                  placeholder={isBtc ? "150000" : "25.00"}
                  inputMode={isBtc ? "numeric" : "decimal"}
                  {...form.register("expected_amount")}
                />
                {btcHint && (
                  <p className="text-xs text-muted-foreground">≈ {btcHint} BTC</p>
                )}
                {errors.expected_amount && (
                  <p className="text-xs text-destructive">{errors.expected_amount.message}</p>
                )}
              </div>
              {selectedNetwork === "lightning" && (
                <div className="space-y-2">
                  <Label htmlFor="description">Invoice description (optional)</Label>
                  <Input
                    id="description"
                    placeholder="Order #1234"
                    maxLength={DESCRIPTION_MAX_CHARS}
                    {...form.register("description")}
                  />
                  {errors.description && (
                    <p className="text-xs text-destructive">{errors.description.message}</p>
                  )}
                </div>
              )}
              <div className="space-y-2">
                <Label htmlFor="expires_at">Expires at (optional)</Label>
                <Input id="expires_at" type="datetime-local" {...form.register("expires_at")} />
                {errors.expires_at ? (
                  <p className="text-xs text-destructive">{errors.expires_at.message}</p>
                ) : (
                  <p className="text-xs text-muted-foreground">
                    Between 1 minute and 24 hours from now. Defaults to the
                    configured payment expiry.
                  </p>
                )}
              </div>
              <div className="space-y-2">
                <Label htmlFor="metadata">Metadata JSON (optional)</Label>
                <Input
                  id="metadata"
                  placeholder='{"order_id": "1234"}'
                  {...form.register("metadata")}
                />
                {errors.metadata && (
                  <p className="text-xs text-destructive">{errors.metadata.message}</p>
                )}
              </div>
              <DialogFooter>
                <Button type="submit" disabled={isSubmitting}>
                  {isSubmitting ? "Creating…" : "Create address"}
                </Button>
              </DialogFooter>
            </form>
          </>
        )}
      </DialogContent>
    </Dialog>
  )
}
