use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, anyhow};
use serde::Deserialize;
use txtx_gql::kit::helpers::fs::FileLocation;

use super::ProgramMetadata;

pub fn get_program_metadata_from_manifest_with_dep(
    dependency_indicator: &str,
    base_location: &FileLocation,
    manifest: &CargoManifestFile,
    artifacts_path: Option<&str>,
) -> Result<Option<ProgramMetadata>> {
    let Some(manifest) =
        manifest.get_manifest_with_dependency(dependency_indicator, base_location)?
    else {
        return Ok(None);
    };

    let Some(package) = manifest.package.as_ref() else {
        return Ok(None);
    };

    let program_name = manifest
        .lib
        .as_ref()
        .and_then(|lib| lib.name.clone())
        .unwrap_or_else(|| package.name.replace('-', "_"));

    let so_exists = {
        let so_path_str = if let Some(artifacts) = artifacts_path {
            format!("{}/{}.so", artifacts, program_name)
        } else {
            format!("target/deploy/{}.so", program_name)
        };
        let mut so_path = base_location.clone();
        so_path.append_path(&so_path_str).map_err(|e| {
            anyhow!("failed to construct path to program .so file for existence check: {e}")
        })?;
        so_path.exists()
    };

    Ok(Some(ProgramMetadata::new(&program_name, so_exists)))
}

#[derive(Debug, Clone, Deserialize)]
pub struct CargoManifestFile {
    pub package: Option<Package>,
    pub lib: Option<Lib>,
    pub dependencies: Option<BTreeMap<String, Dependency>>,
    pub workspace: Option<Workspace>,
}

impl CargoManifestFile {
    pub fn from_manifest_str(manifest: &str) -> Result<Self, String> {
        let manifest: CargoManifestFile =
            toml::from_str(manifest).map_err(|e| format!("failed to parse Cargo.toml: {}", e))?;
        Ok(manifest)
    }

    pub fn get_manifest_with_dependency(
        &self,
        name: &str,
        base_location: &FileLocation,
    ) -> Result<Option<CargoManifestFile>> {
        if let Some(deps) = &self.dependencies {
            if deps.get(name).is_some() {
                return Ok(Some(self.clone()));
            }
        }
        if let Some(workspace) = self.workspace.as_ref() {
            for member_manifest in workspace.get_member_cargo_manifests(base_location)? {
                if let Some(manifest) =
                    member_manifest.get_manifest_with_dependency(name, base_location)?
                {
                    return Ok(Some(manifest));
                }
            }
        }
        Ok(None)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Lib {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub members: Vec<String>,

    #[serde(rename = "workspace.dependencies")]
    #[allow(dead_code)]
    pub workspace_dependencies: Option<HashMap<String, Dependency>>,
}

impl Workspace {
    pub fn get_member_cargo_manifests(
        &self,
        base_location: &FileLocation,
    ) -> Result<Vec<CargoManifestFile>> {
        let mut member_manifests = vec![];
        for member in &self.members {
            let mut member_location = base_location.clone();
            member_location
                .append_path(member)
                .map_err(|e| anyhow!("failed to append path: {}", e))?;
            member_location
                .append_path("Cargo.toml")
                .map_err(|e| anyhow!("failed to append path: {}", e))?;
            if member_location.exists() {
                let manifest = member_location
                    .read_content_as_utf8()
                    .map_err(|e| anyhow!("{e}"))?;
                let manifest = CargoManifestFile::from_manifest_str(&manifest)
                    .map_err(|e| anyhow!("unable to read Cargo.toml: {}", e))?;
                member_manifests.push(manifest);
            }
        }
        Ok(member_manifests)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum Dependency {
    Version(String),
    Detailed(DependencyDetail),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct DependencyDetail {
    pub version: Option<String>,
    pub features: Option<Vec<String>>,
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn manifest_with_program_name(manifest: &str) -> ProgramMetadata {
        let temp = tempdir().unwrap();
        let deploy_dir = temp.path().join("target/deploy");
        fs::create_dir_all(&deploy_dir).unwrap();
        fs::write(deploy_dir.join("awesome_app_v2_core.so"), []).unwrap();

        let base_location =
            FileLocation::from_path_string(temp.path().to_string_lossy().as_ref()).unwrap();
        let manifest = CargoManifestFile::from_manifest_str(manifest).unwrap();

        get_program_metadata_from_manifest_with_dep(
            "solana-program",
            &base_location,
            &manifest,
            None,
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn manifest_program_name_prefers_lib_name() {
        let metadata = manifest_with_program_name(
            r#"
[package]
name = "awesome-app-v2-core"

[lib]
name = "awesome_app_v2_core"

[dependencies]
solana-program = "1"
"#,
        );

        assert_eq!(metadata.name, "awesome_app_v2_core");
        assert!(metadata.so_exists);
    }

    #[test]
    fn manifest_program_name_falls_back_to_package_name_without_splitting_digits() {
        let metadata = manifest_with_program_name(
            r#"
[package]
name = "awesome-app-v2-core"

[dependencies]
solana-program = "1"
"#,
        );

        assert_eq!(metadata.name, "awesome_app_v2_core");
        assert_ne!(metadata.name, "awesome_app_v_2_core");
        assert!(metadata.so_exists);
    }

    #[test]
    fn manifest_program_name_falls_back_when_lib_has_no_name() {
        let metadata = manifest_with_program_name(
            r#"
[package]
name = "awesome-app-v2-core"

[lib]
path = "src/lib.rs"
crate-type = ["cdylib", "lib"]

[dependencies]
solana-program = "1"
"#,
        );

        assert_eq!(metadata.name, "awesome_app_v2_core");
        assert!(metadata.so_exists);
    }
}
