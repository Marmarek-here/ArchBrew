use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use alpm::{Alpm, SigLevel, TransFlag};
use clap::error::ErrorKind;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;

const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";
const KNOWN_COMMANDS: [&str; 10] = [
    "install",
    "update",
    "upgrade",
    "search",
    "info",
    "uninstall",
    "list",
    "doctor",
    "clean",
    "help",
];
const ARCHBREW_CACHE_ROOT: &str = "/var/tmp/archbrew-cache";

#[derive(Debug, Deserialize)]
struct AurRpcResponse {
    #[serde(default)]
    results: Vec<AurPackageInfo>,
}

#[derive(Clone, Debug, Deserialize)]
struct AurPackageInfo {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Description")]
    description: Option<String>,
    #[serde(rename = "PackageBase")]
    package_base: String,
    #[serde(rename = "Depends", default)]
    depends: Vec<String>,
    #[serde(rename = "MakeDepends", default)]
    makedepends: Vec<String>,
    #[serde(rename = "CheckDepends", default)]
    checkdepends: Vec<String>,
}

#[derive(Clone, Debug)]
struct AurBuildUnit {
    info: AurPackageInfo,
    build_dir: PathBuf,
}

#[derive(Parser, Debug)]
#[command(
    name = "archbrew",
    version,
    about = "A Homebrew-like standalone CLI package manager for Arch",
    before_help = "==> ArchBrew - Homebrew-style package management for Arch",
    long_about = "ArchBrew provides a brew-like interface for installing and managing packages from a local ArchBrew catalog.",
    after_help = "Tip: use 'brew install -l <PKGFILE>' or 'brew install --local <PKGFILE>' for local package files.",
    disable_help_flag = true
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Install one or more packages
    Install {
        packages: Vec<String>,
        #[arg(
            short = 'l',
            long = "local",
            value_name = "PKGFILE",
            help = "Install from a local .pkg.tar.zst file"
        )]
        local: Option<String>,
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Update package databases
    Update {
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Upgrade installed packages
    Upgrade {
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Search for packages in Arch repos and/or AUR
    Search {
        query: String,
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Show package details
    Info {
        package: String,
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Remove one or more packages
    Uninstall {
        packages: Vec<String>,
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// List installed packages
    List {
        #[arg(short, long, value_enum, default_value_t = Source::Auto)]
        source: Source,
    },
    /// Run diagnostics
    Doctor,
    /// Clean ArchBrew cache
    Clean,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Source {
    /// Arch repositories only
    Arch,
    /// AUR only (requires yay or paru)
    Aur,
    /// Prefer Arch and fallback to AUR where relevant
    Auto,
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(1).collect();

    // Accept -v as an alias for -V (--version)
    let args: Vec<String> = env::args()
        .map(|a| if a == "-v" { "-V".to_string() } else { a })
        .collect();

    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => return handle_parse_error(err),
    };

    if let Some(code) = maybe_enter_fakeroot(&cli.command, &raw_args) {
        return code;
    }

    let result = match cli.command {
        Commands::Install {
            packages,
            local,
            source,
        } => install_packages(&packages, local.as_deref(), source),
        Commands::Update { source } => update(source),
        Commands::Upgrade { source } => upgrade(source),
        Commands::Search { query, source } => search(&query, source),
        Commands::Info { package, source } => info(&package, source),
        Commands::Uninstall { packages, source } => uninstall_packages(&packages, source),
        Commands::List { source } => list_installed(source),
        Commands::Doctor => doctor(),
        Commands::Clean => clean_cache(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{RED}error:{RESET} {message}");
            ExitCode::FAILURE
        }
    }
}

fn command_needs_root(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Update { .. }
            | Commands::Upgrade { .. }
            | Commands::Uninstall { .. }
    )
}

fn is_running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|uid| uid.trim() == "0")
        .unwrap_or(false)
}

fn maybe_enter_fakeroot(command: &Commands, raw_args: &[String]) -> Option<ExitCode> {
    if !command_needs_root(command) || is_running_as_root() {
        return None;
    }

    println!("{GREEN}==>{RESET} Entering fakeroot environment");
    let exe = match env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{RED}error:{RESET} failed to resolve executable path: {err}");
            return Some(ExitCode::FAILURE);
        }
    };

    match Command::new("sudo").arg(exe).args(raw_args).status() {
        Ok(status) if status.success() => Some(ExitCode::SUCCESS),
        Ok(_) => Some(ExitCode::FAILURE),
        Err(err) => {
            eprintln!("{RED}error:{RESET} failed to enter fakeroot environment: {err}");
            Some(ExitCode::FAILURE)
        }
    }
}

fn run_with_fakeroot(program: &str, args: &[String]) -> Result<(), String> {
    println!("{GREEN}==>{RESET} Entering fakeroot environment");
    let status = Command::new("sudo")
        .arg(program)
        .args(args)
        .status()
        .map_err(|err| format!("failed to enter fakeroot environment: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} exited with status {status}"))
    }
}

fn current_build_user() -> Result<String, String> {
    env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .map_err(|_| "unable to determine build user".to_string())
}

fn cache_root_for_user(user: &str) -> PathBuf {
    PathBuf::from(ARCHBREW_CACHE_ROOT).join(user)
}

fn aur_snapshot_cache_dir(user: &str) -> PathBuf {
    cache_root_for_user(user).join("snapshots")
}

fn aur_source_cache_dir(user: &str) -> PathBuf {
    cache_root_for_user(user).join("sources")
}

