import { type Provider, providers } from '@sylphx/auth/client'
import {
	createMemo,
	createSignal,
	createUniqueId,
	For,
	type JSX,
	Match,
	onMount,
	Show,
	Switch,
} from 'solid-js'
import { authSecondsFormat, resolveAuthLocale } from './locale.js'
import { type AuthLocale, catalogs, localeNames } from './messages.js'
import { AppleIcon, FacebookIcon, GitHubIcon, GoogleIcon, MicrosoftIcon } from './provider-icons.js'
import {
	createSignInController,
	type SignInControllerOptions,
	type SignInStep,
} from './sign-in-controller.js'

const locales = Object.keys(localeNames) as AuthLocale[]

const icons: Partial<Record<Provider, () => JSX.Element>> = {
	google: GoogleIcon,
	github: GitHubIcon,
	apple: AppleIcon,
	facebook: FacebookIcon,
	microsoft: MicrosoftIcon,
}

export type SignInClass =
	| 'panel'
	| 'content'
	| 'kicker'
	| 'title'
	| 'sub'
	| 'error'
	| 'loading'
	| 'providers'
	| 'divider'
	| 'form'
	| 'primary'
	| 'secondary'
	| 'link'
	| 'switch'
	| 'password'
	| 'hint'
	| 'otp'
	| 'cells'
	| 'actions'
	| 'agreement'
	| 'locale'

export type SignInToken =
	| '--auth-surface'
	| '--auth-muted-surface'
	| '--auth-text'
	| '--auth-muted'
	| '--auth-border'
	| '--auth-action'
	| '--auth-action-text'
	| '--auth-accent'
	| '--auth-radius'
	| '--auth-width'
	| '--auth-font'
	| '--auth-font-mono'

export type SignInAppearance = {
	/** Classes applied to the named sign-in parts. */
	classes?: Partial<Record<SignInClass, string>>
	/** CSS custom properties for the default stylesheet. */
	tokens?: Partial<Record<SignInToken, string>>
	/** Omit default classes when the host owns all styling. */
	unstyled?: boolean
}

export type SylphxSignInProps = SignInControllerOptions & {
	title?: JSX.Element
	description?: JSX.Element
	kicker?: JSX.Element
	/** Optional agreement copy and links supplied by the host. */
	agreement?: JSX.Element
	appearance?: SignInAppearance
	/** Controlled locale, or seed SSR with `localePreferences` and auto-detect in the browser. */
	locale?: string
	/** Ordered locale hints resolved by the host, e.g. Accept-Language preferences on SSR. */
	localePreferences?: readonly string[]
	/** Show the language selector (defaults to true for an uncontrolled widget). */
	showLocaleSelector?: boolean
	onLocaleChange?: (locale: AuthLocale) => void
}

