use crate::{get_keypair, is_hidden, keys_sync};
use anchor_client::Cluster;
use anchor_lang_idl::types::Idl;
use anyhow::{anyhow, bail, Context, Error, Result};
use clap::{Parser, ValueEnum};
use dirs::home_dir;
use heck::ToSnakeCase;
use reqwest::Url;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use solana_cli_config::{Config as SolanaConfig, CONFIG_FILE};
use solana_sdk::clock::Slot;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solang_parser::pt::{ContractTy, SourceUnitPart};
use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::prelude::*;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt, io};
use walkdir::WalkDir;

pub trait Merge: Sized {
    fn merge(&mut self, _other: Self) {}
}

#[derive(Default, Debug, Parser)]
pub struct ConfigOverride {
    /// Cluster override.
    #[clap(global = true, long = "provider.cluster")]
    pub cluster: Option<Cluster>,
    /// Wallet override.
    #[clap(global = true, long = "provider.wallet")]
    pub wallet: Option<WalletPath>,
}

#[derive(Debug)]
pub struct WithPath<T> {
    inner: T,
    path: PathBuf,
}

impl<T> WithPath<T> {
    pub fn new(inner: T, path: PathBuf) -> Self {
        Self { inner, path }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> std::convert::AsRef<T> for WithPath<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest(cargo_toml::Manifest);

impl Manifest {
    pub fn from_path(p: impl AsRef<Path>) -> Result<Self> {
        cargo_toml::Manifest::from_path(&p)
            .map(Manifest)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("Error reading manifest from path: {}", p.as_ref().display()))
    }

    pub fn lib_name(&self) -> Result<String> {
        match &self.lib {
            Some(cargo_toml::Product {
                name: Some(name), ..
            }) => Ok(name.to_owned()),
            _ => self
                .package
                .as_ref()
                .ok_or_else(|| anyhow!("package section not provided"))
                .map(|pkg| pkg.name.to_snake_case()),
        }
    }

    pub fn version(&self) -> String {
        match &self.package {
            Some(package) => package.version().to_string(),
            _ => "0.0.0".to_string(),
        }
    }

    // Climbs each parent directory from the current dir until we find a Cargo.toml
    pub fn discover() -> Result<Option<WithPath<Manifest>>> {
        Manifest::discover_from_path(std::env::current_dir()?)
    }

    // Climbs each parent directory from a given starting directory until we find a Cargo.toml.
    pub fn discover_from_path(start_from: PathBuf) -> Result<Option<WithPath<Manifest>>> {
        let mut cwd_opt = Some(start_from.as_path());

        while let Some(cwd) = cwd_opt {
            let mut anchor_toml = false;

            for f in fs::read_dir(cwd).with_context(|| {
                format!("Error reading the directory with path: {}", cwd.display())
            })? {
                let p = f
                    .with_context(|| {
                        format!("Error reading the directory with path: {}", cwd.display())
                    })?
                    .path();
                if let Some(filename) = p.file_name().and_then(|name| name.to_str()) {
                    if filename == "Cargo.toml" {
                        return Ok(Some(WithPath::new(Manifest::from_path(&p)?, p)));
                    }
                    if filename == "Anchor.toml" {
                        anchor_toml = true;
                    }
                }
            }

            // Not found. Go up a directory level, but don't go up from Anchor.toml
            if anchor_toml {
                break;
            }

            cwd_opt = cwd.parent();
        }

        Ok(None)
    }
}

impl Deref for Manifest {
    type Target = cargo_toml::Manifest;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl WithPath<Config> {
    pub fn get_rust_program_list(&self) -> Result<Vec<PathBuf>> {
        // Canonicalize the workspace filepaths to compare with relative paths.
        let (members, exclude) = self.canonicalize_workspace()?;

        // Get all candidate programs.
        //
        // If [workspace.members] exists, then use that.
        // Otherwise, default to `programs/*`.
        let program_paths: Vec<PathBuf> = {
            if members.is_empty() {
                let path = self.path().parent().unwrap().join("programs");
                if let Ok(entries) = fs::read_dir(path) {
                    entries
                        .filter(|entry| entry.as_ref().map(|e| e.path().is_dir()).unwrap_or(false))
                        .map(|dir| dir.map(|d| d.path().canonicalize().unwrap()))
                        .collect::<Vec<Result<PathBuf, std::io::Error>>>()
                        .into_iter()
                        .collect::<Result<Vec<PathBuf>, std::io::Error>>()?
                } else {
                    Vec::new()
                }
            } else {
                members
            }
        };

        // Filter out everything part of the exclude array.
        Ok(program_paths
            .into_iter()
            .filter(|m| !exclude.contains(m))
            .collect())
    }

    /// Parse all the files with the .sol extension, and get a list of the all
    /// contracts defined in them along with their path. One Solidity file may
    /// define multiple contracts.
    pub fn get_solidity_program_list(&self) -> Result<Vec<(String, PathBuf)>> {
        let path = self.path().parent().unwrap().join("solidity");
        let mut res = Vec::new();

        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries {
                let path = entry?.path();

                if !path.is_file() || path.extension() != Some(OsStr::new("sol")) {
                    continue;
                }

                let source = fs::read_to_string(&path)?;

                let tree = match solang_parser::parse(&source, 0) {
                    Ok((tree, _)) => tree,
                    Err(diag) => {
                        // The parser can return multiple errors, however this is exceedingly rare.
                        // Just use the first one, else the formatting will be a mess.
                        bail!(
                            "{}: {}: {}",
                            path.display(),
                            diag[0].level.to_string(),
                            diag[0].message
                        );
                    }
                };

                tree.0.iter().for_each(|part| {
                    if let SourceUnitPart::ContractDefinition(contract) = part {
                        // Must be a contract, not library/interface/abstract contract
                        if matches!(&contract.ty, ContractTy::Contract(..)) {
                            if let Some(name) = &contract.name {
                                res.push((name.name.clone(), path.clone()));
                            }
                        }
                    }
                });
            }
        }

        Ok(res)
    }

