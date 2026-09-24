#!/usr/bin/env node
// `npx @sylphx/mcp`: the Sylphx MCP server over stdio, which is `sylphx mcp`
// of the native CLI binary.

import { spawn } from 'node:child_process'
import { binaryPath } from '@sylphx/cli/resolve'

const args = process.argv.slice(2)
if (args.includes('--version') || args.includes('-V')) {
	// The server is the CLI's `sylphx mcp`; its version is the CLI's.
	args.splice(0, args.length, '--version')
}
let bin
try {
	bin = binaryPath()
} catch (error) {
	console.error(error.message)
	process.exit(1)
}
const child = spawn(bin, args[0] === '--version' ? args : ['mcp', ...args], { stdio: 'inherit' })
child.on('error', (error) => {
	console.error(`failed to run ${bin}: ${error.message}`)
	process.exit(1)
})
child.on('exit', (code, signal) => {
	if (signal) process.kill(process.pid, signal)
	else process.exit(code ?? 1)
})
