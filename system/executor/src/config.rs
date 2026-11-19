// SPDX-License-Identifier: MulanPSL-2.0
// Author: wheatfox <wheatfox17@icloud.com>
//
// Executor config — same three-source resolution as pilot:
//   compiled defaults < YAML at $ROBONIX_CONFIG_PATH < CLI flags / env.

use anyhow::{Context, Result, bail};
use clap::Parser;
use robonix_sentinel::interpreter::adapter::strategy::StrategyConfig;
use serde::Deserialize;
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_EXECUTOR_PROVIDER_ID: &str = "executor";
pub const EXECUTOR_NAMESPACE: &str = "robonix/system/executor";
pub const DEFAULT_ATLAS_ENDPOINT: &str = "127.0.0.1:50051";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:50061";

/// Resolved startup settings for Executor connections, identity, and verification.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Atlas endpoint used for registration and capability discovery.
    pub atlas_endpoint: String,
    /// Socket address on which Executor serves its gRPC capabilities.
    pub listen: String,
    /// Loopback socket address for the Sentinel rule-management service.
    pub sentinel_listen: String,
    /// Executor's registered provider ID, also used as its consumer identity.
    pub id: String,
    /// SRDL source loaded and checked once at startup.
    pub sentinel_rules: PathBuf,
    /// Optional versioned policy copied into place only when no active file exists.
    pub sentinel_rules_seed: Option<PathBuf>,
    /// Enabled Sentinel strategies and their initialization settings.
    pub sentinel_strategies: StrategyConfig,
    /// Whether plan execution and control require a live Keystone session.
    pub require_keystone: bool,
    /// Timing and routing rules for verifying successful capability results.
    pub verification: VerificationConfig,
}

/// Executor-wide verification timing plus capability-specific routes.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct VerificationConfig {
    /// Allow capability execution to continue while matched rules verify.
    #[serde(default)]
    pub overlap: bool,
    /// Rules matching completed calls to the verifier that checks their results.
    #[serde(default)]
    pub rules: Vec<VerificationRule>,
}

/// Route one completed capability call to a verifier provider.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VerificationRule {
    /// Contract ID of the capability calls whose results should be verified.
    pub target_contract_id: String,
    /// Optional provider restriction; omission matches any provider of the contract.
    #[serde(default)]
    pub target_provider_id: Option<String>,
    /// Provider implementing the shared verification contract for matched calls.
    pub verifier_provider_id: String,
    /// Extra JSON object passed to the verifier alongside the original call context.
    #[serde(default = "empty_json_object")]
    pub verifier_args: serde_json::Value,
}

fn empty_json_object() -> serde_json::Value {
    serde_json::json!({})
}

/// CLI and environment inputs used to configure Executor startup and logging.
#[derive(Parser)]
#[command(
    name = "robonix-executor",
    about = "Robonix Executor — tool-call dispatch runtime"
)]
pub struct Args {
    /// Atlas control-plane endpoint. Also reads `ROBONIX_ATLAS` (the var rbnx /
    /// the Python API / liaison use) as an alias; see `env_atlas`.
    #[arg(long, env = "ROBONIX_ATLAS_ENDPOINT")]
    pub atlas: Option<String>,

    /// Address the SystemExecutor gRPC service binds to.
    #[arg(long, env = "ROBONIX_EXECUTOR_LISTEN")]
    pub listen: Option<String>,

    /// Loopback address the robot-local Sentinel management gRPC service binds to.
    #[arg(long, env = "ROBONIX_SENTINEL_LISTEN")]
    pub sentinel_listen: Option<String>,

    /// Override executor's id (singleton; rarely needed).
    #[arg(long, env = "ROBONIX_EXECUTOR_PROVIDER_ID")]
    pub id: Option<String>,

    /// Robot-local Sentinel rules file.
    #[arg(long, env = "ROBONIX_SENTINEL_RULES")]
    pub sentinel_rules: Option<PathBuf>,

