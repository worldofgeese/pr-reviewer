use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::review::parser::FindingKind;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    ClaudeCode,
    Opencode,
    Codex,
}

impl HarnessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HarnessKind::ClaudeCode => "claude-code",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Codex => "codex",
        }
    }
}

impl Display for HarnessKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for HarnessKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::Opencode),
            "codex" => Ok(Self::Codex),
            _ => Err(anyhow!("unsupported harness: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::None => "none",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
        }
    }
}

impl Display for ReasoningEffort {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" | "x-high" => Ok(Self::Xhigh),
            _ => Err(anyhow!("unsupported reasoning effort: {s}")),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ForkPolicy {
    #[default]
    Ignore,
    Limited,
    Full,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum DaemonMode {
    #[default]
    Poll,
    Webhook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "default_harness")]
    pub default: HarnessKind,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Built-in tools that review sessions may use (claude-code harness only).
    /// Empty by default: a review session gets everything it needs in its prompt.
    #[serde(default)]
    pub review_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitHubConfig {
    /// Plain-text token (legacy — triggers deprecation warning at runtime).
    #[serde(default)]
    pub token: Option<String>,
    /// Base64-encoded double-encrypted token blob.
    #[serde(default)]
    pub encrypted_token: Option<String>,
    /// Whether a user passphrase was used for the inner encryption layer.
    #[serde(default)]
    pub passphrase_protected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_poll_interval")]
    pub max_poll_interval_secs: u64,
    #[serde(default)]
    pub mode: DaemonMode,
    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,
    #[serde(default = "default_concurrency")]
    pub max_concurrent_reviews: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default = "default_auto_review")]
    pub auto_review: bool,
    #[serde(default)]
    pub review_drafts: bool,
    #[serde(default = "default_max_files")]
    pub max_files: usize,
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: usize,
    #[serde(default = "default_max_file_size")]
    pub max_file_size_bytes: usize,
    #[serde(default = "default_bot_name")]
    pub bot_name: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_prompt_limit")]
    pub max_prompt_bytes: usize,
    #[serde(default = "default_true")]
    pub skip_docs_only: bool,
    /// Review cycles that may post inline findings. Review cycle 1 is the full
    /// review, cycles 2..=N verify earlier findings, and later cycles report
    /// new non-critical findings only as non-blocking notes. 0 = no limit.
    #[serde(default = "default_max_review_cycles")]
    pub max_review_cycles: u32,
    /// Finding kinds that may still be posted inline, and request changes,
    /// after the review-cycle limit (when blocking and backed by an
    /// `evidence_note`).
    #[serde(default = "default_critical_finding_kinds")]
    pub critical_finding_kinds: Vec<FindingKind>,
    /// Skip the review of a push that leaves the PR's own diff unchanged,
    /// such as a rebase or a cascade from a stacked parent.
    #[serde(default = "default_true")]
    pub skip_unchanged_diff: bool,
    /// Detect stacked PRs, tell the reviewer about the parent PR, and drop
    /// findings that are already tracked on an ancestor PR.
    #[serde(default = "default_true")]
    pub stack_aware: bool,
    /// Dismiss the bot's earlier REQUEST_CHANGES reviews once no blocking
    /// finding is open. The bot never approves, so without this a change
    /// request stays on the PR after its findings are fixed.
    #[serde(default = "default_true")]
    pub dismiss_stale_change_requests: bool,
    /// Post a `success` commit status on the head commit when a re-review
    /// finds nothing new, so a merge gate can tell "reviewed, nothing new"
    /// from "not reviewed yet". Needs `repo:status` (classic token) or
    /// "Commit statuses: write" (fine-grained token).
    #[serde(default)]
    pub status_on_no_new_findings: bool,
    /// GitHub logins of other pr-reviewer daemons that watch the same
    /// repositories under their own account. Their reviews, status comments
    /// and command answers count as done, so a PR is not reviewed twice.
    #[serde(default)]
    pub peer_logins: Vec<String>,
    /// Seconds to wait before this daemon reviews a new head commit or
    /// answers a maintainer command. A standby daemon sets this so that a
    /// peer with no delay goes first. 0 = no wait.
    #[serde(default)]
    pub standby_delay_secs: u64,
}

