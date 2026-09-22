#!/usr/bin/env node
// `npx @sylphx/mcp`: the Sylphx MCP server over stdio, which is `sylphx mcp`
// of the native CLI binary.

import { spawn } from 'node:child_process'
import { binaryPath } from '@sylphx/cli/resolve'

let bin
try {
	bin = binaryPath()
} catch (error) {
	console.error(error.message)
	process.exit(1)
}
const child = spawn(bin, ['mcp', ...process.argv.slice(2)], { stdio: 'inherit' })
child.on('error', (error) => {
	console.error(`failed to run ${bin}: ${error.message}`)
	process.exit(1)
})
child.on('exit', (code, signal) => {
	if (signal) process.kill(process.pid, signal)
	else process.exit(code ?? 1)
})
