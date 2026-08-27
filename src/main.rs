use anyhow::{bail, Context, Result};
use cargo_metadata::{
    CargoOpt, DependencyKind, Metadata, MetadataCommand, PackageId,
};
use clap::Parser;
use semver::Version;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use toml::Value;

#[derive(Parser, Debug)]
#[command(
    name = "cargo-lock-align",
    about = "Align a new project's transitive dependency versions with an existing Cargo.lock"
)]
struct Args {
    /// Path to the old project's Cargo.lock.
    #[arg(long, value_name = "PATH")]
    old_lock: PathBuf,

    /// Path to the new project's Cargo.toml.
    #[arg(long, default_value = "Cargo.toml")]
    manifest_path: PathBuf,

    /// Only print what would be changed. Do not modify Cargo.lock.
    #[arg(long)]
    dry_run: bool,

    /// Do not enable default features.
    #[arg(long)]
    no_default_features: bool,

    /// Enable all features.
    #[arg(long)]
    all_features: bool,

    /// Features to enable.
    ///
    /// Can be specified multiple times:
    /// --features foo --features bar
    #[arg(long = "features", value_delimiter = ',')]
    features: Vec<String>,

    /// Maximum dependency depth to align.
    ///
    /// 1 = direct dependencies' children,
    /// 2 = grandchildren, etc.
    ///
    /// By default there is no limit.
    #[arg(long)]
    max_depth: Option<usize>,

    /// Do not align dev-dependencies.
    #[arg(long)]
    no_dev: bool,