export function SylphxSignIn(props: SylphxSignInProps) {
	const controller = createSignInController(props)
	const {
		config,
		configFailed,
		loadConfig,
		step,
		email,
		setEmail,
		errorCode,
		busy,
		showPassword,
		setShowPassword,
		canPasskey,
		code,
		setCode,
		recovery,
		setRecovery,
		wait,
		continueWithEmail,
		social,
		sendCode,
		verifyCode,
		passkey,
		submitPassword,
		submitMfa,
	} = controller
	const [detected, setDetected] = createSignal<AuthLocale>()
	const [selected, setSelected] = createSignal<AuthLocale>()
	const locale = createMemo(() =>
		props.locale === undefined
			? (selected() ?? detected() ?? resolveAuthLocale(props.localePreferences ?? []))
			: resolveAuthLocale([props.locale, ...(props.localePreferences ?? [])]),
	)
	const text = () => catalogs[locale()]
	const seconds = createMemo(() => authSecondsFormat(locale()))
	const error = () => {
		const code = errorCode()
		return code ? text().errors[code] : ''
	}
	onMount(() => {
		if (props.locale === undefined && !props.localePreferences?.length)
			setDetected(resolveAuthLocale(navigator.languages))
	})

	const prefix = createUniqueId()
	const id = (name: string) => `${prefix}-${name}`
	function className(part: SignInClass) {
		const custom = props.appearance?.classes?.[part]
		if (props.appearance?.unstyled) return custom
		return custom ? `auth-sign-in-${part} ${custom}` : `auth-sign-in-${part}`
	}
	let emailInput: HTMLInputElement | undefined
	function focusOnChange(input: HTMLInputElement) {
		queueMicrotask(() => input.focus())
	}
	function go(to: SignInStep) {
		controller.go(to)
		if (to === 'choose') queueMicrotask(() => emailInput?.focus())
	}

	return (
		<section
			class={className('panel')}
			style={props.appearance?.tokens}
			aria-labelledby={id('title')}
			lang={locale()}
		>
			<div class={className('content')}>
				<Show when={props.kicker}>
					<p class={className('kicker')}>{props.kicker}</p>
				</Show>
				<h1 id={id('title')} class={className('title')}>
					{
						{
							choose: props.title ?? text().signIn,
							password: text().enterPassword,
							signup: text().createYourAccount,
							check_email: text().checkInbox,
							confirm: text().confirmEmail,
							otp: text().checkInbox,
							mfa: text().verifyIdentity,
						}[step()]
					}
				</h1>
				<p class={className('sub')}>
					{
						{
							choose: props.description ?? text().chooseMethod,
							password: email(),
							signup: text().startWithEmail,
							check_email: text().checkEmailDescription,
							confirm: text().confirmEmailDescription,
							otp: text().enterEmailCode(email()),
							mfa: recovery() ? text().enterRecovery : text().enterAuthenticator,
						}[step()]
					}
				</p>
				<Show when={error()}>
					<p role="alert" class={className('error')}>
						{error()}
					</p>
				</Show>
				<Switch>
					<Match when={step() === 'choose'}>
						<Show
							when={config()}
							fallback={
								<Show
									when={configFailed()}
									fallback={<output class={className('loading')}>{text().loading}</output>}
								>
									<p role="alert" class={className('error')}>
										{text().errors.unavailable}
									</p>
									<button type="button" class={className('secondary')} onClick={loadConfig}>
										{text().retry}
									</button>
								</Show>
							}
						>
							{(methods) => (
								<>
									<div class={className('providers')}>
										<For each={methods().providers}>
											{(provider) => {
												const Icon = icons[provider]
												return (
													<button
														type="button"
														class={className('secondary')}
														disabled={!!busy()}
														onClick={() => void social(provider)}
													>
														{Icon && <Icon />}
														{busy() === providers[provider][0]
															? text().openingProvider(providers[provider][0])
															: text().continueProvider(providers[provider][0])}
													</button>
												)
											}}
										</For>
									</div>
									<Show when={methods().password || methods().otp}>
										<Show when={methods().providers.length}>
											<div class={className('divider')}>{text().orEmail}</div>
										</Show>
										<form
											method="post"
											action="/auth/email"
											class={className('form')}
											onSubmit={(event) => {
												event.preventDefault()
												void continueWithEmail()
											}}
										>
											<label for={id('email')}>{text().email}</label>
											<input
												ref={(input) => {
													emailInput = input
												}}
												id={id('email')}
												type="email"
												name="email"
												autocomplete="username"
												required
												maxlength={254}
												value={email()}
												onInput={(event) => setEmail(event.currentTarget.value)}
											/>
											<button type="submit" class={className('primary')} disabled={!!busy()}>
												{busy() === 'email'
													? text().sendingCode
													: methods().otp
														? text().emailCodeAction
														: text().continueEmail}
											</button>
										</form>
									</Show>
									<Show when={methods().passkey && canPasskey()}>
										<button
											type="button"
											class={className('link')}
											disabled={!!busy()}
											onClick={() => void passkey()}
										>
											{busy() === 'passkey' ? text().waitingPasskey : text().usePasskey}
										</button>
									</Show>
									<Show when={methods().signup}>
										<p class={className('switch')}>
											{text().newHere}{' '}
											<button
												type="button"
												class={className('link')}
												disabled={!!busy()}
												onClick={() => go('signup')}
											>
												{text().createAnAccount}
											</button>
										</p>
									</Show>
									<Show
										when={
											!methods().password &&
											!methods().signup &&
											!methods().otp &&
											!methods().providers.length &&
											!(methods().passkey && canPasskey())
										}
									>
										<output>{text().noMethods}</output>
									</Show>
								</>
							)}
						</Show>
					</Match>
					<Match when={step() === 'password' || step() === 'signup' || step() === 'confirm'}>
						<form
							method="post"
							action="/auth/email"
							class={className('form')}
							onSubmit={(event) => {
								event.preventDefault()
								void submitPassword(String(new FormData(event.currentTarget).get('password') ?? ''))
							}}
						>
							<Show
								when={step() !== 'password'}
								fallback={
									<input
										tabindex="-1"
										hidden
										name="username"
										value={email()}
										autocomplete="username"
										readonly
									/>
								}
							>
								<label for={id('account-email')}>{text().email}</label>
								<input
									ref={focusOnChange}
									id={id('account-email')}
									type="email"
									name="username"
									autocomplete="username"
									required
									maxlength={254}
									value={email()}
									onInput={(event) => setEmail(event.currentTarget.value)}
								/>
							</Show>
							<label for={id('password')}>{text().password}</label>
							<div class={className('password')}>
								<input
									ref={(input) => {
										if (step() === 'password') focusOnChange(input)
									}}
									id={id('password')}
									type={showPassword() ? 'text' : 'password'}
									autocomplete={step() === 'signup' ? 'new-password' : 'current-password'}
									name="password"
									required
									minlength={step() === 'signup' ? 12 : undefined}
									maxlength={1024}
									aria-describedby={step() === 'signup' ? id('password-hint') : undefined}
								/>
								<button
									type="button"
									class={className('link')}
									aria-label={showPassword() ? text().hidePassword : text().showPassword}
									onClick={() => setShowPassword(!showPassword())}
								>
									{showPassword() ? text().hide : text().show}
								</button>
							</div>
							<Show when={step() === 'signup'}>
								<p class={className('hint')} id={id('password-hint')}>
									{text().minimumPassword}
								</p>
							</Show>
							<button type="submit" class={className('primary')} disabled={!!busy()}>
								{busy()
									? text().working
									: step() === 'signup'
										? text().createAccount
										: step() === 'confirm'
											? text().confirmAndSignIn
											: text().signIn}
							</button>
						</form>
						<button
							type="button"
							class={className('link')}
							disabled={!!busy()}
							onClick={() => go('choose')}
						>
							{step() === 'password' ? text().changeEmailOrMethod : text().backToSignIn}
						</button>
					</Match>
					<Match when={step() === 'check_email'}>
						<button type="button" class={className('secondary')} onClick={() => go('choose')}>
							{text().backToSignIn}
						</button>
					</Match>
					<Match when={step() === 'otp'}>
						<form
							method="post"
							action="/auth/email"
							class={className('form')}
							onSubmit={(event) => {
								event.preventDefault()
								void verifyCode(code())
							}}
						>
							<label for={id('code')}>{text().emailCode}</label>
							<div class={className('otp')}>
								<input
									ref={focusOnChange}
									id={id('code')}
									type="text"
									inputmode="numeric"
									autocomplete="one-time-code"
									value={code()}
									readonly={!!busy()}
									onInput={(event) => {
										const value = event.currentTarget.value.replace(/\D/g, '').slice(0, 6)
										event.currentTarget.value = value
										setCode(value)
										void verifyCode(value)
									}}
								/>
								<div aria-hidden="true" class={className('cells')}>
									<For each={[0, 1, 2, 3, 4, 5]}>{(i) => <span>{code()[i] ?? ''}</span>}</For>
								</div>
							</div>
							<button
								type="submit"
								class={className('primary')}
								disabled={!!busy() || code().length !== 6}
							>
								{busy() === 'verify' ? text().checkingCode : text().verifyCode}
							</button>
						</form>
						<div class={className('actions')}>
							<button
								type="button"
								class={className('link')}
								disabled={!!busy() || wait() > 0}
								onClick={() => void sendCode()}
							>
								{wait() ? text().resendIn(seconds().format(wait())) : text().resendCode}
							</button>
							<button
								type="button"
								class={className('link')}
								disabled={!!busy()}
								onClick={() => go('choose')}
							>
								{text().changeEmail}
							</button>
							<Show when={config()?.password}>
								<button
									type="button"
									class={className('link')}
									disabled={!!busy()}
									onClick={() => go('password')}
								>
									{text().usePassword}
								</button>
							</Show>
						</div>
					</Match>
					<Match when={step() === 'mfa'}>
						<form
							method="post"
							action="/auth/email"
							class={className('form')}
							onSubmit={(event) => {
								event.preventDefault()
								void submitMfa(String(new FormData(event.currentTarget).get('code') ?? ''))
							}}
						>
							<label for={id('mfa')}>
								{recovery() ? text().recoveryCode : text().authenticatorCode}
							</label>
							<input
								ref={focusOnChange}
								id={id('mfa')}
								name="code"
								inputmode={recovery() ? 'text' : 'numeric'}
								autocomplete="one-time-code"
								required
								maxlength={recovery() ? 128 : 6}
							/>
							<button type="submit" class={className('primary')} disabled={!!busy()}>
								{busy() ? text().checkingCode : text().verifyAndSignIn}
							</button>
						</form>
						<div class={className('actions')}>
							<button
								type="button"
								disabled={!!busy()}
								class={className('link')}
								onClick={() => setRecovery(!recovery())}
							>
								{recovery() ? text().useAuthenticator : text().useRecovery}
							</button>
							<button
								type="button"
								disabled={!!busy()}
								class={className('link')}
								onClick={() => go('choose')}
							>
								{text().backToSignIn}
							</button>
						</div>
					</Match>
				</Switch>
				<Show
					when={
						props.showLocaleSelector !== false &&
						(props.locale === undefined || props.onLocaleChange)
					}
				>
					<div class={className('locale')}>
						<label for={id('locale')}>{text().language}</label>
						<select
							id={id('locale')}
							value={locale()}
							onChange={(event) => {
								const target = resolveAuthLocale([event.currentTarget.value])
								if (props.locale === undefined) setSelected(target)
								props.onLocaleChange?.(target)
							}}
						>
							<For each={locales}>
								{(item) => (
									<option value={item} lang={item} selected={locale() === item}>
										{localeNames[item]}
									</option>
								)}
							</For>
						</select>
					</div>
				</Show>
				<Show when={props.agreement}>
					<p class={className('agreement')}>{props.agreement}</p>
				</Show>
			</div>
		</section>
	)
}
