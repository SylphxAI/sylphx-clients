import { cookies, headers } from 'next/headers.js'
import { redirect } from 'next/navigation.js'
import { type AuthOptions, createSignInActions, type SignInState } from './auth-actions.js'
import { type Appearance, SignInForm } from './auth-client.js'

export type { AuthOptions } from './auth-actions.js'
export type { Appearance } from './auth-client.js'

export interface SignInProps {
	auth: AuthOptions
	appearance?: Appearance
	/** Pass searchParams.auth_ticket from this app's OAuth return page. */
	ticket?: string
}
async function actions(auth: AuthOptions) {
	const incoming = await headers()
	const jar = await cookies()
	return createSignInActions(auth, {
		userAgent: incoming.get('user-agent') ?? '',
		redirect,
		setCookie: (name, value, options) => {
			jar.set(name, value, options)
		},
	})
}

/** Server Component. The secret stays in server action closures, not serialized props. */
export async function SignIn({ auth, appearance, ticket }: SignInProps) {
	const config = await (await actions(auth)).config()
	async function submit(previous: SignInState, form: FormData) {
		'use server'
		return (await actions(auth)).submit(previous, form)
	}
	async function social(form: FormData) {
		'use server'
		return (await actions(auth)).social(form)
	}
	return (
		<SignInForm
			config={{
				branding: config.branding,
				methods: config.methods,
				social: config.social.map(({ provider, credentials }) => ({ provider, credentials })),
			}}
			action={submit}
			socialAction={social}
			appearance={appearance}
			ticket={ticket}
		/>
	)
}
