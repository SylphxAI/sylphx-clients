# Solid sign-in

A Solid package with compiled browser and SSR entry points. Install both SDKs
and Solid in your application, then mount the same-origin `@sylphx/auth` routes
on your application server. It owns the surrounding page, return-path allowlist,
and legal links; this package does not import a router or an application shell.

```tsx
import { SylphxSignIn } from '@sylphx/auth-solid'
import '@sylphx/auth-solid/sign-in.css'

<SylphxSignIn
  next={allowedNext}
  search={location.search}
  title="Sign in"
  description="Choose how you’d like to sign in."
/>
```

`title`, `description`, and `kicker` accept JSX; the first two customize the
initial step. `agreement` accepts optional host-provided JSX. `loginPath`
(default `/login`) is the same-origin path used to scrub an email confirmation
link after reading its proof. Pass the query string for the current login visit;
remount the sign-in when starting a new visit with a different query.

`appearance.classes` adds classes to the named parts. Use
`appearance={{ unstyled: true, classes }}` with a host CSS module, or import the
default sheet and set typed `appearance.tokens` CSS custom properties (e.g.
`--auth-accent`, `--auth-surface`, `--auth-width`). The default stylesheet gives
controls a minimum 44px touch target; preserve that in host styles.

For custom markup call `createSignInController({ next, search, loginPath })`
under a Solid owner. It loads method configuration on mount; the returned Solid
accessors expose the current step, error, busy state and method options. Call
`continueWithEmail`, `submitPassword`, `sendCode`, `verifyCode`, `submitMfa`,
`passkey`, and `social` from the corresponding controls. Supply input/label
associations and manage focus in the custom view. Unmount aborts pending work.

### Language

The component ships English, Traditional Chinese for Hong Kong and Taiwan,
Simplified Chinese, Japanese, Korean, Spanish (`es`), and Brazilian Portuguese (`pt-BR`). It renders a language selector below the
form and preserves values and progress when the language changes. Set
`showLocaleSelector={false}` if the host already provides a selector.

Set `locale` to control the language explicitly, and pass `onLocaleChange` if
the widget's selector should update host state. Without `locale`, use ordered
`localePreferences` to seed SSR and hydration from the same values. Without
those hints, SSR starts in English and the browser's language preferences are
used after mount. Unsupported or malformed tags fall through to the next hint,
then English. `resolveAuthLocale` exposes the same negotiation for hosts.
Supply custom headings, descriptions and legal copy in the active language.
Use `messages` for partial catalog overrides (including individual error strings).
Supply an explicit BCP 47 `locale` with your catalog for additional languages;
missing strings fall back to the nearest bundled catalog or English. The
widget does not change the language of email sent by the Auth service.

Headless clients can render `signInMessages[locale].errors[errorCode]` when
`controller.errorCode()` is nonempty. The existing `controller.error()` still
returns English for callers that have not switched to catalogs.

### Server-rendered methods, clickwrap, and passkeys

Load the public config route from a server loader and pass its JSON as
`initialConfig`, using the same value on SSR and hydration. This renders the
configured Google and email entries in the first HTML, before JavaScript runs.
The interactive widget still needs JavaScript; omitting the seed preserves the
client config fetch. Set the same `routePrefix` as the server handler if you
mount it anywhere other than `/auth`.

Render your consent UI using `agreement` or your own layout. Use
`getAcceptance={() => agreed() ? { terms: termsVersion, privacy: privacyVersion } : undefined}`
so requests send only the versions the user actually accepted. Set
`requireAgreement` and provide a reactive `agreementAccepted` boolean to disable
and block authentication actions until the checkbox is checked. Configure
server-side `clickwrap` to enforce those versions; the UI alone is not a gate.

Set `conditionalPasskey` to opt into discoverable credential autofill on the
first sign-in screen. The identifier input uses `username webauthn` when
passkeys are configured. Unsupported or canceled conditional requests fall
back to the existing explicit passkey and other enabled sign-in controls.

When both email OTP and password are configured, the password option is
available without starting OTP. If `recovery` is enabled in server config, the
password screen offers the host's recovery flow. The confirmation screen hides
mail proof from the URL and allows an independent passkey sign-in when enabled;
a passkey is not submitted as email proof or as a substitute for MFA.

Use `lastUsed="google"` (or another provider, `password`, `otp`, or `passkey`)
to label the method from a host-controlled, non-authoritative hint. Use
`beforeOAuthNavigate` for an optional app hook after validating the provider
destination and before navigation. Do not put secrets in client hints.

Render `<SylphxPasskeyEnroll enabled={config.passkeyEnrollment === true} />`
in a signed-in view. It supports the same `routePrefix`, `locale` and `messages`
options. The handler and Identity, not the visibility of this component,
authorize enrollment and enforce recent authentication.
