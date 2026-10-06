#!/usr/bin/env node
/**
 * LOCAL FALLBACK RUNNER — emulates `cre workflow simulate` for the Sentinel
 * dead-man's-switch when the CRE CLI cannot run headless (no CREATE account /
 * login; docs.chain.link/cre → "cre workflow simulate ... Authentication
 * required"; tracked as STUB-03).
 *
 * It executes the SAME definition as the CRE workflow:
 *   cron trigger -> GET /api/heartbeat-status -> condition (stale && critical)
 *   -> [chain-read leg] -> signed POST /breaker/trigger -> [telegram step]
 *
 * by importing `src/core.ts` — the exact module the CRE handler
 * (`src/workflow.ts`) calls. Only the transport differs:
 *   CRE runtime : @chainlink/cre-sdk HTTPClient + Vault-DON secrets + DON time
 *   this runner : Node fetch() + env-var secrets + local time
 *
 * Usage:
 *   node tools/local-runner.ts [--config config.local.json] [--expect fire|healthy|any]
 *                              [--now-ms N] [--self-test]
 *
 * Exit codes: 0 success (expectation met), 1 sweep/self-test failure or
 * expectation mismatch, 2 usage/config error.
 */
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import {
  buildTriggerBody,
  configSchema,
  runSweep,
  signTriggerBody,
  triggerHeaders,
  type DeadmanConfig,
  type HttpOutcome,
  type SweepTransport,
} from '../src/core.ts'

class UsageError extends Error {}

function argValue(name: string): string | undefined {
  const idx = process.argv.indexOf(name)
  if (idx === -1) return undefined
  const value = process.argv[idx + 1]
  if (!value) throw new UsageError(`missing value for ${name}`)
  return value
}

function hasFlag(name: string): boolean {
  return process.argv.includes(name)
}

function loadConfig(path: string): DeadmanConfig {
  const raw = JSON.parse(readFileSync(path, 'utf8')) as unknown
  return configSchema.parse(raw)
}

function selfTest(): boolean {
  const body = buildTriggerBody(
    '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266',
    'spec-vector',
    1791200000000,
  )
  const signature = signTriggerBody('spec-test-secret', body)
  const wantBody =
    '{"guardian":"0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266","reason":"spec-vector","requested_at_ms":1791200000000}'
  const wantSig = 'sha256=af9a4a973355861bf340577feca0cd2079325849014faaccc98b22d115d5da04'
  const okBody = body === wantBody
  const okSig = signature === wantSig
  process.stdout.write(`[self-test] frozen SPEC-P14 §5 HMAC vector\n`)
  process.stdout.write(`[self-test] body      match=${okBody}\n`)
  process.stdout.write(`[self-test] signature match=${okSig} (${signature})\n`)
  return okBody && okSig
}

function makeTransport(): SweepTransport {
  return {
    async fetchStatus(url: string): Promise<string> {
      const resp = await fetch(url, { headers: { Accept: 'application/json' } })
      const body = await resp.text()
      if (!resp.ok) throw new Error(`heartbeat-status HTTP ${resp.status}: ${body.slice(0, 160)}`)
      return body
    },
    async rpcCall(rpcUrl: string, body: string): Promise<string> {
      const resp = await fetch(rpcUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
      })
      const text = await resp.text()
      if (!resp.ok) throw new Error(`JSON-RPC HTTP ${resp.status}: ${text.slice(0, 160)}`)
      return text
    },
    async postTrigger(url: string, body: string, signature: string): Promise<HttpOutcome> {
      const resp = await fetch(url, {
        method: 'POST',
        headers: triggerHeaders(signature),
        body,
      })
      return { statusCode: resp.status, ok: resp.ok, body: await resp.text() }
    },
    async postTelegram(url: string, body: string): Promise<HttpOutcome> {
      const resp = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body,
      })
      return { statusCode: resp.status, ok: resp.ok, body: await resp.text() }
    },
  }
}

async function main(): Promise<number> {
  const baseDir = resolve(import.meta.dirname, '..')

  if (hasFlag('--self-test')) {
    return selfTest() ? 0 : 1
  }

  const configPath = resolve(baseDir, argValue('--config') ?? 'config.local.json')
  const expect = argValue('--expect') ?? 'any'
  if (!['fire', 'healthy', 'any'].includes(expect)) {
    throw new UsageError(`--expect must be fire|healthy|any, got ${expect}`)
  }
  const config = loadConfig(configPath)
  const nowMs = argValue('--now-ms') ? Number(argValue('--now-ms')) : Date.now()

  const secretId = config.breaker.armSecretId
  const armSecret = process.env[secretId] ?? process.env.BREAKER_ARM_SECRET
  if (!armSecret) {
    process.stderr.write(`[runner] no HMAC secret: set ${secretId} (or BREAKER_ARM_SECRET) in the environment\n`)
    return 2
  }
  const telegramToken = process.env[config.telegram.botTokenSecretId]

  process.stdout.write('════════════════════════════════════════════════════════════════════\n')
  process.stdout.write(' LOCAL SIMULATION (fallback) — NOT the CRE runtime\n')
  process.stdout.write(' `cre workflow simulate` requires a logged-in CRE account (STUB-03);\n')
  process.stdout.write(' this runner executes the same core.ts definition with a Node transport.\n')
  process.stdout.write('════════════════════════════════════════════════════════════════════\n')
  process.stdout.write(`[runner] config=${configPath}\n`)
  process.stdout.write(`[runner] mode=${config.onchain.mode} breaker=${config.breaker.baseUrl} secret-id=${secretId}\n`)
  process.stdout.write(`[runner] TRIGGER (emulated cron tick): schedule=${config.schedule} now_ms=${nowMs}\n`)

  const result = await runSweep(config, makeTransport(), { armSecret, telegramToken }, nowMs, (line) => {
    process.stdout.write(`[runner] ${line}\n`)
  })

  const fired = result.fired.filter((d) => d.fire)
  process.stdout.write(`[runner] RUNNER_SUMMARY ${JSON.stringify(result)}\n`)
  const observed = fired.length > 0 ? 'fire' : 'healthy'
  process.stdout.write(`[runner] outcome=${observed} fired=${fired.length} healthy=${result.healthy_guardians}\n`)

  if (expect !== 'any' && expect !== observed) {
    process.stderr.write(`[runner] expectation mismatch: wanted ${expect}, observed ${observed}\n`)
    return 1
  }
  return 0
}

main()
  .then((code) => process.exit(code))
  .catch((err: unknown) => {
    const message = err instanceof Error ? err.message : String(err)
    process.stderr.write(`[runner] FAILED: ${message}\n`)
    if (err instanceof UsageError) process.exit(2)
    process.exit(1)
  })
