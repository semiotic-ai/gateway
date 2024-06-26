use thegraph_core::types::{DeploymentId, SubgraphId};

/// Check if the given deployment is authorized.
///
/// It checks if the given deployment is contained in the authorized set. If the authorized set is
/// empty, any deployment is considered authorized.
pub fn is_deployment_authorized(authorized: &[DeploymentId], deployment: &DeploymentId) -> bool {
    authorized.is_empty() || authorized.contains(deployment)
}

/// Check if ALL the deployments are authorized.
///
/// It checks if all deployments are contained in the authorized set. If the authorized set is
/// empty, any deployment is considered authorized.
pub fn are_deployments_authorized(
    authorized: &[DeploymentId],
    deployments: &[DeploymentId],
) -> bool {
    authorized.is_empty()
        || deployments
            .iter()
            .all(|deployment| authorized.contains(deployment))
}

/// Check if the given subgraph is authorized.
///
/// It checks if the given subgraph is contained in the authorized set. If the authorized set is
/// empty, any subgraph is considered authorized.
pub fn is_subgraph_authorized(authorized: &[SubgraphId], subgraph: &SubgraphId) -> bool {
    authorized.is_empty() || authorized.contains(subgraph)
}

/// Check if ALL the subgraphs are authorized.
///
/// It checks if all subgraphs are contained in the authorized set. If the authorized set is empty,
/// any subgraph is considered authorized.
pub fn are_subgraphs_authorized(authorized: &[SubgraphId], subgraphs: &[SubgraphId]) -> bool {
    authorized.is_empty()
        || subgraphs
            .iter()
            .all(|subgraph| authorized.contains(subgraph))
}

/// Check if the query origin domain is authorized.
///
/// If the authorized domain starts with a `*`, it is considered a wildcard
/// domain. This means that any origin domain that ends with the wildcard
/// domain is considered authorized.
///
/// If the authorized domains set is empty, all domains are considered authorized.
pub fn is_domain_authorized(authorized: &[&str], origin: &str) -> bool {
    fn match_domain(pattern: &str, origin: &str) -> bool {
        if pattern.starts_with('*') {
            origin.ends_with(pattern.trim_start_matches('*'))
        } else {
            origin == pattern
        }
    }

    authorized.is_empty()
        || authorized
            .iter()
            .any(|pattern| match_domain(pattern, origin))
}

#[cfg(test)]
mod tests {
    use super::is_domain_authorized;

    #[test]
    fn authorized_domains() {
        let authorized_domains = [
            "example.com",
            "localhost",
            "a.b.c",
            "*.d.e",
            "*-foo.vercel.app",
        ];

        let sub_cases = [
            ("", false),
            ("example.com", true),
            ("subdomain.example.com", false),
            ("localhost", true),
            ("badhost", false),
            ("a.b.c", true),
            ("c", false),
            ("b.c", false),
            ("d.b.c", false),
            ("a", false),
            ("a.b", false),
            ("e", false),
            ("d.e", false),
            ("z.d.e", true),
            ("-foo.vercel.app", true),
            ("foo.vercel.app", false),
            ("bar-foo.vercel.app", true),
            ("bar.foo.vercel.app", false),
        ];

        for (input, expected) in sub_cases {
            assert_eq!(
                expected,
                is_domain_authorized(&authorized_domains, input),
                "match '{input}'"
            );
        }
    }

    #[test]
    fn empty_authorized_domains_set() {
        let authorized_domains = [];

        let sub_cases = [
            ("", true),
            ("example.com", true),
            ("subdomain.example.com", true),
            ("localhost", true),
            ("badhost", true),
            ("a.b.c", true),
            ("c", true),
            ("b.c", true),
            ("d.b.c", true),
            ("a", true),
            ("a.b", true),
            ("e", true),
            ("d.e", true),
            ("z.d.e", true),
            ("-foo.vercel.app", true),
            ("foo.vercel.app", true),
            ("bar-foo.vercel.app", true),
            ("bar.foo.vercel.app", true),
        ];

        for (input, expected) in sub_cases {
            assert_eq!(
                expected,
                is_domain_authorized(&authorized_domains, input),
                "match '{input}'"
            );
        }
    }
}