/// Per-repo overrides for the review policy. Unset fields use `[defaults]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Eq, PartialEq)]
pub struct ReviewPolicyOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_review_cycles: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_finding_kinds: Option<Vec<FindingKind>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_unchanged_diff: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack_aware: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismiss_stale_change_requests: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_on_no_new_findings: Option<bool>,
}

impl ReviewPolicyOverrides {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// The effective review policy for one repo: per-repo override > `[defaults]`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ReviewPolicy {
    pub max_review_cycles: u32,
    pub critical_finding_kinds: Vec<FindingKind>,
    pub skip_unchanged_diff: bool,
    pub stack_aware: bool,
    pub dismiss_stale_change_requests: bool,
    pub status_on_no_new_findings: bool,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        DefaultsConfig::default().review_policy()
    }
}

impl DefaultsConfig {
    pub fn review_policy(&self) -> ReviewPolicy {
        ReviewPolicy {
            max_review_cycles: self.max_review_cycles,
            critical_finding_kinds: self.critical_finding_kinds.clone(),
            skip_unchanged_diff: self.skip_unchanged_diff,
            stack_aware: self.stack_aware,
            dismiss_stale_change_requests: self.dismiss_stale_change_requests,
            status_on_no_new_findings: self.status_on_no_new_findings,
        }
    }
}

/// Conditions that must be met for a workflow step to run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowConditions {
    /// Only run if the PR touches files matching any of these globs.
    #[serde(default)]
    pub file_patterns: Vec<String>,
    /// Only run if the diff has at least this many lines.
    #[serde(default)]
    pub min_diff_lines: Option<usize>,
    /// Skip draft PRs.
    #[serde(default)]
    pub skip_drafts: bool,
    /// Only run if the PR has at least one of these labels.
    #[serde(default)]
    pub label_patterns: Vec<String>,
}

/// A single step in a repo's review workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Unique identifier (UUID or user-defined slug).
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: String,
    /// Override the harness for this step. Falls back to repo/global default.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// Override the model for this step. Falls back to repo/global default.
    #[serde(default)]
    pub model: Option<String>,
    /// Additional instructions injected into the prompt for this step.
    #[serde(default)]
    pub custom_instructions: Option<String>,
    /// Conditions that must be met for this step to run.
    #[serde(default)]
    pub conditions: WorkflowConditions,
    /// Whether this step is active. Disabled steps are skipped.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub owner: String,
    pub name: String,
    /// Local path override. When `None`, uses managed clone at `~/.config/pr-reviewer/repos/{owner}/{name}`.
    #[serde(default)]
    pub local_path: Option<PathBuf>,
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Overrides `harness.review_tools` for this repository.
    #[serde(default)]
    pub review_tools: Option<Vec<String>>,
    #[serde(default)]
    pub fork_policy: ForkPolicy,
    #[serde(default)]
    pub trusted_authors: Vec<String>,
    #[serde(default)]
    pub ignore_paths: Vec<String>,
    #[serde(default)]
    pub custom_instructions: Option<String>,
    #[serde(default = "default_gitnexus")]
    pub gitnexus: bool,
    /// Optional multi-step review workflow. When non-empty the daemon runs
    /// each enabled step in order instead of the single-step legacy behaviour.
    #[serde(default)]
    pub workflow: Vec<WorkflowStep>,
    #[serde(default)]
    pub auto_fix: AutoFixConfig,
    #[serde(default, skip_serializing_if = "ReviewPolicyOverrides::is_empty")]
    pub review_policy: ReviewPolicyOverrides,
}