    /// Do not align build-dependencies.
    #[arg(long)]
    no_build: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PackageKey {
    name: String,
    source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolvedPackageKey {
    package: PackageKey,
    version: Version,
}

#[derive(Debug, Clone)]
struct OldPackage {
    key: PackageKey,
    version: Version,
    precise: Option<String>,
}

#[derive(Debug, Clone)]
struct NewPackage {
    id: PackageId,
    name: String,
    version: Version,
    source: Option<String>,
    precise: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let old_packages = load_old_lock(&args.old_lock)?;

    println!(
        "Loaded {} packages from old lockfile",
        old_packages.len()
    );

    let mut aligned: HashSet<ResolvedPackageKey> = HashSet::new();
    let mut skipped: HashSet<ResolvedPackageKey> = HashSet::new();

    let mut pass = 0usize;

    loop {
        println!();
        println!("=== Alignment pass {} ===", pass);

        let metadata = cargo_metadata(&args)?;

        let resolve = metadata
            .resolve
            .as_ref()
            .context("cargo metadata did not return a dependency resolve graph")?;

        let package_map = build_new_package_map(&metadata);

        let new_lock_packages = load_current_lock_for_manifest(&args.manifest_path)
            .unwrap_or_else(|err| {
                eprintln!("warning: failed to load current Cargo.lock: {err}");
                Vec::new()
            });

        let candidates = collect_candidates(
            &metadata,
            resolve,
            &package_map,
            &new_lock_packages,
            &old_packages,
            &mut aligned,
            &mut skipped,
            &args,
        );

        if candidates.is_empty() {
            println!("Nothing left to align.");
            break;
        }

        let ordered_candidates = order_candidates(&candidates, resolve, &args);

        println!();
        println!("Packages to align:");

        for candidate in &ordered_candidates {
            println!(
                "  {} {} -> {}",
                candidate.resolved_key.package.name,
                candidate.current_display(),
                candidate.target_display()
            );
        }

        if args.dry_run {
            println!();
            println!("--dry-run: not modifying Cargo.lock");
            break;
        }

        /*
         * Pin a single package, then re-run cargo metadata. Updating a parent
         * often changes child package versions, so a bulk plan can become stale
         * after the first successful cargo update.
         */
        let Some(candidate) = ordered_candidates.first() else {
            println!("Nothing left to align.");
            break;
        };

        println!(
            "  PIN  {} {} -> {}",
            candidate.resolved_key.package.name,
                candidate.current_display(),
                candidate.target_display()
        );

        cargo_update_precise(
            &args.manifest_path,
            &candidate.resolved_key.package,
            &candidate.new_version,
                &candidate.target_precise,
        )?;

        pass += 1;

        if pass > 500 {
            bail!("Dependency alignment exceeded 500 update passes");
        }
    }

    println!();
    println!("========================================");
    println!("Alignment finished");
    println!("Aligned: {}", aligned.len());
    println!("Skipped: {}", skipped.len());
    println!("========================================");

    Ok(())
}

#[derive(Debug)]
struct Candidate {
    package_id: PackageId,
    resolved_key: ResolvedPackageKey,
    new_version: Version,
    old_version: Version,
    new_precise: Option<String>,
    old_precise: Option<String>,
    target_precise: String,
}

impl Candidate {
    fn current_display(&self) -> String {
        display_package_version(&self.new_version, self.new_precise.as_deref())
    }

    fn target_display(&self) -> String {
        display_package_version(&self.old_version, self.old_precise.as_deref())
    }
}

fn collect_candidates(
    metadata: &Metadata,
    resolve: &cargo_metadata::Resolve,
    package_map: &HashMap<PackageId, NewPackage>,
    new_lock_packages: &[OldPackage],
    old_packages: &[OldPackage],
    aligned: &mut HashSet<ResolvedPackageKey>,
    skipped: &mut HashSet<ResolvedPackageKey>,
    args: &Args,
) -> HashMap<ResolvedPackageKey, Candidate> {
    let roots = workspace_roots(metadata);
    let mut queue = Vec::new();
    let mut seen = HashSet::new();
    let mut candidates = HashMap::new();

    for root in roots {
        let Some(node) = resolve.nodes.iter().find(|node| node.id == root) else {
            continue;
        };

        for dep in &node.deps {
            if should_process_dep(dep.dep_kinds.as_slice(), args) {
                queue.push((dep.pkg.clone(), 1usize));
            }
        }
    }

    while let Some((package_id, depth)) = queue.pop() {
        let Some(new_pkg) = package_map.get(&package_id) else {
            continue;
        };

        let key = PackageKey {
            name: new_pkg.name.clone(),
            source: new_pkg.source.clone(),
        };
        let new_precise = find_old_package(
            new_lock_packages,
            &key,
            &new_pkg.version,
        )
        .and_then(|pkg| pkg.precise.clone())
        .or_else(|| new_pkg.precise.clone());
        let resolved_key = ResolvedPackageKey {
            package: key.clone(),
            version: new_pkg.version.clone(),
        };

        if !seen.insert(resolved_key.clone()) {
            continue;
        }

        if let Some(max_depth) = args.max_depth {
            if depth > max_depth {
                continue;
            }
        }

        if new_pkg.source.is_none() {
            enqueue_dependencies(resolve, &package_id, &mut queue, depth, args);
            continue;
        }

        if aligned.contains(&resolved_key) {
            enqueue_dependencies(resolve, &package_id, &mut queue, depth, args);
            continue;
        }

        if skipped.contains(&resolved_key) {
            continue;
        }

        let Some(old_pkg) =
            find_old_package(old_packages, &key, &new_pkg.version)
        else {
                if has_package_with_matching_source(old_packages, &key) {
                println!(
                    "  SKIP {:<30} new={} (no compatible version in old lock)",
                    key.name,
                    display_package_version(
                        &new_pkg.version,
                        new_precise.as_deref(),
                    )
                );
            } else {
                println!(
                    "  SKIP {:<30} new={} (not found in old lock)",
                    key.name,
                    display_package_version(
                        &new_pkg.version,
                        new_precise.as_deref(),
                    )
                );
            }
            skipped.insert(resolved_key);
            continue;
        };

        if old_pkg.version == new_pkg.version
            && old_pkg.precise == new_pkg.precise
        {
            println!(
                "  OK   {:<30} {}",
                key.name,
                display_package_version(
                    &new_pkg.version,
                    new_precise.as_deref(),
                )
            );

            aligned.insert(resolved_key);
            enqueue_dependencies(resolve, &package_id, &mut queue, depth, args);
            continue;
        }

        let target_precise = old_pkg
            .precise
            .clone()
            .unwrap_or_else(|| old_pkg.version.to_string());

        candidates
            .entry(resolved_key.clone())
            .or_insert_with(|| Candidate {
                package_id: new_pkg.id.clone(),
                resolved_key,
                new_version: new_pkg.version.clone(),
                old_version: old_pkg.version.clone(),
                new_precise,
                old_precise: old_pkg.precise.clone(),
                target_precise,
            });

        enqueue_dependencies(resolve, &package_id, &mut queue, depth, args);
    }

    candidates
}

fn enqueue_dependencies(
    resolve: &cargo_metadata::Resolve,
    package_id: &PackageId,
    queue: &mut Vec<(PackageId, usize)>,
    parent_depth: usize,
    args: &Args,
) {
    let Some(node) = resolve.nodes.iter().find(|node| node.id == *package_id)
    else {
        return;
    };

    for dep in &node.deps {
        if should_process_dep(dep.dep_kinds.as_slice(), args) {
            queue.push((dep.pkg.clone(), parent_depth + 1));
        }
    }
}

fn order_candidates<'a>(
    candidates: &'a HashMap<ResolvedPackageKey, Candidate>,
    resolve: &cargo_metadata::Resolve,
    args: &Args,
) -> Vec<&'a Candidate> {
    let candidates_by_id: HashMap<String, &Candidate> = candidates
        .values()
        .map(|candidate| (candidate.package_id.to_string(), candidate))
        .collect();

    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    let mut incoming_count: HashMap<String, usize> = candidates_by_id
        .keys()
        .map(|id| (id.clone(), 0))
        .collect();