fn aur_sandbox_home_dir(user: &str) -> PathBuf {
    cache_root_for_user(user).join("sandbox-home")
}

fn ensure_dir(path: &PathBuf) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|e| format!("failed to create directory '{}': {e}", path.display()))
}

fn clean_cache() -> Result<(), String> {
    let user = current_build_user()?;
    let user_cache = cache_root_for_user(&user);

    if !user_cache.exists() {
        println!("{GREEN}==>{RESET} ArchBrew cache is already clean");
        return Ok(());
    }

    fs::remove_dir_all(&user_cache)
        .map_err(|e| format!("failed to remove cache '{}': {e}", user_cache.display()))?;
    println!("{GREEN}==>{RESET} Removed ArchBrew cache: {}", user_cache.display());
    Ok(())
}

fn install_packages(packages: &[String], local: Option<&str>, source: Source) -> Result<(), String> {
    if let Some(local_path) = local {
        if !packages.is_empty() {
            return Err("do not mix package names with --local".to_string());
        }
        return install_local_package(local_path);
    }

    if packages.is_empty() {
        return Err("no package names provided (or use --local <file.pkg.tar.zst>)".to_string());
    }

    let mut handle = new_alpm_handle(true)?;
    let localdb = handle.localdb();

    let mut sync_targets = Vec::new();
    let mut aur_targets = Vec::new();
    for package in packages {
        let normalized = package.to_ascii_lowercase();
        if let Ok(installed) = localdb.pkg(normalized.as_str()) {
            println!(
                "{CYAN}==>{RESET} {} already installed ({})",
                installed.name(),
                installed.version().as_str()
            );
            continue;
        }

        if let Some((repo_name, _)) = find_sync_pkg(&handle, &normalized, source) {
            println!("{CYAN}==>{RESET} queued {normalized} from {repo_name}");
            sync_targets.push(normalized);
            continue;
        }

        if source == Source::Arch {
            return Err(format!("package '{normalized}' not found in sync databases"));
        }

        if fetch_aur_info(&normalized)?.is_some() {
            println!("{CYAN}==>{RESET} queued {normalized} from AUR");
            aur_targets.push(normalized);
            continue;
        }

        return Err(format!("package '{normalized}' was not found in Arch repos or AUR"));
    }

    if sync_targets.is_empty() && aur_targets.is_empty() {
        println!("{GREEN}==>{RESET} Nothing to do");
        return Ok(());
    }

    if !sync_targets.is_empty() {
        install_sync_packages(&mut handle, &sync_targets, source)?;
    }

    if !aur_targets.is_empty() {
        install_aur_packages(&aur_targets)?;
    }

    Ok(())
}

fn install_sync_packages(handle: &mut Alpm, targets: &[String], source: Source) -> Result<(), String> {

    if !confirm_action("Proceed with installation?", true)? {
        println!("{YELLOW}==>{RESET} Installation cancelled");
        return Ok(());
    }

    if !is_running_as_root() {
        let mut args = vec!["-S".to_string(), "--needed".to_string()];
        args.extend(targets.iter().cloned());
        run_with_fakeroot("pacman", &args)?;
        for package in targets {
            animate_package_progress("Installing", package)?;
        }
        return Ok(());
    }

    start_transaction(handle)?;

    for package in targets {
        let (_, pkg) = find_sync_pkg(&handle, package, source)
            .ok_or_else(|| format!("package '{package}' disappeared from sync databases"))?;
        handle
            .trans_add_pkg(pkg)
            .map_err(|err| format!("failed to queue '{package}' for install: {}", err.error))?;
    }

    handle
        .trans_prepare()
        .map_err(|err| format!("transaction prepare failed: {}", err.error()))?;
    handle
        .trans_commit()
        .map_err(|err| format!("transaction commit failed: {}", err.error()))?;
    let _ = handle.trans_release();

    for package in targets {
        animate_package_progress("Installing", package)?;
    }

    Ok(())
}

fn install_aur_packages(targets: &[String]) -> Result<(), String> {
    println!("{GREEN}==>{RESET} Resolving AUR targets");
    let mut handle = new_alpm_handle(true)?;

    let mut infos = HashMap::new();
    let mut visiting = HashSet::new();
    let mut topo = Vec::new();

    for target in targets {
        collect_aur_dependency_graph(&handle, target, &mut infos, &mut visiting, &mut topo)?;
    }

    let mut units = prepare_aur_build_units(&topo, &infos)?;
    prompt_pkgbuild_review(&units)?;

    if !confirm_action("Proceed with installation?", true)? {
        println!("{YELLOW}==>{RESET} Installation cancelled");
        return Ok(());
    }

    println!("{GREEN}==>{RESET} Building AUR packages");
    let mut built_packages = Vec::new();
    for unit in &mut units {
        println!("{CYAN}==>{RESET} Building {} {}", unit.info.name, unit.info.version);
        build_aur_unit(unit)?;
        let mut outputs = find_built_package_files(&unit.build_dir)?;
        built_packages.append(&mut outputs);
    }

    install_built_local_packages(&mut handle, &built_packages)
}

