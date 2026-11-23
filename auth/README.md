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