    for candidate in candidates.values() {
        let id = candidate.package_id.to_string();
        let Some(node) = resolve
            .nodes
            .iter()
            .find(|node| node.id == candidate.package_id)
        else {
            continue;
        };

        let mut child_ids = Vec::new();

        for dep in &node.deps {
            if !should_process_dep(dep.dep_kinds.as_slice(), args) {
                continue;
            }

            let dep_id = dep.pkg.to_string();

            if candidates_by_id.contains_key(&dep_id) {
                child_ids.push(dep_id.clone());
                *incoming_count.entry(dep_id).or_insert(0) += 1;
            }
        }

        child_ids.sort();
        child_ids.dedup();
        adjacency.insert(id, child_ids);
    }

    let mut ready = incoming_count
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(id.clone()))
        .collect::<Vec<_>>();
    ready.sort();

    let mut ordered_ids = Vec::new();

    while let Some(id) = ready.first().cloned() {
        ready.remove(0);
        ordered_ids.push(id.clone());

        for child_id in adjacency.get(&id).into_iter().flatten() {
            let Some(count) = incoming_count.get_mut(child_id) else {
                continue;
            };

            *count -= 1;

            if *count == 0 {
                ready.push(child_id.clone());
                ready.sort();
            }
        }
    }

    /*
     * Cycles are unlikely in Cargo's package dependency graph, but keep the
     * output deterministic if the metadata ever contains one.
     */
    let mut remaining = candidates_by_id
        .keys()
        .filter(|id| !ordered_ids.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    remaining.sort();
    ordered_ids.extend(remaining);

    ordered_ids
        .into_iter()
        .filter_map(|id| candidates_by_id.get(&id).copied())
        .collect()
}

fn load_old_lock(path: &Path) -> Result<Vec<OldPackage>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let root: Value = toml::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let packages = root
        .get("package")
        .and_then(Value::as_array)
        .context("old Cargo.lock has no [[package]] entries")?;

    let mut result = Vec::new();

    for package in packages {
        let table = package
            .as_table()
            .context("invalid [[package]] entry")?;

        let name = table
            .get("name")
            .and_then(Value::as_str)
            .context("package.name missing")?
            .to_string();

        let version_str = table
            .get("version")
            .and_then(Value::as_str)
            .context("package.version missing")?;

        let version = Version::parse(version_str)
            .with_context(|| format!("invalid version {version_str}"))?;

        let source = table
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let precise = source.as_deref().and_then(source_precise);

        let key = PackageKey {
            name: name.clone(),
            source: source.as_deref().map(normalize_source_for_key),
        };

        result.push(OldPackage {
            key,
            version,
            precise,
        });
    }

    Ok(result)
}

fn load_current_lock_for_manifest(manifest_path: &Path) -> Result<Vec<OldPackage>> {
    let manifest_path = if manifest_path.is_absolute() {
        manifest_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(manifest_path)
    };
    let manifest_dir = manifest_path
        .parent()
        .with_context(|| format!("{} has no parent directory", manifest_path.display()))?;
    let lock_path = manifest_dir.join("Cargo.lock");

    load_old_lock(&lock_path)
}