fn collect_aur_dependency_graph(
    handle: &Alpm,
    package: &str,
    infos: &mut HashMap<String, AurPackageInfo>,
    visiting: &mut HashSet<String>,
    topo: &mut Vec<String>,
) -> Result<(), String> {
    if topo.iter().any(|p| p == package) {
        return Ok(());
    }
    if !visiting.insert(package.to_string()) {
        return Err(format!("cyclic AUR dependency detected at '{package}'"));
    }

    let info = fetch_aur_info(package)?
        .ok_or_else(|| format!("AUR package '{package}' not found"))?;

    for dep in aur_dependencies(&info) {
        let dep_name = normalize_dep_name(dep);
        if dep_name.is_empty() || dep_name == package {
            continue;
        }

        if handle.localdb().pkg(dep_name.as_str()).is_ok() {
            continue;
        }

        if find_sync_pkg(handle, dep_name.as_str(), Source::Arch).is_some() {
            continue;
        }

        if fetch_aur_info(dep_name.as_str())?.is_some() {
            collect_aur_dependency_graph(handle, dep_name.as_str(), infos, visiting, topo)?;
        }
    }

    infos.insert(package.to_string(), info);
    visiting.remove(package);
    topo.push(package.to_string());
    Ok(())
}

fn aur_dependencies(info: &AurPackageInfo) -> impl Iterator<Item = &str> {
    info.depends
        .iter()
        .chain(info.makedepends.iter())
        .chain(info.checkdepends.iter())
        .map(|s| s.as_str())
}

fn normalize_dep_name(dep: &str) -> String {
    dep.chars()
        .take_while(|c| !matches!(c, '<' | '>' | '='))
        .collect::<String>()
        .trim()
        .to_string()
}

fn fetch_aur_info(name: &str) -> Result<Option<AurPackageInfo>, String> {
    let encoded = url_encode(name);
    let url = format!("https://aur.archlinux.org/rpc/v5/info/{encoded}");
    let body = http_get(&url)?;
    let parsed: AurRpcResponse = serde_json::from_str(&body)
        .map_err(|e| format!("failed to parse AUR response for '{name}': {e}"))?;
    Ok(parsed.results.into_iter().next())
}

fn http_get(url: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsSL", url])
        .output()
        .map_err(|e| format!("failed to execute curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("HTTP request failed for '{url}': {stderr}"));
    }
    String::from_utf8(output.stdout)
        .map_err(|e| format!("response for '{url}' is not UTF-8: {e}"))
}

fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for b in input.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn prepare_aur_build_units(
    order: &[String],
    infos: &HashMap<String, AurPackageInfo>,
) -> Result<Vec<AurBuildUnit>, String> {
    let mut units = Vec::new();
    for name in order {
        let info = infos
            .get(name)
            .ok_or_else(|| format!("missing AUR metadata for '{name}'"))?
            .clone();
        let build_dir = download_aur_snapshot(&info)?;
        units.push(AurBuildUnit { info, build_dir });
    }
    Ok(units)
}

fn download_aur_snapshot(info: &AurPackageInfo) -> Result<PathBuf, String> {
    let build_user = current_build_user()?;
    let snapshot_cache_dir = aur_snapshot_cache_dir(&build_user);
    ensure_dir(&snapshot_cache_dir)?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("failed to read system time: {e}"))?
        .as_millis();
    let root = env::temp_dir().join(format!("archbrew-aur-{}-{stamp}", info.name));
    fs::create_dir_all(&root).map_err(|e| format!("failed to create temp dir: {e}"))?;

    let archive = snapshot_cache_dir.join(format!("{}-{}.tar.gz", info.name, info.package_base));
    let url = format!(
        "https://aur.archlinux.org/cgit/aur.git/snapshot/{}.tar.gz",
        info.package_base
    );

    if !archive.exists() {
        let output = Command::new("curl")
            .args(["-fsSL", &url, "-o"])
            .arg(&archive)
            .output()
            .map_err(|e| format!("failed to download AUR snapshot: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "failed to download AUR snapshot for '{}': {}",
                info.name,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    } else {
        println!(
            "{CYAN}==>{RESET} Using cached AUR snapshot for {}",
            info.name
        );
    }

    let untar = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&root)
        .output()
        .map_err(|e| format!("failed to extract AUR snapshot: {e}"))?;
    if !untar.status.success() {
        return Err(format!(
            "failed to extract AUR snapshot for '{}': {}",
            info.name,
            String::from_utf8_lossy(&untar.stderr)
        ));
    }

    let dir = root.join(&info.package_base);
    if !dir.exists() {
        return Err(format!(
            "snapshot for '{}' did not contain expected directory '{}': {}",
            info.name,
            info.package_base,
            dir.display()
        ));
    }

    Ok(dir)
}

fn prompt_pkgbuild_review(units: &[AurBuildUnit]) -> Result<(), String> {
    if units.is_empty() {
        return Ok(());
    }

    println!("{CYAN}==>{RESET} AUR build queue:");
    for (i, unit) in units.iter().enumerate() {
        println!("  [{}] {} {}", i + 1, unit.info.name, unit.info.version);
        if let Some(desc) = &unit.info.description {
            println!("      {}", desc);
        }
    }

    let number_choices = (1..=units.len())
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("/");
    println!("{CYAN}==>{RESET} Review PKGBUILD files? [A]ll, [X] Number, [N]one");
    let mut stdout = io::stdout();

    loop {
        write!(
            stdout,
            "{CYAN}==>{RESET} Choice [A/{number_choices}/N]: "
        )
            .map_err(|e| format!("failed to write prompt: {e}"))?;
        stdout
            .flush()
            .map_err(|e| format!("failed to flush prompt: {e}"))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("failed to read choice: {e}"))?;

        let trimmed = input.trim().to_ascii_lowercase();

        match trimmed.as_str() {
            "a" | "all" => {
                for unit in units {
                    open_pkgbuild_in_less(unit)?;
                }
                return Ok(());
            }
            "n" | "none" | "" => return Ok(()),
            _ => {
                if let Ok(n) = trimmed.parse::<usize>() {
                    if (1..=units.len()).contains(&n) {
                        open_pkgbuild_in_less(&units[n - 1])?;
                        return Ok(());
                    }
                    println!("{YELLOW}==>{RESET} Package number out of range.");
                    continue;
                }

                println!("{YELLOW}==>{RESET} Enter A, a package number, or N.");
            }
        }
    }
}

