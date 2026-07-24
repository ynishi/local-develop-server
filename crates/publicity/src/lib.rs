#![warn(missing_docs)]

//! Publicity classifier — per-platform PUBLIC/PRIVATE/INTERNAL/LOCAL/NOT_GIT/FORKED/AMBIGUOUS/UNKNOWN.
//!
//! Turns a repository root into one or more [`PublicityResult`]s, one per
//! platform (github, crates, …). Each result carries a canonical
//! [`Publicity`] value plus a `detail` JSON blob with the raw signals the
//! classifier used, so downstream gates can render the reasoning without
//! re-querying the underlying tools.
//!
//! # Design
//!
//! The classifier is deliberately structural (probe → JSON-parse → enum) so
//! that AI-side callers get deterministic mapping without heuristics. Unknown
//! or blocked signals collapse to [`Publicity::Unknown`] (environmental gap:
//! `gh` unauthenticated, `Cargo.toml` unparseable) or [`Publicity::Ambiguous`]
//! (data present but not a single canonical answer: remote host is not
//! recognised, or `gh repo view` returned an error).
//!
//! # Phase 1 platforms
//!
//! - [`Platform::Github`] — `git remote get-url origin` + `gh repo view`.
//! - [`Platform::Crates`] — `Cargo.toml` `[workspace.package].publish` /
//!   `[package].publish` inspection.
//!
//! Additional platforms (npm, pypi, docker registries) are future work.

use std::path::Path;
use std::sync::Arc;

use lds_core::Session;
use lds_gh::GhModule;
use lds_git::GitModule;
use serde::{Deserialize, Serialize};

/// Publisher / registry a repository can be surfaced on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// GitHub / GHE.
    Github,
    /// crates.io (Cargo publish).
    Crates,
}

impl Platform {
    /// Canonical short name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Github => "github",
            Platform::Crates => "crates",
        }
    }

    /// Parse a user-supplied platform label; accepts common aliases.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "github" | "gh" => Some(Platform::Github),
            "crates" | "crates.io" | "cratesio" | "cargo" => Some(Platform::Crates),
            _ => None,
        }
    }
}

/// Canonical publicity value. All platforms return one of these eight states.
///
/// - `Public` — visible to anyone (github public, crates.io publish=true).
/// - `Private` — visibility restricted to explicit collaborators
///   (github private, Cargo `publish = false`).
/// - `Internal` — org-scope visibility (GHE `internal`, Cargo custom
///   registry list).
/// - `Local` — git repository present but no remotes configured on this
///   platform.
/// - `NotGit` — the session root is not a git repository at all
///   (no `.git`, or the repository cannot be opened). Distinct from
///   `Local`, which implies `git init` has run.
/// - `Forked` — github fork (regardless of upstream visibility).
/// - `Ambiguous` — data present but not deterministically classifiable
///   (non-github host, `gh repo view` error, malformed Cargo.toml).
/// - `Unknown` — the tool needed to classify is unavailable (gh not
///   authenticated, Cargo.toml unreadable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Publicity {
    /// Visible to anyone (github public, crates.io `publish = true`).
    Public,
    /// Visibility restricted to explicit collaborators (github private,
    /// Cargo `publish = false`).
    Private,
    /// Org-scope visibility (GHE `internal`, Cargo custom-registry list).
    Internal,
    /// Git repository present but no remotes configured on this platform.
    Local,
    /// The session root is not a git repository at all (no `.git`, or
    /// the repository cannot be opened). Distinct from [`Publicity::Local`].
    NotGit,
    /// Github fork (regardless of upstream visibility). See
    /// [`PublicityResult::underlying_visibility`] for the raw visibility.
    Forked,
    /// Data present but not deterministically classifiable (non-github
    /// host, `gh repo view` error, malformed `Cargo.toml`).
    Ambiguous,
    /// The tool needed to classify is unavailable (gh not authenticated,
    /// `Cargo.toml` unreadable).
    Unknown,
}

