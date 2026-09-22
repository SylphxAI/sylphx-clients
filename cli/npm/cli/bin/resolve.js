// Resolves the native `sylphx` binary: SYLPHX_CLI_BINARY, else the platform
// package npm installed as an optional dependency (esbuild's pattern).

import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import { dirname, join } from 'node:path'

const PLATFORMS = {
	'darwin arm64': '@sylphx/cli-darwin-arm64',
	'darwin x64': '@sylphx/cli-darwin-x64',
	'linux arm64': '@sylphx/cli-linux-arm64',
	'linux x64': '@sylphx/cli-linux-x64',
}

export function binaryPath() {
	const override = process.env.SYLPHX_CLI_BINARY
	if (override) return override
	const pkg = PLATFORMS[`${process.platform} ${process.arch}`]
	if (pkg === undefined) {
		throw new Error(
			`@sylphx/cli has no prebuilt binary for ${process.platform} ${process.arch}; ` +
				'build it with `cargo install sylphx-cli`, then set SYLPHX_CLI_BINARY.',
		)
	}
	const require = createRequire(import.meta.url)
	let root
	try {
		root = dirname(require.resolve(`${pkg}/package.json`))
	} catch {
		throw new Error(`${pkg} is not installed; reinstall @sylphx/cli without --no-optional.`)
	}
	const bin = join(root, 'bin', 'sylphx')
	if (!existsSync(bin)) throw new Error(`${bin} is missing; reinstall ${pkg}.`)
	return bin
}