    /// Optional versioned seed copied to `sentinel_rules` only on first boot.
    #[arg(long, env = "ROBONIX_SENTINEL_RULES_SEED")]
    pub sentinel_rules_seed: Option<PathBuf>,

    /// API key for an enabled Sentinel Summarize strategy.
    #[arg(long, env = "SENTINEL_SUMMARIZE_API_KEY", hide_env_values = true)]
    pub summarize_api_key: Option<String>,

    /// Require a live Keystone session for plan execution and plan-control requests.
    #[arg(long, env = "ROBONIX_EXECUTOR_REQUIRE_KEYSTONE")]
    pub require_keystone: Option<bool>,

    /// Optional YAML config file (rbnx writes this; CLI/env still override).
    #[arg(long, env = "ROBONIX_CONFIG_PATH")]
    pub config: Option<PathBuf>,

    /// Log level for this component (`debug`/`info`/`warn`/`error`). Sets the
    /// scribe log-file floor; falls back to `SCRIBE_FILE_LEVEL` / `info`.
    /// Normally arrives inside `--config-json`, not as a standalone flag.
    #[arg(long)]
    pub log: Option<String>,

    /// The component's `system.executor` manifest block, serialized to JSON by
    /// rbnx and passed as one arg (`--config-json '{…}'`). Parsed by the binary
    /// itself — see `robonix_scribe::init_from_config`, which reads the `log`
    /// key from it so the manifest's per-component level reaches the log.
    #[arg(long)]
    pub config_json: Option<String>,
}

/// Optional YAML settings used when higher-priority startup inputs are absent.
#[derive(Default, Deserialize)]
struct FileConfig {
    /// Atlas connection endpoint supplied by the YAML file.
    #[serde(default)]
    atlas_endpoint: Option<String>,
    /// Executor gRPC bind address supplied by the YAML file.
    #[serde(default)]
    listen: Option<String>,
    /// Executor provider ID supplied by the YAML file.
    #[serde(default)]
    id: Option<String>,
    /// Sentinel management bind address supplied by the YAML file.
    #[serde(default)]
    sentinel_listen: Option<String>,
    /// Active Sentinel policy path supplied by the YAML file.
    #[serde(default)]
    sentinel_rules: Option<PathBuf>,
    /// Optional first-boot Sentinel policy seed supplied by the YAML file.
    #[serde(default)]
    sentinel_rules_seed: Option<PathBuf>,
    /// Strategy settings used when the manifest does not specify them.
    #[serde(default)]
    sentinel_strategies: Option<StrategyConfig>,
    /// Whether Executor requires Keystone sessions when no manifest value exists.
    #[serde(default)]
    require_keystone: Option<bool>,
    /// Verification settings used when the manifest block does not supply them.
    #[serde(default)]
    verification: Option<VerificationConfig>,
}

/// Executor settings decoded from the manifest block passed through `--config-json`.
#[derive(Default, Deserialize)]
struct ManifestConfig {
    /// Sentinel management bind address from the Executor manifest block.
    #[serde(default)]
    sentinel_listen: Option<String>,
    /// Active Sentinel policy path from the Executor manifest block.
    #[serde(default)]
    sentinel_rules: Option<PathBuf>,
    /// Optional first-boot Sentinel policy seed from the Executor manifest block.
    #[serde(default)]
    sentinel_rules_seed: Option<PathBuf>,
    /// Enabled strategies supplied by the Executor manifest block.
    #[serde(default)]
    sentinel_strategies: Option<StrategyConfig>,
    /// Whether plan execution and control require Keystone in this deployment.
    #[serde(default)]
    require_keystone: Option<bool>,
    /// Manifest verification settings, which take precedence over YAML settings.
    #[serde(default)]
    verification: Option<VerificationConfig>,
}

