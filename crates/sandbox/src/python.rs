//! Sandboxed Python execution via subprocess + preamble guard.
//!
//! 3-layer security: sys.modules poisoning, __import__ guard,
//! and os attribute removal. Ported from python-exec-mcp.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

const DEFAULT_DENY: &[&str] = &[
    "subprocess",
    "shutil",
    "ctypes",
    "multiprocessing",
    "signal",
    "importlib",
    "importlib.util",
    "importlib.abc",
    "importlib.machinery",
    "importlib.resources",
];

const DANGEROUS_OS_ATTRS: &[&str] = &[
    "system",
    "popen",
    "exec",
    "execl",
    "execle",
    "execlp",
    "execlpe",
    "execv",
    "execve",
    "execvp",
    "execvpe",
    "spawnl",
    "spawnle",
    "spawnlp",
    "spawnlpe",
    "spawnv",
    "spawnve",
    "spawnvp",
    "spawnvpe",
    "posix_spawn",
    "posix_spawnp",
    "fork",
    "forkpty",
    "kill",
    "killpg",
    "plock",
    "startfile",
];

const MAX_TIMEOUT_SECS: u64 = 300;
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct SandboxPython {
    binary: String,
    workdir: PathBuf,
    timeout: Duration,
    deny_modules: Vec<String>,
    deny_paths: Vec<PathBuf>,
}

impl SandboxPython {
    pub fn new(workdir: &Path) -> Self {
        Self {
            binary: detect_binary().unwrap_or_else(|| "python3".to_string()),
            workdir: workdir.to_path_buf(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            deny_modules: Vec::new(),
            deny_paths: Vec::new(),
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs.min(MAX_TIMEOUT_SECS));
        self
    }

    pub fn with_deny_modules(mut self, modules: Vec<String>) -> Self {
        self.deny_modules = modules;
        self
    }

    pub fn with_deny_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.deny_paths = paths;
        self
    }

    fn all_deny_modules(&self) -> Vec<String> {
        let mut modules: Vec<String> = DEFAULT_DENY.iter().map(|s| s.to_string()).collect();
        for m in &self.deny_modules {
            if !modules.contains(m) {
                modules.push(m.clone());
            }
        }
        modules
    }