/// Autonomous bug/security scan that can patch the managed clone and open a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoFixConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub scan_default_branch: bool,
    #[serde(default = "default_auto_fix_base_branch")]
    pub base_branch: String,
    #[serde(default = "default_auto_fix_branch_prefix")]
    pub branch_prefix: String,
    #[serde(default = "default_true")]
    pub draft_pr: bool,
    #[serde(default = "default_auto_fix_max_changed_files")]
    pub max_changed_files: usize,
    #[serde(default = "default_auto_fix_max_repairs_per_pr")]
    pub max_repairs_per_pr: u32,
    #[serde(default = "default_auto_fix_max_repairs_per_head")]
    pub max_repairs_per_head: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub harness: HarnessConfig,
    #[serde(default)]
    pub github: GitHubConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
    #[serde(default)]
    pub serve: ServeConfig,
    #[serde(default)]
    pub instruction_prs: InstructionPrConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServeConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum InstructionPrMode {
    #[default]
    Conservative,
    Broad,
}

impl Display for InstructionPrMode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            InstructionPrMode::Conservative => "conservative",
            InstructionPrMode::Broad => "broad",
        };
        write!(f, "{value}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionPrConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: InstructionPrMode,
    #[serde(default = "default_instruction_pr_min_distinct_prs")]
    pub min_distinct_prs: u32,
    #[serde(default = "default_instruction_pr_branch_prefix")]
    pub branch_prefix: String,
}

fn default_harness() -> HarnessKind {
    HarnessKind::ClaudeCode
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn default_timeout_secs() -> u64 {
    1800
}

fn default_poll_interval() -> u64 {
    30
}

fn default_max_poll_interval() -> u64 {
    300
}

fn default_webhook_port() -> u16 {
    3847
}

fn default_concurrency() -> usize {
    2
}

fn default_auto_review() -> bool {
    true
}

fn default_max_files() -> usize {
    50
}

fn default_max_diff_lines() -> usize {
    3000
}

fn default_max_file_size() -> usize {
    100_000
}

fn default_bot_name() -> String {
    "pr-reviewer".to_string()
}

fn default_prompt_limit() -> usize {
    200 * 1024
}

fn default_gitnexus() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_max_review_cycles() -> u32 {
    2
}

fn default_critical_finding_kinds() -> Vec<FindingKind> {
    vec![
        FindingKind::Security,
        FindingKind::SecretExposure,
        FindingKind::DataIntegrity,
        FindingKind::Correctness,
    ]
}

fn default_instruction_pr_min_distinct_prs() -> u32 {
    2
}

fn default_instruction_pr_branch_prefix() -> String {
    "pr-reviewer/agents-guardrail".to_string()
}

fn default_auto_fix_base_branch() -> String {
    "main".to_string()
}

fn default_auto_fix_branch_prefix() -> String {
    "pr-reviewer/auto-fix".to_string()
}

fn default_auto_fix_max_changed_files() -> usize {
    20
}

fn default_auto_fix_max_repairs_per_pr() -> u32 {
    10
}

fn default_auto_fix_max_repairs_per_head() -> u32 {
    1
}

impl Default for AutoFixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scan_default_branch: false,
            base_branch: default_auto_fix_base_branch(),
            branch_prefix: default_auto_fix_branch_prefix(),
            draft_pr: true,
            max_changed_files: default_auto_fix_max_changed_files(),
            max_repairs_per_pr: default_auto_fix_max_repairs_per_pr(),
            max_repairs_per_head: default_auto_fix_max_repairs_per_head(),
        }
    }
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            default: default_harness(),
            model: default_model(),
            reasoning_effort: None,
            timeout_secs: default_timeout_secs(),
            review_tools: Vec::new(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            max_poll_interval_secs: default_max_poll_interval(),
            mode: DaemonMode::default(),
            webhook_port: default_webhook_port(),
            max_concurrent_reviews: default_concurrency(),
        }
    }
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            auto_review: default_auto_review(),
            review_drafts: false,
            max_files: default_max_files(),
            max_diff_lines: default_max_diff_lines(),
            max_file_size_bytes: default_max_file_size(),
            bot_name: default_bot_name(),
            dry_run: false,
            max_prompt_bytes: default_prompt_limit(),
            skip_docs_only: true,
            max_review_cycles: default_max_review_cycles(),
            critical_finding_kinds: default_critical_finding_kinds(),
            skip_unchanged_diff: true,
            stack_aware: true,
            dismiss_stale_change_requests: true,
            status_on_no_new_findings: false,
            peer_logins: Vec::new(),
            standby_delay_secs: 0,
        }
    }
}

