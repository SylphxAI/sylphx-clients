# Sylphx Auth handler

Mount the same Fetch handler on `/auth/*` in your server or worker. The host
supplies its public HTTPS origin and the instance ID, public slug, and
environment from Auth. Register the app origin as an Auth redirect origin.

```ts
import { createSylphxAuthHandler } from '@sylphx/auth'

export const handleAuth = createSylphxAuthHandler({
  origin: 'https://app.example.com',
  authApiOrigin: 'https://identity.example.net',
  instance: { id: 'instance-id', slug: 'example', environment: 'production' },
  defaultReturnTo: '/account',
})

// In your router: request => handleAuth(request)
```

This workspace package uses Web Fetch and Web Crypto, without framework or
Node imports. The runtime must supply an HTTPS origin and preserve individual
`Set-Cookie` headers. If terminating TLS in front of the handler, ensure its
Request URL reflects the trusted public origin, or supply `isPublicRequest`
with your edge's trusted-origin verification. Never trust caller-supplied
forwarded headers alone.

### Sylphx Hosting

Set `trustSylphxEdge: true` on the handler when **all ingress paths to it are
restricted to managed literal-host routes** that overwrite `Sylphx-Edge`,
`X-Forwarded-Host` and `X-Forwarded-Proto` for the matched public hostname.
This admits the rewritten internal request URL only with `Sylphx-Edge: public`,
a single exact forwarded host equal to the configured `origin`’s host (including
any non-default port), and `X-Forwarded-Proto: https`. Partial or contradictory
headers are rejected even when the request URL matches. With no edge headers,
the request URL must match the configured HTTPS origin as before.

Do not enable this on wildcard, direct-to-service, or other routes where a
caller can supply either forwarded header. The edge marker is not a credential,
and the HTTPS stamp is the configured public scheme, not a TLS observation.
Remove a hand-rolled `isPublicRequest` when adopting this option; that callback
otherwise retains precedence. Browser Origin and fetch-metadata checks, OAuth
state checks, and redirect destinations are unchanged.

| Route | Method | Input / result |
| --- | --- | --- |
| `/auth/config` | GET | Public capabilities only; no credentials or upstream URLs |
| `/auth/start/:provider` | POST | JSON `{ next }` → `{ authorize }`; navigate to `authorize` |
| `/auth/callback` | GET | Auth's one-use ticket and the state bound to the start cookie |
| `/auth/email` | POST | Password, signup, email verification, email OTP, MFA, or passkey action |
| `/auth/logout` | POST | Revoke the presented session, then clear host-only cookies |

Use `@sylphx/auth-solid` for email and social UI, or its headless controller
for custom layouts. Mutation endpoints require a same-origin `Origin`.
`next` is always local; optionally restrict it further with `allowReturnTo`.
The default `__Host-auth_session` stores the Auth session in a Secure,
HttpOnly, SameSite=Lax cookie; the browser API never returns it in JSON.
Application server routes can present that session to Auth for validation.
The handler does not create application profiles or replace a session store.

For BYO OAuth, set `tenantAuthOrigin` to the exact origin of the verified
Account Portal. Auth discovery supplies the mount path. The server uses that
start URL and checks that the provider's `redirect_uri` points back to the
same mount; an unknown or unverified custom start is never followed. Shared
provider credentials still use the platform protocol callback. Auth owns
provider PKCE, OIDC nonce and Apple's `form_post`; the application callback
receives a GET with a one-use ticket, not raw provider credentials.

Install this package with your package manager. Use a registry version from the
shared clients release, and Node 22.12 or later (or a worker that supplies the
standard Fetch and Web Crypto APIs). The existing `@sylphx/sdk` and React imports
are unchanged.

## Confidential sign-in and versioned acceptance

Set `sessionMode: 'server'` and `sessionSecret` to the instance environment's Access
secret key with `auth:admin`. The handler requests a one-use ticket for OTP,
password (including MFA), passkey, and OAuth, then redeems it against the
configured Auth API. Only redemption receives the secret. The browser gets
an HttpOnly cookie and a local navigation target, never a bearer or an email
or passkey ticket in JSON. A failed redemption never falls back to browser
mode. Omit both options to keep the existing browser-mode upstream contract.

The optional `session.issue(session, request)` adapter receives `session.user`
(verified ID, email status, and profile fields Auth supplied), `sessionId` when
present, `isNewUser` (false if Auth omits `is_new_user`), and `acceptance` when
configured. It retains the existing `token` and raw `expiresAt` fields. An adapter
returns its own cookie tuples and supplies the matching `session.logout` hook.
The default cookie supports Unix seconds (number or decimal string) and ISO
timestamps with explicit timezones.

