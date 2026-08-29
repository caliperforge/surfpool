use std::{
    env,
    fs::{self, File},
    path::Path,
    process::{Command, Stdio},
};

use dialoguer::{Confirm, Input, MultiSelect, console::Style, theme::ColorfulTheme};
use log::debug;
use surfpool_types::{DEFAULT_NETWORK_HOST, DEFAULT_RPC_PORT};
use txtx_addon_network_svm::templates::{
    AccountDirEntry, AccountEntry, GenesisEntry, get_interpolated_addon_template,
    get_interpolated_devnet_signer_template, get_interpolated_header_template,
    get_interpolated_localnet_signer_template, get_interpolated_mainnet_signer_template,
};
use txtx_core::{
    kit::{helpers::fs::FileLocation, indexmap::indexmap},
    manifest::{RunbookMetadata, WorkspaceManifest},
    templates::{TXTX_MANIFEST_TEMPLATE, build_manifest_data},
    types::RunbookSources,
};

use crate::{
    cli::{DEFAULT_SOLANA_KEYPAIR_PATH, get_home_dir},
    types::Framework,
};

pub const SURFPOOL_README_TEMPLATE: &str = include_str!("./templates/readme.md.mst");

mod anchor;
mod native;
mod pinocchio;
mod steel;
mod typhoon;
pub mod utils;

pub struct ProgramFrameworkData {
    pub framework: Framework,
    pub programs: Vec<ProgramMetadata>,
    pub genesis_accounts: Option<Vec<GenesisEntry>>,
    pub accounts: Option<Vec<AccountEntry>>,
    pub accounts_dir: Option<Vec<AccountDirEntry>>,
    pub clones: Option<Vec<String>>,
}

impl ProgramFrameworkData {
    pub fn new(
        framework: Framework,
        programs: Vec<ProgramMetadata>,
        genesis_accounts: Option<Vec<GenesisEntry>>,
        accounts: Option<Vec<AccountEntry>>,
        accounts_dir: Option<Vec<AccountDirEntry>>,
        clones: Option<Vec<String>>,
    ) -> Self {
        Self {
            framework,
            programs,
            genesis_accounts,
            accounts,
            accounts_dir,
            clones,
        }
    }

    pub fn partial(framework: Framework, programs: Vec<ProgramMetadata>) -> Self {
        Self {
            framework,
            programs,
            genesis_accounts: None,
            accounts: None,
            accounts_dir: None,
            clones: None,
        }
    }
}

pub async fn detect_program_frameworks(
    manifest_path: &str,
    test_paths: &[String],
    artifacts_path: Option<&str>,
) -> Result<Option<ProgramFrameworkData>, String> {
    let manifest_location = FileLocation::from_path_string(manifest_path)?;
    let base_dir = manifest_location.get_parent_location()?;
    // Look for Anchor project layout
    // Note: Poseidon projects generate Anchor.toml files, so they will also be identified here
    if let Some(res) =
        anchor::try_get_programs_from_project(base_dir.clone(), test_paths, artifacts_path)
            .map_err(|e| format!("Invalid Anchor project: {e}"))?
    {
        return Ok(Some(res));
    }

    // Look for Steel project layout
    if let Some((framework, programs)) =
        steel::try_get_programs_from_project(base_dir.clone(), artifacts_path)
            .map_err(|e| format!("Invalid Steel project: {e}"))?
    {
        return Ok(Some(ProgramFrameworkData::partial(framework, programs)));
    }

    // Look for Typhoon project layout
    if let Some((framework, programs)) = typhoon::try_get_programs_from_project(base_dir.clone())
        .map_err(|e| format!("Invalid Typhoon project: {e}"))?
    {
        return Ok(Some(ProgramFrameworkData::partial(framework, programs)));
    }

    // Look for Pinocchio project layout
    if let Some((framework, programs)) =
        pinocchio::try_get_programs_from_project(base_dir.clone(), artifacts_path)
            .map_err(|e| format!("Invalid Pinocchio project: {e}"))?
    {
        return Ok(Some(ProgramFrameworkData::partial(framework, programs)));
    }

    // Look for Native project layout
    if let Some((framework, programs)) =
        native::try_get_programs_from_project(base_dir.clone(), artifacts_path)
            .map_err(|e| format!("Invalid Native project: {e}"))?
    {
        return Ok(Some(ProgramFrameworkData::partial(framework, programs)));
    }

    Ok(None)
}

