/**
 * Sentinel dead-man's-switch — shared core logic (runtime-agnostic).
 *
 * This module is imported by BOTH:
 *   1. the CRE workflow (`src/workflow.ts`, runs in the CRE WASM runtime), and
 *   2. the local fallback runner (`tools/local-runner.ts`, runs in Node).
 *
 * It must therefore stay free of `@chainlink/cre-sdk` imports and of Node-only
 * globals: only `@noble/hashes` (bundled in the CRE WASM runtime) and `zod`
 * (a pinned dependency of the CRE SDK) may be used here.
 *
 * The sequence it encodes is the frozen SPEC-P14 §6 definition step by step:
 *   cron trigger -> GET /api/heartbeat-status -> condition (any guardian
 *   stale && critical) -> [optional on-chain corroboration read] -> POST
 *   /breaker/trigger with the HMAC-SHA256 signature -> [optional Telegram alert].
 *
 * The HMAC construction matches the frozen SPEC-P14 §5 test vector:
 *   secret  b"spec-test-secret"
 *   body    {"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","reason":"spec-vector","requested_at_ms":1791200000000}
 *   header  X-Breaker-Signature: sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04
 */
import { hmac } from '@noble/hashes/hmac.js'
import { sha256 } from '@noble/hashes/sha2.js'
import { bytesToHex } from '@noble/hashes/utils.js'
import { z } from 'zod'

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/**
 * `keccak256("Heartbeat(address,bytes32,uint32,uint8)")` — topic0 of the
 * SentinelAuditAnchor Heartbeat event (contracts/src/SentinelAuditAnchor.sol).
 * Verified with `cast keccak "Heartbeat(address,bytes32,uint32,uint8)"`.
 */
export const HEARTBEAT_TOPIC0 =
  '0xa068fbc1b92cb8ea8005e568b0b15b538691078d8437eb83346db7791c7bc6ee'

/** Spec default: cron trigger every 2 minutes (6-field cron, seconds first). */
export const DEFAULT_SCHEDULE = '0 */2 * * * *'

// ---------------------------------------------------------------------------
// Config schema (validated identically in the workflow and the local runner)
// ---------------------------------------------------------------------------

const breakerConfigSchema = z.object({
  /** Base URL of the breaker service, e.g. http://127.0.0.1:9090 */
  baseUrl: z.string(),
  /** GET path returning the frozen heartbeat-status JSON (SPEC-P14 §5). */
  statusPath: z.string().default('/api/heartbeat-status'),
  /** POST path that accepts the dead-man's-switch trigger (SPEC-P14 §5). */
  triggerPath: z.string().default('/breaker/trigger'),
  /** Secret ID (CRE `runtime.getSecret`) / env var name (local runner) of the HMAC key. */
  armSecretId: z.string().default('BREAKER_ARM_SECRET'),
  /** Reason string carried in the trigger body. */
  reason: z.string().default('sentinel-unresponsive-heartbeat-stale-critical'),
})

const onchainConfigSchema = z.object({
  /**
   * off      — skip the chain-read leg entirely;
   * observe  — read the chain, log the derived heartbeat age, never gate;
   * require  — additionally require corroborated on-chain staleness to fire.
   */
  mode: z.enum(['off', 'observe', 'require']).default('observe'),
  /** Plain JSON-RPC endpoint of the chain carrying the anchor contract. */
  rpcUrl: z.string().default('http://127.0.0.1:8547'),
  /** SentinelAuditAnchor deployment to read Heartbeat logs from. */
  anchorAddress: z.string().default('0x5FbDB2315678afecb367f032d93F642f64180aa3'),
  /** eth_getLogs lookback window (blocks) for the Heartbeat topic. */
  lookbackBlocks: z.number().int().nonnegative().default(5000),
  /** Age (seconds) above which the on-chain read counts as "stale (corroborated)". */
  maxAgeSecs: z.number().nonnegative().default(30),
})

const telegramConfigSchema = z.object({
  enabled: z.boolean().default(false),
  /** Secret ID / env var name holding the bot token. */
  botTokenSecretId: z.string().default('TELEGRAM_BOT_TOKEN'),
  chatId: z.string().default(''),
})

