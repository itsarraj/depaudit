use serde::{Deserialize, Serialize};

use crate::lockfile::Package;

/// OSV's own documented batch-size guidance — chunking keeps a large
/// lockfile (a real `Cargo.lock` easily has 200+ entries) from a single
/// oversized request; a real limit, not a guessed-safe number.
const BATCH_SIZE: usize = 500;

#[derive(Serialize)]
struct QueryPackage<'a> {
    name: &'a str,
    ecosystem: &'a str,
}

#[derive(Serialize)]
struct Query<'a> {
    package: QueryPackage<'a>,
    version: &'a str,
}

#[derive(Serialize)]
struct BatchRequest<'a> {
    queries: Vec<Query<'a>>,
}

#[derive(Deserialize, Default)]
struct ResultEntry {
    #[serde(default)]
    vulns: Vec<VulnRef>,
}

#[derive(Deserialize)]
struct VulnRef {
    id: String,
}

#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    results: Vec<ResultEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub package: Package,
    pub vuln_ids: Vec<String>,
}

/// Queries OSV.dev's real, free, no-auth `querybatch` API for every
/// package, chunked to `BATCH_SIZE` per request, and returns only the
/// packages with at least one known vulnerability — the whole point
/// being that a clean dependency tree produces an empty, easy-to-check
/// result, not a wall of "no vulnerabilities" noise per package.
pub async fn query_batch(
    client: &reqwest::Client,
    packages: &[Package],
) -> anyhow::Result<Vec<Finding>> {
    let mut findings = Vec::new();

    for chunk in packages.chunks(BATCH_SIZE) {
        let queries: Vec<Query> = chunk
            .iter()
            .map(|p| Query {
                package: QueryPackage {
                    name: &p.name,
                    ecosystem: p.ecosystem,
                },
                version: &p.version,
            })
            .collect();
        let response: BatchResponse = client
            .post("https://api.osv.dev/v1/querybatch")
            .json(&BatchRequest { queries })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        for (package, result) in chunk.iter().zip(response.results) {
            if !result.vulns.is_empty() {
                findings.push(Finding {
                    package: package.clone(),
                    vuln_ids: result.vulns.into_iter().map(|v| v.id).collect(),
                });
            }
        }
    }

    Ok(findings)
}