impl Default for InstructionPrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: InstructionPrMode::default(),
            min_distinct_prs: default_instruction_pr_min_distinct_prs(),
            branch_prefix: default_instruction_pr_branch_prefix(),
        }
    }
}

/// Ignore patterns that `pr-reviewer add` gives a newly registered repo.
fn default_ignore_paths() -> Vec<String> {
    ["*.lock", "vendor/**", "dist/**"]
        .map(String::from)
        .to_vec()
}

/// Flatten a TOML table into `dotted.key = value` lines.
fn flatten_toml(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, toml::Value>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(inner) => flatten_toml(&path, inner, out),
            other => {
                out.insert(path, other.clone());
            }
        }
    }
}

impl RepoConfig {
    /// The entry that `pr-reviewer add` registers before applying flags.
    pub fn registered_default(owner: String, name: String) -> Self {
        Self {
            owner,
            name,
            local_path: None,
            harness: None,
            model: None,
            reasoning_effort: None,
            review_tools: None,
            fork_policy: ForkPolicy::default(),
            trusted_authors: vec![],
            ignore_paths: default_ignore_paths(),
            custom_instructions: None,
            gitnexus: default_gitnexus(),
            workflow: vec![],
            auto_fix: AutoFixConfig::default(),
            review_policy: ReviewPolicyOverrides::default(),
        }
    }

    /// Settings that differ from a freshly registered entry, as
    /// `key = value` lines. Registration details (owner, name, local path)
    /// are not settings. `remove` uses this to name what it discards.
    pub fn non_default_settings(&self) -> Result<Vec<String>> {
        let flatten = |repo: &Self| -> Result<BTreeMap<String, toml::Value>> {
            let mut flat = BTreeMap::new();
            match toml::Value::try_from(repo)? {
                toml::Value::Table(table) => flatten_toml("", &table, &mut flat),
                _ => return Err(anyhow!("repo config did not serialize to a table")),
            }
            for key in ["owner", "name", "local_path"] {
                flat.remove(key);
            }
            Ok(flat)
        };
        let defaults = flatten(&Self::registered_default(
            self.owner.clone(),
            self.name.clone(),
        ))?;
        Ok(flatten(self)?
            .into_iter()
            .filter(|(key, value)| defaults.get(key) != Some(value))
            .map(|(key, value)| format!("{key} = {value}"))
            .collect())
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Resolve the effective local path for this repo.
    /// Returns `local_path` if set, otherwise the managed clone path.
    pub fn effective_local_path(&self) -> Result<PathBuf> {
        crate::repo_manager::resolve_local_path(self)
    }

    /// Whether this repo uses a managed (auto-cloned) path vs. a manual override.
    pub fn is_managed(&self) -> bool {
        self.local_path.is_none()
    }

    pub fn resolved_harness(&self, config: &AppConfig) -> HarnessKind {
        self.harness.unwrap_or(config.harness.default)
    }

    pub fn resolved_model<'a>(&'a self, config: &'a AppConfig) -> &'a str {
        self.model
            .as_deref()
            .unwrap_or(config.harness.model.as_str())
    }

    pub fn resolved_reasoning_effort(&self, config: &AppConfig) -> Option<ReasoningEffort> {
        self.reasoning_effort.or(config.harness.reasoning_effort)
    }

    pub fn resolved_review_tools(&self, config: &AppConfig) -> Vec<String> {
        self.review_tools
            .clone()
            .unwrap_or_else(|| config.harness.review_tools.clone())
    }

    pub fn resolved_review_policy(&self, config: &AppConfig) -> ReviewPolicy {
        let defaults = config.defaults.review_policy();
        let overrides = &self.review_policy;
        ReviewPolicy {
            max_review_cycles: overrides
                .max_review_cycles
                .unwrap_or(defaults.max_review_cycles),
            critical_finding_kinds: overrides
                .critical_finding_kinds
                .clone()
                .unwrap_or(defaults.critical_finding_kinds),
            skip_unchanged_diff: overrides
                .skip_unchanged_diff
                .unwrap_or(defaults.skip_unchanged_diff),
            stack_aware: overrides.stack_aware.unwrap_or(defaults.stack_aware),
            dismiss_stale_change_requests: overrides
                .dismiss_stale_change_requests
                .unwrap_or(defaults.dismiss_stale_change_requests),
            status_on_no_new_findings: overrides
                .status_on_no_new_findings
                .unwrap_or(defaults.status_on_no_new_findings),
        }
    }
}

impl AppConfig {
    pub fn serve_enabled(&self) -> bool {
        self.serve.enabled
    }

