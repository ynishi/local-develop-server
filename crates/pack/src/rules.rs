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
//! cache_dirs   = ["frontend/dist"]
//! keep         = ["docs/samples/*.pem"]
//! ```
//!
//! Extensions **add to** the built-ins rather than replacing them, so declaring
//! one project-specific name cannot silently disable the rest of the
//! protection. `keep` is the only subtractive list: it names files a built-in
//! rule would exclude but that this project wants packed.
//!
//! # Scoping
//!
//! Every list here follows the convention `.gitignore` already established, so
//! there is no second one to learn:
//!
//! | glob | matched against |
//! |---|---|
//! | no `/` (`*.pem`, `.env`) | the **file name**, at any depth |
//! | contains `/` (`docs/samples/*.pem`) | the **path relative to the project root** |
//!
//! Every built-in is a bare name, so all of them keep reaching the whole tree.
//! Scoping exists for the operator's own rules, where reaching the whole tree
//! is the hazard: `keep = ["*.pem"]` written to carry one sample key carries
//! every private key in the project, and `cache_dirs = ["dist"]` written for a
//! build output drops any hand-written `dist/` that happens to share the name.
//! Anchoring the rule to a path confines it to the case it was written for.

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
    /// Path globs whose symlinks are packed but left out of the link report.
    ///
    /// A symlink is a problem by default — it breaks when the project is
    /// carried elsewhere — so every one is reported. This names the exception:
    /// a directory that is *meant* to be links, such as a shared `.zsh/` tree.
    /// Those are already known to the operator, so listing them is noise that
    /// hides the links that do need attention.
    ///
    /// **No built-in counterpart**, deliberately: only the operator knows which
    /// of their directories are link-by-design. Unset means every symlink is
    /// reported.
    pub no_link_report: Vec<String>,
}

impl RuleOverrides {
    /// Whether the operator supplied anything at all.
    pub fn is_empty(&self) -> bool {
        self.secret_globs.is_empty()
            && self.cache_dirs.is_empty()
            && self.keep.is_empty()
            && self.no_link_report.is_empty()
    }
}

/// What the classification rules decided about one file name.
///
/// Spelled out as three cases rather than an `Option`, because the third one —
/// a `keep` rule overriding a secret rule — used to be indistinguishable from
/// "no rule matched". A file carried past the secret list is exactly the file a
/// reader has to know about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileVerdict {
    /// No rule applies. Packed as ordinary content.
    Ordinary,
    /// A secret rule matched and nothing overrode it. Not packed, reported.
    Secret {
        /// The secret glob that matched.
        pattern: String,
    },
    /// A secret rule matched but a `keep` rule outranked it, so the file *is*
    /// packed.
    ///
    /// The operator asked for this, so it is not an error and not a warning —
    /// but the entire purpose of the secret list is that these files are
    /// dangerous to carry, so the override is recorded instead of applied
    /// silently.
    KeptOverSecret {
        /// The `keep` glob that rescued the file.
        keep_pattern: String,
        /// The secret glob it outranked.
        secret_pattern: String,
    },
}

/// What a compiled glob is matched against.
///
/// Decided from the glob itself, by the same convention `.gitignore` uses, so
/// an operator does not have to learn a second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// No `/` in the glob: matched against the file name, at any depth.
    Name,
    /// The glob contains `/`: matched against the path relative to the project
    /// root, so the rule reaches exactly one place in the tree.
    Path,
}

/// One compiled classification glob and what it is matched against.
#[derive(Debug, Clone)]
struct Rule {
    pattern: Pattern,
    scope: Scope,
}

impl Rule {
    /// Compile `raw`, taking its scope from whether it contains a separator.
    ///
    /// # Errors
    ///
    /// [`PackError::BadPattern`] when the glob is malformed.
    fn compile(raw: &str) -> Result<Self, PackError> {
        Ok(Self {
            pattern: compile_custom(raw)?,
            scope: if raw.contains('/') {
                Scope::Path
            } else {
                Scope::Name
            },
        })
    }

    /// Compile a built-in literal, which is known-good at authoring time.
    fn builtin(raw: &str) -> Option<Self> {
        Self::compile(raw).ok()
    }

    /// # Arguments
    ///
    /// * `name` — File or directory name, no separators.
    /// * `rel` — Path relative to the project root, `/`-separated.
    fn matches(&self, name: &str, rel: &str) -> bool {
        match self.scope {
            Scope::Name => self.pattern.matches(name),
            Scope::Path => self.pattern.matches(rel),
        }
    }

    /// The glob as the operator wrote it.
    fn as_str(&self) -> &str {
        self.pattern.as_str()
    }
}