fn animate_package_progress(action: &str, package: &str) -> Result<(), String> {
    println!("{GREEN}==>{RESET} {action} {YELLOW}{package}{RESET}");
    Ok(())
}

fn package_name_from_pkg_file(path: &PathBuf) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn open_pkgbuild_in_less(unit: &AurBuildUnit) -> Result<(), String> {
    let path = unit.build_dir.join("PKGBUILD");
    println!(
        "{CYAN}==>{RESET} Reviewing PKGBUILD for {} (press 'q' to continue)",
        unit.info.name
    );
    let status = Command::new("less")
        .arg(&path)
        .status()
        .map_err(|e| format!("failed to launch less: {e}"))?;
    if !status.success() {
        return Err(format!("failed to review PKGBUILD: {}", path.display()));
    }
    Ok(())
}

fn build_aur_unit(unit: &AurBuildUnit) -> Result<(), String> {
    let build_user = current_build_user()?;
    let src_cache_dir = aur_source_cache_dir(&build_user).join(&unit.info.name);
    let sandbox_home = aur_sandbox_home_dir(&build_user);
    ensure_dir(&src_cache_dir)?;
    ensure_dir(&sandbox_home)?;

    let bwrap_check = Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|_| {
            "bubblewrap (bwrap) is required for AUR sandbox builds. Install package 'bubblewrap'."
                .to_string()
        })?;
    if !bwrap_check.status.success() {
        return Err(
            "bubblewrap (bwrap) is required for AUR sandbox builds. Install package 'bubblewrap'."
                .to_string(),
        );
    }

    if is_running_as_root() {
        let owner = format!("{build_user}:{build_user}");
        let status = Command::new("chown")
            .args(["-R", owner.as_str()])
            .arg(&unit.build_dir)
            .arg(&src_cache_dir)
            .arg(&sandbox_home)
            .status()
            .map_err(|e| format!("failed to fix build dir ownership: {e}"))?;
        if !status.success() {
            return Err(format!(
                "failed to set writable ownership for AUR build directories for '{}': {}",
                unit.build_dir.display(),
                owner
            ));
        }
    }

    let makepkg = vec![
        "/usr/bin/makepkg".to_string(),
        "--syncdeps".to_string(),
        "--cleanbuild".to_string(),
        "--clean".to_string(),
        "--needed".to_string(),
        "--noconfirm".to_string(),
    ];

    let bwrap_args = vec![
        "--die-with-parent".to_string(),
        "--ro-bind".to_string(),
        "/".to_string(),
        "/".to_string(),
        "--bind".to_string(),
        unit.build_dir.display().to_string(),
        unit.build_dir.display().to_string(),
        "--bind".to_string(),
        src_cache_dir.display().to_string(),
        src_cache_dir.display().to_string(),
        "--bind".to_string(),
        sandbox_home.display().to_string(),
        sandbox_home.display().to_string(),
        "--bind".to_string(),
        "/tmp".to_string(),
        "/tmp".to_string(),
        "--bind".to_string(),
        "/var/tmp".to_string(),
        "/var/tmp".to_string(),
        "--proc".to_string(),
        "/proc".to_string(),
        "--dev".to_string(),
        "/dev".to_string(),
        "--tmpfs".to_string(),
        "/home".to_string(),
        "--setenv".to_string(),
        "HOME".to_string(),
        sandbox_home.display().to_string(),
        "--setenv".to_string(),
        "SRCDEST".to_string(),
        src_cache_dir.display().to_string(),
        "--setenv".to_string(),
        "PACKAGER".to_string(),
        "ArchBrew".to_string(),
        "--chdir".to_string(),
        unit.build_dir.display().to_string(),
    ];

    let output = if is_running_as_root() {
        let mut args = vec!["-u".to_string(), build_user.clone(), "bwrap".to_string()];
        args.extend(bwrap_args);
        args.extend(makepkg);
        Command::new("sudo")
            .args(args)
            .output()
            .map_err(|e| format!("failed to invoke bwrap/makepkg for '{}': {e}", unit.info.name))?
    } else {
        let mut args = bwrap_args;
        args.extend(makepkg);
        Command::new("bwrap")
            .args(args)
            .output()
            .map_err(|e| format!("failed to invoke bwrap/makepkg for '{}': {e}", unit.info.name))?
    };

    if !output.status.success() {
        return Err(format!(
            "AUR build failed for '{}': {}{}",
            unit.info.name,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

fn find_built_package_files(build_dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = fs::read_dir(build_dir)
        .map_err(|e| format!("failed to read build dir '{}': {e}", build_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz"))
        {
            files.push(path);
        }
    }

    files.sort_by_key(|p| Reverse(p.to_string_lossy().to_string()));
    if files.is_empty() {
        return Err(format!("no built package files found in '{}'", build_dir.display()));
    }

    Ok(files)
}

fn install_built_local_packages(handle: &mut Alpm, pkg_files: &[PathBuf]) -> Result<(), String> {
    if !is_running_as_root() {
        let mut args = vec!["-U".to_string()];
        args.extend(pkg_files.iter().map(|p| p.to_string_lossy().to_string()));
        run_with_fakeroot("pacman", &args)?;
        for file in pkg_files {
            let name = package_name_from_pkg_file(file);
            animate_package_progress("Installing", &name)?;
        }
        return Ok(());
    }

    start_transaction(handle)?;

    for file in pkg_files {
        let loaded = handle
            .pkg_load(file.to_string_lossy().as_ref(), true, SigLevel::NONE)
            .map_err(|e| format!("failed to load built package '{}': {e}", file.display()))?;
        handle
            .trans_add_pkg(loaded)
            .map_err(|e| format!("failed to queue built package '{}': {}", file.display(), e.error))?;
    }

    handle
        .trans_prepare()
        .map_err(|err| format!("transaction prepare failed: {}", err.error()))?;
    handle
        .trans_commit()
        .map_err(|err| format!("transaction commit failed: {}", err.error()))?;
    let _ = handle.trans_release();

    for file in pkg_files {
        let name = package_name_from_pkg_file(file);
        animate_package_progress("Installing", &name)?;
    }
    Ok(())
}

fn update(source: Source) -> Result<(), String> {
    if source == Source::Aur {
        return Err("AUR is not available through libalpm sync databases".to_string());
    }

    println!("{GREEN}==>{RESET} Updating sync databases ({})", source.as_str());
    let mut handle = new_alpm_handle(true)?;
    handle
        .syncdbs_mut()
        .update(false)
        .map_err(|err| format!("sync database update failed: {err}"))?;
    println!("{GREEN}==>{RESET} Update complete");
    Ok(())
}

fn upgrade(source: Source) -> Result<(), String> {
    if source == Source::Aur {
        return Err("AUR is not available through libalpm sync databases".to_string());
    }

    println!("{GREEN}==>{RESET} Upgrading packages");

    let mut handle = new_alpm_handle(true)?;
    start_transaction(&handle)?;
    handle
        .sync_sysupgrade(false)
        .map_err(|err| format!("failed to calculate upgrades: {err}"))?;

    if handle.trans_add().is_empty() && handle.trans_remove().is_empty() {
        let _ = handle.trans_release();
        println!("{GREEN}==>{RESET} Nothing to upgrade");
        return Ok(());
    }

    println!("{CYAN}==>{RESET} Packages to be upgraded:");
    let mut upgraded_names = Vec::new();
    for pkg in handle.trans_add().iter() {
        let current = handle
            .localdb()
            .pkg(pkg.name())
            .ok()
            .map(|local| local.version().as_str().to_string())
            .unwrap_or_else(|| "(new)".to_string());
        println!("  - {} {} -> {}", pkg.name(), current, pkg.version().as_str());
        upgraded_names.push(pkg.name().to_string());
    }

    if !confirm_action("Proceed with upgrade?", true)? {
        let _ = handle.trans_release();
        println!("{YELLOW}==>{RESET} Upgrade cancelled");
        return Ok(());
    }

    handle
        .trans_prepare()
        .map_err(|err| format!("transaction prepare failed: {}", err.error()))?;
    handle
        .trans_commit()
        .map_err(|err| format!("transaction commit failed: {}", err.error()))?;
    let _ = handle.trans_release();

    for package in upgraded_names {
        animate_package_progress("Updating", &package)?;
    }

    println!("{GREEN}==>{RESET} Upgraded packages successfully");
    println!("{GREEN}==>{RESET} Upgrade complete");
    Ok(())
}

fn search(query: &str, source: Source) -> Result<(), String> {
    if source == Source::Aur {
        return Err("AUR search is not available through libalpm sync databases".to_string());
    }

    println!("{GREEN}==>{RESET} Searching for {YELLOW}{query}{RESET}");
    let handle = new_alpm_handle(true)?;
    let localdb = handle.localdb();
    let query_lc = query.to_ascii_lowercase();
    let mut seen = HashSet::new();
    let mut entries: Vec<(String, String, String, bool, String)> = Vec::new();

    for db in handle.syncdbs().iter() {
        if !repo_matches_source(db.name(), source) {
            continue;
        }

        for pkg in db.pkgs().iter() {
            let name = pkg.name();
            let desc = pkg.desc().unwrap_or("");
            if !name.contains(&query_lc) && !desc.to_ascii_lowercase().contains(&query_lc) {
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }
            entries.push((
                name.to_string(),
                pkg.version().as_str().to_string(),
                db.name().to_string(),
                localdb.pkg(name).is_ok(),
                desc.to_string(),
            ));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, version, repo, installed, desc) in entries.iter() {
        let installed_suffix = if *installed { " [installed]" } else { "" };
        println!(
            "  {CYAN}*{RESET} {} {} [{}]{}",
            name,
            version,
            repo,
            installed_suffix
        );
        if !desc.is_empty() {
            println!("    {}", desc);
        }
    }

    println!("{GREEN}==>{RESET} {} result entries", entries.len());
    Ok(())
}

fn info(package: &str, source: Source) -> Result<(), String> {
    if source == Source::Aur {
        return Err("AUR info is not available through libalpm sync databases".to_string());
    }

    println!("{GREEN}==>{RESET} Showing info for {YELLOW}{package}{RESET}");
    let normalized = package.to_ascii_lowercase();
    let handle = new_alpm_handle(true)?;

    let local_pkg = handle.localdb().pkg(normalized.as_str()).ok();
    if let Some((repo_name, sync_pkg)) = find_sync_pkg(&handle, &normalized, source) {
        println!(
            "  {CYAN}{}{RESET} {} [{}]",
            sync_pkg.name(),
            sync_pkg.version().as_str(),
            repo_name
        );
        if let Some(desc) = sync_pkg.desc() {
            println!("    {}", desc);
        }
        let deps = sync_pkg.depends().iter().map(|dep| dep.name()).collect::<Vec<_>>();
        if deps.is_empty() {
            println!("    dependencies: (none)");
        } else {
            println!("    dependencies: {}", deps.join(", "));
        }
        if let Some(pkg) = local_pkg {
            println!("    status: installed ({})", pkg.version().as_str());
        } else {
            println!("    status: not installed");
        }
        return Ok(());
    }

    if let Some(pkg) = handle.localdb().pkg(normalized.as_str()).ok() {
        println!(
            "  {CYAN}{}{RESET} {} [local]",
            pkg.name(),
            pkg.version().as_str()
        );
        if let Some(desc) = pkg.desc() {
            println!("    {}", desc);
        }
        println!("    status: installed");
    } else {
        return Err(format!("package '{package}' was not found in sync/local databases"));
    }

    Ok(())
}

fn uninstall_packages(packages: &[String], source: Source) -> Result<(), String> {
    if packages.is_empty() {
        return Err("no package names provided".to_string());
    }

    if source == Source::Aur {
        return Err("AUR uninstall is not available through libalpm local database".to_string());
    }

    println!("{GREEN}==>{RESET} Removing packages");

    if !confirm_action("Proceed with removal?", true)? {
        println!("{YELLOW}==>{RESET} Removal cancelled");
        return Ok(());
    }

    let mut handle = new_alpm_handle(false)?;
    let remove_set: HashSet<String> = packages
        .iter()
        .map(|pkg| pkg.to_ascii_lowercase())
        .collect();

    start_transaction(&handle)?;

    for package in &remove_set {
        let pkg = handle
            .localdb()
            .pkg(package.as_str())
            .map_err(|_| format!("package '{package}' is not installed"))?;
        handle
            .trans_remove_pkg(pkg)
            .map_err(|err| format!("failed to queue '{package}' for removal: {err}"))?;
    }

    handle
        .trans_prepare()
        .map_err(|err| format!("dependency check failed: {}", err.error()))?;
    handle
        .trans_commit()
        .map_err(|err| format!("transaction commit failed: {}", err.error()))?;
    let _ = handle.trans_release();

    for package in remove_set {
        animate_package_progress("Deleting", &package)?;
    }
    println!("{GREEN}==>{RESET} Remove complete");
    Ok(())
}

fn list_installed(source: Source) -> Result<(), String> {
    if source == Source::Aur {
        return Err("AUR listing is not available through libalpm local database".to_string());
    }

    println!("{GREEN}==>{RESET} Listing installed packages");
    let handle = new_alpm_handle(false)?;
    let mut packages = handle
        .localdb()
        .pkgs()
        .iter()
        .collect::<Vec<_>>();
    packages.sort_by(|a, b| a.name().cmp(b.name()));

    for pkg in packages.iter() {
        println!(
            "  {} {YELLOW}{}{RESET}",
            pkg.name(),
            pkg.version().as_str()
        );
    }

    println!("{GREEN}==>{RESET} {} installed package(s)", packages.len());
    Ok(())
}

fn doctor() -> Result<(), String> {
    println!("{GREEN}==>{RESET} Running diagnostics");

    let mut issues = 0usize;
    let handle = new_alpm_handle(true)?;

    println!("{GREEN}ok{RESET} libalpm initialized");
    println!("{CYAN}==>{RESET} local db path: /var/lib/pacman/local");
    println!(
        "{CYAN}==>{RESET} sync db count: {}",
        handle.syncdbs().len()
    );

    if handle.syncdbs().is_empty() {
        println!("{RED}fail{RESET} no sync databases registered");
        issues += 1;
    }

    if handle.localdb().pkgs().is_empty() {
        println!("{YELLOW}warn{RESET} local package database appears empty");
    } else {
        println!("{GREEN}ok{RESET} local package database is readable");
    }

    if issues == 0 {
        println!("{GREEN}==>{RESET} Your system is ready for ArchBrew");
        Ok(())
    } else {
        Err(format!("diagnostics completed with {issues} issue(s)"))
    }
}

fn repo_matches_source(_repo_name: &str, source: Source) -> bool {
    source != Source::Aur
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Arch => "arch",
            Source::Aur => "aur",
            Source::Auto => "auto",
        }
    }
}

fn find_sync_pkg<'a>(handle: &'a Alpm, name: &str, source: Source) -> Option<(&'a str, &'a alpm::Package)> {
    for db in handle.syncdbs().iter() {
        if !repo_matches_source(db.name(), source) {
            continue;
        }

        if let Ok(pkg) = db.pkg(name) {
            return Some((db.name(), pkg));
        }
    }

    None
}

fn install_local_package(local_path: &str) -> Result<(), String> {
    let mut handle = new_alpm_handle(false)?;
    let resolved = resolve_local_pkg_path(local_path)?;
    println!("===> Source path: {}", resolved.display());

    let loaded_pkg = handle
        .pkg_load(resolved.to_string_lossy().as_ref(), true, SigLevel::NONE)
        .map_err(|err| format!("failed to load local package '{}': {err}", resolved.display()))?;

    let pkg_name = loaded_pkg.name().to_string();
    let pkg_version = loaded_pkg.version().as_str().to_string();

    if let Ok(installed) = handle.localdb().pkg(pkg_name.as_str()) {
        if installed.version().as_str() == pkg_version {
            println!(
                "{CYAN}==>{RESET} {} {} is already installed",
                pkg_name, pkg_version
            );
            return Ok(());
        }
    }

    println!(
        "{GREEN}==>{RESET} Installing local package {YELLOW}{}{RESET} {}",
        pkg_name, pkg_version
    );

    if !confirm_action("Proceed with installation?", true)? {
        println!("{YELLOW}==>{RESET} Installation cancelled");
        return Ok(());
    }

    if !is_running_as_root() {
        run_with_fakeroot(
            "pacman",
            &[
                "-U".to_string(),
                resolved.to_string_lossy().to_string(),
            ],
        )?;
        animate_package_progress("Installing", &pkg_name)?;
        return Ok(());
    }

    start_transaction(&handle)?;
    handle
        .trans_add_pkg(loaded_pkg)
        .map_err(|err| format!("failed to queue local package for install: {}", err.error))?;
    handle
        .trans_prepare()
        .map_err(|err| format!("transaction prepare failed: {}", err.error()))?;
    handle
        .trans_commit()
        .map_err(|err| format!("transaction commit failed: {}", err.error()))?;
    let _ = handle.trans_release();

    animate_package_progress("Installing", &pkg_name)?;
    Ok(())
}

fn resolve_local_pkg_path(path: &str) -> Result<PathBuf, String> {
    if !path.ends_with(".pkg.tar.zst") {
        return Err("--local expects a .pkg.tar.zst file".to_string());
    }

    let candidate = PathBuf::from(path);
    let absolute = if candidate.is_absolute() {
        candidate
    } else {
        env::current_dir()
            .map_err(|err| format!("failed to get current directory: {err}"))?
            .join(candidate)
    };

    if !absolute.exists() {
        return Err(format!("local package '{}' does not exist", absolute.display()));
    }

    Ok(absolute)
}

fn new_alpm_handle(with_sync_dbs: bool) -> Result<Alpm, String> {
    let mut handle = Alpm::new("/", "/var/lib/pacman")
        .map_err(|err| format!("failed to initialize libalpm: {err}"))?;

    if with_sync_dbs {
        register_sync_dbs(&mut handle)?;
    }

    Ok(handle)
}

fn register_sync_dbs(handle: &mut Alpm) -> Result<(), String> {
    let arch = std::process::Command::new("uname")
        .arg("-m")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "x86_64".to_string());

    // Parse /etc/pacman.conf into a list of (repo_name, Vec<server_url>)
    let conf = fs::read_to_string("/etc/pacman.conf")
        .map_err(|e| format!("failed to read /etc/pacman.conf: {e}"))?;

    // Collect servers for each repo from the conf
    let mut repos: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_repo: Option<(String, Vec<String>)> = None;

    for line in conf.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(prev) = current_repo.take() {
                repos.push(prev);
            }
            let section = &line[1..line.len() - 1];
            if section != "options" {
                current_repo = Some((section.to_string(), Vec::new()));
            }
            continue;
        }
        if let Some((repo_name, servers)) = current_repo.as_mut() {
            let repo_name = repo_name.clone();
            if let Some(rest) = line.strip_prefix("Server").and_then(|r| r.trim_start().strip_prefix('=')) {
                let url = rest.trim()
                    .replace("$repo", &repo_name)
                    .replace("$arch", &arch);
                servers.push(url);
            } else if let Some(path) = line.strip_prefix("Include").and_then(|r| r.trim_start().strip_prefix('=')) {
                if let Ok(content) = fs::read_to_string(path.trim()) {
                    for ml in content.lines() {
                        let ml = ml.trim();
                        if ml.starts_with('#') { continue; }
                        if let Some(rest) = ml.strip_prefix("Server").and_then(|r| r.trim_start().strip_prefix('=')) {
                            let url = rest.trim()
                                .replace("$repo", &repo_name)
                                .replace("$arch", &arch);
                            servers.push(url);
                        }
                    }
                }
            }
        }
    }
    if let Some(prev) = current_repo.take() {
        repos.push(prev);
    }

    if repos.is_empty() {
        return Err("no repositories found in /etc/pacman.conf".to_string());
    }

    let mut registered = 0usize;
    for (name, servers) in repos {
        let Ok(db) = handle.register_syncdb_mut(name.as_str(), SigLevel::NONE) else {
            continue;
        };
        for url in &servers {
            let _ = db.add_server(url.as_str());
        }
        registered += 1;
    }

    if registered == 0 {
        return Err("no sync databases could be registered".to_string());
    }

    Ok(())
}

