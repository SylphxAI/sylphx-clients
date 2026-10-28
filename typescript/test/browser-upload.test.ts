import { describe, expect, test } from 'bun:test'
import {
	abortBrowserUpload,
	type BrowserUploadBackend,
	localUploadCheckpoint,
	MULTIPART_MIN_BYTES,
	type UploadCheckpoint,
	type UploadCheckpointStore,
	type UploadPart,
	uploadBrowserFile,
} from '../src/browser-upload.js'

function fixture() {
	let json: string | null = null
	const store: UploadCheckpointStore = {
		async load() {
			return json ? JSON.parse(json) : null
		},
		async save(value) {
			json = JSON.stringify(value)
		},
		async clear() {
			json = null
		},
	}
	const bodies = new Map<number, Blob>()
	const parts = new Map<number, UploadPart>()
	const sent: number[] = []
	let single: Blob | undefined
	let complete: Parameters<BrowserUploadBackend['complete']>[0] | undefined
	let failPart = 0
	let status = 200
	let missingEtag = false
	let aborted = false
	let creates = 0
	const backend: BrowserUploadBackend = {
		async put() {
			return { url: 'https://upload.test/0' }
		},
		async create() {
			creates++
			return 'upload-1'
		},
		async listParts() {
			return [...parts.values()]
		},
		async part({ partNumber }) {
			return { url: `https://upload.test/${partNumber}`, headers: { 'x-test': 'signed' } }
		},
		async complete(input) {
			complete = input
		},
		async abort() {
			aborted = true
		},
	}
	const xhr = () => {
		let number = 0
		const mock = {
			upload: {} as { onprogress?: (event: { loaded: number }) => void },
			status,
			onload: () => {},
			onerror: () => {},
			onabort: () => {},
			open(_method: string, url: string) {
				number = Number(new URL(url).pathname.slice(1))
			},
			setRequestHeader() {},
			getResponseHeader() {
				return missingEtag ? null : `"part-${number}"`
			},
			abort() {
				mock.onabort()
			},
			async send(body: Blob) {
				sent.push(number)
				mock.upload.onprogress?.({ loaded: Math.floor(body.size / 2) })
				if (number === failPart && failPart !== 0) {
					mock.onerror()
					return
				}
				if (number === 0) single = body
				else {
					bodies.set(number, body)
					parts.set(number, { partNumber: number, size: body.size, etag: `"part-${number}"` })
				}
				mock.onload()
			},
		}
		return mock as unknown as XMLHttpRequest
	}
	return {
		store,
		backend,
		xhr,
		sent,
		parts,
		bodies,
		set fail(value: number) {
			failPart = value
		},
		set status(value: number) {
			status = value
		},
		set missingEtag(value: boolean) {
			missingEtag = value
		},
		get single() {
			return single
		},
		get complete() {
			return complete
		},
		get aborted() {
			return aborted
		},
		get creates() {
			return creates
		},
	}
}

async function hash(blob: Blob) {
	return Buffer.from(await crypto.subtle.digest('SHA-256', await blob.arrayBuffer())).toString(
		'hex',
	)
}

const options = (f: ReturnType<typeof fixture>, file: Blob) => ({
	key: 'folder/file.bin',
	file,
	backend: f.backend,
	checkpoint: f.store,
	xhr: f.xhr,
})