    fn needs_sandbox(&self) -> bool {
        let name = std::path::Path::new(&self.binary)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.binary);
        name.starts_with("python")
    }

    fn build_preamble(&self) -> String {
        let deny = self.all_deny_modules();
        let module_blocks: String = deny
            .iter()
            .map(|m| format!("_sys.modules['{m}'] = None"))
            .collect::<Vec<_>>()
            .join("\n");

        let os_attrs: String = DANGEROUS_OS_ATTRS
            .iter()
            .map(|a| format!("'{a}'"))
            .collect::<Vec<_>>()
            .join(", ");

        let deny_set: String = deny
            .iter()
            .map(|m| format!("'{m}'"))
            .collect::<Vec<_>>()
            .join(", ");

        let open_guard = if self.deny_paths.is_empty() {
            String::new()
        } else {
            let paths: String = self
                .deny_paths
                .iter()
                .map(|p| format!("'{}'", p.display()))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                r#"
import os.path as _osp
_orig_open = open
_deny_paths = tuple(_osp.realpath(p) for p in ({paths},))
def _safe_open(file, *args, _blocked=_deny_paths, _op=_orig_open, _rp=_osp.realpath, _absp=_osp.abspath, **kwargs):
    _resolved = _rp(_absp(str(file)))
    for _bp in _blocked:
        if _resolved == _bp or _resolved.startswith(_bp + '/'):
            raise PermissionError("access to '{{0}}' is blocked".format(file))
    return _op(file, *args, **kwargs)
if hasattr(__builtins__, 'open'):
    __builtins__.open = _safe_open
else:
    __builtins__['open'] = _safe_open
del _osp, _orig_open, _deny_paths, _safe_open
"#
            )
        };

        format!(
            r#"import sys as _sys
{module_blocks}
import os as _os
for _attr in ({os_attrs}):
    if hasattr(_os, _attr):
        delattr(_os, _attr)
_orig_import = __builtins__.__import__ if hasattr(__builtins__, '__import__') else __import__
_deny_set = frozenset(({deny_set}))
def _safe_import(name, *args, _deny=_deny_set, _imp=_orig_import, **kwargs):
    if name in _deny or name.split('.')[0] in _deny:
        raise ImportError("module '{{0}}' is blocked".format(name))
    return _imp(name, *args, **kwargs)
if hasattr(__builtins__, '__import__'):
    __builtins__.__import__ = _safe_import
else:
    __builtins__['__import__'] = _safe_import
del _os, _sys, _orig_import, _deny_set, _safe_import{open_guard}
"#
        )
    }

    fn wrap_script(&self, script: &str) -> String {
        if self.needs_sandbox() {
            format!("{}\n{script}", self.build_preamble())
        } else {
            script.to_string()
        }
    }

    pub async fn execute(&self, script: &str) -> Result<PythonResult> {
        let wrapped = self.wrap_script(script);

        let mut cmd = tokio::process::Command::new(&self.binary);
        cmd.arg("-c")
            .arg(&wrapped)
            .current_dir(&self.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let result = tokio::time::timeout(self.timeout, cmd.output()).await;
        match result {
            Ok(Ok(output)) => Ok(PythonResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
            }),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(PythonResult {
                stdout: String::new(),
                stderr: format!("python script timed out after {}s", self.timeout.as_secs()),
                exit_code: -1,
                timed_out: true,
            }),
        }
    }

    pub async fn execute_file(&self, path: &Path) -> Result<PythonResult> {
        if !path.exists() {
            bail!("python file not found: {}", path.display());
        }
        let content = std::fs::read_to_string(path)?;
        self.execute(&content).await
    }
}

fn detect_binary() -> Option<String> {
    for candidate in &["python3", "micropython"] {
        if std::process::Command::new(candidate)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return Some(candidate.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PythonResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skip_if_no_python() -> bool {
        detect_binary().is_none_or(|b| !b.starts_with("python"))
    }

    #[tokio::test]
    async fn execute_basic_script() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path());
        let result = py.execute("print('hello')").await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
    }

    #[tokio::test]
    async fn execute_blocks_subprocess() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path());
        let result = py
            .execute("import subprocess; print('reached')")
            .await
            .unwrap();
        assert_ne!(result.exit_code, 0);
        assert!(result.stderr.contains("blocked") || result.stderr.contains("ImportError"));
    }

    #[tokio::test]
    async fn execute_blocks_os_system() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path());
        let result = py
            .execute("import os; os.system('echo bad')")
            .await
            .unwrap();
        assert_ne!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn execute_timeout() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path()).with_timeout(1);
        let result = py
            .execute("import time; time.sleep(5); print('done')")
            .await
            .unwrap();
        assert!(result.timed_out);
    }

    #[tokio::test]
    async fn execute_file_runs_script() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let script_path = tmp.path().join("hello.py");
        std::fs::write(&script_path, "print('from file')").unwrap();

        let py = SandboxPython::new(tmp.path());
        let result = py.execute_file(&script_path).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("from file"));
    }

    #[tokio::test]
    async fn execute_allows_safe_imports() {
        if skip_if_no_python() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path());
        let result = py
            .execute("import json; print(json.dumps({'k': 1}))")
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("\"k\""));
    }

    #[test]
    fn preamble_includes_default_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path());
        let preamble = py.build_preamble();
        for m in DEFAULT_DENY {
            assert!(preamble.contains(&format!("'{m}'")), "missing {m}");
        }
    }

    #[test]
    fn preamble_includes_custom_deny() {
        let tmp = tempfile::tempdir().unwrap();
        let py = SandboxPython::new(tmp.path()).with_deny_modules(vec!["socket".to_string()]);
        let preamble = py.build_preamble();
        assert!(preamble.contains("'socket'"));
    }
}
