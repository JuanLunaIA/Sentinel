/**
 * Sentinel dead-man's-switch — CRE workflow entry point (SPEC-P14 §6).
 *
 * `cre workflow simulate .` / `cre workflow deploy` compile this file plus the
 * modules it imports into a WASM binary and run it:
 *   trigger: cron (every 2 min, from config)
 *   steps:   GET heartbeat-status -> condition -> [chain read] -> signed trigger POST -> [telegram]
 */
import { Runner } from '@chainlink/cre-sdk'
import { configSchema, initWorkflow } from './src/workflow.ts'

export async function main(): Promise<void> {
  const runner = await Runner.newRunner({ configSchema })
  await runner.run(initWorkflow)
}

main()
