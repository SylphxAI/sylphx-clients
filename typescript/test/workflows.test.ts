// Sylphx Workflows interface: the five short forms (`sx.workflows.start`,
// `signal`, `result`, `cancel`, `schedule`) send the same wire call as their
// full-tree twins (docs/services/workflows/interface.md).

import { describe, expect, test } from 'bun:test'
import { type FetchLike, Sylphx } from '../src/index.js'

interface Sent {
	method: string
	url: URL
	body: unknown
}

function fake(body: unknown) {
	const sent: Sent[] = []
	const impl: FetchLike = async (input, init) => {
		sent.push({
			method: init?.method ?? 'GET',
			url: new URL(String(input)),
			body: init?.body == null ? undefined : JSON.parse(String(init.body)),
		})
		return new Response(JSON.stringify(body), {
			status: 200,
			headers: { 'sylphx-request-id': 'req_1' },
		})
	}
	const sx = new Sylphx({ apiKey: 'sylphx_sk_test', fetch: impl, baseUrl: 'https://api.test' })
	return { sx, sent }
}

const ENV = 'orgs/org_a/projects/prj_a/envs/production'
const WORKFLOW = `${ENV}/workflows/hire-screening`
const RUN = `${WORKFLOW}/runs/screen-8f2c`
const RUN_BODY = { name: RUN, state: 'running' }
const OPERATION = { name: 'operations/op_a', done: false }

describe('Sylphx Workflows short forms', () => {
	test('start posts a Run with its key and delay, like runs.create', async () => {
		const request = {
			parent: WORKFLOW,
			runId: 'screen-8f2c',
			run: { input: { applicantId: '8f2c' }, startDelay: '259200s' },
		}
		const short = fake(RUN_BODY)
		const run = await short.sx.workflows.start(request)
		const full = fake(RUN_BODY)
		await full.sx.workflows.runs.create(request)
		expect(short.sent[0]?.method).toBe('POST')
		expect(short.sent[0]?.url.pathname).toBe(`/v1/${WORKFLOW}/runs`)
		expect(short.sent[0]?.url.searchParams.get('run_id')).toBe('screen-8f2c')
		expect(short.sent[0]?.body).toEqual({
			input: { applicantId: '8f2c' },
			start_delay: '259200s',
		})
		expect(full.sent).toEqual(short.sent)
		expect(run.name).toBe(RUN)
	})

	test('signal, result and cancel address the Run like their twins', async () => {
		const pairs = [
			{
				short: (sx: Sylphx) =>
					sx.workflows.signal({ name: RUN, signal: 'approved', payload: { by: 'kim' } }),
				full: (sx: Sylphx) =>
					sx.workflows.runs.signal({ name: RUN, signal: 'approved', payload: { by: 'kim' } }),
				path: `/v1/${RUN}:signal`,
				body: { signal: 'approved', payload: { by: 'kim' } },
			},
			{
				short: (sx: Sylphx) => sx.workflows.result({ name: RUN, timeout: '30s' }),
				full: (sx: Sylphx) => sx.workflows.runs.wait({ name: RUN, timeout: '30s' }),
				path: `/v1/${RUN}:wait`,
				body: { timeout: '30s' },
			},
			{
				short: (sx: Sylphx) => sx.workflows.cancel({ name: RUN, reason: 'withdrawn' }),
				full: (sx: Sylphx) => sx.workflows.runs.cancel({ name: RUN, reason: 'withdrawn' }),
				path: `/v1/${RUN}:cancel`,
				body: { reason: 'withdrawn' },
			},
		]
		for (const p of pairs) {
			const short = fake(RUN_BODY)
			await p.short(short.sx)
			const full = fake(RUN_BODY)
			await p.full(full.sx)
			expect(short.sent[0]?.method).toBe('POST')
			expect(short.sent[0]?.url.pathname).toBe(p.path)
			expect(short.sent[0]?.body).toEqual(p.body)
			expect(full.sent).toEqual(short.sent)
		}
	})

	test('schedule is an upsert by name through schedules.update', async () => {
		const request = {
			schedule: {
				name: `${ENV}/schedules/nightly-report`,
				spec: {
					workflow: `${ENV}/workflows/report`,
					calendar: { cron: '0 3 * * *', timeZone: 'Europe/London' },
					overlapPolicy: 'skip',
				},
			},
			allowMissing: true,
		}
		const short = fake(OPERATION)
		await short.sx.workflows.schedule(request)
		const full = fake(OPERATION)
		await full.sx.workflows.schedules.update(request)
		expect(short.sent[0]?.method).toBe('PATCH')
		expect(short.sent[0]?.url.pathname).toBe(`/v1/${ENV}/schedules/nightly-report`)
		expect(short.sent[0]?.url.searchParams.get('allow_missing')).toBe('true')
		expect(full.sent).toEqual(short.sent)
	})
})