    pub fn config_dir() -> Result<PathBuf> {
        if let Ok(path) = std::env::var("PR_REVIEWER_CONFIG_DIR") {
            return Ok(PathBuf::from(path));
        }
        let base = dirs::config_dir().context("unable to resolve config dir")?;
        Ok(base.join("pr-reviewer"))
    }

    pub fn config_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn db_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("state.db"))
    }

    pub fn pid_file() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("daemon.pid"))
    }

    pub fn load() -> Result<Self> {
        let file = Self::config_file()?;
        let data = fs::read_to_string(&file)
            .with_context(|| format!("failed to read config file at {}", file.display()))?;
        let config: AppConfig = toml::from_str(&data).context("failed to parse config TOML")?;
        Ok(config)
    }

    pub fn load_or_default() -> Result<Self> {
        let file = Self::config_file()?;
        if file.exists() {
            Self::load()
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let file = Self::config_file()?;
        let toml = toml::to_string_pretty(self).context("failed to serialize config")?;
        let tmp = file.with_extension("toml.tmp");
        fs::write(&tmp, toml)
            .with_context(|| format!("failed to write config to {}", tmp.display()))?;
        fs::rename(&tmp, &file)
            .with_context(|| format!("failed to rename tmp config to {}", file.display()))?;
        Ok(())
    }

    pub fn init() -> Result<Self> {
        let cfg = Self::default();
        cfg.save()?;
        Ok(cfg)
    }

    pub fn get_repo(&self, full_name: &str) -> Option<&RepoConfig> {
        self.repos
            .iter()
            .find(|r| r.full_name().eq_ignore_ascii_case(full_name))
    }

    pub fn add_repo(&mut self, repo: RepoConfig) -> Result<()> {
        if self
            .repos
            .iter()
            .any(|r| r.full_name().eq_ignore_ascii_case(&repo.full_name()))
        {
            return Err(anyhow!("repo already configured: {}", repo.full_name()));
        }
        self.repos.push(repo);
        Ok(())
    }

    pub fn remove_repo(&mut self, full_name: &str) -> bool {
        let before = self.repos.len();
        self.repos
            .retain(|r| !r.full_name().eq_ignore_ascii_case(full_name));
        before != self.repos.len()
    }

    pub fn set_key(&mut self, key: &str, raw_value: &str) -> Result<()> {
        let mut value =
            toml::Value::try_from(self.clone()).context("failed to project config to TOML")?;
        set_toml_path(&mut value, key, parse_toml_value(raw_value)?)?;
        let updated: AppConfig = value
            .try_into()
            .context("invalid config after applying key")?;
        // The config types ignore unknown fields, so a key that is not part of
        // the schema vanishes in the round trip above. A schema key, even an
        // optional one that was unset, is present once it holds a value.
        let reprojected =
            toml::Value::try_from(updated.clone()).context("failed to project config to TOML")?;
        if get_toml_path(&reprojected, key).is_none() {
            return Err(anyhow!("unknown config key: {key}"));
        }
        *self = updated;
        Ok(())
    }

    pub fn get_key(&self, key: &str) -> Result<Option<String>> {
        let value =
            toml::Value::try_from(self.clone()).context("failed to project config to TOML")?;
        Ok(get_toml_path(&value, key).map(|v| match v {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        }))
    }

    pub fn list_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize config")
    }
}