#[derive(Debug, Clone)]
pub struct ProgramMetadata {
    name: String,
    so_exists: bool,
}

impl ProgramMetadata {
    pub fn new(name: &str, so_exists: bool) -> Self {
        Self {
            name: name.to_string(),
            so_exists,
        }
    }
}

const DEV_SKILL_REPO: &str = "https://github.com/solana-foundation/solana-dev-skill";

/// Pinned: an unversioned `npx skills` runs whatever the registry calls latest
/// at the moment of someone's first scaffold, which is not a thing this can
/// promise anyone. An exact version is the only form that resolves the same way
/// twice; a range still floats to the newest release inside it.
const DEV_SKILL_INSTALLER: &str = "skills@1.5.22";

/// Named because the installer, detecting none of its own agents, otherwise fans the
/// skill out to every one of the 77 in its registry: `.claude/`, a non-hidden `agent/`
/// and a lock file all land in a project that asked for none of them. `universal` is
/// its own registry key for the canonical `.agents/skills` copy and symlinks nowhere.
const DEV_SKILL_AGENT: &str = "universal";

/// Where `universal` puts the skill, under the project or under a home that took it
/// globally.
const DEV_SKILL_DIR: &str = ".agents/skills/solana-dev";

/// A remembered "no". Its existence is the whole answer, so deleting it asks again.
const DEV_SKILL_DECLINED_MARKER: &str = ".config/surfpool/dev-skill-declined";

/// Builds the skill install as issue #567 specifies it, pinned, restricted to the one
/// agent whose layout the prompt names, and rooted at the project being scaffolded
/// rather than wherever the process happens to sit.
fn dev_skill_install_command(base_location: &FileLocation) -> Command {
    let mut command = Command::new("npx");
    command.args([
        "-y",
        DEV_SKILL_INSTALLER,
        "add",
        DEV_SKILL_REPO,
        "--skill",
        "*",
        "--agent",
        DEV_SKILL_AGENT,
        "-y",
    ]);
    command.current_dir(base_location.expect_path_buf());
    command
}

/// Starts the skill install and returns, whatever becomes of it.
///
/// This runs on the way to booting someone's surfnet, so it gets no say in
/// that: never waited on, output never reaching the terminal, and nothing at
/// all on a machine without Node — the usual case for a Rust user.
fn spawn_dev_skill_install(mut command: Command) {
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    // Reaped off-thread: the child leaves no defunct entry, the scaffold no wait.
    let _ = hiro_system_kit::thread_named("Dev Skill Install").spawn(move || {
        let _ = child.wait();
    });
}

/// Both scopes the install can already have happened in: this project, or a home
/// that took the skill globally.
fn dev_skill_installed(base: &Path, home: &Path) -> bool {
    base.join(DEV_SKILL_DIR).exists() || home.join(DEV_SKILL_DIR).exists()
}

fn dev_skill_declined(home: &Path) -> bool {
    home.join(DEV_SKILL_DECLINED_MARKER).exists()
}

/// `None` when the prompt could not be put on screen at all, which is not an answer
/// and so is not remembered.
fn prompt_for_dev_skill(theme: &ColorfulTheme) -> Option<bool> {
    Confirm::with_theme(theme)
        .with_prompt(format!(
            "Install the solana-dev skill? It writes {DEV_SKILL_DIR} and skills-lock.json here"
        ))
        .default(true)
        .interact()
        .ok()
}

/// Failures ignored the way `spawn_dev_skill_install` ignores its own: a marker that
/// would not write costs one more prompt later, not a failed scaffold now.
fn record_dev_skill_declined(home: &Path) {
    let marker = home.join(DEV_SKILL_DECLINED_MARKER);
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = File::create(marker);
}