const LOCK_FILE: &str = "/var/lib/pacman/db.lck";
const COMPETING_PKG_MANAGERS: &[&str] = &["pacman", "paru", "yay"];

/// Returns the names of any competing package-manager processes currently running.
fn find_running_pkg_managers() -> Vec<String> {
    COMPETING_PKG_MANAGERS
        .iter()
        .filter(|&&name| {
            Command::new("pgrep")
                .args(["-x", name])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
        .map(|s| s.to_string())
        .collect()
}

/// Kill all instances of each named process with SIGTERM, wait briefly, then SIGKILL.
fn kill_pkg_managers(names: &[String]) -> Result<(), String> {
    for name in names {
        // SIGTERM first
        let _ = Command::new("pkill").args(["-TERM", "-x", name]).status();
    }
    // Give them a moment to exit cleanly
    std::thread::sleep(std::time::Duration::from_millis(800));
    for name in names {
        // SIGKILL any survivors
        let _ = Command::new("pkill").args(["-KILL", "-x", name]).status();
    }
    Ok(())
}

fn start_transaction(handle: &Alpm) -> Result<(), String> {
    match handle.trans_init(TransFlag::NONE) {
        Ok(()) => return Ok(()),
        Err(err) if !err.to_string().to_ascii_lowercase().contains("lock") => {
            return Err(format!("failed to start transaction: {err}"));
        }
        Err(_) => {} // lock conflict — handle below
    }

    let lock_exists = std::path::Path::new(LOCK_FILE).exists();
    let running = find_running_pkg_managers();

    if running.is_empty() && !lock_exists {
        // No stale file and no competing process — almost certainly a permissions problem
        return Err(
            "unable to lock the package database (permission denied).\n\
             Try running with sudo: sudo brew upgrade"
                .into(),
        );
    }

    if running.is_empty() {
        // Stale lock — ask to remove it
        eprintln!(
            "{YELLOW}warn:{RESET} The package database is locked but no package manager appears to be running."
        );
        eprintln!("The lock file may be stale: {LOCK_FILE}");
        if !confirm_action("Remove the stale lock file and continue?", false)? {
            return Err("aborted by user.".into());
        }
        match fs::remove_file(LOCK_FILE) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("failed to remove lock file: {e}")),
        }
    } else {
        // Live processes — confirm before killing
        let list = running.join(", ");
        eprintln!("{YELLOW}warn:{RESET} The following package manager(s) are currently running: {RED}{list}{RESET}");
        eprintln!("Forcibly stopping them may leave the package database in an inconsistent state.");
        if !confirm_action(
            &format!("Force-stop {list} and remove the database lock?"),
            false,
        )? {
            return Err("aborted by user.".into());
        }
        kill_pkg_managers(&running)?;
        // Remove the lock file left behind
        let _ = fs::remove_file(LOCK_FILE);
    }

    // Retry now that the lock is gone
    handle
        .trans_init(TransFlag::NONE)
        .map_err(|e| format!("failed to start transaction after clearing lock: {e}"))
}