/// Compiled classification rules used by the scan.
#[derive(Debug, Clone)]
pub struct PackRules {
    secret: Vec<Rule>,
    keep: Vec<Rule>,
    cache_dirs: Vec<Rule>,
    no_link_report: Vec<Rule>,
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
    /// Rules ready to classify paths.
    ///
    /// # Errors
    ///
    /// [`PackError::BadPattern`] when an operator-supplied glob is malformed,
    /// naming the offending pattern. A typo in config must fail loudly rather
    /// than silently classifying nothing.
    pub fn new(overrides: &RuleOverrides) -> Result<Self, PackError> {
        let mut secret = compile_builtin(DEFAULT_SECRET_GLOBS);
        for raw in &overrides.secret_globs {
            secret.push(Rule::compile(raw)?);
        }

        let mut keep = compile_builtin(DEFAULT_KEEP);
        for raw in &overrides.keep {
            keep.push(Rule::compile(raw)?);
        }

        let mut cache_dirs = compile_builtin(DEFAULT_CACHE_DIRS);
        for raw in &overrides.cache_dirs {
            cache_dirs.push(Rule::compile(raw)?);
        }

        // No built-in list to seed from: see `RuleOverrides::no_link_report`.
        let mut no_link_report = Vec::new();
        for raw in &overrides.no_link_report {
            no_link_report.push(Rule::compile(raw)?);
        }

        Ok(Self {
            secret,
            keep,
            cache_dirs,
            no_link_report,
            custom_secret_count: overrides.secret_globs.len(),
            custom_keep_count: overrides.keep.len(),
            custom_cache_count: overrides.cache_dirs.len(),
        })
    }

    /// The `no_link_report` glob that covers this path, if the operator
    /// declared one.
    ///
    /// # Arguments
    ///
    /// * `name` — Link name, no separators.
    /// * `rel` — Path relative to the project root, `/`-separated.
    ///
    /// # Returns
    ///
    /// The glob exactly as configured, so a caller can record which rule
    /// suppressed the report rather than only that something did.
    ///
    /// Always `None` when the operator configured nothing, which is the point:
    /// this crate does not decide on its own that some directory's links are
    /// expected.
    pub fn no_link_report_match(&self, name: &str, rel: &str) -> Option<&str> {
        self.no_link_report
            .iter()
            .find(|r| r.matches(name, rel))
            .map(|r| r.as_str())
    }

    /// Whether a directory is a regenerable cache.
    ///
    /// # Arguments
    ///
    /// * `name` — Directory name, no separators.
    /// * `rel` — Path relative to the project root, `/`-separated.
    pub fn is_cache_dir(&self, name: &str, rel: &str) -> bool {
        // `keep` outranks every exclusion, caches included.
        if self.keep_match(name, rel).is_some() {
            return false;
        }
        self.cache_dirs.iter().any(|r| r.matches(name, rel))
    }

    /// Classify a file against the secret and `keep` lists.
    ///
    /// # Arguments
    ///
    /// * `name` — File name, no separators.
    /// * `rel` — Path relative to the project root, `/`-separated.
    ///
    /// # Returns
    ///
    /// Which of the three outcomes applies, naming every glob involved. A
    /// `keep` rule outranking a secret rule yields
    /// [`FileVerdict::KeptOverSecret`] rather than [`FileVerdict::Ordinary`],
    /// so the caller can record that the file was carried past the secret list
    /// on purpose.
    pub fn classify(&self, name: &str, rel: &str) -> FileVerdict {
        let Some(secret) = self.secret.iter().find(|r| r.matches(name, rel)) else {
            return FileVerdict::Ordinary;
        };
        match self.keep_match(name, rel) {
            Some(keep_pattern) => FileVerdict::KeptOverSecret {
                keep_pattern: keep_pattern.to_string(),
                secret_pattern: secret.as_str().to_string(),
            },
            None => FileVerdict::Secret {
                pattern: secret.as_str().to_string(),
            },
        }
    }

    /// The `keep` glob that rescues this path from an exclusion, if any.
    fn keep_match(&self, name: &str, rel: &str) -> Option<&str> {
        self.keep
            .iter()
            .find(|r| r.matches(name, rel))
            .map(|r| r.as_str())
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
fn compile_builtin(raw: &[&str]) -> Vec<Rule> {
    raw.iter().filter_map(|p| Rule::builtin(p)).collect()
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

    /// Whether a name is excluded as a secret — the outcome most of these
    /// tests are about. A `keep` rule rescuing the file is a *different*
    /// verdict and is asserted on directly where it matters.
    ///
    /// Passing the name as the path too puts the file at the project root,
    /// where the two coincide. Path scoping is exercised separately.
    fn is_secret(r: &PackRules, name: &str) -> bool {
        matches!(r.classify(name, name), FileVerdict::Secret { .. })
    }

    /// A directory at the project root, where its name and path coincide.
    fn is_cache(r: &PackRules, name: &str) -> bool {
        r.is_cache_dir(name, name)
    }

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
            assert!(is_secret(&r, name), "{name} should be treated as a secret");
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
            assert!(!is_secret(&r, name), "{name} must travel");
        }
    }

