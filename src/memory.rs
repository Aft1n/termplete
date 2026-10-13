//! Indexed, bounded command memory. No embeddings or network requests are needed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{ContextConfig, SuggestionScope};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_CHOICE_PREFIX_BYTES: usize = 512;
const MAX_CHOICE_PREFIXES: usize = 128;
const MAX_CHOICES_PER_PREFIX: usize = 8;
const MAX_CHOICE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 512;
const MAX_DIRECTORY_BYTES: usize = 512 * 1024;
const MAX_DIRECTORY_CANDIDATES: usize = 256;
const MAX_PROJECT_SCOPES: usize = 64;
const MAX_PROJECT_COMMANDS: usize = 256;
const MAX_PROJECT_BYTES: usize = 512 * 1024;
const EXECUTION_HALF_LIFE_MS: u64 = 3 * 24 * 60 * 60 * 1000;
const ACCEPTANCE_HALF_LIFE_MS: u64 = 2 * 24 * 60 * 60 * 1000;
const PROJECT_HALF_LIFE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const CONTEXT_HALF_LIFE_MS: u64 = 30 * 60 * 1000;
const TRUST_FRESHNESS_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub uses: u64,
    pub last_used: u64,
    /// Whether `last_used` is an actual execution timestamp rather than an
    /// undated history entry or a legacy value without provenance.
    #[serde(default)]
    pub last_used_known: bool,
    /// Source order for undated history entries; never interpreted as time.
    #[serde(default)]
    pub last_used_order: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct ChoiceUsage {
    uses: u64,
    last_used: u64,
    /// Kept separately from `last_used` so snapshots can distinguish fresh
    /// execution from an old preference that has since been used manually.
    last_accepted: u64,
    /// The working directory in which a relative accepted directory was
    /// shown. Resolved per-directory acceptances below preserve multiple
    /// destinations for the same command spelling.
    cwd: Option<PathBuf>,
    #[serde(default)]
    directory_acceptances: BTreeMap<PathBuf, DirectoryAcceptance>,
    #[serde(default)]
    project_acceptances: BTreeMap<PathBuf, DirectoryAcceptance>,
    /// A known failed execution suppresses the accepted preference until a
    /// later successful execution or a fresh explicit acceptance.
    failed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct DirectoryAcceptance {
    uses: u64,
    last_accepted: u64,
    failed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CommandMemory {
    commands: BTreeMap<String, Usage>,
    /// Full commands explicitly accepted while typing a matching prefix.
    /// Older snapshots omit this field and deserialize with an empty map.
    #[serde(default)]
    choices: BTreeMap<String, BTreeMap<String, ChoiceUsage>>,
    /// Executions of `cd`/`pushd` targets, keyed by resolved directory so
    /// equivalent spellings share evidence.
    #[serde(default)]
    directories: BTreeMap<PathBuf, Usage>,
    /// Executions grouped by nearest Git root (or cwd outside a repository).
    #[serde(default)]
    project_commands: BTreeMap<PathBuf, BTreeMap<String, Usage>>,
    imports: BTreeMap<PathBuf, (u64, u64)>,
    clock: u64,
    #[serde(skip)]
    bytes: usize,
}

/// Serializable execution and acceptance evidence for one candidate.
/// Timestamps are milliseconds since the Unix epoch; zero means unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub uses: u64,
    pub last_used: u64,
    pub last_used_known: bool,
    pub last_used_order: u64,
    pub accepts: u64,
    pub last_accepted: u64,
    /// Length of the most specific accepted prefix, capped during aggregation.
    pub specificity: u16,
    /// Whether recent terminal context explicitly contained this candidate.
    pub context_match: bool,
    /// Observation time in epoch milliseconds; zero means context time is unknown.
    pub context_last_seen: u64,
    /// Project-scoped execution and acceptance evidence. Zero timestamps are unknown.
    #[serde(default)]
    pub project_scope: bool,
    #[serde(default)]
    pub project_match: bool,
    #[serde(default)]
    pub project_uses: u64,
    #[serde(default)]
    pub project_last_used: u64,
    #[serde(default)]
    pub project_accepts: u64,
    #[serde(default)]
    pub project_last_accepted: u64,
}

/// Shared local rank and a bounded measure of the fresh evidence behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RankedEvidence {
    pub score: i32,
    pub confidence: f64,
    pub project_score: i32,
}

/// Rank evidence using the current wall clock.
pub fn rank_evidence(evidence: &Evidence) -> RankedEvidence {
    rank_evidence_at(evidence, now())
}

/// Deterministic form of [`rank_evidence`] for tests and callers with a fixed timestamp.
pub fn rank_evidence_at(evidence: &Evidence, timestamp: u64) -> RankedEvidence {
    let execution_freshness = if evidence.last_used_known {
        freshness(evidence.last_used, timestamp, EXECUTION_HALF_LIFE_MS)
    } else if evidence.uses > 0 {
        0.1
    } else {
        0.0
    };
    let acceptance_freshness =
        freshness(evidence.last_accepted, timestamp, ACCEPTANCE_HALF_LIFE_MS);
    let context_freshness = if evidence.context_match {
        if evidence.context_last_seen == 0 {
            1.0
        } else {
            freshness(evidence.context_last_seen, timestamp, CONTEXT_HALF_LIFE_MS)
        }
    } else {
        0.0
    };
    let execution_frequency = saturated_frequency(evidence.uses, 650, 4);
    let acceptance_frequency = (saturated_frequency(evidence.accepts, 600, 2) as f64
        * acceptance_freshness)
        .round() as i32;
    let execution_recency = (2_000.0 * execution_freshness).round() as i32;
    let acceptance_recency = (2_600.0 * acceptance_freshness).round() as i32;
    let specificity = ((evidence.specificity.min(MAX_CHOICE_PREFIX_BYTES as u16) as f64
        * acceptance_freshness)
        / 4.0)
        .round() as i32;
    // Explicit terminal context stays authoritative over ordinary history;
    // freshness decays only the bonus above its minimum priority.
    let context_score = if evidence.context_match {
        if evidence.context_last_seen == 0 {
            9_000
        } else {
            (9_000.0 + 1_000.0 * context_freshness).round() as i32
        }
    } else {
        0
    };
    let project_execution_freshness =
        freshness(evidence.project_last_used, timestamp, PROJECT_HALF_LIFE_MS);
    let project_acceptance_freshness = freshness(
        evidence.project_last_accepted,
        timestamp,
        PROJECT_HALF_LIFE_MS,
    );
    let project_score = if evidence.project_match {
        ((saturated_frequency(evidence.project_uses, 250, 4) as f64 + 1_700.0)
            * project_execution_freshness
            + (saturated_frequency(evidence.project_accepts, 250, 2) as f64 + 1_700.0)
                * project_acceptance_freshness)
            .round()
            .min(2_800.0) as i32
    } else {
        0
    };
    let score = execution_frequency
        + execution_recency
        + acceptance_frequency
        + acceptance_recency
        + specificity
        + context_score
        + project_score;

    let execution_strength = count_strength(evidence.uses, 3) * execution_freshness;
    let acceptance_strength = count_strength(evidence.accepts, 2) * acceptance_freshness;
    let context_strength = if evidence.context_match {
        if evidence.context_last_seen == 0 {
            0.75
        } else {
            0.75 + 0.20 * context_freshness
        }
    } else {
        0.0
    };
    let project_strength = if evidence.project_match {
        (count_strength(evidence.project_uses, 2) * project_execution_freshness
            + count_strength(evidence.project_accepts, 2) * project_acceptance_freshness)
            .clamp(0.0, 1.0)
    } else {
        0.0
    };
    let confidence = 1.0
        - (1.0 - execution_strength)
            * (1.0 - acceptance_strength)
            * (1.0 - context_strength)
            * (1.0 - project_strength);
    RankedEvidence {
        score,
        confidence: confidence.clamp(0.0, 1.0),
        project_score,
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, Default)]
struct ChoiceStats {
    accepts: u64,
    specificity: usize,
    last_used: u64,
    last_accepted: u64,
}

impl CommandMemory {
    pub fn len(&self) -> usize {
        self.commands.len()
    }
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
    pub fn contains(&self, command: &str) -> bool {
        self.commands.contains_key(command)
    }

    pub fn record(&mut self, command: &str, exit_code: Option<i32>, cfg: &ContextConfig) -> bool {
        self.record_with_cwd(command, exit_code, cfg, None)
    }

    pub fn record_with_cwd(
        &mut self,
        command: &str,
        exit_code: Option<i32>,
        cfg: &ContextConfig,
        cwd: Option<&Path>,
    ) -> bool {
        let Some(command) = clean(command, cfg) else {
            return false;
        };
        let directory = crate::fs_context::directory_identity(&command, cwd);
        let project = crate::fs_context::project_identity(cwd);
        if exit_code.is_some_and(|code| code != 0) {
            self.set_choice_failure(&command, directory.as_deref(), project.as_deref(), true);
            return false;
        }
        self.set_choice_failure(&command, directory.as_deref(), project.as_deref(), false);
        self.clock = now().max(self.clock.saturating_add(1));
        if !self.commands.contains_key(&command) {
            self.bytes += command.len();
        }
        let usage = self.commands.entry(command.clone()).or_default();
        usage.uses = usage.uses.saturating_add(1);
        usage.last_used = self.clock;
        usage.last_used_known = true;
        usage.last_used_order = self.clock;
        if let Some(project) = project {
            let usage = self
                .project_commands
                .entry(project)
                .or_default()
                .entry(command)
                .or_default();
            usage.uses = usage.uses.saturating_add(1);
            usage.last_used = self.clock;
            usage.last_used_known = true;
            usage.last_used_order = self.clock;
        }
        if let Some(directory) = directory {
            let usage = self.directories.entry(directory).or_default();
            usage.uses = usage.uses.saturating_add(1);
            usage.last_used = self.clock;
            usage.last_used_known = true;
            usage.last_used_order = self.clock;
        }
        self.enforce_bounds(cfg);
        true
    }