pub fn parse_repo_full_name(input: &str) -> Result<(String, String)> {
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(anyhow!("invalid repo format, expected owner/repo"));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn parse_toml_value(raw: &str) -> Result<toml::Value> {
    let wrapped = format!("v = {}", raw);
    if let Ok(table) = wrapped.parse::<toml::Table>() {
        if let Some(v) = table.get("v") {
            return Ok(v.clone());
        }
    }
    Ok(toml::Value::String(raw.to_string()))
}

fn set_toml_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<()> {
    let mut parts = key.split('.').peekable();
    let mut current = root;

    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let table = current
                .as_table_mut()
                .ok_or_else(|| anyhow!("key path is not a table at {part}"))?;
            table.insert(part.to_string(), value);
            return Ok(());
        }

        let table = current
            .as_table_mut()
            .ok_or_else(|| anyhow!("key path is not a table at {part}"))?;

        if !table.contains_key(part) {
            table.insert(part.to_string(), toml::Value::Table(Default::default()));
        }

        current = table
            .get_mut(part)
            .ok_or_else(|| anyhow!("failed to descend into key path"))?;
    }

    Err(anyhow!("invalid config key"))
}

fn get_toml_path<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    let mut current = root;
    for part in key.split('.') {
        let table = current.as_table()?;
        current = table.get(part)?;
    }
    Some(current)
}