impl ExecutorConfig {
    /// Resolve startup settings, preferring manifest strategy and verification
    /// configuration over YAML settings.
    pub fn resolve(args: Args) -> Result<Self> {
        let file_cfg: FileConfig = match &args.config {
            Some(path) => load_yaml(path)?,
            None => FileConfig::default(),
        };
        let manifest_cfg: ManifestConfig = match args.config_json.as_deref() {
            Some(raw) => serde_json::from_str(raw).context("parse Executor --config-json")?,
            None => ManifestConfig::default(),
        };
        let mut verification = manifest_cfg
            .verification
            .or(file_cfg.verification)
            .unwrap_or_default();
        verification.rules = validate_verification_rules(verification.rules)?;
        let listen = args
            .listen
            .or(file_cfg.listen)
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
        let sentinel_listen = args
            .sentinel_listen
            .or(manifest_cfg.sentinel_listen)
            .or(file_cfg.sentinel_listen)
            .map(Ok)
            .unwrap_or_else(|| default_sentinel_listen(&listen))?;
        let sentinel_address: std::net::SocketAddr = sentinel_listen
            .parse()
            .with_context(|| format!("invalid Sentinel listen address '{sentinel_listen}'"))?;
        if sentinel_address.ip() != std::net::Ipv4Addr::LOCALHOST {
            bail!(
                "Sentinel management must listen on 127.0.0.1, got '{}'",
                sentinel_listen
            );
        }
        let data_dir = std::env::var_os("ROBONIX_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rbnx-boot/data"));
        let sentinel_rules = args
            .sentinel_rules
            .or(manifest_cfg.sentinel_rules)
            .or(file_cfg.sentinel_rules)
            .unwrap_or_else(|| data_dir.join("sentinel-rules.srdl"));
        let sentinel_rules_seed = args
            .sentinel_rules_seed
            .or(manifest_cfg.sentinel_rules_seed)
            .or(file_cfg.sentinel_rules_seed);
        let require_keystone = args
            .require_keystone
            .or(manifest_cfg.require_keystone)
            .or(file_cfg.require_keystone)
            .unwrap_or(false);
        let mut sentinel_strategies = manifest_cfg
            .sentinel_strategies
            .or(file_cfg.sentinel_strategies)
            .unwrap_or_default();
        if let (Some(config), Some(key)) =
            (&mut sentinel_strategies.summarize, args.summarize_api_key)
        {
            config.api_key = key;
        }
        Ok(Self {
            atlas_endpoint: args
                .atlas
                .or_else(env_atlas)
                .or(file_cfg.atlas_endpoint)
                .unwrap_or_else(|| DEFAULT_ATLAS_ENDPOINT.to_string()),
            listen,
            sentinel_listen,
            id: args
                .id
                .or(file_cfg.id)
                .unwrap_or_else(|| DEFAULT_EXECUTOR_PROVIDER_ID.to_string()),
            sentinel_rules,
            sentinel_rules_seed,
            sentinel_strategies,
            require_keystone,
            verification,
        })
    }
}

fn default_sentinel_listen(listen: &str) -> Result<String> {
    let address: std::net::SocketAddr = listen
        .parse()
        .with_context(|| format!("invalid executor listen address '{listen}'"))?;
    let port = address
        .port()
        .checked_add(1)
        .context("cannot derive Sentinel listen address from port 65535")?;
    Ok(format!("127.0.0.1:{port}"))
}

/// Copy an SRDL seed only when no local rule file exists.
pub(crate) fn bootstrap_sentinel_rules(path: &Path, seed: Option<&Path>) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let Some(seed) = seed else {
        return Ok(());
    };
    let raw = std::fs::read_to_string(seed)
        .with_context(|| format!("read Sentinel rules seed '{}'", seed.display()))?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create Sentinel rules directory '{}'", parent.display()))?;
    // A partial seed can itself be valid SRDL, including the empty policy.
    let mut target = tempfile::NamedTempFile::new_in(parent)?;
    target.write_all(raw.as_bytes())?;
    target.as_file().sync_all()?;
    match target.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error)
            .with_context(|| format!("bootstrap Sentinel rules '{}'", path.display())),
    }
}