/// One platform's publicity classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicityResult {
    /// Platform short name (`"github"`, `"crates"`, …).
    pub platform: String,
    /// Canonical publicity value.
    pub publicity: Publicity,
    /// One-sentence justification citing the observed signal.
    pub reason: String,
    /// When `publicity == Forked`, the underlying repository's raw visibility
    /// (`"PUBLIC"` / `"PRIVATE"` / `"INTERNAL"`). Otherwise `None`.
    ///
    /// Surfaces the "is this a public fork or a private fork" answer at the
    /// top level so callers do not have to inspect `detail.visibility`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlying_visibility: Option<String>,
    /// Raw signals the classifier read (`origin_url`, `is_fork`, `publish`, …).
    pub detail: serde_json::Value,
}

impl PublicityResult {
    fn new(
        platform: Platform,
        publicity: Publicity,
        reason: impl Into<String>,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            platform: platform.as_str().to_string(),
            publicity,
            reason: reason.into(),
            underlying_visibility: None,
            detail,
        }
    }

    fn with_underlying_visibility(mut self, visibility: impl Into<String>) -> Self {
        self.underlying_visibility = Some(visibility.into());
        self
    }
}

/// Session-scoped publicity classifier.
#[derive(Debug)]
pub struct PublicityModule {
    session: Arc<Session>,
}

impl PublicityModule {
    /// Construct a new classifier bound to `session`. All probes issued by
    /// this instance run against `session.root()`.
    pub fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    /// Classify every platform that has any signal in the current session root.
    ///
    /// The github probe always runs — a non-git root returns `NOT_GIT`,
    /// a git root with no remotes returns `LOCAL`, and either is a
    /// meaningful signal to callers. The crates probe runs only when a
    /// `Cargo.toml` is present.
    pub async fn detect_all(&self) -> Vec<PublicityResult> {
        let mut out = Vec::new();
        out.push(self.detect_github().await);
        if self.session.root().join("Cargo.toml").is_file() {
            out.push(self.detect_crates());
        }
        out
    }

    /// Classify exactly one platform.
    pub async fn detect(&self, platform: Platform) -> PublicityResult {
        match platform {
            Platform::Github => self.detect_github().await,
            Platform::Crates => self.detect_crates(),
        }
    }

    /// Classify the current session root's github publicity.
    pub async fn detect_github(&self) -> PublicityResult {
        let git = GitModule::new(self.session.clone());
        let gh = GhModule::new(self.session.clone());
        detect_github_impl(&git, &gh).await
    }

    /// Classify the current session root's crates.io publicity.
    ///
    /// Runs a declared-side classification via [`classify_crates_toml`], then
    /// — when the declared value is [`Publicity::Public`] and a crate name is
    /// present — probes crates.io for actual registration via
    /// [`check_crates_io_registered`]. A declared-PUBLIC crate that is not
    /// registered on crates.io downgrades to [`Publicity::Ambiguous`]
    /// (unpublished-yet or yanked). Live-check failure (offline, curl
    /// missing) is non-fatal and appends a note to `reason`.
    pub fn detect_crates(&self) -> PublicityResult {
        let mut result = detect_crates_impl(self.session.root());
        if result.publicity == Publicity::Public {
            let name = result
                .detail
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from);
            if let Some(ref n) = name {
                match check_crates_io_registered(n) {
                    Some(true) => {
                        result.detail["crates_io_registered"] = serde_json::json!(true);
                        result.reason = format!("{} + registered on crates.io", result.reason);
                    }
                    Some(false) => {
                        result.detail["crates_io_registered"] = serde_json::json!(false);
                        result.publicity = Publicity::Ambiguous;
                        result.reason = format!(
                            "{} but not registered on crates.io (unpublished or yanked)",
                            result.reason
                        );
                    }
                    None => {
                        result.detail["crates_io_registered"] = serde_json::Value::Null;
                        result.reason =
                            format!("{} (crates.io live check unavailable)", result.reason);
                    }
                }
            }
        }
        result
    }
}

