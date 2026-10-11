//! The existing SPIRE customer identity uses actual namespace/ServiceAccount,
//! never author-provided tenant labels, product names or internal role aliases.
//! Any valid actual Kubernetes namespace is accepted; its name is not a tenant
//! binding. The relying party must authorize validated identities against its
//! protected deployment metadata.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    pub trust_domain: &'a str,
    pub namespace: &'a str,
    pub service_account: &'a str,
}

fn dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn parts(trust_domain: &str, namespace: &str, service_account: &str) -> Result<(), &'static str> {
    if trust_domain.len() > 253
        || !trust_domain.split('.').all(dns_label)
        || !dns_label(namespace)
        || !dns_label(service_account)
    {
        return Err("invalid customer workload identity");
    }
    Ok(())
}

/// Build from accepted deployment metadata and its actual local trust domain.
/// This is a name constructor, not proof of identity or tenant authorization.
pub fn spiffe_id(
    trust_domain: &str,
    namespace: &str,
    service_account: &str,
) -> Result<String, &'static str> {
    parts(trust_domain, namespace, service_account)?;
    Ok(format!(
        "spiffe://{trust_domain}/ns/{namespace}/sa/{service_account}"
    ))
}

/// Parse only after cryptographic SVID validation, pinning the relying party's
/// local trust domain even when its SPIRE agent also trusts foreign federation.
pub fn parse<'a>(id: &'a str, expected_trust_domain: &str) -> Result<Identity<'a>, &'static str> {
    let rest = id
        .strip_prefix("spiffe://")
        .ok_or("SPIFFE identity required")?;
    let (domain, path) = rest.split_once('/').ok_or("SPIFFE identity required")?;
    if domain != expected_trust_domain {
        return Err("foreign trust domain");
    }
    let mut segments = path.split('/');
    let (Some("ns"), Some(namespace), Some("sa"), Some(account), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err("customer namespace and service account required");
    };
    parts(domain, namespace, account)?;
    Ok(Identity {
        trust_domain: domain,
        namespace,
        service_account: account,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn construction_and_parsing_preserve_actual_namespace_and_account() {
        for namespace in [
            "env-0123456789abcdef0123",
            "env-0123456789abcdef0123456789abcdef",
            "sylphx-platform",
            "customer-prod",
            "team-7",
        ] {
            let id = spiffe_id("spiffe.sylphx.net", namespace, "api-123456").unwrap();
            assert_eq!(
                parse(&id, "spiffe.sylphx.net").unwrap(),
                Identity {
                    trust_domain: "spiffe.sylphx.net",
                    namespace,
                    service_account: "api-123456",
                }
            );
            assert!(parse(&id, "another.example").is_err());
        }
    }
    #[test]
    fn role_aliases_and_url_variants_are_not_customer_identity() {
        for id in [
            "spiffe://sylphx.local/role/api",
            "spiffe://sylphx.local/ns/env-0123456789abcdef0123/sa/api?other=tenant",
            "spiffe://sylphx.local/ns/env-0123456789abcdef0123/sa/api/extra",
        ] {
            assert!(parse(id, "sylphx.local").is_err());
        }
        assert!(spiffe_id("sylphx.local:443", "env-0123456789abcdef0123", "api").is_err());
        assert!(spiffe_id("sylphx.local", "env-0123456789abcdef0123", "-api").is_err());
    }
    #[test]
    fn namespace_syntax_is_validated_without_inferring_tenant_identity() {
        for namespace in [
            "",
            "-customer",
            "customer-",
            "Customer",
            "customer_prod",
            "customer.prod",
            "customer/other",
            "customer?tenant=other",
            "customer%2fother",
        ] {
            assert!(spiffe_id("sylphx.local", namespace, "api").is_err());
            let id = format!("spiffe://sylphx.local/ns/{namespace}/sa/api");
            assert!(parse(&id, "sylphx.local").is_err());
        }
        let longest = "a".repeat(63);
        let id = spiffe_id("sylphx.local", &longest, "api").unwrap();
        assert_eq!(parse(&id, "sylphx.local").unwrap().namespace, longest);
        let too_long = "a".repeat(64);
        assert!(spiffe_id("sylphx.local", &too_long, "api").is_err());
        let id = format!("spiffe://sylphx.local/ns/{too_long}/sa/api");
        assert!(parse(&id, "sylphx.local").is_err());
    }
}
