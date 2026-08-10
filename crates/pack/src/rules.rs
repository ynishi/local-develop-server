//! Classification rules: which file names are secrets, which directories are
//! caches, and which of those the operator wants carried anyway.
//!
//! The built-in lists are the floor, not the ceiling. Secret file names are
//! open-ended — every ecosystem invents its own (`credentials.toml`,
//! `terraform.tfvars`, `service-account.json`), and a project can always have
//! one nobody has heard of (`my-app-keys.json`). A fixed list is therefore
//! guaranteed to be incomplete, so operators can extend it via
//! `~/.config/lds/config.toml`:
//!
//! ```toml
//! [pack]
//! secret_globs = ["my-app-keys.json", "*.vault"]
//! cache_dirs   = ["dist"]
//! keep         = [".npmrc"]
//! ```
//!
//! Extensions **add to** the built-ins rather than replacing them, so declaring
//! one project-specific name cannot silently disable the rest of the
//! protection. `keep` is the only subtractive list: it names files a built-in
//! rule would exclude but that this project wants packed.

use glob::Pattern;

use crate::error::PackError;

/// Directory names treated as regenerable caches.
///
/// `dist` and `build` are deliberately absent: both are common names for
/// hand-written source in projects that do not use them as output directories,
/// and wrongly dropping source is far worse than carrying a rebuildable tree.
/// A project that does use them as output can add them via `[pack] cache_dirs`.
pub const DEFAULT_CACHE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".turbo",
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".gradle",
];

/// File-name globs treated as secrets.
///
/// Grouped by what they are rather than alphabetically, so a gap is visible as
/// a missing group rather than a missing line.
pub const DEFAULT_SECRET_GLOBS: &[&str] = &[
    // dotenv and friends — `.env.example` and co. are rescued by DEFAULT_KEEP
    ".env",
    ".env.*",
    // per-tool credential files
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".dockercfg",
    ".pgpass",
    ".my.cnf",
    ".htpasswd",
    "credentials",
    "credentials.toml",
    // generically named secret bundles
    "secret.toml",
    "secrets.toml",
    "secret.yaml",
    "secrets.yaml",
    "secret.yml",
    "secrets.yml",
    "secret.json",
    "secrets.json",
    // cloud / infra
    "service-account*.json",
    "terraform.tfvars",
    "*.auto.tfvars",
    "kubeconfig",
    // ssh private keys (the `.pub` counterparts are public and travel)
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    // key / certificate containers
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "*.p8",
    "*.ppk",
    "*.asc",
    "*.gpg",
];

/// File-name globs packed despite matching a secret rule.
///
/// These are the checked-in templates that exist precisely to be shared; they
/// match `.env.*` but hold placeholders, not credentials.
pub const DEFAULT_KEEP: &[&str] = &[
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    ".env.defaults",
];

/// Operator-supplied additions read from `[pack]` in `config.toml`.
#[derive(Debug, Clone, Default)]
pub struct RuleOverrides {
    /// Extra secret globs, added to [`DEFAULT_SECRET_GLOBS`].
    pub secret_globs: Vec<String>,
    /// Extra cache directory names, added to [`DEFAULT_CACHE_DIRS`].
    pub cache_dirs: Vec<String>,
    /// Globs packed anyway, added to [`DEFAULT_KEEP`].
    pub keep: Vec<String>,
}

impl RuleOverrides {
    /// Whether the operator supplied anything at all.
    pub fn is_empty(&self) -> bool {
        self.secret_globs.is_empty() && self.cache_dirs.is_empty() && self.keep.is_empty()
    }
}

/// Compiled classification rules used by the scan.
#[derive(Debug, Clone)]
pub struct PackRules {
    secret: Vec<Pattern>,
    keep: Vec<Pattern>,
    cache_dirs: Vec<String>,
    /// How many of the compiled patterns came from the operator, for reporting.
    pub custom_secret_count: usize,
    /// How many keep patterns came from the operator, for reporting.
    pub custom_keep_count: usize,
    /// How many cache directory names came from the operator, for reporting.
    pub custom_cache_count: usize,
}

impl Default for PackRules {
    fn default() -> Self {
        // Compiling the built-in globs cannot fail; they are literals in this
        // file and are covered by a test that compiles every one of them.
        Self::new(&RuleOverrides::default()).expect("built-in globs must compile")
    }
}