describe('browser upload', () => {
	test('single PUT through exactly 5 MiB, empty files, and real byte progress', async () => {
		for (const size of [0, MULTIPART_MIN_BYTES]) {
			const f = fixture()
			const file = new Blob([new Uint8Array(size).fill(42)])
			const progress: number[] = []
			const result = await uploadBrowserFile({
				...options(f, file),
				onProgress: (p) => progress.push(p.loaded),
			})
			expect(f.sent).toEqual([0])
			expect(f.creates).toBe(0)
			expect(result.sha256).toBe(await hash(f.single!))
			expect(progress).toContain(Math.floor(size / 2))
			expect(progress.at(-1)).toBe(size)
		}
	})

	test('reload resumes confirmed parts and recomputes full-file checksum', async () => {
		const f = fixture()
		const file = new Blob([
			new Uint8Array(MULTIPART_MIN_BYTES).fill(17),
			new Uint8Array(193).fill(23),
		])
		f.fail = 2
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow('network')
		const saved = await f.store.load()
		expect(saved?.parts).toHaveLength(1)
		// Simulate reload after a successful PUT but before saving its receipt.
		await f.store.save({ ...saved!, parts: [] })
		f.fail = 0
		const progress: { loaded: number; resumed: boolean }[] = []
		const result = await uploadBrowserFile({
			...options(f, file),
			onProgress: (p) => progress.push(p),
		})
		expect(f.sent).toEqual([1, 2, 2])
		expect(f.creates).toBe(1)
		expect(progress.some((p) => p.resumed && p.loaded === MULTIPART_MIN_BYTES)).toBe(true)
		expect(progress.every((p, i) => i === 0 || p.loaded >= progress[i - 1]!.loaded)).toBe(true)
		const downloaded = new Blob(
			[...f.bodies.entries()].sort(([a], [b]) => a - b).map(([, body]) => body),
		)
		expect(await hash(downloaded)).toBe(await hash(file))
		expect(result.sha256).toBe(await hash(downloaded))
		expect(f.complete?.sha256).toBe(result.sha256)
		expect(f.complete?.parts.map((p) => p.partNumber)).toEqual([1, 2])
		expect(await f.store.load()).toBeNull()
	})

	test('same size but different source is rejected before resuming', async () => {
		const f = fixture()
		const file = new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)])
		f.fail = 2
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow()
		await expect(
			uploadBrowserFile(options(f, new Blob([new Uint8Array(file.size).fill(1)]))),
		).rejects.toThrow('does not match')
		expect(f.sent).toEqual([1, 2])
	})

	test('server listing, not stale local receipts, determines what needs retransmission', async () => {
		const f = fixture()
		const file = new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)])
		f.fail = 2
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow()
		f.parts.clear()
		f.fail = 0
		await uploadBrowserFile(options(f, file))
		expect(f.sent).toEqual([1, 2, 1, 2])
	})

	test('rejects corrupt listings, invalid part size, and HTTP failure', async () => {
		const f = fixture()
		const file = new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)])
		await expect(uploadBrowserFile({ ...options(f, file), partSize: 12 })).rejects.toThrow(
			'part size',
		)
		f.fail = 2
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow()
		f.parts.set(1, { partNumber: 1, etag: 'bad', size: 4 })
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow('listing')
		const other = fixture()
		other.status = 403
		await expect(uploadBrowserFile(options(other, new Blob(['x'])))).rejects.toThrow('403')
	})

	test('missing CORS ETag fails safely and explicit abort clears state', async () => {
		const f = fixture()
		f.missingEtag = true
		await expect(
			uploadBrowserFile(options(f, new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)]))),
		).rejects.toThrow('ETag')
		expect(await f.store.load()).not.toBeNull()
		await abortBrowserUpload(f.backend, f.store)
		expect(f.aborted).toBe(true)
		expect(await f.store.load()).toBeNull()
	})

	test('in-flight cancellation rejects and preserves resumable multipart state', async () => {
		const f = fixture()
		const controller = new AbortController()
		await expect(
			uploadBrowserFile({
				...options(f, new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)])),
				signal: controller.signal,
				onProgress: ({ loaded }) => {
					if (loaded > 0) controller.abort()
				},
			}),
		).rejects.toMatchObject({ name: 'AbortError' })
		expect(await f.store.load()).not.toBeNull()
		expect(f.complete).toBeUndefined()
	})

	test('completion failure keeps the checkpoint and later completes without resending parts', async () => {
		const f = fixture()
		const complete = f.backend.complete
		f.backend.complete = async () => {
			throw new Error('completion failed')
		}
		const file = new Blob([new Uint8Array(MULTIPART_MIN_BYTES + 1)])
		await expect(uploadBrowserFile(options(f, file))).rejects.toThrow('completion failed')
		expect((await f.store.load())?.parts).toHaveLength(2)
		f.backend.complete = complete
		await uploadBrowserFile(options(f, file))
		expect(f.sent).toEqual([1, 2])
		expect(await f.store.load()).toBeNull()
	})

	test('pre-aborted signals transfer nothing', async () => {
		const f = fixture()
		await expect(
			uploadBrowserFile({ ...options(f, new Blob(['x'])), signal: AbortSignal.abort() }),
		).rejects.toMatchObject({ name: 'AbortError' })
		expect(f.sent).toEqual([])
	})

	test('local storage checkpoint survives a new store instance without signed URLs', async () => {
		const values = new Map<string, string>()
		const storage: Storage = {
			get length() {
				return values.size
			},
			clear: () => {
				values.clear()
			},
			key: (index: number) => [...values.keys()][index] ?? null,
			getItem: (id: string) => values.get(id) ?? null,
			setItem: (id: string, value: string) => {
				values.set(id, value)
			},
			removeItem: (id: string) => {
				values.delete(id)
			},
		}
		const value: UploadCheckpoint = {
			version: 1,
			key: 'x',
			size: 1,
			sha256: 'abc',
			uploadId: 'id',
			partSize: MULTIPART_MIN_BYTES,
			parts: [],
		}
		await localUploadCheckpoint('user/bucket/x', storage).save(value)
		expect(await localUploadCheckpoint('user/bucket/x', storage).load()).toEqual(value)
		await localUploadCheckpoint('user/bucket/x', storage).clear()
		expect(await localUploadCheckpoint('user/bucket/x', storage).load()).toBeNull()
	})
})