/// Normalize identifiers and reject ambiguous rules before Executor starts.
fn validate_verification_rules(rules: Vec<VerificationRule>) -> Result<Vec<VerificationRule>> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::with_capacity(rules.len());
    for mut rule in rules {
        rule.target_contract_id = rule.target_contract_id.trim().to_string();
        rule.verifier_provider_id = rule.verifier_provider_id.trim().to_string();
        rule.target_provider_id = rule
            .target_provider_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if rule.target_contract_id.is_empty() {
            bail!("verification target_contract_id must not be empty");
        }
        if rule.verifier_provider_id.is_empty() {
            bail!(
                "verification verifier_provider_id must not be empty for '{}'",
                rule.target_contract_id
            );
        }
        if !rule.verifier_args.is_object() {
            bail!(
                "verification verifier_args must be a JSON object for '{}'",
                rule.target_contract_id
            );
        }
        let key = (
            rule.target_contract_id.clone(),
            rule.target_provider_id.clone(),
        );
        if !seen.insert(key) {
            bail!(
                "duplicate verification rule for contract '{}' and provider '{}'",
                rule.target_contract_id,
                rule.target_provider_id.as_deref().unwrap_or("*")
            );
        }
        normalized.push(rule);
    }
    Ok(normalized)
}

/// Read the `ROBONIX_ATLAS` env var as an atlas-endpoint alias.
///
/// rbnx, the Python API, and liaison all configure the atlas endpoint via
/// `ROBONIX_ATLAS`, while executor/pilot historically only honored
/// `ROBONIX_ATLAS_ENDPOINT` (the clap `env`). Accepting `ROBONIX_ATLAS` here as
/// well means a single env var configures every component. Without it, setting
/// only `ROBONIX_ATLAS` left executor silently falling back to
/// `DEFAULT_ATLAS_ENDPOINT` (127.0.0.1:50051) — it would then dial the wrong
/// atlas and log 127.0.0.1 even after the operator "changed" the endpoint.
/// Empty values are ignored so an exported-but-blank var doesn't shadow later
/// sources.
fn env_atlas() -> Option<String> {
    std::env::var("ROBONIX_ATLAS")
        .ok()
        .filter(|v| !v.is_empty())
}