    pub fn read_all_programs(&self) -> Result<Vec<Program>> {
        let mut r = vec![];
        for path in self.get_rust_program_list()? {
            let cargo = Manifest::from_path(path.join("Cargo.toml"))?;
            let lib_name = cargo.lib_name()?;

            let idl_filepath = Path::new("target")
                .join("idl")
                .join(&lib_name)
                .with_extension("json");
            let idl = fs::read(idl_filepath)
                .ok()
                .map(|bytes| serde_json::from_reader(&*bytes))
                .transpose()?;

            r.push(Program {
                lib_name,
                solidity: false,
                path,
                idl,
            });
        }
        for (lib_name, path) in self.get_solidity_program_list()? {
            let idl_filepath = Path::new("target")
                .join("idl")
                .join(&lib_name)
                .with_extension("json");
            let idl = fs::read(idl_filepath)
                .ok()
                .map(|bytes| serde_json::from_reader(&*bytes))
                .transpose()?;

            r.push(Program {
                lib_name,
                solidity: true,
                path,
                idl,
            });
        }
        Ok(r)
    }

    /// Read and get all the programs from the workspace.
    ///
    /// This method will only return the given program if `name` exists.
    pub fn get_programs(&self, name: Option<String>) -> Result<Vec<Program>> {
        let programs = self.read_all_programs()?;
        let programs = match name {
            Some(name) => vec![programs
                .into_iter()
                .find(|program| {
                    name == program.lib_name
                        || name == program.path.file_name().unwrap().to_str().unwrap()
                })
                .ok_or_else(|| anyhow!("Program {name} not found"))?],
            None => programs,
        };

        Ok(programs)
    }

    /// Get the specified program from the workspace.
    pub fn get_program(&self, name: &str) -> Result<Program> {
        self.get_programs(Some(name.to_owned()))?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Expected a program"))
    }

    pub fn canonicalize_workspace(&self) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let members = self.process_paths(&self.workspace.members)?;
        let exclude = self.process_paths(&self.workspace.exclude)?;
        Ok((members, exclude))
    }

    fn process_paths(&self, paths: &[String]) -> Result<Vec<PathBuf>, Error> {
        let base_path = self.path().parent().unwrap();
        paths
            .iter()
            .flat_map(|m| {
                let path = base_path.join(m);
                if m.ends_with("/*") {
                    let dir = path.parent().unwrap();
                    match fs::read_dir(dir) {
                        Ok(entries) => entries
                            .filter_map(|entry| entry.ok())
                            .map(|entry| self.process_single_path(&entry.path()))
                            .collect(),
                        Err(e) => vec![Err(Error::new(io::Error::other(format!(
                            "Error reading directory {dir:?}: {e}"
                        ))))],
                    }
                } else {
                    vec![self.process_single_path(&path)]
                }
            })
            .collect()
    }

    fn process_single_path(&self, path: &PathBuf) -> Result<PathBuf, Error> {
        path.canonicalize().map_err(|e| {
            Error::new(io::Error::other(format!(
                "Error canonicalizing path {path:?}: {e}"
            )))
        })
    }
}

impl<T> std::ops::Deref for WithPath<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> std::ops::DerefMut for WithPath<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[derive(Debug, Default)]
pub struct Config {
    pub toolchain: ToolchainConfig,
    pub features: FeaturesConfig,
    pub registry: RegistryConfig,
    pub provider: ProviderConfig,
    pub programs: ProgramsConfig,
    pub scripts: ScriptsConfig,
    pub workspace: WorkspaceConfig,
    // Separate entry next to test_config because
    // "anchor localnet" only has access to the Anchor.toml,
    // not the Test.toml files
    pub test_validator: Option<TestValidator>,
    pub test_config: Option<TestConfig>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct ToolchainConfig {
    pub anchor_version: Option<String>,
    pub solana_version: Option<String>,
    pub package_manager: Option<PackageManager>,
}

/// Package manager to use for the project.
#[derive(Clone, Debug, Default, Eq, PartialEq, Parser, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    /// Use npm as the package manager.
    NPM,
    /// Use yarn as the package manager.
    #[default]
    Yarn,
    /// Use pnpm as the package manager.
    PNPM,
    /// Use bun as the package manager.
    Bun,
}

impl std::fmt::Display for PackageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let pkg_manager_str = match self {
            PackageManager::NPM => "npm",
            PackageManager::Yarn => "yarn",
            PackageManager::PNPM => "pnpm",
            PackageManager::Bun => "bun",
        };

        write!(f, "{pkg_manager_str}")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeaturesConfig {
    /// Enable account resolution.
    ///
    /// Not able to specify default bool value: https://github.com/serde-rs/serde/issues/368
    #[serde(default = "FeaturesConfig::get_default_resolution")]
    pub resolution: bool,
    /// Disable safety comment checks
    #[serde(default, rename = "skip-lint")]
    pub skip_lint: bool,
}