export const configSchema = z.object({
  schedule: z.string().default(DEFAULT_SCHEDULE),
  breaker: breakerConfigSchema,
  /** Optional allowlist of guardian addresses the switch may act on ([] = all). */
  guardianAllowlist: z.array(z.string()).default([]),
  onchain: onchainConfigSchema.default({}),
  telegram: telegramConfigSchema.default({}),
})

export type DeadmanConfig = z.infer<typeof configSchema>
export type OnchainConfig = z.infer<typeof onchainConfigSchema>
export type TelegramConfig = z.infer<typeof telegramConfigSchema>

// ---------------------------------------------------------------------------
// Heartbeat status (frozen breaker surface, SPEC-P14 §5)
// ---------------------------------------------------------------------------

export const guardianStatusSchema = z.object({
  address: z.string(),
  last_ts_ms: z.number(),
  age_secs: z.number(),
  max_tier: z.number(),
  stale: z.boolean(),
  critical: z.boolean(),
  armed: z.boolean().optional(),
})

export const heartbeatStatusSchema = z.object({
  generated_at_ms: z.number(),
  guardians: z.array(guardianStatusSchema),
})

export type GuardianStatus = z.infer<typeof guardianStatusSchema>
export type HeartbeatStatus = z.infer<typeof heartbeatStatusSchema>

export function parseHeartbeatStatus(raw: string): HeartbeatStatus {
  return heartbeatStatusSchema.parse(JSON.parse(raw) as unknown)
}

/** Fire condition (SPEC §6): any guardian stale && critical (plus allowlist, if set). */
export function selectFiring(status: HeartbeatStatus, allowlist: string[] = []): GuardianStatus[] {
  const allow = new Set(allowlist.map((a) => a.toLowerCase()))
  return status.guardians
    .filter((g) => g.stale && g.critical)
    .filter((g) => allow.size === 0 || allow.has(g.address.toLowerCase()))
    .sort((a, b) => b.age_secs - a.age_secs || a.address.localeCompare(b.address))
}

// ---------------------------------------------------------------------------
// Trigger body + HMAC signature (frozen, SPEC-P14 §5)
// ---------------------------------------------------------------------------

/**
 * Body bytes exactly as frozen in SPEC-P14 §5 (key order matters: it is signed).
 * `{"guardian":"0x…","reason":"…","requested_at_ms":n}`
 */
export function buildTriggerBody(guardian: string, reason: string, requestedAtMs: number): string {
  return JSON.stringify({ guardian, reason, requested_at_ms: requestedAtMs })
}

/** `sha256=<hex HMAC_SHA256(secret, raw request body bytes)>` (SPEC-P14 §5). */
export function signTriggerBody(secret: string, body: string): string {
  const mac = hmac(sha256, new TextEncoder().encode(secret), new TextEncoder().encode(body))
  return `sha256=${bytesToHex(mac)}`
}

export function triggerHeaders(signature: string): Record<string, string> {
  return {
    'Content-Type': 'application/json',
    'X-Breaker-Signature': signature,
  }
}

// ---------------------------------------------------------------------------
// Direct chain-read leg (heartbeat age straight from the anchor contract)
// ---------------------------------------------------------------------------

export function toHexQuantity(n: number): string {
  return `0x${n.toString(16)}`
}

export function parseHexQuantity(value: unknown): number | null {
  if (typeof value === 'number' && Number.isInteger(value) && value >= 0) return value
  if (typeof value === 'string' && /^0x[0-9a-fA-F]+$/.test(value)) return Number.parseInt(value, 16)
  return null
}

/** JSON-RPC body: current head block number. */
export function rpcBlockNumberBody(id = 1): string {
  return JSON.stringify({ jsonrpc: '2.0', id, method: 'eth_blockNumber', params: [] })
}

/** JSON-RPC body: Heartbeat logs for the anchor in [fromBlock, toBlock]. */
export function rpcHeartbeatLogsBody(anchorAddress: string, fromBlock: number, toBlock: number | 'latest', id = 2): string {
  return JSON.stringify({
    jsonrpc: '2.0',
    id,
    method: 'eth_getLogs',
    params: [
      {
        address: anchorAddress,
        topics: [HEARTBEAT_TOPIC0],
        fromBlock: toHexQuantity(fromBlock),
        toBlock: toBlock === 'latest' ? 'latest' : toHexQuantity(toBlock),
      },
    ],
  })
}

