use std::collections::HashMap;

use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;
use toml::Value as TomlValue;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// OSV's own ecosystem identifier string (`"crates.io"`, `"npm"`,
    /// `"PyPI"`) — passed straight through to the query, not remapped.
    pub ecosystem: &'static str,
}

/// `Cargo.lock`'s `[[package]] name = "..." version = "..."` entries.
pub fn parse_cargo_lock(content: &str) -> Result<Vec<Package>, String> {
    let parsed: TomlValue = content
        .parse()
        .map_err(|e| format!("not valid TOML: {e}"))?;
    let packages = parsed
        .get("package")
        .and_then(TomlValue::as_array)
        .ok_or_else(|| "no [[package]] entries found".to_string())?;

    Ok(packages
        .iter()
        .filter_map(|p| {
            let name = p.get("name")?.as_str()?.to_string();
            let version = p.get("version")?.as_str()?.to_string();
            Some(Package {
                name,
                version,
                ecosystem: "crates.io",
            })
        })
        .collect())
}

/// `package-lock.json`'s modern (`lockfileVersion` 2/3) `"packages"` map —
/// `node_modules/<name>` (or `node_modules/@scope/name` for a scoped
/// package) -> `{"version": "..."}`. The root `""` entry is the project
/// itself, not a dependency, and is skipped. The older
/// `lockfileVersion` 1 nested `"dependencies"` tree isn't parsed in this
/// v1 — see README.
pub fn parse_npm_lock(content: &str) -> Result<Vec<Package>, String> {
    let parsed: JsonValue =
        serde_json::from_str(content).map_err(|e| format!("not valid JSON: {e}"))?;
    let packages = parsed
        .get("packages")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| {
            "no \"packages\" object — is this a lockfileVersion 2/3 package-lock.json?".to_string()
        })?;

    Ok(packages
        .iter()
        .filter_map(|(path, entry)| {
            if path.is_empty() {
                return None; // the project itself
            }
            let name = path
                .rsplit_once("node_modules/")
                .map(|(_, rest)| rest)
                .unwrap_or(path)
                .to_string();
            let version = entry.get("version")?.as_str()?.to_string();
            Some(Package {
                name,
                version,
                ecosystem: "npm",
            })
        })
        .collect())
}

/// `requirements.txt`'s `name==version` exact pins. Range specifiers
/// (`>=`, `~=`, unpinned) have no single version to query and are
/// skipped, not errored on — a requirements.txt mixing pinned and
/// unpinned lines is completely normal.
pub fn parse_requirements_txt(content: &str) -> Vec<Package> {
    content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or(line).trim(); // strip trailing comment
            let (name, version) = line.split_once("==")?;
            Some(Package {
                name: name.trim().to_string(),
                version: version.trim().to_string(),
                ecosystem: "PyPI",
            })
        })
        .collect()
}

/// Splits a `name@range` (or `@scope/name@range`) descriptor — the shape
/// used by both Yarn's classic descriptor headers and pnpm's `packages`
/// map keys — at the `@` that separates the package name from whatever
/// follows it. A scoped package's own leading `@` would otherwise be
/// mistaken for that separator, so the search starts one character in
/// whenever the descriptor itself begins with `@`.
fn split_scoped_at(spec: &str) -> Option<(&str, &str)> {
    let search_start = if spec.starts_with('@') { 1 } else { 0 };
    let idx = spec[search_start..].find('@')?;
    let split_at = search_start + idx;
    if split_at == 0 {
        return None;
    }
    Some((&spec[..split_at], &spec[split_at + 1..]))
}

/// `yarn.lock` comes in two, entirely unrelated formats sharing one
/// filename: the "classic" v1 format (a custom, YAML-*like* but not
/// actually YAML layout) and the Berry/v2+ format (real YAML). Every
/// real classic lockfile carries the literal `# yarn lockfile v1`
/// header comment; its absence is taken as "assume Berry" and handed to
/// the real YAML parser.
pub fn parse_yarn_lock(content: &str) -> Result<Vec<Package>, String> {
    if content.contains("# yarn lockfile v1") {
        Ok(parse_yarn_classic(content))
    } else {
        parse_yarn_berry(content)
    }
}