    /// Templates are rescued from the `.env.*` rule by the built-in keep list.
    #[test]
    fn test_builtin_keep_rescues_templates() {
        let r = PackRules::default();
        for name in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
            assert!(!is_secret(&r, name), "{name} is a template");
        }
        assert!(is_secret(&r, ".env.local"));
    }

    /// Built-in cache directories are recognized.
    #[test]
    fn test_builtin_cache_dirs() {
        let r = PackRules::default();
        assert!(is_cache(&r, "target"));
        assert!(is_cache(&r, "node_modules"));
        assert!(!is_cache(&r, "src"));
        assert!(!is_cache(&r, "dist"), "dist is source in many projects");
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

        assert!(is_secret(&r, "my-app-keys.json"));
        assert!(is_secret(&r, "prod.vault"));
        // built-ins still in force
        assert!(is_secret(&r, ".env"));
        assert!(is_secret(&r, "secret.toml"));
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
            !is_secret(&r, ".npmrc"),
            "keep must override the built-in secret rule"
        );
        assert!(is_secret(&r, ".netrc"), "siblings unaffected");
    }

    /// `keep` also outranks the cache rule.
    #[test]
    fn test_keep_overrides_cache_dir() {
        let r = PackRules::new(&RuleOverrides {
            keep: vec!["target".to_string()],
            ..Default::default()
        })
        .expect("compile");
        assert!(!is_cache(&r, "target"));
    }

    /// Extra cache directories are honored.
    #[test]
    fn test_custom_cache_dir() {
        let r = PackRules::new(&RuleOverrides {
            cache_dirs: vec!["dist".to_string(), "build".to_string()],
            ..Default::default()
        })
        .expect("compile");
        assert!(is_cache(&r, "dist"));
        assert!(is_cache(&r, "build"));
        assert!(is_cache(&r, "target"), "built-ins remain");
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
            r.classify("prod.vault", "prod.vault"),
            FileVerdict::Secret {
                pattern: "*.vault".to_string()
            }
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

    // ------------------------------------------------------------------
    // scoping: a glob with `/` names a path, one without names a file
    // ------------------------------------------------------------------

    /// A glob with no separator keeps matching at any depth, which is what
    /// every built-in rule relies on.
    #[test]
    fn test_name_glob_matches_at_any_depth() {
        let r = PackRules::default();
        assert!(matches!(
            r.classify("key.pem", "deep/nested/key.pem"),
            FileVerdict::Secret { .. }
        ));
        assert!(matches!(
            r.classify(".env", "services/api/.env"),
            FileVerdict::Secret { .. }
        ));
    }

    /// A glob with a separator is anchored to that path and nowhere else.
    #[test]
    fn test_path_glob_matches_only_its_own_path() {
        let r = PackRules::new(&RuleOverrides {
            secret_globs: vec!["deploy/*.token".to_string()],
            ..Default::default()
        })
        .expect("compile");

        assert!(matches!(
            r.classify("prod.token", "deploy/prod.token"),
            FileVerdict::Secret { .. }
        ));
        assert!(
            matches!(
                r.classify("prod.token", "docs/prod.token"),
                FileVerdict::Ordinary
            ),
            "a path-scoped rule must not reach outside its path"
        );
    }

    /// A path-scoped `keep` rescues its own directory without opening the rule
    /// up everywhere — the reason scoping was worth having.
    #[test]
    fn test_path_scoped_keep_rescues_only_there() {
        let r = PackRules::new(&RuleOverrides {
            keep: vec!["docs/samples/*.pem".to_string()],
            ..Default::default()
        })
        .expect("compile");

        assert!(
            matches!(
                r.classify("demo.pem", "docs/samples/demo.pem"),
                FileVerdict::KeptOverSecret { .. }
            ),
            "the sample is carried, and recorded as an override"
        );
        assert!(
            matches!(
                r.classify("server.pem", "deploy/server.pem"),
                FileVerdict::Secret { .. }
            ),
            "a real key elsewhere stays excluded"
        );
    }

    /// A path-scoped cache rule drops one `dist`, not every directory that
    /// happens to share the name.
    #[test]
    fn test_path_scoped_cache_dir_does_not_catch_namesakes() {
        let r = PackRules::new(&RuleOverrides {
            cache_dirs: vec!["frontend/dist".to_string()],
            ..Default::default()
        })
        .expect("compile");

        assert!(r.is_cache_dir("dist", "frontend/dist"));
        assert!(
            !r.is_cache_dir("dist", "vendor/dist"),
            "a namesake elsewhere may well be hand-written source"
        );
    }
}