/// Validate that a manually-specified repo path exists.
pub fn ensure_repo_path_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("repo path does not exist: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(review_policy: ReviewPolicyOverrides) -> RepoConfig {
        RepoConfig {
            owner: "o".to_string(),
            name: "r".to_string(),
            local_path: None,
            harness: None,
            model: None,
            reasoning_effort: None,
            review_tools: None,
            fork_policy: ForkPolicy::Ignore,
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: false,
            workflow: vec![],
            auto_fix: AutoFixConfig::default(),
            review_policy,
        }
    }

    #[test]
    fn registered_default_has_no_non_default_settings() {
        let repo = RepoConfig::registered_default("o".to_string(), "r".to_string());
        assert!(repo.non_default_settings().unwrap().is_empty());
    }

    #[test]
    fn non_default_settings_names_each_changed_setting() {
        let mut repo = RepoConfig::registered_default("o".to_string(), "r".to_string());
        repo.local_path = Some(PathBuf::from("/somewhere"));
        repo.auto_fix.enabled = true;
        repo.custom_instructions = Some("be kind".to_string());
        repo.trusted_authors = vec!["alice".to_string()];
        repo.ignore_paths.push("docs/**".to_string());
        repo.review_policy.max_review_cycles = Some(5);

        let settings = repo.non_default_settings().unwrap();

        assert!(settings.contains(&"auto_fix.enabled = true".to_string()));
        assert!(settings.contains(&"custom_instructions = \"be kind\"".to_string()));
        assert!(settings.contains(&"trusted_authors = [\"alice\"]".to_string()));
        assert!(settings.contains(&"review_policy.max_review_cycles = 5".to_string()));
        assert!(settings.iter().any(|s| s.starts_with("ignore_paths = ")));
        // Registration details (owner, name, path) are not settings.
        assert_eq!(settings.len(), 5, "{settings:?}");
    }

    #[test]
    fn review_policy_defaults_apply_when_config_omits_them() {
        let config: AppConfig = toml::from_str("[defaults]\nbot_name = \"bot\"\n").unwrap();
        let policy = repo(ReviewPolicyOverrides::default()).resolved_review_policy(&config);
        assert_eq!(policy.max_review_cycles, 2);
        assert!(policy.skip_unchanged_diff);
        assert!(policy.stack_aware);
        assert!(policy.dismiss_stale_change_requests);
        assert!(!policy.status_on_no_new_findings);
        assert_eq!(
            policy.critical_finding_kinds,
            vec![
                FindingKind::Security,
                FindingKind::SecretExposure,
                FindingKind::DataIntegrity,
                FindingKind::Correctness,
            ]
        );
    }

    #[test]
    fn per_repo_review_policy_overrides_defaults() {
        let config: AppConfig = toml::from_str(
            r#"
            [defaults]
            max_review_cycles = 3
            stack_aware = false
            critical_finding_kinds = ["security"]

            [[repos]]
            owner = "o"
            name = "r"

            [repos.review_policy]
            max_review_cycles = 0
            critical_finding_kinds = ["security", "data_integrity"]
            "#,
        )
        .unwrap();
        let policy = config.repos[0].resolved_review_policy(&config);
        assert_eq!(policy.max_review_cycles, 0);
        assert!(
            !policy.stack_aware,
            "unset per-repo field falls back to [defaults]"
        );
        assert_eq!(
            policy.critical_finding_kinds,
            vec![FindingKind::Security, FindingKind::DataIntegrity]
        );
    }

    #[test]
    fn empty_review_policy_overrides_are_not_serialized() {
        let rendered = toml::to_string(&repo(ReviewPolicyOverrides::default())).unwrap();
        assert!(!rendered.contains("review_policy"), "{rendered}");
        let rendered = toml::to_string(&repo(ReviewPolicyOverrides {
            max_review_cycles: Some(1),
            ..Default::default()
        }))
        .unwrap();
        assert!(rendered.contains("max_review_cycles = 1"), "{rendered}");
    }

    #[test]
    fn set_key_rejects_an_unknown_key_and_names_it() {
        let mut config = AppConfig::default();
        let err = config.set_key("nonsense.key", "1").unwrap_err();
        assert!(err.to_string().contains("nonsense.key"), "{err}");
        assert_eq!(
            config.list_toml().unwrap(),
            AppConfig::default().list_toml().unwrap()
        );
    }

    #[test]
    fn set_key_rejects_a_typo_inside_a_known_table() {
        let mut config = AppConfig::default();
        let err = config
            .set_key("daemon.poll_interval_sec", "31")
            .unwrap_err();
        assert!(
            err.to_string().contains("daemon.poll_interval_sec"),
            "{err}"
        );
        assert_eq!(config.daemon.poll_interval_secs, 30);
    }

    #[test]
    fn review_sessions_get_no_tools_unless_configured() {
        let config = AppConfig::default();
        let repo = repo(ReviewPolicyOverrides::default());
        assert!(repo.resolved_review_tools(&config).is_empty());
    }

    #[test]
    fn a_repo_can_override_the_global_review_tools() {
        let config: AppConfig = toml::from_str(
            "[harness]\nreview_tools = [\"Read\"]\n\n[[repos]]\nowner = \"o\"\nname = \"a\"\n\n[[repos]]\nowner = \"o\"\nname = \"b\"\nreview_tools = [\"Read\", \"Grep\"]\n\n[[repos]]\nowner = \"o\"\nname = \"c\"\nreview_tools = []\n",
        )
        .unwrap();
        let tools: Vec<Vec<String>> = config
            .repos
            .iter()
            .map(|r| r.resolved_review_tools(&config))
            .collect();
        assert_eq!(tools[0], ["Read"]);
        assert_eq!(tools[1], ["Read", "Grep"]);
        assert!(tools[2].is_empty());
    }

    #[test]
    fn set_key_writes_a_valid_key_that_get_key_returns() {
        let mut config = AppConfig::default();
        config.set_key("daemon.poll_interval_secs", "31").unwrap();
        assert_eq!(
            config
                .get_key("daemon.poll_interval_secs")
                .unwrap()
                .as_deref(),
            Some("31")
        );
    }

    #[test]
    fn set_key_accepts_an_optional_key_that_is_currently_unset() {
        let mut config = AppConfig::default();
        assert_eq!(config.get_key("harness.reasoning_effort").unwrap(), None);
        config.set_key("harness.reasoning_effort", "low").unwrap();
        assert_eq!(
            config
                .get_key("harness.reasoning_effort")
                .unwrap()
                .as_deref(),
            Some("low")
        );
    }

    #[test]
    fn set_key_still_rejects_an_invalid_value_for_a_valid_key() {
        let mut config = AppConfig::default();
        assert!(config
            .set_key("daemon.poll_interval_secs", "\"soon\"")
            .is_err());
    }
}