For clickwrap, the host shows its own copy and sends an `acceptance` map after
explicit agreement, e.g. `{ terms: 'v2', privacy: '2026-11-24' }`. Configure
`clickwrap` on the server to require and validate the map:

```ts
const clickwrap = {
  signingKey: serverEnv.AUTH_CLICKWRAP_KEY, // independent, random, shared by all app replicas
  validate: (versions: Readonly<Record<string, string>>) =>
    versions.terms === currentTermsVersion && versions.privacy === currentPrivacyVersion,
}
```

The validator is a repeatable, read-only policy check, not an audit hook.
The handler checks consent before starting sign-in and before issuing app
cookies. For OAuth, the signed flow cookie binds the versions to the callback;
if policy changed during the redirect, the app asks the user to accept again.
For OTP, MFA, password, and passkey, include acceptance in each POST. The host
owns its evidence store and can record the accepted versions for the verified
principal in `session.issue` before returning cookies. This only gates sessions
issued through this handler; it does not change Auth instance policy or revoke
sessions that already exist.

Mount under another path with `routePrefix: '/account/auth'` and give the Solid
widget the same prefix. For SSR, call the handler's GET config route from your
server loader and pass its JSON as the widget's `initialConfig`. Use
`allowPasswordRegistration: false` to hide and refuse new password signup via
this handler; password sign-in, confirmation of prior registrations, OTP and
OAuth remain available according to the instance's capabilities.

## Instance keys and entry policy

Pass `publishableKey: env.SYLPHX_PUBLISHABLE_KEY` instead of `instance` to
resolve the tenant via its Access publishable key. The handler uses the bearer
header on Identity client calls, never the public config response, browser
URL, or tenant portal. You can provide both to additionally pin the resolved
ID, slug and environment. Confidential ticket redemption still uses only the
server key.

Set `allowedMethods: ['oauth', 'otp', 'passkey']`, for example, to suppress and
reject password sign-in, password MFA, registration, and recovery. An empty
array denies all sign-in methods. The allowlist is local to this handler.

An unverified password sign-in is denied by default. To continue a legacy mail
proof flow, use `issueUnverifiedPassword` to return a short-lived, proof-only
cookie. Never grant normal application permissions based on this callback;
verify the mailed proof against the same principal before upgrading the
session. The normal `session.issue` callback is never invoked in this case.

## App session integration

- `session.issue` also receives `method`, `provider` (OAuth only), and normalized
  `returnTo`. Optional `signUpSignal(context, request)` emits only the fixed
  `X-Sylphx-Auth-Event: sign_up` response header, and only on confirmed new-user
  sign-in with no GPC header. Return true only after checking affirmative,
  current consent. It emits no user or session identifiers, and never changes
  the HttpOnly session headers. Redirect response headers are not exposed to
  browser JS; consume the marker server-side for OAuth.
- `readAuthSessionCookie(request, { sessionCookie, legacyCookies })` reads
  the preferred credential for your app's auth middleware. A canonical cookie
  takes precedence; malformed or duplicate candidates do not cause fallback.
  Validate the credential with its authoritative issuer before authorizing
  requests or migrating it. Your middleware can then rotate it into the
  canonical cookie without extending its lifetime. The default logout route
  revokes the presented Identity session and clears the configured names;
  a custom adapter owns revocation and cleanup of its own cookies.
- `passkeyEnrollment: true` enables POST `/auth/passkey/enroll` using the
  canonical or configured legacy Identity session cookie. Hosts with their own
  session store can provide an async resolver for an Identity credential after
  authenticating the app session. Identity checks the tenant, verified email,
  recent session and WebAuthn proof. Use `SylphxPasskeyEnroll` in a signed-in view.
- `recoverPassword({ email, returnTo }, request)` enables the recovery-start
  action. Handle the send without disclosing account existence, enforce rate
  limits, and validate the mailed proof in your reset page. Identity's recovery
  operations require their own authorization; this hook does not proxy them
  using a publishable key or issue a session.
- `directOAuth: true` enables GET `/auth/:provider?returnTo=/account` from a
  same-origin top-level browser navigation. External requests, background
  fetches and requests without browser navigation headers cannot start it.
  Apps using server-side clickwrap must use POST `/auth/start/:provider` to
  bind acceptance to the OAuth flow.
