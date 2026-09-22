# sylphx

The Sylphx SDK for Rust: one API (`https://api.sylphx.com`), one key, one
namespace per service.

```toml
[dependencies]
sylphx = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust
#[tokio::main]
async fn main() -> Result<(), sylphx::Error> {
    let sx = sylphx::Client::from_env()?; // reads SYLPHX_API_KEY (and SYLPHX_BASE_URL)
    let me = sx.access().whoami(Default::default()).await?;
    println!("{} in {}", me.principal, me.org);

    let mut req = sylphx::access::ListProjectsRequest::default();
    req.parent = me.org.clone();
    for project in sx.access().projects().list_all(req).await? {
        println!("{}", project.name);
    }
    Ok(())
}
```

- Every service is a namespace (`sx.data()`, `sx.hosting()`, …) and every
  collection a type on it (`sx.data().databases()`), with the standard methods
  `get`, `list`, `create`, `update`, `delete` and each custom method.
- Mutations of reconciled Resources return an `Operation`; `.wait().await`
  returns the settled Resource.
- Errors are `sylphx::Error::Api { code, status, retryable, effect, detail,
  request_id, .. }` from the one RFC 9457 problem body.
- The client retries retryable answers with full-jitter backoff, honours
  `Retry-After`, and sends one `Idempotency-Key` per logical mutation.
- `Client::invoke("data.databases.get", json!({"name": "…"}))` calls any
  method by id with wire JSON.

This crate is generated from the Sylphx schema registry by `sylphx-gen`; do
not edit it by hand. Source: `clients/rust` in the Sylphx monorepo, published
from its MIT mirror.