fn handle_parse_error(err: clap::Error) -> ExitCode {
    let kind = err.kind();
    if matches!(kind, ErrorKind::InvalidSubcommand | ErrorKind::UnknownArgument) {
        let maybe_cmd = env::args().nth(1).filter(|arg| !arg.starts_with('-'));
        if let Some(cmd) = maybe_cmd
            && let Some(suggested) = suggest_command(&cmd)
        {
            eprintln!("{RED}error:{RESET} unknown command '{cmd}'");
            eprintln!("Did you meant brew {suggested} ?");
            eprintln!("For help, use 'brew help'.");
            return ExitCode::FAILURE;
        }
    }

    eprintln!("{err}");
    ExitCode::FAILURE
}

fn suggest_command(input: &str) -> Option<&'static str> {
    let input = input.to_ascii_lowercase();
    let mut best: Option<(&str, usize)> = None;

    for cmd in KNOWN_COMMANDS {
        let distance = levenshtein(&input, cmd);
        match best {
            Some((_, best_distance)) if distance >= best_distance => {}
            _ => best = Some((cmd, distance)),
        }
    }

    let (candidate, distance) = best?;
    if distance <= 3 || candidate.starts_with(&input) || input.starts_with(candidate) {
        Some(candidate)
    } else {
        None
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut costs: Vec<usize> = (0..=b_chars.len()).collect();

    for (i, ca) in a.chars().enumerate() {
        let mut prev_diag = costs[0];
        costs[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let temp = costs[j + 1];
            let insert = costs[j + 1] + 1;
            let delete = costs[j] + 1;
            let replace = prev_diag + usize::from(ca != *cb);
            costs[j + 1] = insert.min(delete).min(replace);
            prev_diag = temp;
        }
    }

    costs[b_chars.len()]
}

fn confirm_action(prompt: &str, default_yes: bool) -> Result<bool, String> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let mut stdout = io::stdout();

    loop {
        write!(stdout, "{CYAN}==>{RESET} {prompt} {suffix} ")
            .map_err(|err| format!("failed to write prompt: {err}"))?;
        stdout
            .flush()
            .map_err(|err| format!("failed to flush prompt: {err}"))?;

        let mut input = String::new();
        let bytes = io::stdin()
            .read_line(&mut input)
            .map_err(|err| format!("failed to read input: {err}"))?;

        if bytes == 0 {
            return Ok(default_yes);
        }

        let answer = input.trim().to_ascii_lowercase();

        if answer.is_empty() {
            return Ok(default_yes);
        }

        if answer == "y" || answer == "yes" {
            return Ok(true);
        }

        if answer == "n" || answer == "no" {
            return Ok(false);
        }

        println!("{YELLOW}==>{RESET} Please answer 'y' or 'n'.");
    }
}