/// Yarn classic v1: an unindented descriptor line ending in `:` (one or
/// more comma-separated `name@range` alternatives, quoted or not — a
/// scoped package's descriptor looks like `"@scope/name@^1.0.0"`),
/// followed by indented `key "value"` fields. Only the `version` field
/// is needed — the descriptor's range(s) are what a `yarn install` was
/// asked for, not what got resolved, so the name (not the version) is
/// the only thing taken from the header itself.
fn parse_yarn_classic(content: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    let mut pending_name: Option<String> = None;

    for line in content.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            let header = line.trim_end_matches(':');
            let first_descriptor = header.split(", ").next().unwrap_or(header).trim();
            let first_descriptor = first_descriptor.trim_matches('"');
            pending_name = split_scoped_at(first_descriptor).map(|(name, _range)| name.to_string());
            continue;
        }
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("version ") {
            if let Some(name) = pending_name.take() {
                let version = rest.trim().trim_matches('"').to_string();
                packages.push(Package {
                    name,
                    version,
                    ecosystem: "npm",
                });
            }
        }
    }

    packages
}

/// Yarn Berry (v2+): real YAML. Top-level keys are descriptor strings —
/// possibly several comma-separated alternatives sharing one resolution,
/// e.g. `"@babel/core@npm:^7.0.0, @babel/core@npm:^7.10.0"` — with a
/// `version` field in the value. `__metadata` is the lockfile's own
/// schema-version block, not a dependency, and is skipped. Only the
/// `npm:` protocol is resolved to a queryable package: `workspace:`,
/// `patch:`, and `portal:` descriptors point at local or derived
/// packages with no real, independently-versioned identity on npm for
/// OSV to check.
fn parse_yarn_berry(content: &str) -> Result<Vec<Package>, String> {
    let parsed: YamlValue =
        serde_yaml::from_str(content).map_err(|e| format!("not valid YAML: {e}"))?;
    let mapping = parsed
        .as_mapping()
        .ok_or_else(|| "not a YAML mapping — is this a Yarn Berry (v2+) yarn.lock?".to_string())?;

    Ok(mapping
        .iter()
        .filter_map(|(key, value)| {
            let key = key.as_str()?;
            if key == "__metadata" {
                return None;
            }
            let (name, _range) = key.split_once("@npm:")?;
            let version = value.get("version")?.as_str()?.to_string();
            Some(Package {
                name: name.to_string(),
                version,
                ecosystem: "npm",
            })
        })
        .collect())
}

/// `pnpm-lock.yaml`'s `packages` map — real YAML, keys shaped
/// `name@version` (`@scope/name@version` for scoped packages), e.g.
/// `'@babel/code-frame@7.29.0':`. Unlike npm/Yarn, the resolved version
/// lives in the key itself, not a separate field. Peer-dependency
/// resolutions recorded in the newer `snapshots` section duplicate the
/// same identities with a `(peer@version)` suffix appended to the key —
/// deliberately not read here, since they'd just be the same package
/// already captured from `packages` under a noisier key; a defensive
/// `(`-strip handles the rarer case of that suffix appearing directly
/// on a `packages` key in older `lockfileVersion`s. The `lockfileVersion`
/// and `importers` top-level keys are simply never looked at.
pub fn parse_pnpm_lock(content: &str) -> Result<Vec<Package>, String> {
    let parsed: YamlValue =
        serde_yaml::from_str(content).map_err(|e| format!("not valid YAML: {e}"))?;
    let packages = parsed
        .get("packages")
        .and_then(YamlValue::as_mapping)
        .ok_or_else(|| {
            "no \"packages\" map found — is this a pnpm-lock.yaml?".to_string()
        })?;

    Ok(packages
        .iter()
        .filter_map(|(key, _meta)| {
            let key = key.as_str()?;
            let key = key.split('(').next().unwrap_or(key);
            let (name, version) = split_scoped_at(key)?;
            Some(Package {
                name: name.to_string(),
                version: version.to_string(),
                ecosystem: "npm",
            })
        })
        .collect())
}

/// `go.sum`'s `module version hash` triplets. Every module normally
/// appears on two lines — one hashing the module's actual content, one
/// suffixed `/go.mod` hashing just its `go.mod` file — which would
/// otherwise look like two different versions of the same module; the
/// suffix is stripped so both collapse to one `Package` via `dedupe`.
/// A module with no tagged release gets a Go "pseudo-version"
/// (`v0.0.0-<UTC timestamp>-<12-hex-char commit>`, e.g.
/// `v0.0.0-20161208181325-20d25e280405`) instead of a normal semver —
/// captured verbatim, since a go.sum line is just whitespace-separated
/// fields with no version-format validation of its own.
pub fn parse_go_sum(content: &str) -> Vec<Package> {
    content
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let module = parts.next()?;
            let version = parts.next()?;
            let version = version.strip_suffix("/go.mod").unwrap_or(version);
            Some(Package {
                name: module.to_string(),
                version: version.to_string(),
                ecosystem: "Go",
            })
        })
        .collect()
}

