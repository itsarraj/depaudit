use std::fs;
use std::path::PathBuf;

use clap::Parser;
use depaudit::lockfile::{
    dedupe, parse_cargo_lock, parse_gemfile_lock, parse_go_sum, parse_npm_lock, parse_pnpm_lock,
    parse_requirements_txt, parse_yarn_lock, Package,
};
use depaudit::osv::query_batch;

#[derive(Parser)]
#[command(
    name = "depaudit",
    about = "Checks lockfiles across ecosystems against OSV.dev for known vulnerabilities, in one pass"
)]
struct Cli {
    /// One or more lockfiles: Cargo.lock, package-lock.json, yarn.lock,
    /// pnpm-lock.yaml, requirements.txt, go.sum, Gemfile.lock. Ecosystem is
    /// auto-detected from the filename.
    files: Vec<PathBuf>,
}

fn load_packages(path: &PathBuf) -> anyhow::Result<Vec<Package>> {
    let content =
        fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");

    match filename {
        "Cargo.lock" => parse_cargo_lock(&content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        "package-lock.json" => parse_npm_lock(&content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        "yarn.lock" => parse_yarn_lock(&content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        "pnpm-lock.yaml" => parse_pnpm_lock(&content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        "requirements.txt" => Ok(parse_requirements_txt(&content)),
        "go.sum" => Ok(parse_go_sum(&content)),
        "Gemfile.lock" => Ok(parse_gemfile_lock(&content)),
        other => anyhow::bail!("{}: don't know how to parse '{other}' — expected Cargo.lock, package-lock.json, yarn.lock, pnpm-lock.yaml, requirements.txt, go.sum, or Gemfile.lock", path.display()),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.files.is_empty() {
        anyhow::bail!("give at least one lockfile, e.g. `depaudit Cargo.lock`");
    }

    let mut all_packages = Vec::new();
    for file in &cli.files {
        all_packages.extend(load_packages(file)?);
    }
    let packages = dedupe(all_packages);
    println!("checking {} package(s) against OSV.dev...", packages.len());

    let client = reqwest::Client::new();
    let findings = query_batch(&client, &packages).await?;

    if findings.is_empty() {
        println!("no known vulnerabilities found");
        Ok(())
    } else {
        println!(
            "\n{} package(s) with known vulnerabilities:",
            findings.len()
        );
        for f in &findings {
            println!(
                "  {} {} ({})  {}",
                f.package.name,
                f.package.version,
                f.package.ecosystem,
                f.vuln_ids.join(", ")
            );
        }
        std::process::exit(1);
    }
}
