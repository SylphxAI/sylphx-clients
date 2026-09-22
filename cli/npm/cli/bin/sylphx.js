#!/usr/bin/env node
// npm shim of the native `sylphx` binary: resolve it and run it.

import { spawn } from 'node:child_process'
import { binaryPath } from './resolve.js'

let bin
try {
	bin = binaryPath()
} catch (error) {
	console.error(error.message)
	process.exit(1)
}
const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' })
child.on('error', (error) => {
	console.error(`failed to run ${bin}: ${error.message}`)
	process.exit(1)
})
child.on('exit', (code, signal) => {
	if (signal) process.kill(process.pid, signal)
	else process.exit(code ?? 1)
})