/// `Gemfile.lock`'s `GEM` section: a `  remote:` line, a `  specs:`
/// line, then 4-space-indented `name (version)` entries, each optionally
/// followed by their own 6-space-indented dependency constraints (which
/// are skipped — only the top-level entry is a resolved package). A gem
/// with native extensions can appear more than once for different
/// platforms, e.g. `nokogiri (1.19.4)` alongside
/// `nokogiri (1.19.4-x86_64-linux-gnu)`; the platform suffix is stripped
/// so `dedupe` collapses these back into one entry instead of querying
/// OSV for a fake `"1.19.4-x86_64-linux-gnu"` version that doesn't exist
/// on RubyGems. Every other top-level section (`PLATFORMS`,
/// `DEPENDENCIES`, `RUBY VERSION`, and the `BUNDLED WITH` bundler-version
/// footer) ends the `specs:` block and is otherwise ignored entirely —
/// `BUNDLED WITH`'s own indented version line is not a gem and must not
/// be parsed as one.
pub fn parse_gemfile_lock(content: &str) -> Vec<Package> {
    let mut packages = Vec::new();
    let mut in_gem_specs = false;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            // Any top-level section header. `GEM` opens a section that
            // may contain a `specs:` block once we see it; every other
            // header (or a repeated `GEM` from a second remote) closes
            // whatever `specs:` block was open.
            in_gem_specs = false;
            continue;
        }

        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if indent == 2 && trimmed == "specs:" {
            in_gem_specs = true;
            continue;
        }
        if in_gem_specs && indent == 4 {
            if let Some(pkg) = parse_gem_spec_line(trimmed) {
                packages.push(pkg);
            }
        }
    }

    packages
}

/// Parses one `specs:`-block line: `name (version)`, or
/// `name (version-platform)` for a platform-specific gem build. RubyGems
/// versions themselves are dot-separated (`1.2.3`, `1.2.3.pre.1`) and
/// never contain a hyphen — a hyphen only ever introduces the platform
/// suffix Bundler appends — so splitting the parenthesized part on the
/// first `-` cleanly recovers the real version.
fn parse_gem_spec_line(trimmed: &str) -> Option<Package> {
    let open = trimmed.find('(')?;
    let name = trimmed[..open].trim().to_string();
    let inside = trimmed[open + 1..].trim_end_matches(')').trim();
    let version = inside.split('-').next().unwrap_or(inside).to_string();
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some(Package {
        name,
        version,
        ecosystem: "RubyGems",
    })
}