// ---------------------------------------------------------------------------
// GitHub classifier
// ---------------------------------------------------------------------------

async fn detect_github_impl(git: &GitModule, gh: &GhModule) -> PublicityResult {
    // 1. Enumerate remotes; failure to open the repo → NOT_GIT.
    //    `remote_list` shells out through `git`, so the error surfaces
    //    "not a git repository" / "repository at <root> not found" the
    //    same way `git remote -v` does. Distinct from LOCAL (git repo
    //    present but no remotes) so callers can tell "path has no .git"
    //    from "git init done, no push target".
    let remotes = match git.remote_list().await {
        Ok(r) => r.remotes,
        Err(e) => {
            return PublicityResult::new(
                Platform::Github,
                Publicity::NotGit,
                format!("git remote_list failed (not a git repository?): {e}"),
                serde_json::json!({}),
            );
        }
    };

    // 2. No remotes at all → LOCAL.
    if remotes.is_empty() {
        return PublicityResult::new(
            Platform::Github,
            Publicity::Local,
            "no git remotes configured",
            serde_json::json!({ "remotes": [] }),
        );
    }

    // 3. Prefer origin. If absent, remotes exist but no canonical entry → AMBIGUOUS.
    let origin = match remotes.iter().find(|r| r.name == "origin") {
        Some(o) => o,
        None => {
            return PublicityResult::new(
                Platform::Github,
                Publicity::Ambiguous,
                "remotes exist but no 'origin' entry",
                serde_json::json!({
                    "remote_names": remotes.iter().map(|r| &r.name).collect::<Vec<_>>()
                }),
            );
        }
    };

    let origin_url = origin
        .fetch_url
        .clone()
        .or_else(|| origin.push_url.clone())
        .unwrap_or_default();

    // 4. Host must be github.com or a GHE-style host. Non-github → AMBIGUOUS.
    if !is_github_host(&origin_url) {
        return PublicityResult::new(
            Platform::Github,
            Publicity::Ambiguous,
            format!("origin host is not a recognised github endpoint: {origin_url}"),
            serde_json::json!({ "origin_url": origin_url }),
        );
    }

    // 5. gh must be authenticated; otherwise UNKNOWN.
    if !gh.is_authenticated() {
        return PublicityResult::new(
            Platform::Github,
            Publicity::Unknown,
            "gh CLI not authenticated (run `gh auth login`)",
            serde_json::json!({ "origin_url": origin_url }),
        );
    }

    // 6. Ask gh for visibility / isFork / parent.
    let json = match gh.repo_visibility() {
        Ok(s) => s,
        Err(e) => {
            return PublicityResult::new(
                Platform::Github,
                Publicity::Ambiguous,
                format!("gh repo view failed: {e}"),
                serde_json::json!({ "origin_url": origin_url }),
            );
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            return PublicityResult::new(
                Platform::Github,
                Publicity::Ambiguous,
                format!("gh repo view returned unparseable JSON: {e}"),
                serde_json::json!({ "origin_url": origin_url, "raw": json }),
            );
        }
    };

    classify_github_json(&parsed, &origin_url)
}