/// The gate, in the order that decides it: an install already on disk and a recorded
/// no both outrank `--yes`, so a second scaffold stays silent and a machine that has
/// declined once is never asked again. `ask` is reached only when nothing on disk has
/// already answered. Paths are arguments so this is testable without a home directory.
fn dev_skill_install_if_wanted(
    base_location: &FileLocation,
    base: &Path,
    home: &Path,
    auto_accept: bool,
    ask: impl FnOnce() -> Option<bool>,
) -> Option<Command> {
    if dev_skill_installed(base, home) || dev_skill_declined(home) {
        return None;
    }
    let consented = match auto_accept {
        true => true,
        false => match ask() {
            Some(true) => true,
            Some(false) => {
                record_dev_skill_declined(home);
                false
            }
            None => false,
        },
    };
    consented.then(|| dev_skill_install_command(base_location))
}

pub fn scaffold_in_memory_iac(
    framework: &Framework,
    programs: &[ProgramMetadata],
    genesis_accounts: &Option<Vec<GenesisEntry>>,
    accounts: &Option<Vec<AccountEntry>>,
    accounts_dir: &Option<Vec<AccountDirEntry>>,
    artifacts_path: Option<&str>,
) -> Result<(String, RunbookSources, WorkspaceManifest), String> {
    let mut deployment_runbook_src: String = String::new();

    deployment_runbook_src.push_str(&get_interpolated_addon_template(
        "input.rpc_api_url",
        "input.network_id",
    ));
    deployment_runbook_src.push_str(&get_interpolated_localnet_signer_template(&format!(
        "\"{}\"",
        *DEFAULT_SOLANA_KEYPAIR_PATH
    )));

    for program_metadata in programs.iter() {
        if program_metadata.so_exists {
            deployment_runbook_src.push_str(
                &framework.get_in_memory_interpolated_program_deployment_template(
                    &program_metadata.name,
                    artifacts_path,
                ),
            );
        } else {
            debug!(
                "Skipping program {} deployment in in-memory IaC since the .so file was not found",
                program_metadata.name
            );
        }
    }

    if let Some(setup_surfnet_iac) = framework.get_interpolated_setup_surfnet_template(
        genesis_accounts.as_ref().unwrap_or(&vec![]),
        accounts.as_ref().unwrap_or(&vec![]),
        accounts_dir.as_ref().unwrap_or(&vec![]),
    ) {
        deployment_runbook_src.push_str(&setup_surfnet_iac);
    }

    let runbook_id = "deployment";
    let mut manifest = WorkspaceManifest::new("memory".to_string());
    let runbook = RunbookMetadata::new(runbook_id, runbook_id, Some("Deploy programs".to_string()));
    manifest.runbooks.push(runbook);

    manifest.environments.insert(
        "localnet".into(),
        indexmap! {
            "network_id".to_string() => "localnet".to_string(),
            "rpc_api_url".to_string() => format!("http://{}:{}", DEFAULT_NETWORK_HOST, DEFAULT_RPC_PORT),
        },
    );

    let mut runbook_sources = RunbookSources::new();
    runbook_sources.add_source(
        runbook_id.to_string(),
        FileLocation::working_dir(),
        deployment_runbook_src,
    );

    Ok((runbook_id.into(), runbook_sources, manifest))
}