impl PackRules {
    /// Compile the built-in rules plus the operator's additions.
    ///
    /// # Arguments
    ///
    /// * `overrides` — Extra globs and directory names from `[pack]`.
    ///
    /// # Returns
    ///
    /// Rules ready to classify file names.
    ///
    /// # Errors
    ///
    /// [`PackError::BadPattern`] when an operator-supplied glob is malformed,
    /// naming the offending pattern. A typo in config must fail loudly rather
    /// than silently classifying nothing.
    pub fn new(overrides: &RuleOverrides) -> Result<Self, PackError> {
        let mut secret = compile_builtin(DEFAULT_SECRET_GLOBS);
        for raw in &overrides.secret_globs {
            secret.push(compile_custom(raw)?);
        }

        let mut keep = compile_builtin(DEFAULT_KEEP);
        for raw in &overrides.keep {
            keep.push(compile_custom(raw)?);
        }

        let mut cache_dirs: Vec<String> = DEFAULT_CACHE_DIRS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        cache_dirs.extend(overrides.cache_dirs.iter().cloned());

        Ok(Self {
            secret,
            keep,
            cache_dirs,
            custom_secret_count: overrides.secret_globs.len(),
            custom_keep_count: overrides.keep.len(),
            custom_cache_count: overrides.cache_dirs.len(),
        })
    }

    /// Whether a directory name is a regenerable cache.
    pub fn is_cache_dir(&self, name: &str) -> bool {
        // `keep` outranks every exclusion, caches included.
        if self.is_kept(name) {
            return false;
        }
        self.cache_dirs.iter().any(|d| d == name)
    }

    /// Whether a file name must be treated as a secret, and which rule said so.
    ///
    /// # Arguments
    ///
    /// * `name` — File name, not a path.
    ///
    /// # Returns
    ///
    /// `Some(reason)` naming the matched glob when the file must not be packed.
    pub fn secret_reason(&self, name: &str) -> Option<String> {
        if self.is_kept(name) {
            return None;
        }
        let matched = self.secret.iter().find(|p| p.matches(name))?;
        Some(format!("secret pattern: {}", matched.as_str()))
    }

    /// Whether a name is explicitly kept despite matching an exclusion.
    fn is_kept(&self, name: &str) -> bool {
        self.keep.iter().any(|p| p.matches(name))
    }

    /// Total number of secret patterns in force.
    pub fn secret_pattern_count(&self) -> usize {
        self.secret.len()
    }

    /// Whether the operator customized anything.
    pub fn is_customized(&self) -> bool {
        self.custom_secret_count + self.custom_keep_count + self.custom_cache_count > 0
    }
}

/// Compile built-in literals, which are known-good at authoring time.
fn compile_builtin(raw: &[&str]) -> Vec<Pattern> {
    raw.iter()
        .filter_map(|p| Pattern::new(p).ok())
        .collect::<Vec<_>>()
}