/// Map a `gh repo view --json visibility,isFork,parent,url,nameWithOwner`
/// payload to a [`PublicityResult`]. Extracted for direct unit testing.
pub fn classify_github_json(parsed: &serde_json::Value, origin_url: &str) -> PublicityResult {
    let visibility_raw = parsed
        .get("visibility")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_fork = parsed
        .get("isFork")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let parent = parsed
        .get("parent")
        .and_then(|p| p.get("nameWithOwner"))
        .and_then(|s| s.as_str())
        .map(String::from);
    let name_with_owner = parsed
        .get("nameWithOwner")
        .and_then(|s| s.as_str())
        .map(String::from);

    let visibility_upper = visibility_raw.to_ascii_uppercase();

    let detail = serde_json::json!({
        "origin_url": origin_url,
        "visibility": visibility_upper,
        "is_fork": is_fork,
        "parent": parent,
        "name_with_owner": name_with_owner,
    });

    // Fork status wins over raw visibility: FORKED conveys the primary signal
    // (this is not our upstream). Visibility is surfaced on the top-level
    // `underlying_visibility` field so callers can distinguish a public fork
    // from a private one without inspecting `detail`.
    if is_fork {
        return PublicityResult::new(
            Platform::Github,
            Publicity::Forked,
            format!(
                "gh reports isFork=true (parent={}, underlying visibility={})",
                parent.as_deref().unwrap_or("unknown"),
                if visibility_upper.is_empty() {
                    "unknown"
                } else {
                    visibility_upper.as_str()
                }
            ),
            detail,
        )
        .with_underlying_visibility(if visibility_upper.is_empty() {
            "UNKNOWN".to_string()
        } else {
            visibility_upper
        });
    }

    match visibility_upper.as_str() {
        "PUBLIC" => PublicityResult::new(
            Platform::Github,
            Publicity::Public,
            "gh reports visibility=public, isFork=false",
            detail,
        ),
        "PRIVATE" => PublicityResult::new(
            Platform::Github,
            Publicity::Private,
            "gh reports visibility=private",
            detail,
        ),
        "INTERNAL" => PublicityResult::new(
            Platform::Github,
            Publicity::Internal,
            "gh reports visibility=internal (GHE)",
            detail,
        ),
        other => PublicityResult::new(
            Platform::Github,
            Publicity::Ambiguous,
            format!("gh reports unexpected visibility value: {other}"),
            detail,
        ),
    }
}

/// Recognise `github.com`, `www.github.com`, and typical GHE hostnames.
///
/// Accepts both SSH (`git@github.com:owner/repo.git`) and HTTPS
/// (`https://github.com/owner/repo`) URL forms.
fn is_github_host(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    // SSH: `git@<host>:owner/repo`
    if let Some(rest) = url.strip_prefix("git@")
        && let Some((host, _)) = rest.split_once(':')
    {
        return host_is_github(host);
    }
    // ssh://git@<host>/...
    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        if let Some((host, _)) = rest.split_once('/') {
            return host_is_github(host.split('@').next_back().unwrap_or(host));
        }
    }
    // https:// or http://
    for prefix in ["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            let host_part = rest.split('/').next().unwrap_or("");
            return host_is_github(host_part);
        }
    }
    false
}

fn host_is_github(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    // github.com and www.github.com
    if host == "github.com" || host == "www.github.com" {
        return true;
    }
    // GitHub Enterprise: hostnames commonly contain "github" (e.g.
    // github.example.com, ghe.internal.example.com). We accept anything
    // whose leftmost label includes "github" or "ghe" as GHE; a false
    // positive here downgrades to AMBIGUOUS at the next step (gh repo view
    // will fail) rather than mislabelling.
    let first_label = host.split('.').next().unwrap_or("");
    first_label.contains("github")
        || first_label.contains("ghe")
        || host.contains(".github.")
        || host.contains(".ghe.")
}

// ---------------------------------------------------------------------------
// Crates live check
// ---------------------------------------------------------------------------