/** JSON-RPC body: block header for a given block (timestamp source). */
export function rpcBlockBody(block: number | 'latest', id = 3): string {
  return JSON.stringify({
    jsonrpc: '2.0',
    id,
    method: 'eth_getBlockByNumber',
    params: [block === 'latest' ? 'latest' : toHexQuantity(block), false],
  })
}

/** Extract the JSON-RPC `result` field or throw with the JSON-RPC error text. */
export function parseJsonRpcResult(raw: string): unknown {
  const envelope = JSON.parse(raw) as { result?: unknown; error?: { message?: string } }
  if (envelope.error) {
    throw new Error(`JSON-RPC error: ${envelope.error.message ?? 'unknown'}`)
  }
  if (envelope.result === undefined) {
    throw new Error('JSON-RPC response missing result')
  }
  return envelope.result
}

export type HeartbeatLog = { blockNumber: number; guardian: string }

/**
 * Keep only Heartbeat logs whose (unindexed) first data word matches `guardian`.
 * Layout: data = guardian(32) | riskStateHash(32) | openPositions(32) | maxTier(32).
 */
export function parseHeartbeatLogs(result: unknown, guardian: string): HeartbeatLog[] {
  if (!Array.isArray(result)) return []
  const want = guardian.toLowerCase().replace(/^0x/, '')
  const out: HeartbeatLog[] = []
  for (const entry of result) {
    const log = entry as { blockNumber?: unknown; data?: unknown }
    const blockNumber = parseHexQuantity(log.blockNumber)
    if (blockNumber === null || typeof log.data !== 'string') continue
    const data = log.data.startsWith('0x') ? log.data.slice(2) : log.data
    if (data.length < 64) continue
    const guardianWord = data.slice(24, 64) // last 20 bytes of word0
    if (guardianWord.toLowerCase() !== want) continue
    out.push({ blockNumber, guardian })
  }
  return out.sort((a, b) => a.blockNumber - b.blockNumber)
}

export type OnchainProbe = {
  heartbeat_count: number
  last_heartbeat_block: number | null
  onchain_age_secs: number | null
}

export function emptyProbe(): OnchainProbe {
  return { heartbeat_count: 0, last_heartbeat_block: null, onchain_age_secs: null }
}

// ---------------------------------------------------------------------------
// Telegram alert (optional step, SPEC §6)
// ---------------------------------------------------------------------------

export function buildTelegramRequest(botToken: string, chatId: string, text: string): { url: string; body: string } {
  return {
    url: `https://api.telegram.org/bot${botToken}/sendMessage`,
    body: JSON.stringify({ chat_id: chatId, text, disable_web_page_preview: true }),
  }
}

export function buildAlertText(guardian: string, ageSecs: number, maxTier: number, triggerStatus: number | null, onchainAgeSecs: number | null): string {
  const chainPart = onchainAgeSecs === null ? 'chain-read: n/a' : `chain-read age: ${onchainAgeSecs}s`
  const triggerPart = triggerStatus === null ? 'trigger: not posted' : `trigger: HTTP ${triggerStatus}`
  return (
    `SENTINEL CRE dead-man's-switch: Sentinel unresponsive — guardian ${guardian} ` +
    `heartbeat age ${ageSecs}s (tier ${maxTier}); ${triggerPart}; ${chainPart}`
  )
}

// ---------------------------------------------------------------------------
// The sweep: the frozen trigger -> step -> target sequence (shared by both runtimes)
// ---------------------------------------------------------------------------

export type HttpOutcome = { statusCode: number; ok: boolean; body: string }

export type SweepTransport = {
  /** GET a URL, return the raw response body. */
  fetchStatus(url: string): Promise<string>
  /** POST a JSON-RPC body, return the raw response body (chain-read leg). */
  rpcCall(rpcUrl: string, body: string): Promise<string>
  /** POST the trigger body with the HMAC header; `body` is the exact signed string. */
  postTrigger(url: string, body: string, signature: string): Promise<HttpOutcome>
  /** Optional best-effort Telegram POST. */
  postTelegram?(url: string, body: string): Promise<HttpOutcome>
}

export type SweepLogger = (line: string) => void

/** Secret values resolved by the caller (Vault DON in CRE, env vars in the runner). */
export type SweepSecrets = {
  /** The breaker HMAC key (secret id `breaker.armSecretId`). */
  armSecret: string
  /** Telegram bot token, when alerts are enabled. */
  telegramToken?: string
}