fn load_yaml(path: &Path) -> Result<FileConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read executor config '{}'", path.display()))?;
    serde_yaml::from_str(&raw)
        .with_context(|| format!("parse executor config '{}'", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(config_json: Option<&str>) -> Args {
        Args {
            atlas: None,
            listen: None,
            sentinel_listen: None,
            id: None,
            sentinel_rules: None,
            sentinel_rules_seed: None,
            summarize_api_key: None,
            require_keystone: None,
            config: None,
            log: None,
            config_json: config_json.map(str::to_string),
        }
    }

    #[test]
    fn sentinel_defaults_to_executor_adjacent_port() {
        let cfg = ExecutorConfig::resolve(args(None)).unwrap();
        assert_eq!(cfg.sentinel_listen, "127.0.0.1:50062");
        assert_eq!(
            cfg.sentinel_rules,
            PathBuf::from("rbnx-boot/data/sentinel-rules.srdl")
        );
    }

    #[test]
    fn sentinel_default_remains_local_for_external_executor() {
        let mut args = args(None);
        args.listen = Some("0.0.0.0:50061".to_string());
        let cfg = ExecutorConfig::resolve(args).unwrap();
        assert_eq!(cfg.sentinel_listen, "127.0.0.1:50062");
    }

    #[test]
    fn parses_manifest_sentinel_config() {
        let cfg = ExecutorConfig::resolve(args(Some(
            r#"{
                "sentinel_listen": "127.0.0.1:51062",
                "sentinel_rules": "/var/lib/robonix/sentinel-rules.srdl"
            }"#,
        )))
        .unwrap();
        assert_eq!(cfg.sentinel_listen, "127.0.0.1:51062");
        assert_eq!(
            cfg.sentinel_rules,
            PathBuf::from("/var/lib/robonix/sentinel-rules.srdl")
        );
    }

    #[test]
    fn sentinel_strategies_use_manifest_over_yaml_without_logging_credentials() {
        assert!(
            ExecutorConfig::resolve(args(None))
                .unwrap()
                .sentinel_strategies
                .summarize
                .is_none()
        );
        let path =
            std::env::temp_dir().join(format!("executor-config-{}.yaml", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"
sentinel_strategies:
  Summarize:
    api_base: https://example.invalid/v1
    model: file-model
    api_key: test-private-key
"#,
        )
        .unwrap();
        for (manifest, model) in [
            (None, Some("file-model")),
            (Some(r#"{"sentinel_strategies":{}}"#), None),
            (
                Some(
                    r#"{"sentinel_strategies":{"Summarize":{"api_base":"https://example.invalid/v1","model":"manifest-model","api_key":"test-private-key"}}}"#,
                ),
                Some("manifest-model"),
            ),
        ] {
            let mut inputs = args(manifest);
            inputs.config = Some(path.clone());
            let config = ExecutorConfig::resolve(inputs).unwrap();
            assert_eq!(
                config
                    .sentinel_strategies
                    .summarize
                    .as_ref()
                    .map(|config| config.model.as_str()),
                model
            );
            assert!(!format!("{config:?}").contains("test-private-key"));
        }
        std::fs::remove_file(path).unwrap();
        let mut inputs = args(Some(
            r#"{"sentinel_strategies":{"Summarize":{"api_base":"https://example.invalid","model":"model"}}}"#,
        ));
        inputs.summarize_api_key = Some("strategy-only".into());
        assert_eq!(
            ExecutorConfig::resolve(inputs)
                .unwrap()
                .sentinel_strategies
                .summarize
                .unwrap()
                .api_key,
            "strategy-only"
        );
        assert!(
            ExecutorConfig::resolve(args(Some(r#"{"sentinel_strategies":{"Unknown":{}}}"#)))
                .is_err()
        );
    }

    #[test]
    fn rejects_external_sentinel_listen() {
        let error = ExecutorConfig::resolve(args(Some(r#"{"sentinel_listen":"0.0.0.0:50062"}"#)))
            .unwrap_err();

        assert!(error.to_string().contains("must listen on 127.0.0.1"));
    }

    #[test]
    fn require_keystone_defaults_off_and_reads_manifest() {
        let cfg = ExecutorConfig::resolve(args(None)).expect("default config");
        assert!(!cfg.require_keystone);

        let cfg = ExecutorConfig::resolve(args(Some(r#"{"require_keystone":true}"#)))
            .expect("manifest config");
        assert!(cfg.require_keystone);
    }

    #[test]
    fn versioned_seed_bootstraps_fresh_rules_without_overwriting_changes() {
        let temp = std::env::temp_dir().join(format!(
            "executor-sentinel-seed-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp).expect("test directory");
        let seed = temp.join("sentinel-rules.v1.srdl");
        let target = temp.join("data/sentinel-rules.srdl");
        let seed_source = "trigger demo before always; rule demo { require false; enforce deny; }";
        std::fs::write(&seed, seed_source).expect("versioned seed");
        let config_json = serde_json::json!({
            "sentinel_rules": target,
            "sentinel_rules_seed": seed,
        })
        .to_string();

        let config = ExecutorConfig::resolve(args(Some(&config_json))).expect("seeded config");
        assert!(
            !target.exists(),
            "resolving config must not write Sentinel rules"
        );
        bootstrap_sentinel_rules(
            &config.sentinel_rules,
            config.sentinel_rules_seed.as_deref(),
        )
        .expect("bootstrap rules");
        assert_eq!(config.sentinel_rules, target);
        assert_eq!(
            std::fs::read_to_string(&target).expect("bootstrapped rules"),
            seed_source
        );

        std::fs::write(&target, "# local policy\n").expect("local replacement");
        let config =
            ExecutorConfig::resolve(args(Some(&config_json))).expect("existing rules config");
        bootstrap_sentinel_rules(
            &config.sentinel_rules,
            config.sentinel_rules_seed.as_deref(),
        )
        .expect("existing rules remain untouched");
        assert_eq!(
            std::fs::read_to_string(&target).expect("preserved rules"),
            "# local policy\n"
        );
        std::fs::remove_dir_all(temp).expect("remove test directory");
    }

    #[test]
    fn seed_copy_does_not_interpret_the_source() {
        let temp =
            std::env::temp_dir().join(format!("executor-srdl-seed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&temp).unwrap();
        let seed = temp.join("source.srdl");
        let target = temp.join("active.srdl");
        std::fs::write(&seed, "unchecked source").unwrap();
        bootstrap_sentinel_rules(&target, Some(&seed)).unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchecked source");
        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn rejects_adjacent_sentinel_port_overflow() {
        let mut args = args(None);
        args.listen = Some("127.0.0.1:65535".to_string());
        let error = ExecutorConfig::resolve(args).unwrap_err();
        assert!(error.to_string().contains("port 65535"));
    }

    #[test]
    fn parses_manifest_verification_rules() {
        let cfg = ExecutorConfig::resolve(args(Some(
            r#"{
                "verification": {
                    "overlap": true,
                    "rules": [{
                        "target_contract_id": "robonix/service/navigation/navigate",
                        "verifier_provider_id": "scene_verifier",
                        "verifier_args": {"scene_provider_id": "scene"}
                    }]
                }
            }"#,
        )))
        .unwrap();
        assert_eq!(cfg.verification.rules.len(), 1);
        assert!(cfg.verification.overlap);
        assert_eq!(
            cfg.verification.rules[0].verifier_args["scene_provider_id"],
            "scene"
        );
    }

    /// Verification configs that omit overlap retain synchronous behavior.
    #[test]
    fn verification_overlap_defaults_to_false() {
        let cfg = ExecutorConfig::resolve(args(Some(
            r#"{
                "verification": {
                    "rules": [{
                        "target_contract_id": "cap/a",
                        "verifier_provider_id": "verifier"
                    }]
                }
            }"#,
        )))
        .unwrap();

        assert!(!cfg.verification.overlap);
    }

    #[test]
    fn explicit_empty_manifest_rules_disable_defaults() {
        let cfg =
            ExecutorConfig::resolve(args(Some(r#"{"verification": {"rules": []}}"#))).unwrap();
        assert!(cfg.verification.rules.is_empty());
    }

    #[test]
    fn rejects_duplicate_rules_at_the_same_specificity() {
        let error = ExecutorConfig::resolve(args(Some(
            r#"{
                "verification": {"rules": [
                    {"target_contract_id":"cap/a","verifier_provider_id":"v1"},
                    {"target_contract_id":"cap/a","verifier_provider_id":"v2"}
                ]}
            }"#,
        )))
        .unwrap_err();
        assert!(error.to_string().contains("duplicate verification rule"));
    }

    #[test]
    fn rejects_non_object_verifier_args() {
        let error = ExecutorConfig::resolve(args(Some(
            r#"{
                "verification": {"rules": [{
                    "target_contract_id":"cap/a",
                    "verifier_provider_id":"v1",
                    "verifier_args": ["bad"]
                }]}
            }"#,
        )))
        .unwrap_err();
        assert!(error.to_string().contains("must be a JSON object"));
    }
}