fn find_old_package<'a>(
    packages: &'a [OldPackage],
    key: &PackageKey,
    current_version: &Version,
) -> Option<&'a OldPackage> {
    packages
        .iter()
        .filter(|pkg| {
            package_source_matches(&pkg.key, key)
                && same_semver_compatibility_line(
                    &pkg.version,
                    current_version,
                )
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

fn has_package_with_matching_source(
    packages: &[OldPackage],
    key: &PackageKey,
) -> bool {
    packages
        .iter()
        .any(|pkg| package_source_matches(&pkg.key, key))
}

fn package_source_matches(old_key: &PackageKey, new_key: &PackageKey) -> bool {
    if old_key.name != new_key.name {
        return false;
    }

    if old_key.source == new_key.source {
        return true;
    }

    /*
     * Cargo.lock omits `source` for path packages. In the old rust-sdk lockfile,
     * some crates.io packages are patched to local vendor/trick paths, so they
     * appear without a source even though the standalone project resolves the
     * same package from the registry. Treat that as a version-alignment match
     * for registry packages only.
     */
    old_key.source.is_none()
        && new_key
            .source
            .as_deref()
            .is_some_and(is_registry_source)
}

fn is_registry_source(source: &str) -> bool {
    source.starts_with("registry+")
}

fn same_semver_compatibility_line(
    old_version: &Version,
    current_version: &Version,
) -> bool {
    if old_version.major != current_version.major {
        return false;
    }

    if current_version.major > 0 {
        return true;
    }

    if old_version.minor != current_version.minor {
        return false;
    }

    if current_version.minor > 0 {
        return true;
    }

    old_version.patch == current_version.patch
}

fn build_new_package_map(metadata: &Metadata) -> HashMap<PackageId, NewPackage> {
    metadata
        .packages
        .iter()
        .map(|pkg| {
            (
                pkg.id.clone(),
                NewPackage {
                    id: pkg.id.clone(),
                    name: pkg.name.to_string(),
                    version: pkg.version.clone(),
                    source: pkg.source.as_ref().map(|s| {
                        normalize_source_for_key(s.repr.as_str())
                    }),
                    precise: pkg
                        .source
                        .as_ref()
                        .and_then(|s| source_precise(s.repr.as_str())),
                },
            )
        })
        .collect()
}

fn workspace_roots(metadata: &Metadata) -> Vec<PackageId> {
    if let Some(root) = &metadata.resolve {
        if let Some(root_id) = &root.root {
            return vec![root_id.clone()];
        }
    }

    /*
     * For a virtual workspace, use workspace members as roots.
     */
    metadata.workspace_members.clone()
}

fn should_process_dep(
    kinds: &[cargo_metadata::DepKindInfo],
    args: &Args,
) -> bool {
    if kinds.is_empty() {
        return true;
    }

    kinds.iter().any(|kind| match kind.kind {
        DependencyKind::Normal => true,
        DependencyKind::Development => !args.no_dev,
        DependencyKind::Build => !args.no_build,
        _ => true,
    })
}

fn cargo_metadata(args: &Args) -> Result<Metadata> {
    let mut command = MetadataCommand::new();

    command.manifest_path(&args.manifest_path);

    if args.no_default_features {
        command.features(CargoOpt::NoDefaultFeatures);
    }

    if args.all_features {
        command.features(CargoOpt::AllFeatures);
    }

    if !args.features.is_empty() {
        command.features(CargoOpt::SomeFeatures(args.features.clone()));
    }

    let metadata = command
        .exec()
        .context("cargo metadata failed")?;

    Ok(metadata)
}

fn cargo_update_precise(
    manifest_path: &Path,
    key: &PackageKey,
    current_version: &Version,
    target_precise: &str,
) -> Result<()> {
    /*
     * The package specification must identify the package version that is
     * currently present in Cargo.lock. `--precise` is the target version.
     *
     * If there are multiple versions of the same package in the lockfile,
     * Cargo's package specification may need a version qualifier.
     */
    let package_spec = package_spec_for_current_version(key, current_version);

    let status = Command::new("cargo")
        .arg("update")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg("-p")
        .arg(&package_spec)
        .arg("--precise")
        .arg(target_precise)
        .status()
        .with_context(|| {
            format!(
                "failed to execute cargo update for {}",
                key.name
            )
        })?;

    if !status.success() {
        bail!(
            "cargo update failed for {} -> {}",
            key.name,
            target_precise
        );
    }

    Ok(())
}

fn normalize_source_for_key(source: &str) -> String {
    let source_without_precise = source.split_once('#').map_or(source, |(base, _)| base);

    if source_without_precise.starts_with("git+") {
        percent_decode(source_without_precise)
    } else {
        source_without_precise.to_string()
    }
}

fn source_precise(source: &str) -> Option<String> {
    source
        .split_once('#')
        .map(|(_, precise)| precise.to_string())
}

fn percent_decode(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (
                hex_value(bytes[index + 1]),
                hex_value(bytes[index + 2]),
            ) {
                decoded.push((high * 16 + low) as char);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index] as char);
        index += 1;
    }

    decoded
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn display_package_version(version: &Version, precise: Option<&str>) -> String {
    match precise {
        Some(precise) => format!("{}#{}", version, short_precise(precise)),
        None => version.to_string(),
    }
}

fn short_precise(precise: &str) -> &str {
    precise.get(..8).unwrap_or(precise)
}

fn package_spec_for_current_version(
    key: &PackageKey,
    current_version: &Version,
) -> String {
    format!("{}@{}", key.name, current_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> Version {
        Version::parse(value).unwrap()
    }

    fn package(name: &str, version: &str) -> OldPackage {
        OldPackage {
            key: PackageKey {
                name: name.to_string(),
                source: Some(
                    "registry+https://github.com/rust-lang/crates.io-index"
                        .to_string(),
                ),
            },
            version: self::version(version),
            precise: None,
        }
    }

    #[test]
    fn finds_old_package_on_same_compatible_line() {
        let packages = vec![
            package("proc-macro2", "0.4.30"),
            package("proc-macro2", "1.0.95"),
            package("proc-macro2", "1.0.40"),
        ];
        let key = PackageKey {
            name: "proc-macro2".to_string(),
            source: Some(
                "registry+https://github.com/rust-lang/crates.io-index"
                    .to_string(),
            ),
        };

        let old = find_old_package(&packages, &key, &version("1.0.107"))
            .unwrap();

        assert_eq!(old.version, version("1.0.95"));
    }

    #[test]
    fn rejects_incompatible_zero_major_versions() {
        assert!(!same_semver_compatibility_line(
            &version("0.4.30"),
            &version("0.5.0"),
        ));
        assert!(same_semver_compatibility_line(
            &version("0.4.30"),
            &version("0.4.31"),
        ));
    }

    #[test]
    fn package_spec_uses_current_version() {
        let key = PackageKey {
            name: "extend".to_string(),
            source: Some(
                "registry+https://github.com/rust-lang/crates.io-index"
                    .to_string(),
            ),
        };

        assert_eq!(
            package_spec_for_current_version(&key, &version("1.2.0")),
            "extend@1.2.0",
        );
    }

    #[test]
    fn normalizes_git_source_for_matching_but_keeps_precise_rev() {
        let old =
            "git+ssh://code.byted.org/lark/molten-ffi.git?branch=v/0.14.x#090efe06";
        let new =
            "git+ssh://code.byted.org/lark/molten-ffi.git?branch=v%2F0.14.x#87226568";

        assert_eq!(
            normalize_source_for_key(old),
            normalize_source_for_key(new),
        );
        assert_eq!(source_precise(old).as_deref(), Some("090efe06"));
    }

    #[test]
    fn matches_source_less_old_package_to_registry_package() {
        let old = PackageKey {
            name: "backtrace".to_string(),
            source: None,
        };
        let new = PackageKey {
            name: "backtrace".to_string(),
            source: Some(
                "registry+https://github.com/rust-lang/crates.io-index"
                    .to_string(),
            ),
        };

        assert!(package_source_matches(&old, &new));
    }

    #[test]
    fn does_not_match_source_less_old_package_to_git_package() {
        let old = PackageKey {
            name: "molten-ffi".to_string(),
            source: None,
        };
        let new = PackageKey {
            name: "molten-ffi".to_string(),
            source: Some(
                "git+ssh://code.byted.org/lark/molten-ffi.git?branch=v/0.14.x"
                    .to_string(),
            ),
        };

        assert!(!package_source_matches(&old, &new));
    }
}