    /// Remember an explicitly accepted completion for the prefix that produced it.
    /// Neither the prefix nor command is stored if it looks malformed or secret.
    pub fn accept(&mut self, prefix: &str, command: &str, cfg: &ContextConfig) -> bool {
        self.accept_with_cwd(prefix, command, cfg, None)
    }

    pub fn accept_with_cwd(
        &mut self,
        prefix: &str,
        command: &str,
        cfg: &ContextConfig,
        cwd: Option<&Path>,
    ) -> bool {
        let prefix = prefix.trim_start();
        let safe_prefix = prefix.trim_end();
        if safe_prefix.is_empty()
            || prefix.len() > MAX_CHOICE_PREFIX_BYTES
            || clean(safe_prefix, cfg).as_deref() != Some(safe_prefix)
        {
            return false;
        }
        let Some(command) = clean(command, cfg) else {
            return false;
        };
        if !command.starts_with(prefix) || command.len() <= prefix.len() {
            return false;
        }

        let directory = crate::fs_context::directory_identity(&command, cwd);
        let project = crate::fs_context::project_identity(cwd);
        self.clock = now().max(self.clock.saturating_add(1));
        let usage = self
            .choices
            .entry(prefix.to_string())
            .or_default()
            .entry(command)
            .or_default();
        usage.uses = usage.uses.saturating_add(1);
        usage.last_used = self.clock;
        usage.last_accepted = self.clock;
        usage.cwd = cwd.map(Path::to_path_buf);
        if let Some(directory) = directory {
            let acceptance = usage.directory_acceptances.entry(directory).or_default();
            acceptance.uses = acceptance.uses.saturating_add(1);
            acceptance.last_accepted = self.clock;
            acceptance.failed = false;
        }
        if let Some(project) = project {
            let acceptance = usage.project_acceptances.entry(project).or_default();
            acceptance.uses = acceptance.uses.saturating_add(1);
            acceptance.last_accepted = self.clock;
            acceptance.failed = false;
        }
        usage.failed = false;
        self.enforce_bounds(cfg);
        true
    }

    pub fn enforce_bounds(&mut self, cfg: &ContextConfig) {
        if self.commands.len() > cfg.command_memory_entries || self.bytes > MAX_BYTES {
            let timestamp = now();
            let mut oldest: Vec<_> = self
                .commands
                .iter()
                .map(|(key, usage)| {
                    let evidence = usage_evidence(usage);
                    let score = rank_evidence_at(&evidence, timestamp).score;
                    (
                        key.clone(),
                        score,
                        evidence.last_used_known,
                        evidence.last_used,
                        evidence.last_used_order,
                    )
                })
                .collect();
            oldest.sort_unstable_by_key(|(_, score, known, last_used, order)| {
                (*score, *known, *last_used, *order)
            });
            for (key, _, _, _, _) in oldest {
                if self.commands.len() <= cfg.command_memory_entries && self.bytes <= MAX_BYTES {
                    break;
                }
                self.bytes -= key.len();
                self.commands.remove(&key);
            }
        }
        while self.imports.len() > 16 {
            let oldest = self
                .imports
                .iter()
                .min_by_key(|(_, stamp)| stamp.1)
                .map(|(path, _)| path.clone())
                .unwrap();
            self.imports.remove(&oldest);
        }

        while self.directories.len() > MAX_DIRECTORY_ENTRIES
            || self.directory_bytes() > MAX_DIRECTORY_BYTES
        {
            let timestamp = now();
            let oldest = self
                .directories
                .iter()
                .min_by_key(|(path, usage)| {
                    let evidence = usage_evidence(usage);
                    let ranked = rank_evidence_at(&evidence, timestamp);
                    (
                        ranked.score,
                        evidence.last_used_known,
                        evidence.last_used,
                        evidence.last_used_order,
                        (*path).clone(),
                    )
                })
                .map(|(path, _)| path.clone())
                .unwrap();
            self.directories.remove(&oldest);
        }

        let commands = &self.commands;
        self.project_commands.retain(|_, project_commands| {
            project_commands.retain(|command, _| commands.contains_key(command));
            !project_commands.is_empty()
        });
        for project_commands in self.project_commands.values_mut() {
            while project_commands.len() > MAX_PROJECT_COMMANDS {
                let timestamp = now();
                let oldest = project_commands
                    .iter()
                    .min_by_key(|(command, usage)| {
                        let evidence = usage_evidence(usage);
                        (
                            rank_evidence_at(&evidence, timestamp).score,
                            evidence.last_used,
                            (*command).clone(),
                        )
                    })
                    .map(|(command, _)| command.clone())
                    .unwrap();
                project_commands.remove(&oldest);
            }
        }
        while self.project_commands.len() > MAX_PROJECT_SCOPES
            || self.project_command_bytes() > MAX_PROJECT_BYTES
        {
            let oldest = self
                .project_commands
                .iter()
                .min_by_key(|(_, commands)| {
                    commands
                        .values()
                        .map(|usage| usage.last_used)
                        .max()
                        .unwrap_or(0)
                })
                .map(|(project, _)| project.clone())
                .unwrap();
            self.project_commands.remove(&oldest);
        }

        self.choices.retain(|prefix, choices| {
            let safe_prefix = prefix.trim_end();
            let prefix_is_safe = !safe_prefix.is_empty()
                && prefix.len() <= MAX_CHOICE_PREFIX_BYTES
                && clean(safe_prefix, cfg).as_deref() == Some(safe_prefix);
            if !prefix_is_safe {
                return false;
            }
            choices.retain(|command, usage| {
                while usage.project_acceptances.len() > MAX_PROJECT_SCOPES {
                    let oldest = usage
                        .project_acceptances
                        .iter()
                        .min_by_key(|(_, acceptance)| acceptance.last_accepted)
                        .map(|(project, _)| project.clone())
                        .unwrap();
                    usage.project_acceptances.remove(&oldest);
                }
                clean(command, cfg).as_deref() == Some(command.as_str())
                    && command.starts_with(prefix)
                    && command.len() > prefix.len()
            });
            !choices.is_empty()
        });
        for (prefix, choices) in &mut self.choices {
            while choices.len() > MAX_CHOICES_PER_PREFIX {
                let timestamp = now();
                let oldest = choices
                    .iter()
                    .min_by_key(|(command, usage)| {
                        let evidence = choice_usage_evidence(prefix, usage);
                        let score = if choice_is_failed(usage) {
                            0
                        } else {
                            rank_evidence_at(&evidence, timestamp).score
                        };
                        (score, evidence.last_accepted, (*command).clone())
                    })
                    .map(|(command, _)| command.clone())
                    .unwrap();
                choices.remove(&oldest);
            }
        }
        while self.choices.len() > MAX_CHOICE_PREFIXES || self.choice_bytes() > MAX_CHOICE_BYTES {
            let timestamp = now();
            let oldest = self
                .choices
                .iter()
                .min_by_key(|(prefix, choices)| {
                    let evidence = prefix_evidence(prefix, choices);
                    (
                        rank_evidence_at(&evidence, timestamp).score,
                        evidence.last_accepted,
                        (*prefix).clone(),
                    )
                })
                .map(|(prefix, _)| prefix.clone())
                .unwrap();
            self.choices.remove(&oldest);
        }
    }

    /// Rank matching command history with the shared execution/acceptance score.
    pub fn prefix(&self, line: &str, max: usize) -> Vec<(String, i32)> {
        self.prefix_with_cwd(line, max, None)
    }

    pub fn prefix_with_cwd(
        &self,
        line: &str,
        max: usize,
        cwd: Option<&Path>,
    ) -> Vec<(String, i32)> {
        self.prefix_with_cwd_filtered(line, max, cwd, |_| true)
    }

    /// Rank matching command history after filtering candidates and before the
    /// result limit, so excluded high-ranked entries cannot starve later ones.
    pub fn prefix_with_cwd_filtered(
        &self,
        line: &str,
        max: usize,
        cwd: Option<&Path>,
        include: impl Fn(&str) -> bool,
    ) -> Vec<(String, i32)> {
        self.prefix_with_scope_filtered(line, max, cwd, SuggestionScope::Global, include)
    }

