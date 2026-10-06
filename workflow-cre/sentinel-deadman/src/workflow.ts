/**
 * Sentinel dead-man's-switch — CRE workflow definition (SPEC-P14 §6).
 *
 * Trigger: cron every 2 minutes (config `schedule`).
 * Steps:
 *   1. HTTP GET the breaker's `/api/heartbeat-status` (frozen surface, SPEC §5).
 *   2. Condition: any guardian `stale && critical` (+ optional allowlist).
 *   3. Optional direct chain-read leg: latest SentinelAuditAnchor `Heartbeat`
 *      event age straight from a JSON-RPC endpoint (over the HTTP capability;
 *      see README for why this does not use the EVM capability).
 *   4. Target: HTTP POST `/breaker/trigger` with the frozen
 *      `X-Breaker-Signature: sha256=<HMAC_SHA256(secret, body)>` header.
 *   5. Optional Telegram alert step (best effort).
 *
 * All decision/signature logic lives in `./core.ts`, which is shared verbatim
 * with the Node fallback runner (`tools/local-runner.ts`).
 */
import {
  CronCapability,
  HTTPClient,
  consensusIdenticalAggregation,
  handler,
  ok,
  text,
  type CronPayload,
  type HTTPSendRequester,
  type Runtime,
  type Workflow,
} from '@chainlink/cre-sdk'
import {
  configSchema,
  runSweep,
  type DeadmanConfig,
  type HttpOutcome,
  type SweepTransport,
} from './core.ts'

// ---------------------------------------------------------------------------
// Node-mode step functions (executed by every DON node; consensus-aggregated)
// ---------------------------------------------------------------------------

type StepResponse = HttpOutcome

const toBase64 = (body: string): string =>
  Buffer.from(new TextEncoder().encode(body)).toString('base64')

const doGet = (sendRequester: HTTPSendRequester, url: string): StepResponse => {
  const resp = sendRequester.sendRequest({ url, method: 'GET' }).result()
  return { statusCode: Number(resp.statusCode), ok: ok(resp), body: text(resp) }
}

const doPostJson = (sendRequester: HTTPSendRequester, req: { url: string; body: string }): StepResponse => {
  const resp = sendRequester
    .sendRequest({
      url: req.url,
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: toBase64(req.body),
    })
    .result()
  return { statusCode: Number(resp.statusCode), ok: ok(resp), body: text(resp) }
}

/**
 * Signed trigger POST. `cacheSettings` deduplicates the call across DON nodes
 * (single execution for this non-idempotent POST; the breaker additionally
 * dedupes per (guardian, epoch) per SPEC §3).
 */
const doPostSigned = (
  sendRequester: HTTPSendRequester,
  req: { url: string; body: string; signature: string },
): StepResponse => {
  const resp = sendRequester
    .sendRequest({
      url: req.url,
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-Breaker-Signature': req.signature,
      },
      body: toBase64(req.body),
      cacheSettings: { store: true, maxAge: '30s' },
    })
    .result()
  return { statusCode: Number(resp.statusCode), ok: ok(resp), body: text(resp) }
}

// ---------------------------------------------------------------------------
// Cron handler
// ---------------------------------------------------------------------------

export const onCronTrigger = async (runtime: Runtime<DeadmanConfig>, payload: CronPayload): Promise<string> => {
  const config = runtime.config
  const scheduledMs = payload?.scheduledExecutionTime
    ? Number(payload.scheduledExecutionTime.seconds) * 1000
    : null
  const nowMs = runtime.now().getTime()
  runtime.log(
    `[trigger] cron fired schedule=${config.schedule} scheduled_ms=${scheduledMs ?? 'n/a'} don_now_ms=${nowMs}`,
  )

  // Secrets: Vault DON in production, `.env`/env vars during simulation
  // (secrets.yaml declares the logical IDs; see workflow-cre/README.md).
  const armSecret = runtime.getSecret({ id: config.breaker.armSecretId }).result().value
  let telegramToken: string | undefined
  if (config.telegram.enabled && config.telegram.chatId) {
    try {
      telegramToken = runtime.getSecret({ id: config.telegram.botTokenSecretId }).result().value
    } catch (err) {
      runtime.log(`[secrets] telegram token unavailable: ${err instanceof Error ? err.message : String(err)}`)
    }
  }

  const httpClient = new HTTPClient()
  const log = (line: string): void => runtime.log(line)

  const transport: SweepTransport = {
    async fetchStatus(url: string): Promise<string> {
      return httpClient
        .sendRequest(runtime, doGet, consensusIdenticalAggregation<StepResponse>())(url)
        .result()
        .body
    },
    async rpcCall(rpcUrl: string, body: string): Promise<string> {
      const out = httpClient
        .sendRequest(runtime, doPostJson, consensusIdenticalAggregation<StepResponse>())({ url: rpcUrl, body })
        .result()
      if (!out.ok) throw new Error(`JSON-RPC endpoint answered HTTP ${out.statusCode}`)
      return out.body
    },
    async postTrigger(url: string, body: string, signature: string): Promise<HttpOutcome> {
      return httpClient
        .sendRequest(runtime, doPostSigned, consensusIdenticalAggregation<StepResponse>())({ url, body, signature })
        .result()
    },
    async postTelegram(url: string, body: string): Promise<HttpOutcome> {
      return httpClient
        .sendRequest(runtime, doPostJson, consensusIdenticalAggregation<StepResponse>())({ url, body })
        .result()
    },
  }

  const result = await runSweep(config, transport, { armSecret, telegramToken }, nowMs, log)
  const summary = JSON.stringify(result)
  runtime.log(`[done] ${summary}`)
  return summary
}

// ---------------------------------------------------------------------------
// Workflow registration
// ---------------------------------------------------------------------------

export function initWorkflow(config: DeadmanConfig): Workflow<DeadmanConfig> {
  const cron = new CronCapability()
  return [handler(cron.trigger({ schedule: config.schedule }), onCronTrigger)]
}

/** Re-exported so `main.ts` can pass the same schema to `Runner.newRunner`. */
export { configSchema }
