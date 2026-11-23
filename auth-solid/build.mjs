import { copyFile } from 'node:fs/promises'
import { build } from 'vite'
import solid from 'vite-plugin-solid'

const external = (id) => /^(?:solid-js|@sylphx\/auth)(?:\/|$)/.test(id)
for (const ssr of [false, true]) {
	const name = ssr ? 'server.js' : 'browser.js'
	await build({
		configFile: false,
		plugins: [solid({ ssr: true })],
		build: {
			lib: { entry: 'src/index.ts', formats: ['es'], fileName: () => name },
			ssr,
			target: 'es2022',
			outDir: 'dist',
			emptyOutDir: false,
			rollupOptions: { external, output: { entryFileNames: name } },
		},
	})
}
await copyFile('src/sign-in.css', 'dist/sign-in.css')
