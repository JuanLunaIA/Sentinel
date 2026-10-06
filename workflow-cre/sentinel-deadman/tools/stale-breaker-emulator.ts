#!/usr/bin/env node
/**
 * EMULATED breaker endpoint — the SPEC-P14 §6 fallback input path.
 *
 * NOT the real breaker (crates/breaker). This is a tiny, dependency-free HTTP
 * server implementing exactly the two frozen surfaces of SPEC-P14 §5:
 *
 *   GET  /api/heartbeat-status  -> {"generated_at_ms":n,"guardians":[{...}]}
 *   POST /breaker/trigger       -> verifies `X-Breaker-Signature:
 *                                  sha256=<HMAC_SHA256(secret, raw body)>`
 *                                  (constant-time), 202/401/400/423 semantics
 *
 * It lets the CRE workflow simulation (or, in this environment, the local
 * fallback runner) be exercised against a *deterministic injected stale state*
 * without depending on the real breaker's timing.
 *
 * Environment:
 *   EMULATOR_PORT (9091) | EMULATOR_GUARDIAN (anvil key #0 address)
 *   EMULATOR_STALE (true) | EMULATOR_AGE_SECS (36) | EMULATOR_MAX_TIER (2)
 *   EMULATOR_HEARTBEAT_INTERVAL_SECS (8) | BREAKER_ARM_SECRET (demo-secret)
 */
import { createHmac, timingSafeEqual } from 'node:crypto'
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http'

const PORT = Number(process.env.EMULATOR_PORT ?? '9091')
const GUARDIAN = process.env.EMULATOR_GUARDIAN ?? '0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266'
const STALE = (process.env.EMULATOR_STALE ?? 'true') !== 'false'
const AGE_SECS = Number(process.env.EMULATOR_AGE_SECS ?? '36')
const MAX_TIER = Number(process.env.EMULATOR_MAX_TIER ?? '2')
const HEARTBEAT_INTERVAL_SECS = Number(process.env.EMULATOR_HEARTBEAT_INTERVAL_SECS ?? '8')
const SECRET = process.env.BREAKER_ARM_SECRET ?? 'demo-secret'

const firedEpochs = new Set<string>()

const log = (line: string): void => {
  process.stdout.write(`[emulator] ${line}\n`)
}

function readBody(req: IncomingMessage): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = []
    req.on('data', (chunk: Buffer) => chunks.push(chunk))
    req.on('end', () => resolve(Buffer.concat(chunks)))
    req.on('error', reject)
  })
}

function verifySignature(rawBody: Buffer, header: string | undefined): boolean {
  if (!header || !header.startsWith('sha256=')) return false
  const got = Buffer.from(header.slice('sha256='.length), 'hex')
  const want = createHmac('sha256', SECRET).update(rawBody).digest()
  return got.length === want.length && timingSafeEqual(got, want)
}

function heartbeatStatus(): string {
  const nowMs = Date.now()
  const ageSecs = STALE ? AGE_SECS : Math.floor(HEARTBEAT_INTERVAL_SECS / 2)
  return JSON.stringify({
    generated_at_ms: nowMs,
    guardians: [
      {
        address: GUARDIAN,
        last_ts_ms: nowMs - ageSecs * 1000,
        age_secs: ageSecs,
        max_tier: STALE ? MAX_TIER : 0,
        stale: STALE,
        critical: STALE && MAX_TIER >= 2,
        armed: false,
      },
    ],
  })
}

const server = createServer((req: IncomingMessage, res: ServerResponse) => {
  const url = req.url ?? '/'
  const method = req.method ?? 'GET'

  if (method === 'GET' && url === '/api/heartbeat-status') {
    const body = heartbeatStatus()
    log(`GET ${url} -> 200 stale=${STALE} age=${STALE ? AGE_SECS : HEARTBEAT_INTERVAL_SECS / 2}s tier=${STALE ? MAX_TIER : 0}`)
    res.writeHead(200, { 'Content-Type': 'application/json' })
    res.end(body)
    return
  }

  if (method === 'GET' && url === '/healthz') {
    res.writeHead(200, { 'Content-Type': 'application/json' })
    res.end(JSON.stringify({ ok: true, emulated: true }))
    return
  }

  if (method === 'POST' && url === '/breaker/trigger') {
    void readBody(req).then((raw) => {
      const signature = req.headers['x-breaker-signature']
      const sigHeader = Array.isArray(signature) ? signature[0] : signature
      if (!verifySignature(raw, sigHeader)) {
        log(`POST ${url} -> 401 bad signature (header=${sigHeader ?? 'missing'})`)
        res.writeHead(401, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ accepted: false, error: 'bad signature' }))
        return
      }
      let parsed: { guardian?: unknown; reason?: unknown; requested_at_ms?: unknown }
      try {
        parsed = JSON.parse(raw.toString('utf8')) as typeof parsed
      } catch {
        res.writeHead(400, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ accepted: false, error: 'bad body' }))
        return
      }
      if (typeof parsed.guardian !== 'string' || typeof parsed.reason !== 'string' || typeof parsed.requested_at_ms !== 'number') {
        log(`POST ${url} -> 400 bad body shape: ${raw.toString('utf8').slice(0, 160)}`)
        res.writeHead(400, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ accepted: false, error: 'bad body' }))
        return
      }
      if (!STALE) {
        log(`POST ${url} -> 423 guardian fresh`)
        res.writeHead(423, { 'Content-Type': 'application/json' })
        res.end(JSON.stringify({ accepted: false, error: 'guardian fresh' }))
        return
      }
      const epoch = 1
      const key = `${parsed.guardian}:${epoch}`
      const alreadyFired = firedEpochs.has(key)
      firedEpochs.add(key)
      log(
        `POST ${url} -> 202 signature OK guardian=${parsed.guardian} reason=${parsed.reason} ` +
          `requested_at_ms=${parsed.requested_at_ms} fired=${!alreadyFired}`,
      )
      res.writeHead(202, { 'Content-Type': 'application/json' })
      res.end(JSON.stringify({ accepted: true, epoch, fired: !alreadyFired }))
    })
    return
  }

  log(`${method} ${url} -> 404`)
  res.writeHead(404, { 'Content-Type': 'application/json' })
  res.end(JSON.stringify({ error: 'not found' }))
})

server.listen(PORT, '127.0.0.1', () => {
  log(`listening on http://127.0.0.1:${PORT} (emulated breaker; stale=${STALE} guardian=${GUARDIAN})`)
})

for (const signal of ['SIGINT', 'SIGTERM'] as const) {
  process.on(signal, () => {
    log(`received ${signal}, shutting down`)
    server.close(() => process.exit(0))
  })
}