export type SweepDecision = {
  guardian: string
  age_secs: number
  max_tier: number
  onchain_age_secs: number | null
  fire: boolean
  action: 'trigger-posted' | 'trigger-rejected' | 'trigger-error' | 'skipped-onchain-gate'
  trigger_status: number | null
  trigger_body: string
  signature: string
  telegram: 'sent' | 'failed' | 'disabled' | null
}

export type SweepResult = {
  fired: SweepDecision[]
  healthy_guardians: number
  total_guardians: number
  status_generated_at_ms: number
  onchain: OnchainProbe & { mode: string; error?: string }
}

function joinUrl(baseUrl: string, path: string): string {
  return `${baseUrl.replace(/\/+$/, '')}${path.startsWith('/') ? path : `/${path}`}`
}

/** Read the on-chain heartbeat age with the transport's JSON-RPC leg. */
async function readOnchainProbe(
  transport: SweepTransport,
  cfg: OnchainConfig,
  guardian: string,
  log: SweepLogger,
): Promise<OnchainProbe> {
  const latestHex = await transport.rpcCall(cfg.rpcUrl, rpcBlockNumberBody())
  const latestBlock = parseHexQuantity(parseJsonRpcResult(latestHex))
  if (latestBlock === null) throw new Error('eth_blockNumber returned a non-quantity')
  const fromBlock = Math.max(0, latestBlock - cfg.lookbackBlocks)

  const logsRaw = await transport.rpcCall(cfg.rpcUrl, rpcHeartbeatLogsBody(cfg.anchorAddress, fromBlock, 'latest'))
  const beats = parseHeartbeatLogs(parseJsonRpcResult(logsRaw), guardian)
  log(`[onchain] anchor=${cfg.anchorAddress} head=${latestBlock} window=[${fromBlock}..latest] beats-for-${guardian}=${beats.length}`)
  if (beats.length === 0) {
    return { heartbeat_count: 0, last_heartbeat_block: null, onchain_age_secs: null }
  }

  const lastBeat = beats[beats.length - 1]!
  const latestBlockRaw = await transport.rpcCall(cfg.rpcUrl, rpcBlockBody('latest'))
  const latestTs = parseHexQuantity((parseJsonRpcResult(latestBlockRaw) as { timestamp?: unknown }).timestamp)
  let eventTs = latestTs
  if (lastBeat.blockNumber !== latestBlock) {
    const eventBlockRaw = await transport.rpcCall(cfg.rpcUrl, rpcBlockBody(lastBeat.blockNumber))
    eventTs = parseHexQuantity((parseJsonRpcResult(eventBlockRaw) as { timestamp?: unknown }).timestamp)
  }
  const ageSecs = latestTs !== null && eventTs !== null ? Math.max(0, latestTs - eventTs) : null
  log(`[onchain] last Heartbeat block=${lastBeat.blockNumber} age=${ageSecs ?? 'n/a'}s`)
  return { heartbeat_count: beats.length, last_heartbeat_block: lastBeat.blockNumber, onchain_age_secs: ageSecs }
}

/**
 * Execute one dead-man's-switch sweep. Both runtimes call this with their own
 * `SweepTransport`, so the decision/signature/alert logic is literally the same
 * code in simulation and in the fallback runner.
 *
 * @param nowMs wall-clock (DON time in CRE; local time in the runner) in ms.
 */