/// Probe crates.io for whether `name` is a registered crate.
///
/// Returns `Some(true)` when the API responds 200, `Some(false)` on 404, and
/// `None` on any other outcome (curl missing, network error, non-2xx/404
/// response, timeout). Callers should treat `None` as a live-check
/// unavailability rather than as evidence in either direction.
///
/// Implemented via `curl` subprocess to avoid pulling a full HTTP client into
/// the dependency graph; a 5-second timeout keeps a hung request from
/// stalling gate evaluation.
pub fn check_crates_io_registered(name: &str) -> Option<bool> {
    let url = format!("https://crates.io/api/v1/crates/{name}");
    let output = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "5",
            "-A",
            "lds-publicity/0.9.0 (github.com/ynishi/local-develop-server)",
            &url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let code_str = String::from_utf8_lossy(&output.stdout);
    let code: u16 = code_str.trim().parse().ok()?;
    match code {
        200 => Some(true),
        404 => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Crates classifier
// ---------------------------------------------------------------------------

fn detect_crates_impl(root: &Path) -> PublicityResult {
    let cargo_path = root.join("Cargo.toml");
    let text = match std::fs::read_to_string(&cargo_path) {
        Ok(t) => t,
        Err(e) => {
            return PublicityResult::new(
                Platform::Crates,
                Publicity::Local,
                format!("Cargo.toml not found: {e}"),
                serde_json::json!({}),
            );
        }
    };

    classify_crates_toml(&text)
}

/// Map a Cargo.toml text to a [`PublicityResult`]. Extracted for direct
/// unit testing.
pub fn classify_crates_toml(text: &str) -> PublicityResult {
    let parsed: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            return PublicityResult::new(
                Platform::Crates,
                Publicity::Unknown,
                format!("Cargo.toml parse error: {e}"),
                serde_json::json!({}),
            );
        }
    };

    // Prefer [workspace.package] (workspace crate), fall back to [package].
    let (section_name, section) =
        if let Some(wp) = parsed.get("workspace").and_then(|w| w.get("package")) {
            ("workspace.package", wp)
        } else if let Some(p) = parsed.get("package") {
            ("package", p)
        } else {
            return PublicityResult::new(
                Platform::Crates,
                Publicity::Unknown,
                "Cargo.toml has neither [workspace.package] nor [package]",
                serde_json::json!({}),
            );
        };

    let name = section
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let publish_value = section.get("publish");

    let mut detail = serde_json::json!({
        "section": section_name,
        "name": name,
    });

    match publish_value {
        None => {
            // Cargo default when the field is absent: publishable to crates.io.
            detail["publish"] = serde_json::Value::Null;
            PublicityResult::new(
                Platform::Crates,
                Publicity::Public,
                "publish field absent (Cargo default: crates.io)",
                detail,
            )
        }
        Some(toml::Value::Boolean(false)) => {
            detail["publish"] = serde_json::json!(false);
            PublicityResult::new(
                Platform::Crates,
                Publicity::Private,
                "publish = false",
                detail,
            )
        }
        Some(toml::Value::Boolean(true)) => {
            detail["publish"] = serde_json::json!(true);
            PublicityResult::new(
                Platform::Crates,
                Publicity::Public,
                "publish = true",
                detail,
            )
        }
        Some(toml::Value::Array(arr)) => {
            let registries: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            detail["publish"] = serde_json::json!(registries.clone());
            if registries.is_empty() {
                // `publish = []` is equivalent to `publish = false` per Cargo.
                PublicityResult::new(
                    Platform::Crates,
                    Publicity::Private,
                    "publish = [] (equivalent to false)",
                    detail,
                )
            } else if registries.iter().any(|r| r == "crates-io") {
                PublicityResult::new(
                    Platform::Crates,
                    Publicity::Public,
                    format!("publish = {registries:?} includes crates-io"),
                    detail,
                )
            } else {
                PublicityResult::new(
                    Platform::Crates,
                    Publicity::Internal,
                    format!("publish = {registries:?} (custom registry only)"),
                    detail,
                )
            }
        }
        Some(other) => {
            detail["publish_raw"] = serde_json::json!(other.to_string());
            PublicityResult::new(
                Platform::Crates,
                Publicity::Ambiguous,
                format!("publish field has unexpected shape: {other}"),
                detail,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- host detection -----------------------------------------------------

    #[test]
    fn github_host_recognises_common_forms() {
        assert!(is_github_host("git@github.com:owner/repo.git"));
        assert!(is_github_host("https://github.com/owner/repo"));
        assert!(is_github_host("https://github.com/owner/repo.git"));
        assert!(is_github_host("ssh://git@github.com/owner/repo.git"));
        assert!(is_github_host("http://github.com/owner/repo"));
    }

    #[test]
    fn github_host_recognises_ghe() {
        assert!(is_github_host("git@github.example.com:owner/repo.git"));
        assert!(is_github_host("https://ghe.internal.corp/owner/repo"));
    }

    #[test]
    fn github_host_rejects_non_github() {
        assert!(!is_github_host("git@gitlab.com:owner/repo.git"));
        assert!(!is_github_host("https://bitbucket.org/owner/repo"));
        assert!(!is_github_host("git@codeberg.org:owner/repo"));
        assert!(!is_github_host(""));
    }

    // -- classify_github_json ----------------------------------------------

    #[test]
    fn github_public_non_fork() {
        let v = serde_json::json!({
            "visibility": "public",
            "isFork": false,
            "parent": null,
            "nameWithOwner": "ynishi/local-develop-server",
        });
        let r = classify_github_json(&v, "git@github.com:ynishi/local-develop-server.git");
        assert_eq!(r.publicity, Publicity::Public);
        assert_eq!(r.platform, "github");
    }

    #[test]
    fn github_private() {
        let v = serde_json::json!({
            "visibility": "private",
            "isFork": false,
        });
        let r = classify_github_json(&v, "git@github.com:owner/priv.git");
        assert_eq!(r.publicity, Publicity::Private);
    }

    #[test]
    fn github_internal_ghe() {
        let v = serde_json::json!({
            "visibility": "internal",
            "isFork": false,
        });
        let r = classify_github_json(&v, "git@github.example.com:owner/repo.git");
        assert_eq!(r.publicity, Publicity::Internal);
    }

    #[test]
    fn github_fork_beats_visibility() {
        // fork of a public repo → FORKED, not PUBLIC.
        let v = serde_json::json!({
            "visibility": "public",
            "isFork": true,
            "parent": { "nameWithOwner": "upstream/orig" },
        });
        let r = classify_github_json(&v, "git@github.com:me/fork.git");
        assert_eq!(r.publicity, Publicity::Forked);
        // fork of a private repo → still FORKED.
        let v2 = serde_json::json!({
            "visibility": "private",
            "isFork": true,
        });
        let r2 = classify_github_json(&v2, "git@github.com:me/priv-fork.git");
        assert_eq!(r2.publicity, Publicity::Forked);
    }

    #[test]
    fn github_fork_surfaces_underlying_visibility() {
        // Public fork: underlying_visibility = "PUBLIC".
        let v = serde_json::json!({
            "visibility": "public",
            "isFork": true,
            "parent": { "nameWithOwner": "upstream/orig" },
        });
        let r = classify_github_json(&v, "git@github.com:me/fork.git");
        assert_eq!(r.underlying_visibility.as_deref(), Some("PUBLIC"));
        // Private fork: underlying_visibility = "PRIVATE".
        let v2 = serde_json::json!({
            "visibility": "private",
            "isFork": true,
        });
        let r2 = classify_github_json(&v2, "git@github.com:me/priv-fork.git");
        assert_eq!(r2.underlying_visibility.as_deref(), Some("PRIVATE"));
        // Missing visibility field: underlying_visibility = "UNKNOWN".
        let v3 = serde_json::json!({ "isFork": true });
        let r3 = classify_github_json(&v3, "git@github.com:me/fork.git");
        assert_eq!(r3.underlying_visibility.as_deref(), Some("UNKNOWN"));
        // Non-fork: underlying_visibility must be absent.
        let v4 = serde_json::json!({ "visibility": "public", "isFork": false });
        let r4 = classify_github_json(&v4, "git@github.com:owner/repo.git");
        assert!(r4.underlying_visibility.is_none());
    }

    #[test]
    fn github_unexpected_visibility_ambiguous() {
        let v = serde_json::json!({
            "visibility": "something-new",
            "isFork": false,
        });
        let r = classify_github_json(&v, "git@github.com:owner/repo.git");
        assert_eq!(r.publicity, Publicity::Ambiguous);
    }

    // -- classify_crates_toml ----------------------------------------------

    #[test]
    fn crates_publish_field_absent_is_public() {
        let toml_text = r#"
[package]
name = "demo"
version = "0.1.0"
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Public);
        assert_eq!(r.platform, "crates");
    }

    #[test]
    fn crates_publish_false_is_private() {
        let toml_text = r#"
[package]
name = "demo"
version = "0.1.0"
publish = false
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Private);
    }

    #[test]
    fn crates_publish_true_is_public() {
        let toml_text = r#"
[package]
name = "demo"
publish = true
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Public);
    }

    #[test]
    fn crates_publish_empty_array_is_private() {
        let toml_text = r#"
[package]
name = "demo"
publish = []
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Private);
    }

    #[test]
    fn crates_publish_custom_registry_is_internal() {
        let toml_text = r#"
[package]
name = "demo"
publish = ["my-corp-registry"]
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Internal);
    }

    #[test]
    fn crates_publish_crates_io_in_array_is_public() {
        let toml_text = r#"
[package]
name = "demo"
publish = ["crates-io"]
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Public);
    }

    #[test]
    fn crates_workspace_package_wins_over_package() {
        // Emulate a workspace root: only [workspace.package] is present.
        let toml_text = r#"
[workspace]
members = ["crates/a"]

[workspace.package]
version = "0.1.0"
publish = false
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Private);
        assert_eq!(r.detail["section"], "workspace.package");
    }

    #[test]
    fn crates_no_section_is_unknown() {
        let toml_text = r#"
[dependencies]
serde = "1"
"#;
        let r = classify_crates_toml(toml_text);
        assert_eq!(r.publicity, Publicity::Unknown);
    }

    #[test]
    fn crates_parse_error_is_unknown() {
        let r = classify_crates_toml("this is [ not = valid toml @@@");
        assert_eq!(r.publicity, Publicity::Unknown);
    }

    // -- Platform parse -----------------------------------------------------

    #[tokio::test]
    async fn detect_github_non_git_root_returns_not_git() {
        // A tempdir with no `.git` inside must classify as NOT_GIT — the
        // classifier distinguishes "not a git repository at all" from
        // "git repo present but no remotes" (LOCAL).
        let tmp = tempfile::tempdir().expect("tempdir");
        let session = std::sync::Arc::new(
            lds_core::Session::new(lds_core::SessionConfig {
                root: tmp.path().to_path_buf(),
                ..Default::default()
            })
            .expect("session"),
        );
        let module = PublicityModule::new(session);
        let result = module.detect_github().await;
        assert_eq!(
            result.publicity,
            Publicity::NotGit,
            "expected NOT_GIT for non-git tempdir, got {:?}: {}",
            result.publicity,
            result.reason
        );
        assert_eq!(result.platform, "github");
    }

    #[test]
    fn platform_parse_accepts_aliases() {
        assert_eq!(Platform::parse("github"), Some(Platform::Github));
        assert_eq!(Platform::parse("gh"), Some(Platform::Github));
        assert_eq!(Platform::parse("GitHub"), Some(Platform::Github));
        assert_eq!(Platform::parse("crates"), Some(Platform::Crates));
        assert_eq!(Platform::parse("crates.io"), Some(Platform::Crates));
        assert_eq!(Platform::parse("cargo"), Some(Platform::Crates));
        assert_eq!(Platform::parse("npm"), None);
    }
}