/// Deduplicates (the same package can legitimately appear more than once
/// across a lockfile's dependency graph at the same version) while
/// preserving distinct entries for genuinely different versions of the
/// same package (a real, normal situation in `Cargo.lock`/npm lockfiles).
pub fn dedupe(packages: Vec<Package>) -> Vec<Package> {
    let mut seen: HashMap<(String, String, &'static str), ()> = HashMap::new();
    packages
        .into_iter()
        .filter(|p| {
            seen.insert((p.name.clone(), p.version.clone(), p.ecosystem), ())
                .is_none()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_realistic_cargo_lock() {
        let content = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.229"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "myapp"
version = "0.1.0"
"#;
        let packages = parse_cargo_lock(content).unwrap();
        assert!(packages.contains(&Package {
            name: "serde".to_string(),
            version: "1.0.229".to_string(),
            ecosystem: "crates.io"
        }));
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn parses_a_realistic_npm_lockfile_v3() {
        let content = r#"{
            "name": "myapp",
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "myapp", "version": "1.0.0"},
                "node_modules/lodash": {"version": "4.17.21"},
                "node_modules/@babel/core": {"version": "7.20.0"}
            }
        }"#;
        let packages = parse_npm_lock(content).unwrap();
        assert!(packages.contains(&Package {
            name: "lodash".to_string(),
            version: "4.17.21".to_string(),
            ecosystem: "npm"
        }));
        assert!(packages.contains(&Package {
            name: "@babel/core".to_string(),
            version: "7.20.0".to_string(),
            ecosystem: "npm"
        }));
        assert_eq!(
            packages.len(),
            2,
            "the root project entry must not be treated as a dependency"
        );
    }

    #[test]
    fn parses_requirements_txt_exact_pins_only() {
        let content = "\
# a comment
requests==2.28.0
flask>=2.0  # not an exact pin, skipped
numpy==1.24.0  # trailing comment stripped
";
        let packages = parse_requirements_txt(content);
        assert_eq!(packages.len(), 2);
        assert!(packages.contains(&Package {
            name: "requests".to_string(),
            version: "2.28.0".to_string(),
            ecosystem: "PyPI"
        }));
        assert!(packages.contains(&Package {
            name: "numpy".to_string(),
            version: "1.24.0".to_string(),
            ecosystem: "PyPI"
        }));
    }

    #[test]
    fn dedupe_collapses_identical_entries_but_keeps_distinct_versions() {
        let packages = vec![
            Package {
                name: "a".to_string(),
                version: "1.0".to_string(),
                ecosystem: "npm",
            },
            Package {
                name: "a".to_string(),
                version: "1.0".to_string(),
                ecosystem: "npm",
            },
            Package {
                name: "a".to_string(),
                version: "2.0".to_string(),
                ecosystem: "npm",
            },
        ];
        let result = dedupe(packages);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn malformed_cargo_lock_is_a_clean_error() {
        assert!(parse_cargo_lock("not toml at all {{{").is_err());
    }

    #[test]
    fn malformed_npm_lock_is_a_clean_error() {
        assert!(parse_npm_lock("not json").is_err());
    }

    #[test]
    fn parses_a_realistic_yarn_classic_v1_lockfile() {
        // Verbatim excerpts (each stanza byte-for-byte, only reordered
        // adjacent) from gatsbyjs/gatsby's real, committed yarn.lock: a
        // scoped package, a header listing seven comma-separated semver
        // ranges collapsing to one resolved version, and a package whose
        // real entry has no `integrity` line at all.
        let content = r#"# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
# yarn lockfile v1


"@aashutoshrathi/word-wrap@^1.2.3":
  version "1.2.6"
  resolved "https://registry.yarnpkg.com/@aashutoshrathi/word-wrap/-/word-wrap-1.2.6.tgz#bd9154aec9983f77b3a034ecaa015c2e4201f6cf"
  integrity sha512-1Yjs2SvM8TflER/OD3cOjhWWOZb58A2t7wpE2S9XfBYTiIl+XFhQG2bjy4Pu1I+EAlCNUzRDYDdFwFYUKvXcIA==

minimist@^1.1.0, minimist@^1.1.1, minimist@^1.1.3, minimist@^1.2.0, minimist@^1.2.3, minimist@^1.2.5, minimist@^1.2.6:
  version "1.2.6"
  resolved "https://registry.yarnpkg.com/minimist/-/minimist-1.2.6.tgz#8637a5b759ea0d6e98702cfb3a9283323c93af44"
  integrity sha512-Jsjnk4bw3YJqYzbdyBiNsPWHPfO++UGG749Cxs6peCu5Xg4nrena6OVxOYxrQTqww0Jmwt+Ref8rggumkTLz9Q==

path-to-regexp@0.1.7:
  version "0.1.7"
  resolved "https://registry.yarnpkg.com/path-to-regexp/-/path-to-regexp-0.1.7.tgz#df604178005f522f15eb4490e7247a1bfaa67f8c"

ip@1.1.5:
  version "1.1.5"
  resolved "https://registry.yarnpkg.com/ip/-/ip-1.1.5.tgz#bdded70114290828c0a039e72ef25f5aaec4354a"
  integrity sha1-vd7XARQpCCjAoDnnLvJfWq7ENUo=
"#;
        let packages = parse_yarn_lock(content).unwrap();
        assert!(packages.contains(&Package {
            name: "@aashutoshrathi/word-wrap".to_string(),
            version: "1.2.6".to_string(),
            ecosystem: "npm"
        }));
        assert!(
            packages.contains(&Package {
                name: "minimist".to_string(),
                version: "1.2.6".to_string(),
                ecosystem: "npm"
            }),
            "a 7-way comma-separated range header must still resolve to one package"
        );
        assert!(packages.contains(&Package {
            name: "path-to-regexp".to_string(),
            version: "0.1.7".to_string(),
            ecosystem: "npm"
        }));
        assert!(
            packages.contains(&Package {
                name: "ip".to_string(),
                version: "1.1.5".to_string(),
                ecosystem: "npm"
            }),
            "a real entry with no integrity line must still parse"
        );
        assert_eq!(packages.len(), 4);
    }

    #[test]
    fn parses_a_realistic_yarn_berry_lockfile() {
        // Verbatim excerpts from yarnpkg/berry's own real yarn.lock
        // (Berry/v2+ format, real YAML): a scoped `npm:`-protocol
        // descriptor, a 6-way comma-separated descriptor sharing one
        // resolution, and a real `workspace:` descriptor (a local
        // monorepo package with a synthetic `0.0.0-use.local` version —
        // not a real, independently published version OSV could ever
        // have an opinion on, so it must be skipped, not queried).
        let content = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 10
  cacheKey: 10

"@aashutoshrathi/word-wrap@npm:^1.2.3":
  version: 1.2.6
  resolution: "@aashutoshrathi/word-wrap@npm:1.2.6"
  checksum: 10/6eebd12a5cd03cee38fcb915ef9f4ea557df6a06f642dfc7fe8eb4839eb5c9ca55a382f3604d52c14200b0c214c12af5e1f23d2a6d8e23ef2d016b105a9d6c0a
  languageName: node
  linkType: hard

"@babel/code-frame@npm:^7.0.0, @babel/code-frame@npm:^7.10.4, @babel/code-frame@npm:^7.12.13, @babel/code-frame@npm:^7.16.0, @babel/code-frame@npm:^7.26.2, @babel/code-frame@npm:^7.8.3":
  version: 7.26.2
  resolution: "@babel/code-frame@npm:7.26.2"
  dependencies:
    "@babel/helper-validator-identifier": "npm:^7.25.9"
    js-tokens: "npm:^4.0.0"
    picocolors: "npm:^1.0.0"
  checksum: 10/db2c2122af79d31ca916755331bb4bac96feb2b334cdaca5097a6b467fdd41963b89b14b6836a14f083de7ff887fc78fa1b3c10b14e743d33e12dbfe5ee3d223
  languageName: node
  linkType: hard

"@yarnpkg/builder@workspace:^, @yarnpkg/builder@workspace:packages/yarnpkg-builder":
  version: 0.0.0-use.local
  resolution: "@yarnpkg/builder@workspace:packages/yarnpkg-builder"
  dependencies:
    "@types/semver": "npm:^7.1.0"
    "@yarnpkg/cli": "workspace:^"
    "@yarnpkg/core": "workspace:^"
    "@yarnpkg/fslib": "workspace:^"
    "@yarnpkg/monorepo": "workspace:^"
    chalk: "npm:^4.1.2"
    clipanion: "npm:^4.0.0-rc.2"
    esbuild: "npm:esbuild-wasm@^0.23.0"
    semver: "npm:^7.8.5"
    tslib: "npm:^2.4.0"
    typescript: "catalog:"
  bin:
    builder: ./sources/boot-cli-dev.js
  languageName: unknown
  linkType: soft
"#;
        let packages = parse_yarn_lock(content).unwrap();
        assert!(packages.contains(&Package {
            name: "@aashutoshrathi/word-wrap".to_string(),
            version: "1.2.6".to_string(),
            ecosystem: "npm"
        }));
        assert!(packages.contains(&Package {
            name: "@babel/code-frame".to_string(),
            version: "7.26.2".to_string(),
            ecosystem: "npm"
        }));
        assert_eq!(
            packages.len(),
            2,
            "__metadata and the workspace:-protocol local package must both be excluded"
        );
    }

    #[test]
    fn parses_a_realistic_pnpm_lockfile() {
        // Verbatim excerpts from unjs/defu's real pnpm-lock.yaml
        // (lockfileVersion '9.0'): an unquoted plain key, a scoped
        // package needing a quoted key, and the `lockfileVersion` field
        // itself sitting right next to `packages` — it must not be
        // mistaken for one.
        let content = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    devDependencies:
      typescript:
        specifier: ^6.0.3
        version: 6.0.3

packages:

  defu@6.1.4:
    resolution: {integrity: sha512-BXE4cR/kVZhKZX/RjPEflHaKVhUVl85noU3v6b8apfQEc1x4A+zBxjZ4lN8LqGd6WZ3dl98pY4o717VFmoPp+A==}

  '@babel/code-frame@7.29.0':
    resolution: {integrity: sha512-9NhCeYjq9+3uxgdtp20LSiJXJvN0FeCtNGpJxuMFZ1Kv3cWUNb6DOhJwUvcVCzKGR66cw4njwM6hrJLqgOwbcw==}
    engines: {node: '>=6.9.0'}
"#;
        let packages = parse_pnpm_lock(content).unwrap();
        assert!(packages.contains(&Package {
            name: "defu".to_string(),
            version: "6.1.4".to_string(),
            ecosystem: "npm"
        }));
        assert!(packages.contains(&Package {
            name: "@babel/code-frame".to_string(),
            version: "7.29.0".to_string(),
            ecosystem: "npm"
        }));
        assert_eq!(
            packages.len(),
            2,
            "lockfileVersion/settings/importers must not be read as packages"
        );
    }

    #[test]
    fn parses_a_realistic_go_sum() {
        // Verbatim from spf13/cobra's real go.sum: every module appears
        // on a content-hash line and a `/go.mod`-suffixed line, and
        // `gopkg.in/check.v1` has no tagged release, so it's pinned by a
        // real Go pseudo-version instead of a normal semver.
        let content = "\
github.com/spf13/pflag v1.0.9 h1:9exaQaMOCwffKiiiYk6/BndUBv+iRViNW+4lEMi0PvY=
github.com/spf13/pflag v1.0.9/go.mod h1:McXfInJRrz4CZXVZOBLb0bTZqETkiAhM9Iw0y3An2Bg=
gopkg.in/check.v1 v0.0.0-20161208181325-20d25e280405 h1:yhCVgyC4o1eVCa2tZl7eS0r+SDo693bJlVdllGtEeKM=
gopkg.in/check.v1 v0.0.0-20161208181325-20d25e280405/go.mod h1:Co6ibVJAznAaIkqp8huTwlJQCZ016jof/cbN4VW5Yz0=
";
        let packages = dedupe(parse_go_sum(content));
        assert!(
            packages.contains(&Package {
                name: "github.com/spf13/pflag".to_string(),
                version: "v1.0.9".to_string(),
                ecosystem: "Go"
            }),
            "the content-hash line and the /go.mod-hash line for the same module@version must collapse into one"
        );
        assert!(
            packages.contains(&Package {
                name: "gopkg.in/check.v1".to_string(),
                version: "v0.0.0-20161208181325-20d25e280405".to_string(),
                ecosystem: "Go"
            }),
            "a Go pseudo-version must be captured verbatim, not truncated or reformatted"
        );
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn parses_a_realistic_gemfile_lock() {
        // Verbatim excerpts from heroku/ruby-getting-started's real
        // Gemfile.lock: `rack` (a normal gem), `nokogiri` appearing
        // three times for three different native-extension platform
        // builds all at the same real version, the `PLATFORMS` /
        // `DEPENDENCIES` / `RUBY VERSION` sections in between, and the
        // real `BUNDLED WITH` footer whose own version line must not be
        // mistaken for a gem.
        let content = "\
GEM
  remote: https://rubygems.org/
  specs:
    nokogiri (1.19.4)
      mini_portile2 (~> 2.8.2)
      racc (~> 1.4)
    nokogiri (1.19.4-aarch64-linux-gnu)
      racc (~> 1.4)
    nokogiri (1.19.4-x86_64-linux-gnu)
      racc (~> 1.4)
    rack (3.2.7)

PLATFORMS
  aarch64-linux
  ruby
  x86_64-linux

DEPENDENCIES
  rack

RUBY VERSION
  ruby 3.4.9

BUNDLED WITH
  4.0.5
";
        let packages = dedupe(parse_gemfile_lock(content));
        assert!(
            packages.contains(&Package {
                name: "nokogiri".to_string(),
                version: "1.19.4".to_string(),
                ecosystem: "RubyGems"
            }),
            "the platform suffix on the aarch64/x86_64 builds must be stripped so all three collapse to one real version"
        );
        assert!(packages.contains(&Package {
            name: "rack".to_string(),
            version: "3.2.7".to_string(),
            ecosystem: "RubyGems"
        }));
        assert!(
            !packages.iter().any(|p| p.version == "4.0.5"),
            "BUNDLED WITH's bundler version must never be parsed as a gem"
        );
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn malformed_yarn_berry_lock_is_a_clean_error() {
        assert!(parse_yarn_berry("not valid yaml: [").is_err());
    }

    #[test]
    fn malformed_pnpm_lock_is_a_clean_error() {
        assert!(parse_pnpm_lock("not valid yaml: [").is_err());
    }

    #[test]
    fn pnpm_lock_without_packages_map_is_a_clean_error() {
        assert!(parse_pnpm_lock("lockfileVersion: '9.0'\n").is_err());
    }
}
