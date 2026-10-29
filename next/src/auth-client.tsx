'use client'

import type { AuthConfig } from '@sylphx/sdk/auth/client'
import { type CSSProperties, useActionState, useEffect, useRef } from 'react'
import type { SignInAction, SignInState } from './auth-actions.js'

export interface Appearance {
	variables?: {
		colorPrimary?: string
		colorText?: string
		colorBackground?: string
		colorDanger?: string
		borderRadius?: string
		fontFamily?: string
	}
	elements?: Partial<
		Record<
			'root' | 'logo' | 'title' | 'form' | 'label' | 'input' | 'button' | 'socialButton' | 'error',
			string
		>
	>
}
export interface SignInFormProps {
	config: Pick<AuthConfig, 'branding' | 'methods'> & {
		social: Pick<AuthConfig['social'][number], 'provider' | 'credentials'>[]
	}
	action: SignInAction
	socialAction: (form: FormData) => Promise<void>
	appearance?: Appearance
	initialState?: SignInState
	/** One-time capability, never a session bearer. */
	ticket?: string
}
function customerLogo(value: string | null): string | undefined {
	if (!value) return undefined
	try {
		const u = new URL(value)
		return u.protocol === 'https:' && !/sylphx/i.test(u.href) ? u.href : undefined
	} catch {
		return undefined
	}
}

export function SignInForm({
	config,
	action,
	socialAction,
	appearance = {},
	initialState = { step: 'identifier' },
	ticket,
}: SignInFormProps) {
	const [state, submit, pending] = useActionState(action, initialState)
	const form = useRef<HTMLFormElement>(null)
	const root = useRef<HTMLElement>(null)
	const sent = useRef(false)
	useEffect(() => {
		if (ticket && !sent.current) {
			sent.current = true
			form.current?.requestSubmit()
		}
	}, [ticket])
	useEffect(() => {
		if (state.step === 'complete') return
		root.current
			?.querySelector<HTMLInputElement>('input:not([type="hidden"]):not([readonly])')
			?.focus()
	}, [state.step])
	const e = appearance.elements ?? {}
	const v = appearance.variables ?? {}
	const style = {
		'--auth-primary': v.colorPrimary ?? config.branding.primaryColor ?? '#2563eb',
		'--auth-text': v.colorText ?? '#171717',
		'--auth-background': v.colorBackground ?? '#fff',
		'--auth-danger': v.colorDanger ?? '#b91c1c',
		'--auth-radius': v.borderRadius ?? '0.5rem',
		fontFamily: v.fontFamily ?? 'inherit',
		color: 'var(--auth-text)',
		background: 'var(--auth-background)',
		borderRadius: 'var(--auth-radius)',
	} as CSSProperties
	const inputStyle = { borderRadius: 'var(--auth-radius)', color: 'inherit', font: 'inherit' }
	const buttonStyle = {
		borderRadius: 'var(--auth-radius)',
		background: 'var(--auth-primary)',
		color: 'var(--auth-background)',
		font: 'inherit',
	}
	const logo = customerLogo(config.branding.logoUrl)
	const name = /sylphx/i.test(config.branding.name) ? 'your account' : config.branding.name
	const password = state.step === 'password'
	const code = state.step === 'email_otp'
	const email = password || code ? state.email : ''
	return (
		<section ref={root} className={e.root} style={style} aria-label="Sign in" aria-busy={pending}>
			{logo && (
				// biome-ignore lint/performance/noImgElement: Customer logo origins are configured per instance, not in the consuming app image allowlist.
				<img className={e.logo} src={logo} alt={name} />
			)}
			<h1 className={e.title}>Sign in to {name}</h1>
			{state.step === 'complete' ? (
				<output>You are signed in.</output>
			) : (
				<>
					<div
						className={e.error}
						role="alert"
						aria-live="assertive"
						style={{ color: 'var(--auth-danger)' }}
					>
						{state.step === 'error' ? state.message : null}
					</div>
					{(ticket || password || code || config.methods.password || config.methods.email_otp) && (
						<form ref={form} action={submit} className={e.form}>
							<input
								type="hidden"
								name="intent"
								value={ticket ? 'ticket' : code ? 'code' : password ? 'password' : 'continue'}
							/>
							{ticket ? (
								<input type="hidden" name="ticket" value={ticket} />
							) : (
								<>
									<label className={e.label}>
										Email address
										<input
											style={inputStyle}
											className={e.input}
											name="email"
											type="email"
											autoComplete="username webauthn"
											required
											defaultValue={email}
											readOnly={password || code}
										/>
									</label>
									{password && (
										<label className={e.label}>
											Password
											<input
												style={inputStyle}
												className={e.input}
												name="password"
												type="password"
												autoComplete="current-password"
												required
											/>
										</label>
									)}
									{code && (
										<>
											<input type="hidden" name="flow" value={state.flow} />
											<label className={e.label}>
												Verification code
												<input
													style={inputStyle}
													className={e.input}
													name="code"
													autoComplete="one-time-code"
													inputMode="numeric"
													pattern="[0-9]{6}"
													required
												/>
											</label>
										</>
									)}
								</>
							)}
							<button type="submit" className={e.button} style={buttonStyle} disabled={pending}>
								{pending
									? 'Signing in…'
									: ticket
										? 'Complete sign-in'
										: password || code
											? 'Sign in'
											: 'Continue'}
							</button>
						</form>
					)}
					{password && config.methods.email_otp && (
						<form action={submit}>
							<input type="hidden" name="intent" value="email_otp" />
							<input type="hidden" name="email" value={email} />
							<button type="submit" className={e.button} disabled={pending}>
								Use an email code
							</button>
						</form>
					)}
					{!ticket &&
						config.social
							.filter(
								(p) =>
									['google', 'apple'].includes(p.provider) &&
									['custom', 'shared'].includes(p.credentials),
							)
							.map((p) => (
								<form action={socialAction} key={p.provider}>
									<input type="hidden" name="provider" value={p.provider} />
									<button
										type="submit"
										className={e.socialButton}
										style={buttonStyle}
										disabled={pending}
									>
										Continue with {p.provider === 'google' ? 'Google' : 'Apple'}
									</button>
								</form>
							))}
				</>
			)}
		</section>
	)
}