impl FeaturesConfig {
    fn get_default_resolution() -> bool {
        true
    }
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            resolution: Self::get_default_resolution(),
            skip_lint: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryConfig {
    pub url: String,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            url: "https://api.apr.dev".to_string(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ProviderConfig {
    pub cluster: Cluster,
    pub wallet: WalletPath,
}

pub type ScriptsConfig = BTreeMap<String, String>;

pub type ProgramsConfig = BTreeMap<Cluster, BTreeMap<String, ProgramDeployment>>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub types: String,
}

#[derive(ValueEnum, Parser, Clone, PartialEq, Eq, Debug)]
pub enum BootstrapMode {
    None,
    Debian,
}

#[derive(ValueEnum, Parser, Clone, PartialEq, Eq, Debug)]
pub enum ProgramArch {
    Bpf,
    Sbf,
}
impl ProgramArch {
    pub fn build_subcommand(&self) -> &str {
        match self {
            Self::Bpf => "build-bpf",
            Self::Sbf => "build-sbf",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub verifiable: bool,
    pub solana_version: Option<String>,
    pub docker_image: String,
    pub bootstrap: BootstrapMode,
}

impl Config {
    pub fn add_test_config(
        &mut self,
        root: impl AsRef<Path>,
        test_paths: Vec<PathBuf>,
    ) -> Result<()> {
        self.test_config = TestConfig::discover(root, test_paths)?;
        Ok(())
    }

    pub fn docker(&self) -> String {
        let version = self
            .toolchain
            .anchor_version
            .as_deref()
            .unwrap_or(crate::DOCKER_BUILDER_VERSION);
        format!("solanafoundation/anchor:v{version}")
    }

    pub fn discover(cfg_override: &ConfigOverride) -> Result<Option<WithPath<Config>>> {
        Config::_discover().map(|opt| {
            opt.map(|mut cfg| {
                if let Some(cluster) = cfg_override.cluster.clone() {
                    cfg.provider.cluster = cluster;
                }
                if let Some(wallet) = cfg_override.wallet.clone() {
                    cfg.provider.wallet = wallet;
                }
                cfg
            })
        })
    }

    // Climbs each parent directory until we find an Anchor.toml.
    fn _discover() -> Result<Option<WithPath<Config>>> {
        let _cwd = std::env::current_dir()?;
        let mut cwd_opt = Some(_cwd.as_path());

        while let Some(cwd) = cwd_opt {
            for f in fs::read_dir(cwd).with_context(|| {
                format!("Error reading the directory with path: {}", cwd.display())
            })? {
                let p = f
                    .with_context(|| {
                        format!("Error reading the directory with path: {}", cwd.display())
                    })?
                    .path();
                if let Some(filename) = p.file_name() {
                    if filename.to_str() == Some("Anchor.toml") {
                        // Make sure the program id is correct (only on the initial build)
                        let mut cfg = Config::from_path(&p)?;
                        let deploy_dir = p.parent().unwrap().join("target").join("deploy");
                        if !deploy_dir.exists() && !cfg.programs.contains_key(&Cluster::Localnet) {
                            println!("Updating program ids...");
                            fs::create_dir_all(deploy_dir)?;
                            keys_sync(&ConfigOverride::default(), None)?;
                            cfg = Config::from_path(&p)?;
                        }

                        return Ok(Some(WithPath::new(cfg, p)));
                    }
                }
            }

            cwd_opt = cwd.parent();
        }

        Ok(None)
    }

    fn from_path(p: impl AsRef<Path>) -> Result<Self> {
        fs::read_to_string(&p)
            .with_context(|| format!("Error reading the file with path: {}", p.as_ref().display()))?
            .parse::<Self>()
    }

    pub fn wallet_kp(&self) -> Result<Keypair> {
        get_keypair(&self.provider.wallet.to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct _Config {
    toolchain: Option<ToolchainConfig>,
    features: Option<FeaturesConfig>,
    programs: Option<BTreeMap<String, BTreeMap<String, serde_json::Value>>>,
    registry: Option<RegistryConfig>,
    provider: Provider,
    workspace: Option<WorkspaceConfig>,
    scripts: Option<ScriptsConfig>,
    test: Option<_TestValidator>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Provider {
    #[serde(serialize_with = "ser_cluster", deserialize_with = "des_cluster")]
    cluster: Cluster,
    wallet: String,
}

fn ser_cluster<S: Serializer>(cluster: &Cluster, s: S) -> Result<S::Ok, S::Error> {
    match cluster {
        Cluster::Custom(http, ws) => {
            match (Url::parse(http), Url::parse(ws)) {
                // If `ws` was derived from `http`, serialize `http` as string
                (Ok(h), Ok(w)) if h.domain() == w.domain() => s.serialize_str(http),
                _ => {
                    let mut map = s.serialize_map(Some(2))?;
                    map.serialize_entry("http", http)?;
                    map.serialize_entry("ws", ws)?;
                    map.end()
                }
            }
        }
        _ => s.serialize_str(&cluster.to_string()),
    }
}

fn des_cluster<'de, D>(deserializer: D) -> Result<Cluster, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrCustomCluster(PhantomData<fn() -> Cluster>);

    impl<'de> Visitor<'de> for StringOrCustomCluster {
        type Value = Cluster;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or map")
        }

        fn visit_str<E>(self, value: &str) -> Result<Cluster, E>
        where
            E: de::Error,
        {
            value.parse().map_err(de::Error::custom)
        }

        fn visit_map<M>(self, mut map: M) -> Result<Cluster, M::Error>
        where
            M: MapAccess<'de>,
        {
            // Gets keys
            if let (Some((http_key, http_value)), Some((ws_key, ws_value))) = (
                map.next_entry::<String, String>()?,
                map.next_entry::<String, String>()?,
            ) {
                // Checks keys
                if http_key != "http" || ws_key != "ws" {
                    return Err(de::Error::custom("Invalid key"));
                }

                // Checks urls
                Url::parse(&http_value).map_err(de::Error::custom)?;
                Url::parse(&ws_value).map_err(de::Error::custom)?;

                Ok(Cluster::Custom(http_value, ws_value))
            } else {
                Err(de::Error::custom("Invalid entry"))
            }
        }
    }
    deserializer.deserialize_any(StringOrCustomCluster(PhantomData))
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let programs = {
            let c = ser_programs(&self.programs);
            if c.is_empty() {
                None
            } else {
                Some(c)
            }
        };
        let cfg = _Config {
            toolchain: Some(self.toolchain.clone()),
            features: Some(self.features.clone()),
            registry: Some(self.registry.clone()),
            provider: Provider {
                cluster: self.provider.cluster.clone(),
                wallet: self.provider.wallet.stringify_with_tilde(),
            },
            test: self.test_validator.clone().map(Into::into),
            scripts: match self.scripts.is_empty() {
                true => None,
                false => Some(self.scripts.clone()),
            },
            programs,
            workspace: (!self.workspace.members.is_empty() || !self.workspace.exclude.is_empty())
                .then(|| self.workspace.clone()),
        };

        let cfg = toml::to_string(&cfg).expect("Must be well formed");
        write!(f, "{cfg}")
    }
}

impl FromStr for Config {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let cfg: _Config =
            toml::from_str(s).map_err(|e| anyhow!("Unable to deserialize config: {e}"))?;
        Ok(Config {
            toolchain: cfg.toolchain.unwrap_or_default(),
            features: cfg.features.unwrap_or_default(),
            registry: cfg.registry.unwrap_or_default(),
            provider: ProviderConfig {
                cluster: cfg.provider.cluster,
                wallet: shellexpand::tilde(&cfg.provider.wallet).parse()?,
            },
            scripts: cfg.scripts.unwrap_or_default(),
            test_validator: cfg.test.map(Into::into),
            test_config: None,
            programs: cfg.programs.map_or(Ok(BTreeMap::new()), deser_programs)?,
            workspace: cfg.workspace.unwrap_or_default(),
        })
    }
}

pub fn get_solana_cfg_url() -> Result<String, io::Error> {
    let config_file = CONFIG_FILE.as_ref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Default Solana config was not found",
        )
    })?;
    SolanaConfig::load(config_file).map(|config| config.json_rpc_url)
}

fn ser_programs(
    programs: &BTreeMap<Cluster, BTreeMap<String, ProgramDeployment>>,
) -> BTreeMap<String, BTreeMap<String, serde_json::Value>> {
    programs
        .iter()
        .map(|(cluster, programs)| {
            let cluster = cluster.to_string();
            let programs = programs
                .iter()
                .map(|(name, deployment)| {
                    (
                        name.clone(),
                        to_value(&_ProgramDeployment::from(deployment)),
                    )
                })
                .collect::<BTreeMap<String, serde_json::Value>>();
            (cluster, programs)
        })
        .collect::<BTreeMap<String, BTreeMap<String, serde_json::Value>>>()
}

fn to_value(dep: &_ProgramDeployment) -> serde_json::Value {
    if dep.path.is_none() && dep.idl.is_none() {
        return serde_json::Value::String(dep.address.to_string());
    }
    serde_json::to_value(dep).unwrap()
}

fn deser_programs(
    programs: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
) -> Result<BTreeMap<Cluster, BTreeMap<String, ProgramDeployment>>> {
    programs
        .iter()
        .map(|(cluster, programs)| {
            let cluster: Cluster = cluster.parse()?;
            let programs = programs
                .iter()
                .map(|(name, program_id)| {
                    Ok((
                        name.clone(),
                        ProgramDeployment::try_from(match &program_id {
                            serde_json::Value::String(address) => _ProgramDeployment {
                                address: address.parse()?,
                                path: None,
                                idl: None,
                            },

                            serde_json::Value::Object(_) => {
                                serde_json::from_value(program_id.clone())
                                    .map_err(|_| anyhow!("Unable to read toml"))?
                            }
                            _ => return Err(anyhow!("Invalid toml type")),
                        })?,
                    ))
                })
                .collect::<Result<BTreeMap<String, ProgramDeployment>>>()?;
            Ok((cluster, programs))
        })
        .collect::<Result<BTreeMap<Cluster, BTreeMap<String, ProgramDeployment>>>>()
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct TestValidator {
    pub genesis: Option<Vec<GenesisEntry>>,
    pub validator: Option<Validator>,
    pub startup_wait: i32,
    pub shutdown_wait: i32,
    pub upgradeable: bool,
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct _TestValidator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genesis: Option<Vec<GenesisEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validator: Option<_Validator>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_wait: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_wait: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgradeable: Option<bool>,
}

pub const STARTUP_WAIT: i32 = 5000;
pub const SHUTDOWN_WAIT: i32 = 2000;

impl From<_TestValidator> for TestValidator {
    fn from(_test_validator: _TestValidator) -> Self {
        Self {
            shutdown_wait: _test_validator.shutdown_wait.unwrap_or(SHUTDOWN_WAIT),
            startup_wait: _test_validator.startup_wait.unwrap_or(STARTUP_WAIT),
            genesis: _test_validator.genesis,
            validator: _test_validator.validator.map(Into::into),
            upgradeable: _test_validator.upgradeable.unwrap_or(false),
        }
    }
}

impl From<TestValidator> for _TestValidator {
    fn from(test_validator: TestValidator) -> Self {
        Self {
            shutdown_wait: Some(test_validator.shutdown_wait),
            startup_wait: Some(test_validator.startup_wait),
            genesis: test_validator.genesis,
            validator: test_validator.validator.map(Into::into),
            upgradeable: Some(test_validator.upgradeable),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestConfig {
    pub test_suite_configs: HashMap<PathBuf, TestToml>,
}

impl Deref for TestConfig {
    type Target = HashMap<PathBuf, TestToml>;

    fn deref(&self) -> &Self::Target {
        &self.test_suite_configs
    }
}

impl TestConfig {
    pub fn discover(root: impl AsRef<Path>, test_paths: Vec<PathBuf>) -> Result<Option<Self>> {
        let walker = WalkDir::new(root).into_iter();
        let mut test_suite_configs = HashMap::new();
        for entry in walker.filter_entry(|e| !is_hidden(e)) {
            let entry = entry?;
            if entry.file_name() == "Test.toml" {
                let entry_path = entry.path();
                let test_toml = TestToml::from_path(entry_path)?;
                if test_paths.is_empty() || test_paths.iter().any(|p| entry_path.starts_with(p)) {
                    test_suite_configs.insert(entry.path().into(), test_toml);
                }
            }
        }

        Ok(match test_suite_configs.is_empty() {
            true => None,
            false => Some(Self { test_suite_configs }),
        })
    }
}

// This file needs to have the same (sub)structure as Anchor.toml
// so it can be parsed as a base test file from an Anchor.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct _TestToml {
    pub extends: Option<Vec<String>>,
    pub test: Option<_TestValidator>,
    pub scripts: Option<ScriptsConfig>,
}

impl _TestToml {
    fn from_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let s = fs::read_to_string(&path)?;
        let parsed_toml: Self = toml::from_str(&s)?;
        let mut current_toml = _TestToml {
            extends: None,
            test: None,
            scripts: None,
        };
        if let Some(bases) = &parsed_toml.extends {
            for base in bases {
                let mut canonical_base = base.clone();
                canonical_base = canonicalize_filepath_from_origin(&canonical_base, &path)?;
                current_toml.merge(_TestToml::from_path(&canonical_base)?);
            }
        }
        current_toml.merge(parsed_toml);

        if let Some(test) = &mut current_toml.test {
            if let Some(genesis_programs) = &mut test.genesis {
                for entry in genesis_programs {
                    entry.program = canonicalize_filepath_from_origin(&entry.program, &path)?;
                }
            }
            if let Some(validator) = &mut test.validator {
                if let Some(ledger_dir) = &mut validator.ledger {
                    *ledger_dir = canonicalize_filepath_from_origin(&ledger_dir, &path)?;
                }
                if let Some(accounts) = &mut validator.account {
                    for entry in accounts {
                        entry.filename = canonicalize_filepath_from_origin(&entry.filename, &path)?;
                    }
                }
            }
        }
        Ok(current_toml)
    }
}

/// canonicalizes the `file_path` arg.
/// uses the `path` arg as the current dir
/// from which to turn the relative path
/// into a canonical one
fn canonicalize_filepath_from_origin(
    file_path: impl AsRef<Path>,
    origin: impl AsRef<Path>,
) -> Result<String> {
    let previous_dir = std::env::current_dir()?;
    std::env::set_current_dir(origin.as_ref().parent().unwrap())?;
    let result = fs::canonicalize(&file_path)
        .with_context(|| {
            format!(
                "Error reading (possibly relative) path: {}. If relative, this is the path that was used as the current path: {}",
                &file_path.as_ref().display(),
                &origin.as_ref().display()
            )
        })?
        .display()
        .to_string();
    std::env::set_current_dir(previous_dir)?;
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestToml {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<TestValidator>,
    pub scripts: ScriptsConfig,
}

impl TestToml {
    pub fn from_path(p: impl AsRef<Path>) -> Result<Self> {
        WithPath::new(_TestToml::from_path(&p)?, p.as_ref().into()).try_into()
    }
}

impl Merge for _TestToml {
    fn merge(&mut self, other: Self) {
        let mut my_scripts = self.scripts.take();
        match &mut my_scripts {
            None => my_scripts = other.scripts,
            Some(my_scripts) => {
                if let Some(other_scripts) = other.scripts {
                    for (name, script) in other_scripts {
                        my_scripts.insert(name, script);
                    }
                }
            }
        }

        let mut my_test = self.test.take();
        match &mut my_test {
            Some(my_test) => {
                if let Some(other_test) = other.test {
                    if let Some(startup_wait) = other_test.startup_wait {
                        my_test.startup_wait = Some(startup_wait);
                    }
                    if let Some(other_genesis) = other_test.genesis {
                        match &mut my_test.genesis {
                            Some(my_genesis) => {
                                for other_entry in other_genesis {
                                    match my_genesis
                                        .iter()
                                        .position(|g| *g.address == other_entry.address)
                                    {
                                        None => my_genesis.push(other_entry),
                                        Some(i) => my_genesis[i] = other_entry,
                                    }
                                }
                            }
                            None => my_test.genesis = Some(other_genesis),
                        }
                    }
                    let mut my_validator = my_test.validator.take();
                    match &mut my_validator {
                        None => my_validator = other_test.validator,
                        Some(my_validator) => {
                            if let Some(other_validator) = other_test.validator {
                                my_validator.merge(other_validator)
                            }
                        }
                    }

                    my_test.validator = my_validator;
                }
            }
            None => my_test = other.test,
        };

        // Instantiating a new Self object here ensures that
        // this function will fail to compile if new fields get added
        // to Self. This is useful as a reminder if they also require merging
        *self = Self {
            test: my_test,
            scripts: my_scripts,
            extends: self.extends.take(),
        };
    }
}

impl TryFrom<WithPath<_TestToml>> for TestToml {
    type Error = Error;

    fn try_from(mut value: WithPath<_TestToml>) -> Result<Self, Self::Error> {
        Ok(Self {
            test: value.test.take().map(Into::into),
            scripts: value
                .scripts
                .take()
                .ok_or_else(|| anyhow!("Missing 'scripts' section in Test.toml file."))?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenesisEntry {
    // Base58 pubkey string.
    pub address: String,
    // Filepath to the compiled program to embed into the genesis.
    pub program: String,
    // Whether the genesis program is upgradeable.
    pub upgradeable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneEntry {
    // Base58 pubkey string.
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountEntry {
    // Base58 pubkey string.
    pub address: String,
    // Name of JSON file containing the account data.
    pub filename: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDirEntry {
    // Directory containing account JSON files
    pub directory: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct _Validator {
    // Load an account from the provided JSON file
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Vec<AccountEntry>>,
    // Load all the accounts from the JSON files found in the specified DIRECTORY
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_dir: Option<Vec<AccountDirEntry>>,
    // IP address to bind the validator ports. [default: 0.0.0.0]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    // Copy an account from the cluster referenced by the url argument.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone: Option<Vec<CloneEntry>>,
    // Range to use for dynamically assigned ports. [default: 1024-65535]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_port_range: Option<String>,
    // Enable the faucet on this port [default: 9900].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faucet_port: Option<u16>,
    // Give the faucet address this much SOL in genesis. [default: 1000000]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faucet_sol: Option<String>,
    // Geyser plugin config location
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geyser_plugin_config: Option<String>,
    // Gossip DNS name or IP address for the validator to advertise in gossip. [default: 127.0.0.1]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gossip_host: Option<String>,
    // Gossip port number for the validator
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gossip_port: Option<u16>,
    // URL for Solana's JSON RPC or moniker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    // Use DIR as ledger location
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ledger: Option<String>,
    // Keep this amount of shreds in root slots. [default: 10000]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_ledger_size: Option<String>,
    // Enable JSON RPC on this port, and the next port for the RPC websocket. [default: 8899]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_port: Option<u16>,
    // Override the number of slots in an epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots_per_epoch: Option<String>,
    // The number of ticks in a slot
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticks_per_slot: Option<u16>,
    // Warp the ledger to WARP_SLOT after starting the validator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warp_slot: Option<Slot>,
    // Deactivate one or more features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deactivate_feature: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Validator {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Vec<AccountEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_dir: Option<Vec<AccountDirEntry>>,
    pub bind_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clone: Option<Vec<CloneEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_port_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faucet_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faucet_sol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geyser_plugin_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gossip_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gossip_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub ledger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_ledger_size: Option<String>,
    pub rpc_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots_per_epoch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticks_per_slot: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warp_slot: Option<Slot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deactivate_feature: Option<Vec<String>>,
}

impl From<_Validator> for Validator {
    fn from(_validator: _Validator) -> Self {
        Self {
            account: _validator.account,
            account_dir: _validator.account_dir,
            bind_address: _validator
                .bind_address
                .unwrap_or_else(|| DEFAULT_BIND_ADDRESS.to_string()),
            clone: _validator.clone,
            dynamic_port_range: _validator.dynamic_port_range,
            faucet_port: _validator.faucet_port,
            faucet_sol: _validator.faucet_sol,
            geyser_plugin_config: _validator.geyser_plugin_config,
            gossip_host: _validator.gossip_host,
            gossip_port: _validator.gossip_port,
            url: _validator.url,
            ledger: _validator
                .ledger
                .unwrap_or_else(|| get_default_ledger_path().display().to_string()),
            limit_ledger_size: _validator.limit_ledger_size,
            rpc_port: _validator
                .rpc_port
                .unwrap_or(solana_sdk::rpc_port::DEFAULT_RPC_PORT),
            slots_per_epoch: _validator.slots_per_epoch,
            ticks_per_slot: _validator.ticks_per_slot,
            warp_slot: _validator.warp_slot,
            deactivate_feature: _validator.deactivate_feature,
        }
    }
}

impl From<Validator> for _Validator {
    fn from(validator: Validator) -> Self {
        Self {
            account: validator.account,
            account_dir: validator.account_dir,
            bind_address: Some(validator.bind_address),
            clone: validator.clone,
            dynamic_port_range: validator.dynamic_port_range,
            faucet_port: validator.faucet_port,
            faucet_sol: validator.faucet_sol,
            geyser_plugin_config: validator.geyser_plugin_config,
            gossip_host: validator.gossip_host,
            gossip_port: validator.gossip_port,
            url: validator.url,
            ledger: Some(validator.ledger),
            limit_ledger_size: validator.limit_ledger_size,
            rpc_port: Some(validator.rpc_port),
            slots_per_epoch: validator.slots_per_epoch,
            ticks_per_slot: validator.ticks_per_slot,
            warp_slot: validator.warp_slot,
            deactivate_feature: validator.deactivate_feature,
        }
    }
}

pub fn get_default_ledger_path() -> PathBuf {
    Path::new(".anchor").join("test-ledger")
}

const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0";

impl Merge for _Validator {
    fn merge(&mut self, other: Self) {
        // Instantiating a new Self object here ensures that
        // this function will fail to compile if new fields get added
        // to Self. This is useful as a reminder if they also require merging
        *self = Self {
            account: match self.account.take() {
                None => other.account,
                Some(mut entries) => match other.account {
                    None => Some(entries),
                    Some(other_entries) => {
                        for other_entry in other_entries {
                            match entries
                                .iter()
                                .position(|my_entry| *my_entry.address == other_entry.address)
                            {
                                None => entries.push(other_entry),
                                Some(i) => entries[i] = other_entry,
                            };
                        }
                        Some(entries)
                    }
                },
            },
            account_dir: match self.account_dir.take() {
                None => other.account_dir,
                Some(mut entries) => match other.account_dir {
                    None => Some(entries),
                    Some(other_entries) => {
                        for other_entry in other_entries {
                            match entries
                                .iter()
                                .position(|my_entry| *my_entry.directory == other_entry.directory)
                            {
                                None => entries.push(other_entry),
                                Some(i) => entries[i] = other_entry,
                            };
                        }
                        Some(entries)
                    }
                },
            },
            bind_address: other.bind_address.or_else(|| self.bind_address.take()),
            clone: match self.clone.take() {
                None => other.clone,
                Some(mut entries) => match other.clone {
                    None => Some(entries),
                    Some(other_entries) => {
                        for other_entry in other_entries {
                            match entries
                                .iter()
                                .position(|my_entry| *my_entry.address == other_entry.address)
                            {
                                None => entries.push(other_entry),
                                Some(i) => entries[i] = other_entry,
                            };
                        }
                        Some(entries)
                    }
                },
            },
            dynamic_port_range: other
                .dynamic_port_range
                .or_else(|| self.dynamic_port_range.take()),
            faucet_port: other.faucet_port.or_else(|| self.faucet_port.take()),
            faucet_sol: other.faucet_sol.or_else(|| self.faucet_sol.take()),
            geyser_plugin_config: other
                .geyser_plugin_config
                .or_else(|| self.geyser_plugin_config.take()),
            gossip_host: other.gossip_host.or_else(|| self.gossip_host.take()),
            gossip_port: other.gossip_port.or_else(|| self.gossip_port.take()),
            url: other.url.or_else(|| self.url.take()),
            ledger: other.ledger.or_else(|| self.ledger.take()),
            limit_ledger_size: other
                .limit_ledger_size
                .or_else(|| self.limit_ledger_size.take()),
            rpc_port: other.rpc_port.or_else(|| self.rpc_port.take()),
            slots_per_epoch: other
                .slots_per_epoch
                .or_else(|| self.slots_per_epoch.take()),
            ticks_per_slot: other.ticks_per_slot.or_else(|| self.ticks_per_slot.take()),
            warp_slot: other.warp_slot.or_else(|| self.warp_slot.take()),
            deactivate_feature: other
                .deactivate_feature
                .or_else(|| self.deactivate_feature.take()),
        };
    }
}

#[derive(Debug, Clone)]
pub struct Program {
    pub lib_name: String,
    pub solidity: bool,
    // Canonicalized path to the program directory or Solidity source file
    pub path: PathBuf,
    pub idl: Option<Idl>,
}

impl Program {
    pub fn pubkey(&self) -> Result<Pubkey> {
        self.keypair().map(|kp| kp.pubkey())
    }

    pub fn keypair(&self) -> Result<Keypair> {
        let file = self.keypair_file()?;
        get_keypair(file.path().to_str().unwrap())
    }

    // Lazily initializes the keypair file with a new key if it doesn't exist.
    pub fn keypair_file(&self) -> Result<WithPath<File>> {
        let deploy_dir_path = Path::new("target").join("deploy");
        fs::create_dir_all(&deploy_dir_path)
            .with_context(|| format!("Error creating directory with path: {deploy_dir_path:?}"))?;
        let path = std::env::current_dir()
            .expect("Must have current dir")
            .join(deploy_dir_path.join(format!("{}-keypair.json", self.lib_name)));
        if path.exists() {
            return Ok(WithPath::new(
                File::open(&path)
                    .with_context(|| format!("Error opening file with path: {}", path.display()))?,
                path,
            ));
        }
        let program_kp = Keypair::new();
        let mut file = File::create(&path)
            .with_context(|| format!("Error creating file with path: {}", path.display()))?;
        file.write_all(format!("{:?}", &program_kp.to_bytes()).as_bytes())?;
        Ok(WithPath::new(file, path))
    }

    pub fn binary_path(&self, verifiable: bool) -> PathBuf {
        let path = Path::new("target")
            .join(if verifiable { "verifiable" } else { "deploy" })
            .join(&self.lib_name)
            .with_extension("so");

        std::env::current_dir()
            .expect("Must have current dir")
            .join(path)
    }
}

#[derive(Debug, Default)]
pub struct ProgramDeployment {
    pub address: Pubkey,
    pub path: Option<String>,
    pub idl: Option<String>,
}

impl TryFrom<_ProgramDeployment> for ProgramDeployment {
    type Error = anyhow::Error;
    fn try_from(pd: _ProgramDeployment) -> Result<Self, Self::Error> {
        Ok(ProgramDeployment {
            address: pd.address.parse()?,
            path: pd.path,
            idl: pd.idl,
        })
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct _ProgramDeployment {
    pub address: String,
    pub path: Option<String>,
    pub idl: Option<String>,
}

impl From<&ProgramDeployment> for _ProgramDeployment {
    fn from(pd: &ProgramDeployment) -> Self {
        Self {
            address: pd.address.to_string(),
            path: pd.path.clone(),
            idl: pd.idl.clone(),
        }
    }
}

pub struct ProgramWorkspace {
    pub name: String,
    pub program_id: Pubkey,
    pub idl: Idl,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnchorPackage {
    pub name: String,
    pub address: String,
    pub idl: Option<String>,
}

impl AnchorPackage {
    pub fn from(name: String, cfg: &WithPath<Config>) -> Result<Self> {
        let cluster = &cfg.provider.cluster;
        if cluster != &Cluster::Mainnet {
            return Err(anyhow!("Publishing requires the mainnet cluster"));
        }
        let program_details = cfg
            .programs
            .get(cluster)
            .ok_or_else(|| anyhow!("Program not provided in Anchor.toml"))?
            .get(&name)
            .ok_or_else(|| anyhow!("Program not provided in Anchor.toml"))?;
        let idl = program_details.idl.clone();
        let address = program_details.address.to_string();
        Ok(Self { name, address, idl })
    }
}

#[macro_export]
macro_rules! home_path {
    ($my_struct:ident, $path:literal) => {
        #[derive(Clone, Debug)]
        pub struct $my_struct(String);

        impl Default for $my_struct {
            fn default() -> Self {
                $my_struct(
                    home_dir()
                        .unwrap()
                        .join($path.replace('/', std::path::MAIN_SEPARATOR_STR))
                        .display()
                        .to_string(),
                )
            }
        }

        impl $my_struct {
            fn stringify_with_tilde(&self) -> String {
                self.0
                    .replacen(home_dir().unwrap().to_str().unwrap(), "~", 1)
            }
        }

        impl FromStr for $my_struct {
            type Err = anyhow::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(s.to_owned()))
            }
        }

        impl fmt::Display for $my_struct {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

home_path!(WalletPath, ".config/solana/id.json");

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_CONFIG: &str = "
        [provider]
        cluster = \"localnet\"
        wallet = \"id.json\"
    ";

    #[test]
    fn parse_custom_cluster_str() {
        let config = Config::from_str(
            "
        [provider]
        cluster = \"http://my-url.com\"
        wallet = \"id.json\"
    ",
        )
        .unwrap();
        assert!(!config.features.skip_lint);

        // Make sure the layout of `provider.cluster` stays the same after serialization
        assert!(config
            .to_string()
            .contains(r#"cluster = "http://my-url.com""#));
    }

    #[test]
    fn parse_custom_cluster_map() {
        let config = Config::from_str(
            "
        [provider]
        cluster = { http = \"http://my-url.com\", ws = \"ws://my-url.com\" }
        wallet = \"id.json\"
    ",
        )
        .unwrap();
        assert!(!config.features.skip_lint);
    }

    #[test]
    fn parse_skip_lint_no_section() {
        let config = Config::from_str(BASE_CONFIG).unwrap();
        assert!(!config.features.skip_lint);
    }

    #[test]
    fn parse_skip_lint_no_value() {
        let string = BASE_CONFIG.to_owned() + "[features]";
        let config = Config::from_str(&string).unwrap();
        assert!(!config.features.skip_lint);
    }

    #[test]
    fn parse_skip_lint_true() {
        let string = BASE_CONFIG.to_owned() + "[features]\nskip-lint = true";
        let config = Config::from_str(&string).unwrap();
        assert!(config.features.skip_lint);
    }

    #[test]
    fn parse_skip_lint_false() {
        let string = BASE_CONFIG.to_owned() + "[features]\nskip-lint = false";
        let config = Config::from_str(&string).unwrap();
        assert!(!config.features.skip_lint);
    }
}
