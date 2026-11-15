#!/usr/bin/env bun
// Rebuild the desktop app and relaunch it without `cargo tauri dev`.
//
// `cargo tauri dev` rebuilds the .app bundle, builds the CEF helper, and
// refreshes the crates.io index on every start, which costs about nine seconds
// more than this loop. It also runs `beforeDevCommand`, which rebuilds the
// canvas wasm when the network is unavailable.
//
// The Next dev server on port 3000 serves the UI, so a frontend-only change
// needs no rebuild and no restart at all. Use this script when Rust changed.
//
//   bun scripts/dev-reload.ts              build, then relaunch
//   bun scripts/dev-reload.ts --no-build   relaunch the current binary
//   bun scripts/dev-reload.ts --timeout 90 readiness budget in seconds

import { existsSync } from 'node:fs'
import { copyFile } from 'node:fs/promises'
import path from 'node:path'
import { parseArgs } from 'node:util'

const DEVTOOLS_PORT = 4000
const DEFAULT_TIMEOUT_SECONDS = 120

type Options = {
  build: boolean
  timeoutMs: number
}

function parseOptions(): Options {
  const { values } = parseArgs({
    options: {
      'no-build': { type: 'boolean', default: false },
      timeout: { type: 'string' },
    },
  })
  const timeout = Number(values.timeout ?? DEFAULT_TIMEOUT_SECONDS)
  if (!Number.isFinite(timeout) || timeout <= 0) {
    throw new Error(`--timeout expects a positive number of seconds, got ${values.timeout}`)
  }
  return { build: !values['no-build'], timeoutMs: timeout * 1000 }
}

const repoRoot = path.resolve(import.meta.dir, '..')
const hostBinary = path.join(repoRoot, 'target/debug/koharu')
const bundle = path.join(repoRoot, 'target/debug/bundle/macos/koharu.app')
const bundledBinary = path.join(bundle, 'Contents/MacOS/koharu')
const devtoolsVersion = `http://127.0.0.1:${DEVTOOLS_PORT}/json/version`

function run(command: string[]): void {
  const [program, ...args] = command
  if (program === undefined) throw new Error('empty command')
  const result = Bun.spawnSync({ cmd: [program, ...args], stdout: 'inherit', stderr: 'inherit' })
  if (result.exitCode !== 0) {
    throw new Error(`${command.join(' ')} exited with ${result.exitCode ?? 'a signal'}`)
  }
}

async function devtoolsReachable(): Promise<boolean> {
  try {
    const response = await fetch(devtoolsVersion, { signal: AbortSignal.timeout(2_000) })
    return response.ok
  } catch {
    return false
  }
}

async function waitForDevtools(timeoutMs: number): Promise<number> {
  const started = Date.now()
  while (Date.now() - started < timeoutMs) {
    if (await devtoolsReachable()) return Date.now() - started
    await Bun.sleep(500)
  }
  throw new Error(
    `the app did not expose its DevTools endpoint on port ${DEVTOOLS_PORT} within ${timeoutMs / 1000}s`,
  )
}

const options = parseOptions()

if (process.platform !== 'darwin') {
  throw new Error(
    'this script stages a macOS .app bundle; use `cargo tauri dev` on other platforms',
  )
}
if (!existsSync(bundle)) {
  throw new Error(
    `${path.relative(repoRoot, bundle)} is missing; run \`cargo tauri dev\` once to create it`,
  )
}

if (options.build) {
  const started = Date.now()
  run(['cargo', 'build', '-p', 'koharu'])
  console.log(`build: ${((Date.now() - started) / 1000).toFixed(1)}s`)
}

// An instance holding the project lock would be mistaken for the new one, so the
// old process has to be gone before the bundle is replaced.
Bun.spawnSync({ cmd: ['pkill', '-9', '-f', bundledBinary] })
while (Bun.spawnSync({ cmd: ['pgrep', '-f', `${bundledBinary}$`] }).exitCode === 0) {
  await Bun.sleep(200)
}

await copyFile(hostBinary, bundledBinary)
run(['open', bundle])

const ready = await waitForDevtools(options.timeoutMs)
console.log(`ready: ${(ready / 1000).toFixed(1)}s`)
