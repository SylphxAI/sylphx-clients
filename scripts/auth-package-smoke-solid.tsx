import type { SylphxAuthOptions } from '@sylphx/auth'
import { safeReturnTo } from '@sylphx/auth/client'
import {
	type SignInAppearance,
	SylphxPasskeyEnroll,
	SylphxSignIn,
	type SylphxSignInProps,
} from '@sylphx/auth-solid'
import { createSignal } from 'solid-js'

const options: Pick<SylphxAuthOptions, 'defaultReturnTo'> = { defaultReturnTo: '/home' }
const props: SylphxSignInProps = {
	title: 'Package sign-in fixture',
	next: safeReturnTo(options.defaultReturnTo),
	search: '',
	locale: 'en',
	routePrefix: '/account/auth',
	conditionalPasskey: true,
	initialConfig: {
		providers: ['google'],
		password: true,
		signup: false,
		otp: true,
		passkey: true,
		recovery: true,
	},
	getAcceptance: () => ({ terms: 'v2' }),
}

export function appearancePage(appearance: SignInAppearance) {
	return <SylphxSignIn {...props} appearance={appearance} />
}

export function page() {
	const [accepted, setAccepted] = createSignal(false)
	return (
		<>
			<SylphxSignIn
				{...props}
				requireAgreement
				agreementAccepted={accepted()}
				lastUsed="password"
				agreement={
					<label>
						<input
							type="checkbox"
							checked={accepted()}
							onChange={(event) => setAccepted(event.currentTarget.checked)}
						/>{' '}
						Accept terms
					</label>
				}
			/>
			<SylphxPasskeyEnroll enabled routePrefix={props.routePrefix} locale="pt-BR" />
		</>
	)
}