export async function runSweep(
  config: DeadmanConfig,
  transport: SweepTransport,
  secrets: SweepSecrets,
  nowMs: number,
  log: SweepLogger,
): Promise<SweepResult> {
  const statusUrl = joinUrl(config.breaker.baseUrl, config.breaker.statusPath)
  log(`[step 1/4] GET ${statusUrl}`)
  const status = parseHeartbeatStatus(await transport.fetchStatus(statusUrl))
  const firing = selectFiring(status, config.guardianAllowlist)
  log(
    `[step 1/4] status @${status.generated_at_ms}: ${status.guardians.length} guardian(s), ` +
      `${firing.length} stale && critical -> fire`,
  )

  const result: SweepResult = {
    fired: [],
    healthy_guardians: status.guardians.length - firing.length,
    total_guardians: status.guardians.length,
    status_generated_at_ms: status.generated_at_ms,
    onchain: { ...emptyProbe(), mode: config.onchain.mode },
  }

  if (firing.length === 0) {
    log('[done] no guardian stale && critical — nothing to do')
    return result
  }

  for (const guardian of firing) {
    log(`[fire] guardian=${guardian.address} age=${guardian.age_secs}s tier=${guardian.max_tier} armed=${String(guardian.armed ?? 'n/a')}`)

    // Step 2/4 — optional direct chain read (corroboration from the anchor contract).
    let onchainAgeSecs: number | null = null
    if (config.onchain.mode !== 'off') {
      log(`[step 2/4] chain-read leg (${config.onchain.mode}) via ${config.onchain.rpcUrl}`)
      try {
        const probe = await readOnchainProbe(transport, config.onchain, guardian.address, log)
        result.onchain = { ...probe, mode: config.onchain.mode }
        onchainAgeSecs = probe.onchain_age_secs
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err)
        result.onchain = { ...emptyProbe(), mode: config.onchain.mode, error: message }
        log(`[step 2/4] chain-read failed (continuing per mode=${config.onchain.mode}): ${message}`)
      }
    } else {
      log('[step 2/4] chain-read leg disabled (mode=off)')
    }

    const corroborated = onchainAgeSecs !== null && onchainAgeSecs > config.onchain.maxAgeSecs
    if (config.onchain.mode === 'require' && !corroborated) {
      log(`[gate] on-chain staleness NOT corroborated (age=${onchainAgeSecs ?? 'n/a'}s, need >${config.onchain.maxAgeSecs}s) — skipping trigger`)
      result.fired.push({
        guardian: guardian.address,
        age_secs: guardian.age_secs,
        max_tier: guardian.max_tier,
        onchain_age_secs: onchainAgeSecs,
        fire: false,
        action: 'skipped-onchain-gate',
        trigger_status: null,
        trigger_body: '',
        signature: '',
        telegram: null,
      })
      continue
    }

    // Step 3/4 — signed trigger POST.
    const body = buildTriggerBody(guardian.address, config.breaker.reason, nowMs)
    const signature = signTriggerBody(secrets.armSecret, body)
    const triggerUrl = joinUrl(config.breaker.baseUrl, config.breaker.triggerPath)
    log(`[step 3/4] POST ${triggerUrl} (X-Breaker-Signature ${signature.slice(0, 21)}…)`)
    log(`[step 3/4] body: ${body}`)
    let outcome: HttpOutcome
    let action: SweepDecision['action']
    try {
      outcome = await transport.postTrigger(triggerUrl, body, signature)
      action = outcome.ok ? 'trigger-posted' : 'trigger-rejected'
      log(`[step 3/4] breaker answered HTTP ${outcome.statusCode}: ${outcome.body.slice(0, 200)}`)
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err)
      outcome = { statusCode: 0, ok: false, body: message }
      action = 'trigger-error'
      log(`[step 3/4] trigger POST failed: ${message}`)
    }

    // Step 4/4 — optional Telegram alert (best effort, never fails the sweep).
    let telegram: SweepDecision['telegram'] = null
    if (config.telegram.enabled && config.telegram.chatId) {
      telegram = 'failed'
      const token = secrets.telegramToken
      if (token) {
        try {
          const text = buildAlertText(guardian.address, guardian.age_secs, guardian.max_tier, outcome.ok ? outcome.statusCode : null, onchainAgeSecs)
          const req = buildTelegramRequest(token, config.telegram.chatId, text)
          const resp = await transport.postTelegram?.(req.url, req.body)
          telegram = resp?.ok ? 'sent' : 'failed'
          log(`[step 4/4] telegram: HTTP ${resp?.statusCode ?? 'n/a'}`)
        } catch (err) {
          log(`[step 4/4] telegram failed: ${err instanceof Error ? err.message : String(err)}`)
        }
      } else {
        log('[step 4/4] telegram enabled but no bot token available')
      }
    } else {
      telegram = 'disabled'
      log('[step 4/4] telegram disabled')
    }

    result.fired.push({
      guardian: guardian.address,
      age_secs: guardian.age_secs,
      max_tier: guardian.max_tier,
      onchain_age_secs: onchainAgeSecs,
      fire: action === 'trigger-posted',
      action,
      trigger_status: outcome.statusCode || null,
      trigger_body: body,
      signature,
      telegram,
    })
  }

  return result
}
