import type { SylphxAuthOptions } from '@sylphx/auth'
import { safeReturnTo } from '@sylphx/auth/client'
import { SylphxSignIn, type SylphxSignInProps } from '@sylphx/auth-solid'

const options: Pick<SylphxAuthOptions, 'defaultReturnTo'> = { defaultReturnTo: '/home' }
const props: SylphxSignInProps = {
	title: 'Package sign-in fixture',
	next: safeReturnTo(options.defaultReturnTo),
	search: '',
	locale: 'en',
}

export function page() {
	return <SylphxSignIn {...props} />
}