    pub fn prefix_with_scope_filtered(
        &self,
        line: &str,
        max: usize,
        cwd: Option<&Path>,
        scope: SuggestionScope,
        include: impl Fn(&str) -> bool,
    ) -> Vec<(String, i32)> {
        let prefix = line.trim_start();
        if prefix.is_empty() || max == 0 {
            return Vec::new();
        }
        let leading = &line[..line.len() - prefix.len()];
        let timestamp = now();
        let max = max.min(20);
        let project = (scope == SuggestionScope::Folder)
            .then(|| crate::fs_context::project_identity(cwd))
            .flatten();
        let mut candidates = std::collections::BTreeSet::new();
        for (command, _) in self
            .commands
            .range(prefix.to_string()..)
            .take_while(|(command, _)| command.starts_with(prefix))
            .filter(|(command, _)| command.as_str() != prefix)
        {
            candidates.insert(command.clone());
        }
        for command in self
            .matching_choices(prefix, cwd, project.as_deref())
            .into_keys()
        {
            candidates.insert(command);
        }
        let mut matches: Vec<(String, Evidence, i32)> = candidates
            .into_iter()
            .filter(|command| include(command))
            .map(|command| {
                let evidence =
                    self.candidate_evidence_in_project(&command, cwd, scope, project.as_deref());
                let ranked = rank_evidence_at(&evidence, timestamp);
                (command, evidence, ranked.score)
            })
            .collect();
        matches.sort_unstable_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| b.1.last_used_known.cmp(&a.1.last_used_known))
                .then_with(|| b.1.last_used.cmp(&a.1.last_used))
                .then_with(|| b.1.last_accepted.cmp(&a.1.last_accepted))
                .then_with(|| b.1.last_used_order.cmp(&a.1.last_used_order))
                .then_with(|| a.0.cmp(&b.0))
        });
        matches.truncate(max);
        matches
            .into_iter()
            .map(|(command, _, score)| (format!("{leading}{command}"), score))
            .collect()
    }

    /// Accepted completions that match this prefix, with most accepted first.
    pub fn accepted_choices(&self, line: &str, max: usize) -> Vec<(String, u64)> {
        self.accepted_choices_with_scope(line, max, None, SuggestionScope::Global)
    }

    pub fn accepted_choices_with_scope(
        &self,
        line: &str,
        max: usize,
        cwd: Option<&Path>,
        scope: SuggestionScope,
    ) -> Vec<(String, u64)> {
        if max == 0 {
            return Vec::new();
        }
        let prefix = line.trim_start();
        let timestamp = now();
        let project = (scope == SuggestionScope::Folder)
            .then(|| crate::fs_context::project_identity(cwd))
            .flatten();
        let mut choices: Vec<_> = self
            .matching_choices(prefix, cwd, project.as_deref())
            .into_iter()
            .map(|(command, stats)| {
                let evidence =
                    self.candidate_evidence_in_project(&command, cwd, scope, project.as_deref());
                (
                    command,
                    stats.accepts,
                    stats.last_accepted.max(stats.last_used),
                    rank_evidence_at(&evidence, timestamp).score,
                )
            })
            .collect();
        choices.sort_unstable_by(|a, b| {
            b.3.cmp(&a.3)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        choices
            .into_iter()
            .take(max.min(MAX_CHOICES_PER_PREFIX))
            .map(|(command, accepts, _, _)| (command, accepts))
            .collect()
    }

    /// A repeated acceptance for this prefix is strong enough to skip the model.
    pub fn has_trusted_choice(&self, line: &str) -> bool {
        self.has_trusted_choice_with_scope(line, None, SuggestionScope::Global)
    }

    pub fn has_trusted_choice_with_scope(
        &self,
        line: &str,
        cwd: Option<&Path>,
        scope: SuggestionScope,
    ) -> bool {
        let timestamp = now();
        let candidates = self.prefix_with_scope_filtered(line, 20, cwd, scope, |_| true);
        let Some((winner, _)) = candidates.first() else {
            return false;
        };
        let command = winner.trim_start();
        let evidence = self.candidate_evidence_with_scope(command, cwd, scope);
        let (accepts, last_accepted) = if evidence.project_scope {
            (evidence.project_accepts, evidence.project_last_accepted)
        } else {
            (evidence.accepts, evidence.last_accepted)
        };
        if accepts < 2
            || last_accepted == 0
            || timestamp.saturating_sub(last_accepted) > TRUST_FRESHNESS_MS
        {
            return false;
        }
        let runner_up = candidates.get(1).map(|(_, score)| *score).unwrap_or(0);
        let ranked = rank_evidence_at(&evidence, timestamp);
        ranked.score >= 2_500
            && ranked.score.saturating_sub(runner_up) >= 300
            && ranked.confidence >= 0.45
    }

    /// Evidence for a command candidate, including its resolved directory and
    /// accepted preference when those identities match the supplied cwd.
    pub fn candidate_evidence(&self, command: &str, cwd: Option<&Path>) -> Evidence {
        self.candidate_evidence_with_scope(command, cwd, SuggestionScope::Global)
    }

    pub fn candidate_evidence_with_scope(
        &self,
        command: &str,
        cwd: Option<&Path>,
        scope: SuggestionScope,
    ) -> Evidence {
        let project = (scope == SuggestionScope::Folder)
            .then(|| crate::fs_context::project_identity(cwd))
            .flatten();
        self.candidate_evidence_in_project(command, cwd, scope, project.as_deref())
    }

    pub(crate) fn candidate_evidence_in_project(
        &self,
        command: &str,
        cwd: Option<&Path>,
        scope: SuggestionScope,
        project: Option<&Path>,
    ) -> Evidence {
        let mut evidence =
            if let Some(directory) = crate::fs_context::directory_identity(command, cwd) {
                self.directory_evidence(&directory)
            } else if crate::fs_context::is_directory_command(command) {
                Evidence::default()
            } else {
                let mut evidence = Evidence::default();
                if let Some(usage) = self.commands.get(command) {
                    merge_usage(&mut evidence, usage);
                }
                self.merge_command_acceptances(command, None, &mut evidence);
                evidence
            };
        if scope == SuggestionScope::Folder {
            evidence.project_scope = project.is_some();
            if let Some(project) = project {
                self.merge_project_evidence(command, project, &mut evidence);
            }
        }
        evidence
    }

    /// Evidence learned for one normalized directory identity.
    pub fn directory_evidence(&self, identity: &Path) -> Evidence {
        let mut evidence = Evidence::default();
        if let Some(usage) = self.directories.get(identity) {
            merge_usage(&mut evidence, usage);
        }
        for (prefix, choices) in &self.choices {
            for usage in choices.values() {
                let Some(acceptance) = usage.directory_acceptances.get(identity) else {
                    continue;
                };
                if !acceptance.failed {
                    add_directory_acceptance_evidence(&mut evidence, prefix, acceptance);
                }
            }
        }
        evidence
    }

    /// Recent remembered paths for directory candidate injection, already
    /// ranked with the same evidence function used for command suggestions.
    pub fn directory_candidates(&self) -> Vec<(PathBuf, Evidence)> {
        let mut identities: BTreeSet<PathBuf> = self.directories.keys().cloned().collect();
        for choices in self.choices.values() {
            for usage in choices.values() {
                identities.extend(
                    usage
                        .directory_acceptances
                        .iter()
                        .filter(|(_, acceptance)| !acceptance.failed)
                        .map(|(identity, _)| identity.clone()),
                );
            }
        }
        let timestamp = now();
        let mut candidates: Vec<_> = identities
            .into_iter()
            .map(|identity| {
                let evidence = self.directory_evidence(&identity);
                let score = rank_evidence_at(&evidence, timestamp).score;
                (identity, evidence, score)
            })
            .collect();
        candidates.sort_unstable_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| b.1.last_used_known.cmp(&a.1.last_used_known))
                .then_with(|| b.1.last_used.cmp(&a.1.last_used))
                .then_with(|| b.1.last_accepted.cmp(&a.1.last_accepted))
                .then_with(|| b.1.last_used_order.cmp(&a.1.last_used_order))
                .then_with(|| a.0.cmp(&b.0))
        });
        candidates
            .into_iter()
            .take(MAX_DIRECTORY_CANDIDATES)
            .map(|(identity, evidence, _)| (identity, evidence))
            .collect()
    }

    fn merge_command_acceptances(
        &self,
        command: &str,
        identity: Option<&Path>,
        evidence: &mut Evidence,
    ) {
        for (prefix, choices) in &self.choices {
            let Some(usage) = choices.get(command) else {
                continue;
            };
            if let Some(identity) = identity {
                if let Some(acceptance) = usage.directory_acceptances.get(identity) {
                    if !acceptance.failed {
                        add_directory_acceptance_evidence(evidence, prefix, acceptance);
                    }
                }
            } else if usage.directory_acceptances.is_empty() && !usage.failed {
                add_choice_evidence(evidence, prefix, usage);
            }
        }
    }

    fn merge_project_evidence(&self, command: &str, project: &Path, evidence: &mut Evidence) {
        if let Some(usage) = self
            .project_commands
            .get(project)
            .and_then(|commands| commands.get(command))
        {
            evidence.project_match = true;
            evidence.project_uses = usage.uses;
            evidence.project_last_used = usage.last_used;
        }
        for choices in self.choices.values() {
            if let Some(acceptance) = choices
                .get(command)
                .and_then(|usage| usage.project_acceptances.get(project))
                .filter(|acceptance| !acceptance.failed && acceptance.uses > 0)
            {
                evidence.project_match = true;
                evidence.project_accepts = evidence.project_accepts.saturating_add(acceptance.uses);
                evidence.project_last_accepted =
                    evidence.project_last_accepted.max(acceptance.last_accepted);
            }
        }
    }

    fn matching_choices(
        &self,
        prefix: &str,
        cwd: Option<&Path>,
        project: Option<&Path>,
    ) -> BTreeMap<String, ChoiceStats> {
        if prefix.is_empty() {
            return BTreeMap::new();
        }
        let mut matches = BTreeMap::<String, ChoiceStats>::new();
        for (learned_prefix, choices) in &self.choices {
            if !prefix.starts_with(learned_prefix) && !learned_prefix.starts_with(prefix) {
                continue;
            }
            for (command, usage) in choices {
                if command == prefix || !command.starts_with(prefix) {
                    continue;
                }
                let identity = crate::fs_context::directory_identity(command, cwd);
                let local = project
                    .and_then(|project| usage.project_acceptances.get(project))
                    .filter(|acceptance| acceptance.failed || acceptance.uses > 0);
                let fallback = if let Some(acceptance) = identity
                    .as_ref()
                    .and_then(|identity| usage.directory_acceptances.get(identity))
                {
                    Some((acceptance.uses, acceptance.last_accepted, acceptance.failed))
                } else if !crate::fs_context::is_directory_command(command)
                    && usage.directory_acceptances.is_empty()
                {
                    Some((usage.uses, usage.last_accepted, usage.failed))
                } else {
                    None
                };
                let (accepts, last_accepted, failed) = match local {
                    Some(acceptance) => {
                        (acceptance.uses, acceptance.last_accepted, acceptance.failed)
                    }
                    None => match fallback {
                        Some(stats) => stats,
                        None => continue,
                    },
                };
                if failed {
                    continue;
                }
                let stats = matches.entry(command.clone()).or_default();
                stats.accepts = stats.accepts.saturating_add(accepts);
                stats.specificity = stats
                    .specificity
                    .max(learned_prefix.len().min(MAX_CHOICE_PREFIX_BYTES));
                stats.last_used = stats.last_used.max(last_accepted);
                stats.last_accepted = stats.last_accepted.max(last_accepted);
            }
        }
        matches
    }

    fn choice_bytes(&self) -> usize {
        self.choices
            .iter()
            .map(|(prefix, choices)| {
                prefix.len()
                    + choices
                        .iter()
                        .map(|(command, usage)| {
                            command.len()
                                + usage.cwd.as_ref().map_or(0, |path| path.as_os_str().len())
                                + usage
                                    .directory_acceptances
                                    .keys()
                                    .map(|path| path.as_os_str().len())
                                    .sum::<usize>()
                                + usage
                                    .project_acceptances
                                    .keys()
                                    .map(|path| path.as_os_str().len())
                                    .sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum()
    }

    fn directory_bytes(&self) -> usize {
        self.directories
            .keys()
            .map(|path| path.as_os_str().len())
            .sum()
    }

    fn project_command_bytes(&self) -> usize {
        self.project_commands
            .iter()
            .map(|(project, commands)| {
                project.as_os_str().len() + commands.keys().map(String::len).sum::<usize>()
            })
            .sum()
    }

    fn set_choice_failure(
        &mut self,
        command: &str,
        identity: Option<&Path>,
        project: Option<&Path>,
        failed: bool,
    ) {
        for choices in self.choices.values_mut() {
            if let Some(usage) = choices.get_mut(command) {
                if let Some(identity) = identity {
                    if let Some(acceptance) = usage.directory_acceptances.get_mut(identity) {
                        acceptance.failed = failed;
                    }
                }
                if let Some(project) = project {
                    usage
                        .project_acceptances
                        .entry(project.to_path_buf())
                        .or_default()
                        .failed = failed;
                }
                if usage.directory_acceptances.is_empty() {
                    usage.failed = failed;
                }
            }
        }
    }

    /// Retrieve options for the same executable even when flags differ.
    pub fn related(&self, line: &str, max: usize) -> Vec<(&str, &Usage)> {
        self.related_with_scope(line, max, None, SuggestionScope::Global)
    }

    pub fn related_with_scope(
        &self,
        line: &str,
        max: usize,
        cwd: Option<&Path>,
        scope: SuggestionScope,
    ) -> Vec<(&str, &Usage)> {
        let Some(executable) = line.split_whitespace().next() else {
            return Vec::new();
        };
        let terms: Vec<_> = line.split_whitespace().skip(1).collect();
        let timestamp = now();
        let max = max.min(20);
        if max == 0 {
            return Vec::new();
        }
        let project = (scope == SuggestionScope::Folder)
            .then(|| crate::fs_context::project_identity(cwd))
            .flatten();
        let mut matches = Vec::with_capacity(max + 1);
        for (command, usage) in self
            .commands
            .range(executable.to_string()..)
            .take_while(|(command, _)| command.starts_with(executable))
            .filter(|(command, _)| command.split_whitespace().next() == Some(executable))
        {
            let overlap = terms
                .iter()
                .filter(|term| command.split_whitespace().any(|part| part == **term))
                .count();
            let evidence =
                self.candidate_evidence_in_project(command, cwd, scope, project.as_deref());
            matches.push((
                command.as_str(),
                usage,
                evidence,
                rank_evidence_at(&evidence, timestamp).score + (overlap as i32).min(20) * 50,
            ));
            matches.sort_unstable_by(|a, b| {
                b.3.cmp(&a.3)
                    .then_with(|| b.2.last_used_known.cmp(&a.2.last_used_known))
                    .then_with(|| b.2.last_used.cmp(&a.2.last_used))
                    .then_with(|| b.2.last_accepted.cmp(&a.2.last_accepted))
                    .then_with(|| b.2.last_used_order.cmp(&a.2.last_used_order))
                    .then_with(|| a.0.cmp(b.0))
            });
            matches.truncate(max);
        }
        matches
            .into_iter()
            .map(|(command, usage, _, _)| (command, usage))
            .collect()
    }

    pub fn import_file(&mut self, path: &Path, cfg: &ContextConfig) -> std::io::Result<usize> {
        let mut file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let stamp = (
            metadata.len(),
            modified
                .as_secs()
                .saturating_mul(1_000_000_000)
                .saturating_add(u64::from(modified.subsec_nanos())),
        );
        if self.imports.get(path) == Some(&stamp) {
            return Ok(0);
        }
        let offset = metadata.len().saturating_sub(MAX_BYTES as u64);
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let text = if offset > 0 {
            text.split_once('\n').map(|(_, tail)| tail).unwrap_or("")
        } else {
            &text
        };
        let mut counts: BTreeMap<String, Usage> = BTreeMap::new();
        let lines: Vec<_> = text.lines().collect();
        let timestamped_bash = lines.iter().any(|line| {
            line.trim_end_matches('\r')
                .strip_prefix('#')
                .is_some_and(|value| value.parse::<u64>().is_ok())
        });
        let mut timestamp = None;
        let mut skip_bash_record = false;
        let mut continuation = false;
        for (index, line) in lines.iter().enumerate() {
            let line = line.trim_end_matches('\r');
            let continued = line.ends_with('\\');
            if continuation || continued {
                continuation = continued;
                continue;
            }
            if let Some(value) = line
                .strip_prefix('#')
                .and_then(|value| value.parse::<u64>().ok())
            {
                timestamp = Some(value);
                // Timestamped Bash records can span several physical lines.
                // They cannot be used as single-line inline completions.
                skip_bash_record = timestamped_bash
                    && lines[index + 1..]
                        .iter()
                        .take_while(|line| {
                            !line.strip_prefix('#').is_some_and(|value| {
                                value.trim_end_matches('\r').parse::<u64>().is_ok()
                            })
                        })
                        .take(2)
                        .count()
                        > 1;
                continue;
            }
            if skip_bash_record {
                continue;
            }
            let (command, native_time) = match line
                .strip_prefix(": ")
                .and_then(|value| value.split_once(';'))
            {
                Some((header, command))
                    if header.split_once(':').is_some_and(|(time, duration)| {
                        time.parse::<u64>().is_ok() && duration.parse::<u64>().is_ok()
                    }) =>
                {
                    (
                        command,
                        header
                            .split(':')
                            .next()
                            .and_then(|time| time.parse::<u64>().ok()),
                    )
                }
                _ => (line, timestamp.take()),
            };
            let Some(command) = clean(command, cfg) else {
                continue;
            };
            let usage = counts.entry(command).or_default();
            usage.uses = usage.uses.saturating_add(1);
            let order = (index as u64).saturating_add(1);
            if let Some(native_time) = native_time {
                let timestamp = native_time.saturating_mul(1000).min(now());
                if !usage.last_used_known || timestamp >= usage.last_used {
                    usage.last_used = timestamp;
                    usage.last_used_known = true;
                    usage.last_used_order = order;
                }
            } else if !usage.last_used_known {
                usage.last_used = 0;
                usage.last_used_order = usage.last_used_order.max(order);
            }
        }
        let imported = counts.len();
        let mut directory_counts = BTreeMap::<PathBuf, Usage>::new();
        for (command, usage) in &counts {
            if let Some(directory) = crate::fs_context::directory_identity(command, None) {
                let directory_usage = directory_counts.entry(directory).or_default();
                directory_usage.uses = directory_usage.uses.saturating_add(usage.uses);
                merge_last_used(directory_usage, usage);
            }
        }
        for (command, imported) in counts {
            if !self.commands.contains_key(&command) {
                self.bytes += command.len();
            }
            let usage = self.commands.entry(command).or_default();
            // Native and TermPlete histories overlap. Never count the same file
            // again, or add its entire count to executions already recorded live.
            usage.uses = usage.uses.max(imported.uses);
            merge_last_used(usage, &imported);
        }
        for (directory, imported) in directory_counts {
            let usage = self.directories.entry(directory).or_default();
            usage.uses = usage.uses.max(imported.uses);
            merge_last_used(usage, &imported);
        }
        self.imports.insert(path.to_path_buf(), stamp);
        self.enforce_bounds(cfg);
        Ok(imported)
    }

    pub fn load(path: &Path, cfg: &ContextConfig) -> std::io::Result<Self> {
        let mut memory: Self = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error),
        };
        memory
            .commands
            .retain(|command, _| clean(command, cfg).as_ref() == Some(command));
        for usage in memory.commands.values_mut() {
            if !usage.last_used_known {
                // Older snapshots used `last_used` for synthetic import
                // timestamps. Preserve only its relative tie-break value.
                usage.last_used_order = usage.last_used_order.max(usage.last_used);
                usage.last_used = 0;
            }
        }
        for usage in memory.directories.values_mut() {
            if !usage.last_used_known {
                usage.last_used_order = usage.last_used_order.max(usage.last_used);
                usage.last_used = 0;
            }
        }
        for choices in memory.choices.values_mut() {
            for (command, usage) in choices {
                if usage.last_accepted == 0 {
                    usage.last_accepted = usage.last_used;
                }
                if usage.directory_acceptances.is_empty() {
                    if let Some(directory) =
                        crate::fs_context::directory_identity(command, usage.cwd.as_deref())
                    {
                        usage.directory_acceptances.insert(
                            directory,
                            DirectoryAcceptance {
                                uses: usage.uses,
                                last_accepted: usage.last_accepted,
                                failed: usage.failed,
                            },
                        );
                    }
                }
            }
        }
        if memory.directories.is_empty() {
            let mut inferred = BTreeMap::<PathBuf, Usage>::new();
            for (command, usage) in &memory.commands {
                if let Some(directory) = crate::fs_context::directory_identity(command, None) {
                    let directory_usage = inferred.entry(directory).or_default();
                    directory_usage.uses = directory_usage.uses.saturating_add(usage.uses);
                    merge_last_used(directory_usage, usage);
                }
            }
            memory.directories = inferred;
        }
        memory.bytes = memory.commands.keys().map(String::len).sum();
        memory.clock = memory
            .commands
            .values()
            .filter(|usage| usage.last_used_known)
            .map(|usage| usage.last_used)
            .chain(
                memory
                    .directories
                    .values()
                    .filter(|usage| usage.last_used_known)
                    .map(|usage| usage.last_used),
            )
            .chain(memory.choices.values().flat_map(|choices| {
                choices
                    .values()
                    .flat_map(|usage| [usage.last_used, usage.last_accepted])
            }))
            .max()
            .unwrap_or(0);
        memory.enforce_bounds(cfg);
        Ok(memory)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("memory path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut file, self).map_err(std::io::Error::other)?;
        file.flush()?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn freshness(last_seen: u64, timestamp: u64, half_life_ms: u64) -> f64 {
    if last_seen == 0 {
        return 0.0;
    }
    let age = timestamp.saturating_sub(last_seen) as f64;
    2.0_f64.powf(-age / half_life_ms as f64)
}

fn saturated_frequency(count: u64, maximum: u64, offset: u64) -> i32 {
    if count == 0 {
        return 0;
    }
    let count = u128::from(count);
    let score = u128::from(maximum) * count / (count + u128::from(offset));
    score.min(i32::MAX as u128) as i32
}

fn count_strength(count: u64, offset: u64) -> f64 {
    if count == 0 {
        return 0.0;
    }
    count as f64 / (count.saturating_add(offset)) as f64
}

fn merge_usage(evidence: &mut Evidence, usage: &Usage) {
    evidence.uses = evidence.uses.max(usage.uses);
    if usage.last_used_known {
        if !evidence.last_used_known || usage.last_used > evidence.last_used {
            evidence.last_used = usage.last_used;
            evidence.last_used_known = true;
            evidence.last_used_order = usage.last_used_order;
        } else if usage.last_used == evidence.last_used {
            evidence.last_used_order = evidence.last_used_order.max(usage.last_used_order);
        }
    } else if !evidence.last_used_known {
        evidence.last_used = 0;
        evidence.last_used_order = evidence.last_used_order.max(usage.last_used_order);
    }
}

fn merge_last_used(target: &mut Usage, source: &Usage) {
    if source.last_used_known {
        if !target.last_used_known || source.last_used > target.last_used {
            target.last_used = source.last_used;
            target.last_used_known = true;
            target.last_used_order = source.last_used_order;
        } else if source.last_used == target.last_used {
            target.last_used_order = target.last_used_order.max(source.last_used_order);
        }
    } else if !target.last_used_known {
        target.last_used = 0;
        target.last_used_order = target.last_used_order.max(source.last_used_order);
    }
}

fn usage_evidence(usage: &Usage) -> Evidence {
    Evidence {
        uses: usage.uses,
        last_used: usage.last_used,
        last_used_known: usage.last_used_known,
        last_used_order: usage.last_used_order,
        ..Evidence::default()
    }
}

fn choice_usage_evidence(prefix: &str, usage: &ChoiceUsage) -> Evidence {
    Evidence {
        accepts: usage.uses,
        last_accepted: usage.last_accepted,
        specificity: prefix
            .len()
            .min(MAX_CHOICE_PREFIX_BYTES)
            .min(u16::MAX as usize) as u16,
        ..Evidence::default()
    }
}

fn choice_is_failed(usage: &ChoiceUsage) -> bool {
    if usage.directory_acceptances.is_empty() {
        return usage.failed;
    }
    usage
        .directory_acceptances
        .values()
        .all(|acceptance| acceptance.failed)
}

fn prefix_evidence(prefix: &str, choices: &BTreeMap<String, ChoiceUsage>) -> Evidence {
    let mut evidence = Evidence {
        specificity: prefix
            .len()
            .min(MAX_CHOICE_PREFIX_BYTES)
            .min(u16::MAX as usize) as u16,
        ..Evidence::default()
    };
    for usage in choices.values().filter(|usage| !choice_is_failed(usage)) {
        evidence.accepts = evidence.accepts.saturating_add(usage.uses);
        evidence.last_accepted = evidence.last_accepted.max(usage.last_accepted);
    }
    evidence
}

fn add_choice_evidence(evidence: &mut Evidence, prefix: &str, usage: &ChoiceUsage) {
    evidence.accepts = evidence.accepts.saturating_add(usage.uses);
    evidence.last_accepted = evidence.last_accepted.max(usage.last_accepted);
    evidence.specificity = evidence.specificity.max(
        prefix
            .len()
            .min(MAX_CHOICE_PREFIX_BYTES)
            .min(u16::MAX as usize) as u16,
    );
}

fn add_directory_acceptance_evidence(
    evidence: &mut Evidence,
    prefix: &str,
    acceptance: &DirectoryAcceptance,
) {
    evidence.accepts = evidence.accepts.saturating_add(acceptance.uses);
    evidence.last_accepted = evidence.last_accepted.max(acceptance.last_accepted);
    evidence.specificity = evidence.specificity.max(
        prefix
            .len()
            .min(MAX_CHOICE_PREFIX_BYTES)
            .min(u16::MAX as usize) as u16,
    );
}

fn clean(command: &str, cfg: &ContextConfig) -> Option<String> {
    let command = command.trim();
    if command.is_empty()
        || command.contains('\u{fffd}')
        || command.len() > MAX_COMMAND_BYTES
        || command.chars().any(char::is_control)
        || cfg
            .ignore_prefixes
            .iter()
            .any(|prefix| !prefix.is_empty() && command.starts_with(prefix))
        || command.starts_with('#')
        || command.starts_with("sshpass ")
        || command.to_ascii_lowercase().contains("authorization:")
        || ["tp auth ", "termplete auth "]
            .iter()
            .any(|prefix| command.starts_with(prefix))
        || command.split_whitespace().any(|part| {
            ["--password", "--token", "--api-key", "--secret", "--key"]
                .iter()
                .any(|flag| {
                    part == *flag
                        || part
                            .strip_prefix(flag)
                            .is_some_and(|tail| tail.starts_with('='))
                })
        })
        || crate::redact::scrub(command) != command
    {
        return None;
    }
    Some(command.to_string())
}

pub fn shell_history_path(cfg: &ContextConfig) -> Option<PathBuf> {
    if let Some(path) = &cfg.shell_history_file {
        return Some(crate::config::expand_path(path));
    }
    for variable in ["TERMPLETE_HISTFILE", "HISTFILE"] {
        if let Some(path) = std::env::var_os(variable).filter(|path| !path.is_empty()) {
            return Some(path.into());
        }
    }
    let home = dirs::home_dir()?;
    [home.join(".zsh_history"), home.join(".bash_history")]
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_cwd(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let cwd = root.join("packages/app");
        std::fs::create_dir_all(&cwd).unwrap();
        cwd
    }

    #[test]
    fn folder_scope_prefers_local_executions_and_global_scope_remains_shared() {
        let cfg = ContextConfig::default();
        let workspace = tempfile::tempdir().unwrap();
        let project_a = project_cwd(workspace.path(), "project-a");
        let project_b = project_cwd(workspace.path(), "project-b");
        let local = "docker compose up api";
        let other = "docker compose logs web";
        let mut memory = CommandMemory::default();
        memory.record_with_cwd(local, Some(0), &cfg, Some(&project_a));
        for _ in 0..8 {
            memory.record_with_cwd(other, Some(0), &cfg, Some(&project_b));
        }

        assert_eq!(memory.prefix("docker", 2)[0].0, other);
        assert_eq!(
            memory.prefix_with_scope_filtered(
                "docker",
                2,
                Some(&project_a),
                SuggestionScope::Folder,
                |_| true
            )[0]
            .0,
            local
        );
        assert_eq!(
            memory.prefix_with_scope_filtered(
                "docker",
                2,
                Some(&project_b),
                SuggestionScope::Folder,
                |_| true
            )[0]
            .0,
            other
        );
        assert_eq!(
            memory.prefix_with_scope_filtered(
                "docker",
                2,
                Some(&project_a),
                SuggestionScope::Global,
                |_| true
            )[0]
            .0,
            other
        );

        memory.accept_with_cwd("docker ", local, &cfg, Some(&project_a));
        let path = workspace.path().join("commands.json");
        memory.save(&path).unwrap();
        let restored = CommandMemory::load(&path, &cfg).unwrap();
        assert_eq!(
            restored.prefix_with_scope_filtered(
                "docker",
                2,
                Some(&project_a),
                SuggestionScope::Folder,
                |_| true
            )[0]
            .0,
            local
        );
        assert_eq!(
            restored
                .candidate_evidence_with_scope(local, Some(&project_a), SuggestionScope::Folder)
                .project_accepts,
            1
        );
    }

    #[test]
    fn accepted_preferences_are_project_local_and_global_accepts_are_fallback_only_for_trust() {
        let cfg = ContextConfig::default();
        let workspace = tempfile::tempdir().unwrap();
        let project_a = project_cwd(workspace.path(), "project-a");
        let project_b = project_cwd(workspace.path(), "project-b");
        let accepted_a = "kubectl -n alpha get pods";
        let accepted_b = "kubectl -n beta get pods";
        let mut memory = CommandMemory::default();
        for _ in 0..2 {
            memory.accept_with_cwd("kub", accepted_a, &cfg, Some(&project_a));
        }

        let evidence_a = memory.candidate_evidence_with_scope(
            accepted_a,
            Some(&project_a),
            SuggestionScope::Folder,
        );
        let evidence_b = memory.candidate_evidence_with_scope(
            accepted_a,
            Some(&project_b),
            SuggestionScope::Folder,
        );
        assert_eq!(evidence_a.project_accepts, 2);
        assert_eq!(evidence_b.project_accepts, 0);
        assert!(memory.has_trusted_choice_with_scope(
            "kub",
            Some(&project_a),
            SuggestionScope::Folder,
        ));
        assert!(!memory.has_trusted_choice_with_scope(
            "kub",
            Some(&project_b),
            SuggestionScope::Folder,
        ));
        assert!(memory.has_trusted_choice_with_scope(
            "kub",
            Some(&project_b),
            SuggestionScope::Global
        ));

        for _ in 0..2 {
            memory.accept_with_cwd("kub", accepted_b, &cfg, Some(&project_b));
        }
        assert_eq!(
            memory.prefix_with_scope_filtered(
                "kub",
                2,
                Some(&project_b),
                SuggestionScope::Folder,
                |_| true
            )[0]
            .0,
            accepted_b
        );
    }

    #[test]
    fn failed_project_execution_suppresses_global_and_folder_preferences() {
        let cfg = ContextConfig::default();
        let workspace = tempfile::tempdir().unwrap();
        let cwd = project_cwd(workspace.path(), "project");
        let mut memory = CommandMemory::default();
        for _ in 0..2 {
            memory.accept_with_cwd("docker p", "docker ps", &cfg, Some(&cwd));
        }
        memory.record_with_cwd("docker ps", Some(1), &cfg, Some(&cwd));
        for scope in [SuggestionScope::Global, SuggestionScope::Folder] {
            assert!(memory
                .accepted_choices_with_scope("docker p", 4, Some(&cwd), scope)
                .is_empty());
            assert!(!memory.has_trusted_choice_with_scope("docker p", Some(&cwd), scope));
        }
        memory.record_with_cwd("docker ps", Some(0), &cfg, Some(&cwd));
        for scope in [SuggestionScope::Global, SuggestionScope::Folder] {
            assert!(memory.has_trusted_choice_with_scope("docker p", Some(&cwd), scope));
        }
    }

    #[test]
    fn project_signal_decays_and_has_a_fixed_score_ceiling() {
        let timestamp = now();
        let fresh = Evidence {
            project_scope: true,
            project_match: true,
            project_uses: u64::MAX,
            project_last_used: timestamp,
            project_accepts: u64::MAX,
            project_last_accepted: timestamp,
            ..Evidence::default()
        };
        let stale = Evidence {
            project_last_used: timestamp.saturating_sub(180 * 24 * 60 * 60 * 1000),
            project_last_accepted: timestamp.saturating_sub(180 * 24 * 60 * 60 * 1000),
            ..fresh
        };
        let fresh_rank = rank_evidence_at(&fresh, timestamp);
        let stale_rank = rank_evidence_at(&stale, timestamp);
        assert_eq!(fresh_rank.project_score, 2_800);
        assert!(stale_rank.project_score < 1);
    }

    #[test]
    fn usual_ssh_target_beats_recent_one_off_and_frequency_survives_restart() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for _ in 0..8 {
            memory.record("ssh root@203.0.113.7", Some(0), &cfg);
        }
        memory.record("ssh test@203.0.113.9", Some(0), &cfg);
        assert_eq!(memory.prefix("ssh ", 3)[0].0, "ssh root@203.0.113.7");
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("commands.json");
        memory.save(&path).unwrap();
        let restored = CommandMemory::load(&path, &cfg).unwrap();
        assert_eq!(restored.related("ssh -i", 2)[0].1.uses, 8);
        assert_eq!(restored.prefix("ssh ", 3), memory.prefix("ssh ", 3));
    }

    #[test]
    fn prefix_preserves_leading_whitespace_skips_exact_matches_and_never_rewards_length() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        memory.record("ssh root@203.0.113.7 -v -v -v", Some(0), &cfg);
        memory.record("ssh root@203.0.113.9", Some(0), &cfg);
        assert_eq!(memory.prefix("  ssh ", 3)[0].0, "  ssh root@203.0.113.9");
        assert!(memory.prefix("ssh root@203.0.113.9", 3).is_empty());
        assert!(memory.prefix("", 3).is_empty());
        assert!(memory.prefix("ssh ", 0).is_empty());
        assert!(memory.related("git", 3).is_empty());
    }

    #[test]
    fn filtered_prefix_fills_limit_after_excluding_the_top_candidate() {
        const STALE: &str = "omp --resume 01a11392-12ae-73bd-9b8f-4d3cf02df1b7";
        const CURRENT: &str = "omp --resume 01a113d4-0766-72cf-b033-71928b1dd054";
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for _ in 0..8 {
            assert!(memory.record(STALE, Some(0), &cfg));
        }
        for _ in 0..3 {
            assert!(memory.accept("omp", STALE, &cfg));
        }
        assert!(memory.record(CURRENT, Some(0), &cfg));

        let unfiltered = memory.prefix_with_cwd("omp", 20, None);
        assert_eq!(unfiltered[0].0, STALE);
        let expected = unfiltered
            .into_iter()
            .filter(|(command, _)| command != STALE)
            .take(1)
            .collect::<Vec<_>>();
        let filtered = memory.prefix_with_cwd_filtered("omp", 1, None, |command| command != STALE);

        assert_eq!(filtered, expected);
        assert_eq!(filtered[0].0, CURRENT);
    }

    #[test]
    fn secrets_ignored_commands_and_failed_typos_never_become_candidates() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for command in [
            "sudo ssh secret",
            "sshpass -p private ssh host",
            "OPENAI_API_KEY=private tool",
            "tp auth login --key private",
            "tool --token=private",
            "tool --password private",
            "tool https://host/?token=private",
        ] {
            assert!(!memory.record(command, Some(0), &cfg), "{command}");
        }
        assert!(!memory.record("ssh typooo@invalid", Some(255), &cfg));
        assert!(memory.is_empty());
        memory.record("ssh good@203.0.113.7", Some(0), &cfg);
        memory.record("ssh good@203.0.113.7", Some(255), &cfg);
        assert_eq!(memory.related("ssh", 3)[0].1.uses, 1);
    }

    #[test]
    fn accepted_choices_outweigh_execution_frequency_and_are_prefix_specific() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for _ in 0..30 {
            memory.record("git checkout main", Some(0), &cfg);
        }
        memory.accept("git ch", "git cherry-pick abc", &cfg);
        memory.accept("git ch", "git cherry-pick abc", &cfg);
        memory.accept("git st", "git status --short", &cfg);

        assert_eq!(memory.prefix("git ch", 3)[0].0, "git cherry-pick abc");
        assert_eq!(memory.prefix("git chec", 3)[0].0, "git checkout main");
        assert_eq!(memory.prefix("git st", 3)[0].0, "git status --short");
        assert_eq!(
            memory.accepted_choices("git c", 3),
            vec![("git cherry-pick abc".to_string(), 2)]
        );
        assert_eq!(
            memory.accepted_choices("git s", 3),
            vec![("git status --short".to_string(), 1)]
        );
        assert!(memory.has_trusted_choice("git ch"));
        assert!(!memory.has_trusted_choice("git st"));
    }

    #[test]
    fn equally_supported_choices_are_not_trusted() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for command in ["git cherry-pick abc", "git checkout main"] {
            memory.accept("git ch", command, &cfg);
            memory.accept("git ch", command, &cfg);
        }
        assert!(!memory.has_trusted_choice("git ch"));
    }

    #[test]
    fn ranking_uses_smooth_freshness_and_bounded_lifetime_frequency() {
        let day = 24 * 60 * 60 * 1000;
        let timestamp = 100 * day;
        let recent = Evidence {
            uses: 1,
            last_used: timestamp,
            ..Evidence::default()
        };
        let one_day_old = Evidence {
            last_used: timestamp - day,
            ..recent
        };
        let three_days_old = Evidence {
            last_used: timestamp - 3 * day,
            ..recent
        };
        let recent = Evidence {
            last_used_known: true,
            ..recent
        };
        let one_day_old = Evidence {
            last_used_known: true,
            ..one_day_old
        };
        let three_days_old = Evidence {
            last_used_known: true,
            ..three_days_old
        };
        assert!(
            rank_evidence_at(&recent, timestamp).score
                > rank_evidence_at(&one_day_old, timestamp).score
        );
        assert!(
            rank_evidence_at(&one_day_old, timestamp).score
                > rank_evidence_at(&three_days_old, timestamp).score
        );

        let ordinary_frequency = Evidence {
            uses: 1_000_000,
            last_used: timestamp,
            ..Evidence::default()
        };
        let enormous_frequency = Evidence {
            uses: u64::MAX,
            ..ordinary_frequency
        };
        let difference = rank_evidence_at(&enormous_frequency, timestamp).score
            - rank_evidence_at(&ordinary_frequency, timestamp).score;
        assert!(
            difference < 1,
            "lifetime frequency must saturate: {difference}"
        );

        let old_acceptance = Evidence {
            accepts: 2,
            last_accepted: timestamp - 30 * day,
            specificity: 6,
            ..Evidence::default()
        };
        let fresh_acceptance = Evidence {
            last_accepted: timestamp,
            ..old_acceptance
        };
        assert!(
            rank_evidence_at(&fresh_acceptance, timestamp).score
                > rank_evidence_at(&old_acceptance, timestamp).score
        );
    }

    #[test]
    fn explicit_recent_context_beats_maximal_fresh_history_and_acceptance() {
        let timestamp = 100 * 24 * 60 * 60 * 1000;
        let habit = Evidence {
            uses: u64::MAX,
            last_used: timestamp,
            last_used_known: true,
            accepts: u64::MAX,
            last_accepted: timestamp,
            specificity: MAX_CHOICE_PREFIX_BYTES as u16,
            ..Evidence::default()
        };
        let context = Evidence {
            context_match: true,
            context_last_seen: timestamp,
            ..Evidence::default()
        };

        let habit_rank = rank_evidence_at(&habit, timestamp);
        assert_eq!(habit_rank.score, 5_976);

        let ages = [
            0,
            12 * 60 * 1000 + 30 * 1000,
            CONTEXT_HALF_LIFE_MS,
            24 * 60 * 60 * 1000,
            7 * 24 * 60 * 60 * 1000,
        ];
        for age in ages {
            let context = Evidence {
                context_last_seen: timestamp - age,
                ..context
            };
            let context_rank = rank_evidence_at(&context, timestamp);
            assert!(
                context_rank.score > habit_rank.score,
                "explicit context must outrank maximal history at age {age}: context={}, history={}",
                context_rank.score,
                habit_rank.score,
            );
            assert_eq!(context.context_last_seen, timestamp - age);
        }
    }

    #[test]
    fn context_score_decays_and_unknown_time_stays_unknown() {
        let timestamp = 10_000_000;
        let half_life = CONTEXT_HALF_LIFE_MS;
        let fresh = Evidence {
            context_match: true,
            context_last_seen: timestamp,
            ..Evidence::default()
        };
        let one_half_life = Evidence {
            context_last_seen: timestamp - half_life,
            ..fresh
        };
        let two_half_lives = Evidence {
            context_last_seen: timestamp - 2 * half_life,
            ..fresh
        };
        assert_eq!(rank_evidence_at(&fresh, timestamp).score, 10_000);
        assert_eq!(rank_evidence_at(&one_half_life, timestamp).score, 9_500);
        assert_eq!(rank_evidence_at(&two_half_lives, timestamp).score, 9_250);
        assert!(
            rank_evidence_at(&fresh, timestamp).confidence
                > rank_evidence_at(&one_half_life, timestamp).confidence
        );
        assert!((rank_evidence_at(&fresh, timestamp).confidence - 0.95).abs() < f64::EPSILON);
        assert!(
            (rank_evidence_at(&one_half_life, timestamp).confidence - 0.85).abs() < f64::EPSILON
        );

        let untimed = Evidence {
            context_match: true,
            context_last_seen: 0,
            ..Evidence::default()
        };
        assert_eq!(rank_evidence_at(&untimed, timestamp).score, 9_000);
        assert_eq!(rank_evidence_at(&untimed, timestamp).confidence, 0.75);
        assert_eq!(untimed.context_last_seen, 0);
        assert_eq!(
            rank_evidence_at(&Evidence::default(), timestamp).score,
            0,
            "context ranking must not affect candidates without a context match"
        );
    }

    #[test]
    fn explicit_context_outranks_maximum_project_history() {
        let timestamp = 10_000_000;
        let maximum_history = Evidence {
            uses: u64::MAX,
            last_used: timestamp,
            last_used_known: true,
            accepts: u64::MAX,
            last_accepted: timestamp,
            specificity: u16::MAX,
            project_match: true,
            project_uses: u64::MAX,
            project_last_used: timestamp,
            project_accepts: u64::MAX,
            project_last_accepted: timestamp,
            ..Evidence::default()
        };
        let unknown_context = Evidence {
            context_match: true,
            context_last_seen: 0,
            ..Evidence::default()
        };

        assert!(
            rank_evidence_at(&unknown_context, timestamp).score
                > rank_evidence_at(&maximum_history, timestamp).score
        );
    }

    #[test]
    fn recent_execution_beats_a_stale_accepted_choice_and_cannot_be_trusted() {
        let timestamp = now();
        let mut memory = CommandMemory::default();
        memory.commands.insert(
            "git checkout main".to_string(),
            Usage {
                uses: 30,
                last_used: timestamp,
                last_used_known: true,
                last_used_order: timestamp,
            },
        );
        memory.choices.insert(
            "git ch".to_string(),
            BTreeMap::from([(
                "git cherry-pick abc".to_string(),
                ChoiceUsage {
                    uses: 2,
                    last_used: timestamp - 30 * 24 * 60 * 60 * 1000,
                    last_accepted: timestamp - 30 * 24 * 60 * 60 * 1000,
                    ..ChoiceUsage::default()
                },
            )]),
        );

        let cfg = ContextConfig::default();
        assert!(memory.record("git cherry-pick abc", Some(0), &cfg));
        let evidence = memory.candidate_evidence("git cherry-pick abc", None);
        assert_eq!(
            evidence.last_accepted,
            timestamp - 30 * 24 * 60 * 60 * 1000,
            "manual execution must not refresh acceptance age"
        );

        let candidates = memory.prefix_with_cwd("git ch", 5, None);
        assert_eq!(candidates[0].0, "git checkout main");
        assert!(!memory.has_trusted_choice_with_scope("git ch", None, SuggestionScope::Global));
    }

    #[test]
    fn directory_spellings_share_usage_and_accepted_identity_survives_restart() {
        let cfg = ContextConfig::default();
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let project = cwd.join("project");
        std::fs::create_dir(&project).unwrap();
        let identity = crate::fs_context::directory_identity("cd project", Some(cwd)).unwrap();
        let mut memory = CommandMemory::default();
        assert!(memory.record_with_cwd("cd project", Some(0), &cfg, Some(cwd)));
        assert!(memory.record_with_cwd("cd ./project/", Some(0), &cfg, Some(cwd)));
        assert!(memory.record_with_cwd(
            &format!("cd {}", project.display()),
            Some(0),
            &cfg,
            Some(cwd)
        ));
        assert!(memory.record_with_cwd(
            &format!("cd {}/", project.display()),
            Some(0),
            &cfg,
            Some(cwd)
        ));
        assert_eq!(memory.directory_evidence(&identity).uses, 4);
        assert_eq!(memory.directory_candidates().len(), 1);

        assert!(memory.accept_with_cwd("cd ", "cd project", &cfg, Some(cwd)));
        let path = temp.path().join("memory.json");
        memory.save(&path).unwrap();
        let restored = CommandMemory::load(&path, &cfg).unwrap();
        let evidence = restored.candidate_evidence("cd ./project/", Some(cwd));
        assert_eq!(evidence.accepts, 1);
        assert_eq!(evidence.uses, 4);
        assert_eq!(restored.directory_candidates()[0].0, identity);
    }

    #[test]
    fn relative_directory_choices_keep_evidence_per_working_directory() {
        let cfg = ContextConfig::default();
        let temp = tempfile::tempdir().unwrap();
        let cwd_a = temp.path().join("a");
        let cwd_b = temp.path().join("b");
        std::fs::create_dir(&cwd_a).unwrap();
        std::fs::create_dir(&cwd_b).unwrap();
        std::fs::create_dir(cwd_a.join("project")).unwrap();
        std::fs::create_dir(cwd_b.join("project")).unwrap();
        let identity_a = crate::fs_context::directory_identity("cd project", Some(&cwd_a)).unwrap();
        let identity_b = crate::fs_context::directory_identity("cd project", Some(&cwd_b)).unwrap();
        assert_ne!(identity_a, identity_b);

        let mut memory = CommandMemory::default();
        memory.record_with_cwd("cd project", Some(0), &cfg, Some(&cwd_a));
        memory.record_with_cwd("cd project", Some(0), &cfg, Some(&cwd_b));
        memory.accept_with_cwd("cd ", "cd project", &cfg, Some(&cwd_a));
        memory.accept_with_cwd("cd ", "cd project", &cfg, Some(&cwd_b));
        let a = memory.candidate_evidence("cd project", Some(&cwd_a));
        let b = memory.candidate_evidence("cd project", Some(&cwd_b));
        assert_eq!((a.uses, a.accepts), (1, 1));
        assert_eq!((b.uses, b.accepts), (1, 1));

        let path = temp.path().join("memory.json");
        memory.save(&path).unwrap();
        let restored = CommandMemory::load(&path, &cfg).unwrap();
        let a = restored.candidate_evidence("cd project", Some(&cwd_a));
        let b = restored.candidate_evidence("cd project", Some(&cwd_b));
        assert_eq!((a.uses, a.accepts), (1, 1));
        assert_eq!((b.uses, b.accepts), (1, 1));
    }

    #[test]
    fn legacy_snapshots_infer_absolute_directory_usage() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let command = format!("cd {}/", project.display());
        let identity = crate::fs_context::directory_identity(&command, None).unwrap();
        let path = temp.path().join("legacy.json");
        let mut commands = serde_json::Map::new();
        commands.insert(command, serde_json::json!({"uses": 4, "last_used": 200}));
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "commands": commands,
                "choices": {},
                "imports": {},
                "clock": 200,
            }))
            .unwrap(),
        )
        .unwrap();

        let memory = CommandMemory::load(&path, &ContextConfig::default()).unwrap();
        let evidence = memory.directory_evidence(&identity);
        assert_eq!(evidence.uses, 4);
        assert_eq!(evidence.last_used, 0);
        assert!(!evidence.last_used_known);
        assert_eq!(evidence.last_used_order, 200);
    }

    #[test]
    fn accepted_choices_reject_secrets_malformed_prefixes_and_non_extensions() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for (prefix, command) in [
            ("", "ls -la"),
            ("git checkout main", "git checkout main"),
            ("git ch", "docker ps"),
            ("git --token=private ", "git --token=private status"),
            ("git ch", "git checkout --token=private"),
            ("git\nch", "git\ncheckout main"),
            (&"x".repeat(MAX_CHOICE_PREFIX_BYTES + 1), "x command"),
        ] {
            assert!(
                !memory.accept(prefix, command, &cfg),
                "{prefix:?} => {command:?}"
            );
        }
        assert!(memory.is_empty());
        assert!(memory.choices.is_empty());
    }

    #[test]
    fn accepted_choices_are_bounded_per_prefix_and_globally() {
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        for index in 0..MAX_CHOICES_PER_PREFIX + 5 {
            let command = format!("tool choice-{index}");
            assert!(memory.accept("tool ", &command, &cfg));
        }
        assert_eq!(memory.choices["tool "].len(), MAX_CHOICES_PER_PREFIX);
        for index in 0..MAX_CHOICE_PREFIXES + 5 {
            let prefix = format!("cmd{index:03} ");
            let command = format!("cmd{index:03} option");
            memory.accept(&prefix, &command, &cfg);
        }
        assert!(memory.choices.len() <= MAX_CHOICE_PREFIXES);
        assert!(memory.choice_bytes() <= MAX_CHOICE_BYTES);
    }

    #[test]
    fn legacy_snapshot_without_choices_loads_and_malformed_choice_data_is_filtered() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("commands.json");
        std::fs::write(
            &path,
            r#"{"commands":{"git status":{"uses":3,"last_used":10}},"imports":{},"clock":10}"#,
        )
        .unwrap();
        let old = CommandMemory::load(&path, &ContextConfig::default()).unwrap();
        assert_eq!(old.prefix("git st", 1)[0].0, "git status");
        assert!(old.accepted_choices("git st", 2).is_empty());

        std::fs::write(
            &path,
            r#"{"commands":{},"choices":{"tool --token=bad ":{"tool --token=bad status":{"uses":2,"last_used":20}},"git st":{"git status":{"uses":2,"last_used":21},"other status":{"uses":3,"last_used":22}}},"imports":{},"clock":22}"#,
        )
        .unwrap();
        let loaded = CommandMemory::load(&path, &ContextConfig::default()).unwrap();
        assert_eq!(loaded.prefix("git st", 2)[0].0, "git status");
        assert_eq!(loaded.choices.len(), 1);
    }

    #[test]
    fn zsh_import_is_idempotent_and_keeps_native_recency_and_live_counts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".zsh_history");
        let cfg = ContextConfig::default();
        std::fs::write(
            &path,
            ": 10:0;ssh old@203.0.113.7\n: 11:1;ssh old@203.0.113.7\n: 12:0;git status\n",
        )
        .unwrap();
        let mut memory = CommandMemory::default();
        assert_eq!(memory.import_file(&path, &cfg).unwrap(), 2);
        assert_eq!(memory.import_file(&path, &cfg).unwrap(), 0);
        assert_eq!(memory.related("ssh", 3)[0].1.uses, 2);
        memory.record("ssh old@203.0.113.7", Some(0), &cfg);
        let timestamp = memory.related("ssh", 3)[0].1.last_used;
        std::fs::write(&path, ": 10:0;ssh old@203.0.113.7\n: 11:1;ssh old@203.0.113.7\n: 12:0;ssh old@203.0.113.7\n: 13:0;git status\n").unwrap();
        memory.import_file(&path, &cfg).unwrap();
        assert_eq!(memory.related("ssh", 3)[0].1.uses, 3);
        assert_eq!(memory.related("ssh", 3)[0].1.last_used, timestamp);
        std::fs::write(&path, ": 12:0;ssh old@203.0.113.7\n").unwrap();
        memory.import_file(&path, &cfg).unwrap();
        assert_eq!(memory.related("ssh", 3)[0].1.uses, 3);
    }

    #[test]
    fn bash_import_ignores_timestamps_and_multiline_fragments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".bash_history");
        std::fs::write(
            &path,
            "#10\r\nssh root@203.0.113.7\r\n#11\r\necho 'a\r\nb'\r\n#12\r\ngit status\r\n",
        )
        .unwrap();
        let mut memory = CommandMemory::default();
        assert_eq!(
            memory
                .import_file(&path, &ContextConfig::default())
                .unwrap(),
            2
        );
        assert!(memory.prefix("echo", 3).is_empty());
        assert!(memory.prefix("b", 3).is_empty());
        assert_eq!(memory.prefix("ssh ", 3)[0].0, "ssh root@203.0.113.7");
    }

    #[test]
    fn undated_history_gets_weak_recency_and_recent_live_usage_wins() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("history");
        std::fs::write(&history, "git checkout main\n".repeat(100)).unwrap();
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        memory.import_file(&history, &cfg).unwrap();
        let imported = memory.candidate_evidence("git checkout main", None);
        assert!(!imported.last_used_known);
        assert_eq!(imported.last_used, 0);
        assert!(imported.last_used_order > 0);

        memory.record("git cherry-pick abc", Some(0), &cfg);
        memory.record("git cherry-pick abc", Some(0), &cfg);
        assert_eq!(memory.prefix("git ch", 2)[0].0, "git cherry-pick abc");
    }

    #[test]
    fn undated_history_uses_file_order_to_break_equal_frequency_ties() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("history");
        std::fs::write(&history, "git checkout main\ngit cherry-pick abc\n").unwrap();
        let mut memory = CommandMemory::default();
        memory
            .import_file(&history, &ContextConfig::default())
            .unwrap();

        assert_eq!(memory.prefix("git ch", 2)[0].0, "git cherry-pick abc");
    }

    #[test]
    fn timestamped_history_keeps_native_recency() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join(".zsh_history");
        let now_seconds = now() / 1000;
        let old_seconds = now_seconds.saturating_sub(30 * 24 * 60 * 60);
        let mut contents = String::new();
        for _ in 0..100 {
            contents.push_str(&format!(": {old_seconds}:0;git checkout main\n"));
        }
        contents.push_str(&format!(": {now_seconds}:0;git cherry-pick abc\n"));
        contents.push_str(&format!(": {now_seconds}:0;git cherry-pick abc\n"));
        std::fs::write(&history, contents).unwrap();

        let mut memory = CommandMemory::default();
        memory
            .import_file(&history, &ContextConfig::default())
            .unwrap();
        let old = memory.candidate_evidence("git checkout main", None);
        let recent = memory.candidate_evidence("git cherry-pick abc", None);
        assert!(old.last_used_known);
        assert!(recent.last_used_known);
        assert_eq!(old.last_used, old_seconds * 1000);
        assert_eq!(recent.last_used, now_seconds * 1000);
        assert_eq!(memory.prefix("git ch", 2)[0].0, "git cherry-pick abc");
    }

    #[test]
    fn rewritten_undated_history_does_not_refresh_a_live_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let history = temp.path().join("history");
        std::fs::write(&history, "git status\n").unwrap();
        let cfg = ContextConfig::default();
        let mut memory = CommandMemory::default();
        memory.import_file(&history, &cfg).unwrap();
        memory.record("git status", Some(0), &cfg);
        let live_time = memory.commands["git status"].last_used;

        std::fs::write(&history, "git status\n".repeat(100)).unwrap();
        memory.import_file(&history, &cfg).unwrap();
        let usage = &memory.commands["git status"];
        assert_eq!(usage.uses, 100);
        assert!(usage.last_used_known);
        assert_eq!(usage.last_used, live_time);
    }

    #[test]
    fn shared_ranking_preserves_fresh_low_count_entries_when_bounded() {
        let cfg = ContextConfig {
            command_memory_entries: 1,
            ..ContextConfig::default()
        };
        let timestamp = now();
        let mut memory = CommandMemory::default();
        memory.commands.insert(
            "old high count".to_string(),
            Usage {
                uses: u64::MAX,
                last_used: 0,
                last_used_order: 1,
                ..Usage::default()
            },
        );
        memory.commands.insert(
            "fresh low count".to_string(),
            Usage {
                uses: 2,
                last_used: timestamp,
                last_used_known: true,
                last_used_order: timestamp,
            },
        );
        memory.bytes = memory.commands.keys().map(String::len).sum();
        for index in 0..MAX_DIRECTORY_ENTRIES {
            memory.directories.insert(
                PathBuf::from(format!("/history/{index:04}")),
                Usage {
                    uses: u64::MAX,
                    last_used_order: index as u64,
                    ..Usage::default()
                },
            );
        }
        let fresh_directory = PathBuf::from("/recent/live");
        memory.directories.insert(
            fresh_directory.clone(),
            Usage {
                uses: 2,
                last_used: timestamp,
                last_used_known: true,
                last_used_order: timestamp,
            },
        );

        memory.enforce_bounds(&cfg);
        assert!(memory.commands.contains_key("fresh low count"));
        assert!(!memory.commands.contains_key("old high count"));
        assert!(memory.directories.contains_key(&fresh_directory));
    }

    #[test]
    fn memory_cap_preserves_habits_and_rejects_oversized_commands() {
        let cfg = ContextConfig {
            command_memory_entries: 3,
            ..ContextConfig::default()
        };
        let mut memory = CommandMemory::default();
        for _ in 0..10 {
            memory.record("ssh usual@203.0.113.7", Some(0), &cfg);
        }
        for index in 0..20 {
            memory.record(&format!("echo {index}"), Some(0), &cfg);
        }
        assert_eq!(memory.len(), 3);
        assert_eq!(memory.prefix("ssh", 3)[0].0, "ssh usual@203.0.113.7");
        assert!(!memory.record(&"x".repeat(MAX_COMMAND_BYTES + 1), None, &cfg));
    }

    #[test]
    fn bounded_import_discards_partial_tail_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history");
        std::fs::write(
            &path,
            format!("{}\nssh root@203.0.113.7\n", "x".repeat(MAX_BYTES + 100)),
        )
        .unwrap();
        let mut memory = CommandMemory::default();
        assert_eq!(
            memory
                .import_file(&path, &ContextConfig::default())
                .unwrap(),
            1
        );
        assert_eq!(memory.len(), 1);
    }
}