pub fn scaffold_iac_layout(
    framework: &Framework,
    programs: &[ProgramMetadata],
    base_location: &FileLocation,
    auto_generate_runbooks: bool,
) -> Result<(), String> {
    let mut target_location = base_location.clone();
    target_location.append_path("target")?;

    let mut txtx_manifest_location = base_location.clone();
    txtx_manifest_location.append_path("txtx.yml")?;

    let manifest_res = WorkspaceManifest::from_location(&txtx_manifest_location);

    let theme = ColorfulTheme {
        values_style: Style::new().green(),
        hint_style: Style::new().cyan(),
        ..ColorfulTheme::default()
    };

    let selected_programs = match auto_generate_runbooks {
        true => programs,
        false => {
            let selection = MultiSelect::with_theme(&theme)
                .with_prompt("Select the programs to deploy (all by default):")
                .items_checked(
                    &programs
                        .iter()
                        .map(|p| (p.name.as_str(), true))
                        .collect::<Vec<_>>(),
                )
                .interact()
                .map_err(|e| format!("unable to select programs to deploy: {e}"))?;

            &selection
                .iter()
                .map(|i| programs[*i].clone())
                .collect::<Vec<_>>()
        }
    };

    // Pick a name for the workspace. By default, we suggest the name of the current directory
    let mut manifest = match manifest_res {
        Ok(manifest) => manifest,
        Err(_) => {
            let current_dir = env::current_dir()
                .ok()
                .and_then(|d| d.file_name().map(|f| f.to_string_lossy().to_string()));
            let default = match current_dir {
                Some(dir) => dir,
                _ => "".to_string(),
            };

            // Ask for the name of the workspace
            let name: String = match auto_generate_runbooks {
                true => default,
                false => Input::with_theme(&theme)
                    .with_prompt("Enter the name of this workspace")
                    .default(default)
                    .interact_text()
                    .map_err(|e| format!("unable to get workspace name: {e}"))?,
            };
            WorkspaceManifest::new(name)
        }
    };

    let mut deployment_runbook_src: String = String::new();
    deployment_runbook_src.push_str(&get_interpolated_header_template(&format!(
        "Manage {} deployment through Crypto Infrastructure as Code",
        manifest.name
    )));
    deployment_runbook_src.push_str(&get_interpolated_addon_template(
        "input.rpc_api_url",
        "input.network_id",
    ));

    let mut signer_mainnet = String::new();
    // signer_mainnet.push_str(&get_interpolated_header_template(&format!("Runbook")));
    // signer_mainnet.push_str(&get_interpolated_addon_template("http://localhost:8899"));
    signer_mainnet.push_str(&get_interpolated_mainnet_signer_template(
        "input.authority_keypair_json",
    ));

    let mut signer_devnet = String::new();
    // signer_testnet.push_str(&get_interpolated_header_template(&format!("Runbook")));
    // signer_testnet.push_str(&get_interpolated_addon_template("http://localhost:8899"));
    signer_devnet.push_str(&get_interpolated_devnet_signer_template());

    let mut signer_localnet = String::new();
    // signer_simnet.push_str(&get_interpolated_header_template(&format!("Runbook")));
    // signer_simnet.push_str(&get_interpolated_addon_template("http://localhost:8899"));
    signer_localnet.push_str(&get_interpolated_localnet_signer_template(&format!(
        "\"{}\"",
        *DEFAULT_SOLANA_KEYPAIR_PATH
    )));

    for program_metadata in selected_programs.iter() {
        deployment_runbook_src.push_str(
            &framework.get_interpolated_program_deployment_template(&program_metadata.name),
        );
    }

    let runbook_name = "deployment";
    let description = Some("Deploy programs".to_string());
    let location = "runbooks/deployment".to_string();

    let runbook = RunbookMetadata {
        location,
        description,
        name: runbook_name.to_string(),
        state: None,
    };

    let mut collision = false;
    for r in manifest.runbooks.iter() {
        if r.name.eq(&runbook.name) {
            collision = true;
        }
    }
    if !collision {
        manifest.runbooks.push(runbook);
    } else {
        // todo
    }

    let mut runbook_file_location = base_location.clone();
    runbook_file_location.append_path("runbooks")?;

    let manifest_location = if let Some(location) = manifest.location.clone() {
        location
    } else {
        let manifest_name = "txtx.yml";
        let mut manifest_location = base_location.clone();
        let _ = manifest_location.append_path(manifest_name);
        let _ = File::create(manifest_location.to_string()).map_err(|e| {
            format!(
                "Failed to create Runbook manifest {}: {e}",
                manifest_location
            )
        })?;
        println!("{} {}", green!("Created manifest"), manifest_name);
        manifest_location
    };

    manifest.environments.insert(
        "localnet".into(),
        indexmap! {
            "network_id".to_string() => "localnet".to_string(),
            "rpc_api_url".to_string() => format!("http://{}:{}", DEFAULT_NETWORK_HOST, DEFAULT_RPC_PORT),
        },
    );
    manifest.environments.insert(
        "devnet".into(),
        indexmap! {
            "network_id".to_string() => "devnet".to_string(),
            "rpc_api_url".to_string() => "https://api.devnet.solana.com".to_string(),
            "payer_keypair_json".to_string() => DEFAULT_SOLANA_KEYPAIR_PATH.clone(),
            "authority_keypair_json".to_string() => DEFAULT_SOLANA_KEYPAIR_PATH.clone(),
        },
    );

    let mut manifest_file = File::create(manifest_location.to_string()).map_err(|e| {
        format!(
            "Failed to create Runbook manifest file {}: {e}",
            manifest_location
        )
    })?;

    let manifest_file_data = build_manifest_data(&manifest);
    let template = mustache::compile_str(TXTX_MANIFEST_TEMPLATE)
        .map_err(|e| format!("Failed to generate Runbook manifest: {e}"))?;
    template
        .render_data(&mut manifest_file, &manifest_file_data)
        .map_err(|e| format!("Failed to render Runbook manifest: {e}"))?;

    // Create runbooks directory
    match runbook_file_location.exists() {
        true => {}
        false => {
            fs::create_dir_all(runbook_file_location.to_string()).map_err(|e| {
                format!(
                    "unable to create parent directory {}\n{}",
                    runbook_file_location, e
                )
            })?;
        }
    }

    let mut readme_file_path = runbook_file_location.clone();
    readme_file_path.append_path("README.md")?;
    match readme_file_path.exists() {
        true => {}
        false => {
            let mut readme_file = File::create(readme_file_path.to_string()).map_err(|e| {
                format!("Failed to create Runbook README {}: {e}", readme_file_path)
            })?;
            let readme_file_data = build_manifest_data(&manifest);
            let template = mustache::compile_str(SURFPOOL_README_TEMPLATE)
                .map_err(|e| format!("Failed to generate Runbook README: {e}"))?;
            template
                .render_data(&mut readme_file, &readme_file_data)
                .map_err(|e| format!("Failed to render Runbook README: {e}"))?;
            println!("{} runbooks/README.md", green!("Created file"));
        }
    }

    runbook_file_location.append_path("deployment")?;
    match runbook_file_location.exists() {
        true => {}
        false => {
            fs::create_dir_all(runbook_file_location.to_string()).map_err(|e| {
                format!(
                    "unable to create parent directory {}\n{}",
                    runbook_file_location, e
                )
            })?;
        }
    }

    // Create runbook
    let runbook_folder_location = runbook_file_location.clone();
    runbook_file_location.append_path("main.tx")?;
    match runbook_file_location.exists() {
        true => {
            // return Err(format!(
            //     "file {} already exists. choose a different runbook name, or rename the existing file",
            //     runbook_file_location.to_string()
            // ))
            return Ok(());
        }
        false => {
            // write main.tx
            let _ = File::create(runbook_file_location.to_string())
                .map_err(|e| format!("Runbook file creation failed: {e}"))?;
            runbook_file_location
                .write_content(deployment_runbook_src.as_bytes())
                .map_err(|e| format!("Failed to write data to Runbook: {e}"))?;
            println!(
                "{} {}",
                green!("Created file"),
                runbook_file_location
                    .get_relative_path_from_base(base_location)
                    .map_err(|e| format!("Invalid Runbook file location: {e}"))?
            );

            // Create local signer
            let mut base_dir = runbook_folder_location.clone();
            base_dir.append_path("signers.localnet.tx")?;
            let _ = File::create(base_dir.to_string())
                .map_err(|e| format!("Failed to create Runbook signer file: {e}"))?;
            base_dir
                .write_content(signer_localnet.as_bytes())
                .map_err(|e| format!("Failed to write data to Runbook signer file: {e}"))?;
            println!(
                "{} {}",
                green!("Created file"),
                base_dir
                    .get_relative_path_from_base(base_location)
                    .map_err(|e| format!("Invalid Runbook file location: {e}"))?
            );

            // Create devnet signer
            let mut base_dir = runbook_folder_location.clone();
            base_dir.append_path("signers.devnet.tx")?;
            let _ = File::create(base_dir.to_string())
                .map_err(|e| format!("Failed to create Runbook signer file: {e}"))?;
            base_dir
                .write_content(signer_devnet.as_bytes())
                .map_err(|e| format!("Failed to write data to Runbook signer file: {e}"))?;
            println!(
                "{} {}",
                green!("Created file"),
                base_dir
                    .get_relative_path_from_base(base_location)
                    .map_err(|e| format!("Invalid Runbook file location: {e}"))?
            );

            // Create mainnet signer
            let mut base_dir = runbook_folder_location.clone();
            base_dir.append_path("signers.mainnet.tx")?;
            let _ = File::create(base_dir.to_string())
                .map_err(|e| format!("Failed to create Runbook signer file: {e}"))?;
            base_dir
                .write_content(signer_mainnet.as_bytes())
                .map_err(|e| format!("Failed to write data to Runbook signer file: {e}"))?;
            println!(
                "{} {}",
                green!("Created file"),
                base_dir
                    .get_relative_path_from_base(base_location)
                    .map_err(|e| format!("Invalid Runbook file location: {e}"))?
            );
        }
    }

    println!("\n{}\n", deployment_runbook_src);

    let confirmation = match auto_generate_runbooks {
        true => true,
        false => Confirm::with_theme(&theme)
            .with_prompt(
                "Review your deployment in 'runbooks/deployment/main.tx' and confirm to continue",
            )
            .default(true)
            .interact()
            .map_err(|e| format!("Failed to confirm write to runbook: {e}"))?,
    };

    if !confirmation {
        println!("Deployment canceled");
    }

    // Last, so the install only follows a scaffold that finished. No longer keyed to
    // `confirmation`: declining the deployment is "not now", and never was "never".
    let home = get_home_dir();
    let base = base_location.expect_path_buf();
    if let Some(command) = dev_skill_install_if_wanted(
        base_location,
        &base,
        Path::new(&home),
        auto_generate_runbooks,
        || prompt_for_dev_skill(&theme),
    ) {
        spawn_dev_skill_install(command);
        println!("{} {}", green!("Installing"), DEV_SKILL_DIR);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, File},
        path::Path,
        process::Command,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::{
        DEV_SKILL_AGENT, DEV_SKILL_DECLINED_MARKER, DEV_SKILL_DIR, DEV_SKILL_INSTALLER,
        DEV_SKILL_REPO, FileLocation, dev_skill_install_command, dev_skill_install_if_wanted,
        spawn_dev_skill_install,
    };

    /// Every gate test runs against these, never a real home directory.
    fn scratch() -> (TempDir, TempDir, FileLocation) {
        let base = TempDir::new().unwrap();
        let home = TempDir::new().unwrap();
        let location = FileLocation::from_path_string(base.path().to_str().unwrap()).unwrap();
        (base, home, location)
    }

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// #567 supplied this invocation literally: the `-y` flags keep it off a
    /// prompt and `--skill "*"` is what makes it the whole bundle. The one
    /// departure from its text is the pinned version, which is here in full so
    /// that dropping the pin has to fail this.
    #[test]
    fn the_install_is_the_command_issue_567_asked_for() {
        let base = FileLocation::from_path_string("/tmp/surfpool-567-scaffold").unwrap();
        let command = dev_skill_install_command(&base);
        assert_eq!(command.get_program(), "npx");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "-y",
                DEV_SKILL_INSTALLER,
                "add",
                DEV_SKILL_REPO,
                "--skill",
                "*",
                "--agent",
                DEV_SKILL_AGENT,
                "-y"
            ]
        );
        // "*" is the installer's own alias for every agent it knows, so naming one
        // agent and naming all of them are one typo apart.
        assert_ne!(DEV_SKILL_AGENT, "*");
        let (package, version) = DEV_SKILL_INSTALLER
            .split_once('@')
            .expect("installer is pinned");
        assert_eq!(package, "skills");
        assert!(
            version.split('.').count() == 3
                && version
                    .split('.')
                    .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            "the installer must stay pinned to an exact version, not a range: {version}"
        );
    }

    /// The install writes into the project being scaffolded, which is the
    /// manifest's directory rather than the shell's. `surfpool start -m
    /// ../elsewhere/txtx.yml` scaffolds a tree the caller is not standing in.
    #[test]
    fn the_install_runs_in_the_scaffolded_project_not_the_process_cwd() {
        let base = FileLocation::from_path_string("/tmp/surfpool-567-scaffold").unwrap();
        assert_eq!(
            dev_skill_install_command(&base).get_current_dir(),
            Some(Path::new("/tmp/surfpool-567-scaffold"))
        );
    }

    /// A skill already on disk is the reviewer's first item: say nothing, do nothing.
    /// Either scope counts, since a globally installed skill is already available here.
    #[test]
    fn an_installed_skill_is_left_alone() {
        for (in_base, in_home) in [(true, false), (false, true)] {
            let (base, home, location) = scratch();
            let root = if in_base { base.path() } else { home.path() };
            fs::create_dir_all(root.join(DEV_SKILL_DIR)).unwrap();
            assert!(
                dev_skill_install_if_wanted(&location, base.path(), home.path(), true, || {
                    panic!("an installed skill must not prompt")
                })
                .is_none(),
                "installed in base={in_base} home={in_home} still produced an install"
            );
        }
    }

    /// The remembered no, and the reason it is checked before `--yes` rather than
    /// after: "never" has to survive the flag, or a CI machine relitigates it hourly.
    #[test]
    fn a_recorded_decline_outranks_the_yes_flag() {
        let (base, home, location) = scratch();
        let marker = home.path().join(DEV_SKILL_DECLINED_MARKER);
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        File::create(&marker).unwrap();
        assert!(
            dev_skill_install_if_wanted(&location, base.path(), home.path(), true, || {
                panic!("a recorded decline must not prompt")
            })
            .is_none()
        );
    }

    /// `--yes` is the codebase's existing "answered in advance", so with nothing on
    /// disk saying otherwise it installs without prompting.
    #[test]
    fn the_yes_flag_installs_without_prompting() {
        let (base, home, location) = scratch();
        assert!(
            dev_skill_install_if_wanted(&location, base.path(), home.path(), true, || {
                panic!("--yes must not prompt")
            })
            .is_some()
        );
    }

    /// The property the old confirmation test guarded, re-expressed against the gate
    /// that replaced it: a no starts no install. It now also has to be remembered.
    #[test]
    fn a_declined_prompt_starts_no_install_and_is_remembered() {
        let (base, home, location) = scratch();
        assert!(
            dev_skill_install_if_wanted(&location, base.path(), home.path(), false, || Some(false))
                .is_none()
        );
        assert!(
            home.path().join(DEV_SKILL_DECLINED_MARKER).exists(),
            "a decline that is not written down is a decline that gets asked again"
        );
        assert!(
            dev_skill_install_if_wanted(&location, base.path(), home.path(), false, || {
                panic!("the recorded decline must not prompt again")
            })
            .is_none()
        );
    }

    /// Accepting is the only path that installs, and a yes is deliberately not
    /// written down: installs are per project, so the question is asked per project.
    #[test]
    fn an_accepted_prompt_installs_and_records_nothing() {
        let (base, home, location) = scratch();
        assert!(
            dev_skill_install_if_wanted(&location, base.path(), home.path(), false, || Some(true))
                .is_some()
        );
        assert!(!home.path().join(DEV_SKILL_DECLINED_MARKER).exists());
    }

    /// The three ways this goes wrong on a real machine: no Node at all, an
    /// install that fails, an install that hangs. Each returns at once and
    /// returns nothing, so the scaffold has neither a value to branch on nor a
    /// wait to be held by — its result and its output are the same either way.
    #[cfg(unix)]
    #[test]
    fn no_outcome_of_the_install_reaches_the_scaffold() {
        let started = Instant::now();
        spawn_dev_skill_install(Command::new("surfpool-567-no-such-binary"));
        spawn_dev_skill_install(sh("exit 1"));
        spawn_dev_skill_install(sh("sleep 30"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the scaffold waited on the install: {:?}",
            started.elapsed()
        );
    }
}