/// Compile an operator-supplied glob, reporting the pattern on failure.
fn compile_custom(raw: &str) -> Result<Pattern, PackError> {
    Pattern::new(raw).map_err(|e| PackError::BadPattern {
        pattern: raw.to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // built-in coverage
    // ------------------------------------------------------------------

    /// Every built-in glob compiles — `PackRules::default` relies on this.
    #[test]
    fn test_all_builtin_globs_compile() {
        for raw in DEFAULT_SECRET_GLOBS.iter().chain(DEFAULT_KEEP.iter()) {
            assert!(
                Pattern::new(raw).is_ok(),
                "built-in glob is malformed: {raw}"
            );
        }
        let rules = PackRules::new(&RuleOverrides::default()).expect("defaults compile");
        assert_eq!(rules.secret_pattern_count(), DEFAULT_SECRET_GLOBS.len());
    }

    /// The names that motivated making this configurable are covered by default.
    #[test]
    fn test_builtin_covers_common_secret_names() {
        let r = PackRules::default();
        for name in [
            ".env",
            ".env.production",
            "secret.toml",
            "secrets.yaml",
            "secrets.json",
            "credentials.toml",
            "terraform.tfvars",
            "prod.auto.tfvars",
            "service-account-prod.json",
            "kubeconfig",
            ".pgpass",
            ".pypirc",
            "id_ed25519",
            "server.pem",
            "signing.p8",
            "putty.ppk",
            "key.asc",
        ] {
            assert!(
                r.secret_reason(name).is_some(),
                "{name} should be treated as a secret"
            );
        }
    }

    /// Ordinary project files are not secrets.
    #[test]
    fn test_builtin_passes_ordinary_files() {
        let r = PackRules::default();
        for name in [
            "main.rs",
            "README.md",
            ".mcp.json",
            "Cargo.toml",
            "id_rsa.pub",
        ] {
            assert!(r.secret_reason(name).is_none(), "{name} must travel");
        }
    }

    /// Templates are rescued from the `.env.*` rule by the built-in keep list.
    #[test]
    fn test_builtin_keep_rescues_templates() {
        let r = PackRules::default();
        for name in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
            assert!(r.secret_reason(name).is_none(), "{name} is a template");
        }
        assert!(r.secret_reason(".env.local").is_some());
    }

    /// Built-in cache directories are recognized.
    #[test]
    fn test_builtin_cache_dirs() {
        let r = PackRules::default();
        assert!(r.is_cache_dir("target"));
        assert!(r.is_cache_dir("node_modules"));
        assert!(!r.is_cache_dir("src"));
        assert!(!r.is_cache_dir("dist"), "dist is source in many projects");
    }

    // ------------------------------------------------------------------
    // operator overrides
    // ------------------------------------------------------------------

    /// A project-specific secret name can be added without losing the built-ins.
    #[test]
    fn test_custom_secret_glob_adds_without_replacing() {
        let r = PackRules::new(&RuleOverrides {
            secret_globs: vec!["my-app-keys.json".to_string(), "*.vault".to_string()],
            ..Default::default()
        })
        .expect("compile");

        assert!(r.secret_reason("my-app-keys.json").is_some());
        assert!(r.secret_reason("prod.vault").is_some());
        // built-ins still in force
        assert!(r.secret_reason(".env").is_some());
        assert!(r.secret_reason("secret.toml").is_some());
        assert_eq!(r.custom_secret_count, 2);
        assert!(r.is_customized());
    }

    /// `keep` subtracts: a built-in exclusion can be overridden per project.
    #[test]
    fn test_keep_overrides_builtin_secret() {
        let r = PackRules::new(&RuleOverrides {
            keep: vec![".npmrc".to_string()],
            ..Default::default()
        })
        .expect("compile");

        assert!(
            r.secret_reason(".npmrc").is_none(),
            "keep must override the built-in secret rule"
        );
        assert!(r.secret_reason(".netrc").is_some(), "siblings unaffected");
    }

    /// `keep` also outranks the cache rule.
    #[test]
    fn test_keep_overrides_cache_dir() {
        let r = PackRules::new(&RuleOverrides {
            keep: vec!["target".to_string()],
            ..Default::default()
        })
        .expect("compile");
        assert!(!r.is_cache_dir("target"));
    }

    /// Extra cache directories are honored.
    #[test]
    fn test_custom_cache_dir() {
        let r = PackRules::new(&RuleOverrides {
            cache_dirs: vec!["dist".to_string(), "build".to_string()],
            ..Default::default()
        })
        .expect("compile");
        assert!(r.is_cache_dir("dist"));
        assert!(r.is_cache_dir("build"));
        assert!(r.is_cache_dir("target"), "built-ins remain");
        assert_eq!(r.custom_cache_count, 2);
    }

    /// A malformed operator glob fails loudly and names itself.
    #[test]
    fn test_malformed_custom_glob_is_reported() {
        let err = PackRules::new(&RuleOverrides {
            secret_globs: vec!["broken[".to_string()],
            ..Default::default()
        })
        .expect_err("malformed glob must fail");

        match err {
            PackError::BadPattern { pattern, .. } => assert_eq!(pattern, "broken["),
            other => panic!("expected BadPattern, got {other:?}"),
        }
    }

    /// The reason string names the glob that matched, so a surprising exclusion
    /// can be traced back to the rule responsible for it.
    #[test]
    fn test_reason_names_the_matching_pattern() {
        let r = PackRules::new(&RuleOverrides {
            secret_globs: vec!["*.vault".to_string()],
            ..Default::default()
        })
        .expect("compile");
        assert_eq!(
            r.secret_reason("prod.vault").as_deref(),
            Some("secret pattern: *.vault")
        );
    }

    /// An empty override set leaves the defaults untouched.
    #[test]
    fn test_empty_overrides_are_defaults() {
        let o = RuleOverrides::default();
        assert!(o.is_empty());
        let r = PackRules::new(&o).expect("compile");
        assert!(!r.is_customized());
    }
}
