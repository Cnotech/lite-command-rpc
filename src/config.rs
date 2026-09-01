use crate::process::ExecRequest;
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;
use std::{
    fs::{self, File},
    path::{Component, Path, PathBuf},
};

const FORBIDDEN_PROGRAM_INTERPRETERS: &[&str] = &[
    "bash",
    "bun",
    "cmd",
    "command",
    "cscript",
    "deno",
    "dotnet",
    "fish",
    "java",
    "javaw",
    "mshta",
    "node",
    "perl",
    "php",
    "powershell",
    "powershell_ise",
    "pwsh",
    "py",
    "python",
    "python3",
    "pythonw",
    "ruby",
    "sh",
    "wscript",
    "wsl",
    "zsh",
];

#[derive(Debug, Default)]
pub(crate) struct PathGuard {
    #[cfg(windows)]
    _handles: Vec<File>,
}

#[derive(Debug)]
pub struct GuardedPath {
    pub path: PathBuf,
    _guard: PathGuard,
}

#[derive(Debug)]
pub struct GuardedDownload {
    pub path: PathBuf,
    pub file: File,
    _guard: PathGuard,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    work_dir: Option<WorkDirConfig>,
    #[serde(default)]
    command_allowlist: Vec<String>,
    #[serde(default)]
    allow_elevation: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum WorkDirConfig {
    One(PathBuf),
    Many(Vec<PathBuf>),
}

#[derive(Debug)]
enum CommandRule {
    Prefix(String),
    Regex(Regex),
}

#[derive(Debug, Default)]
pub struct RuntimePolicy {
    work_dirs: Vec<PathBuf>,
    default_work_dir: Option<PathBuf>,
    command_allowlist: Vec<CommandRule>,
    allow_elevation: bool,
}

impl RuntimePolicy {
    pub fn load(path: Option<&Path>) -> Result<(Self, Option<PathBuf>), String> {
        if let Some(path) = path {
            let path = path.to_path_buf();
            let text = fs::read_to_string(&path)
                .map_err(|err| format!("failed to read config {}: {err}", path.display()))?;
            return Ok((Self::parse_config(&path, &text)?, Some(path)));
        }

        let current_dir = std::env::current_dir()
            .map_err(|err| format!("failed to locate current working directory: {err}"))?;
        let exe =
            std::env::current_exe().map_err(|err| format!("failed to locate lcr.exe: {err}"))?;
        Self::load_default(&current_dir, &exe)
    }

    fn load_default(current_dir: &Path, exe: &Path) -> Result<(Self, Option<PathBuf>), String> {
        for path in [current_dir.join("lcr.toml"), exe.with_file_name("lcr.toml")] {
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(format!("failed to read config {}: {err}", path.display()));
                }
            };
            return Ok((Self::parse_config(&path, &text)?, Some(path)));
        }
        Ok((Self::default(), None))
    }

    fn parse_config(path: &Path, text: &str) -> Result<Self, String> {
        let config: FileConfig = toml::from_str(text)
            .map_err(|err| format!("invalid config {}: {err}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        let (work_dirs, has_default) = match config.work_dir {
            None => (Vec::new(), false),
            Some(WorkDirConfig::One(value)) => (vec![value], true),
            Some(WorkDirConfig::Many(values)) if values.is_empty() => {
                return Err("work_dir array cannot be empty".to_string());
            }
            Some(WorkDirConfig::Many(values)) => (values, false),
        };
        let work_dirs = work_dirs
            .into_iter()
            .map(|value| {
                let value = if value.is_absolute() {
                    value
                } else {
                    base.join(value)
                };
                let resolved = fs::canonicalize(&value)
                    .map_err(|err| format!("invalid work_dir {}: {err}", value.display()))?;
                if !resolved.is_dir() {
                    return Err(format!(
                        "work_dir must be an existing directory: {}",
                        value.display()
                    ));
                }
                Ok(resolved)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let default_work_dir = has_default.then(|| work_dirs[0].clone());

        let command_allowlist = config
            .command_allowlist
            .into_iter()
            .map(parse_command_rule)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            work_dirs,
            default_work_dir,
            command_allowlist,
            allow_elevation: config.allow_elevation,
        })
    }

    pub fn prepare_exec(&self, request: &mut ExecRequest) -> Result<(), String> {
        if request.require_admin && !self.allow_elevation {
            return Err(
                "administrator elevation is disabled; set allow_elevation = true in lcr.toml"
                    .to_string(),
            );
        }
        if !self.command_allowlist.is_empty() {
            if request.command.is_some() {
                return Err(format!(
                    "command is disabled when command_allowlist is configured; use program and args; {}",
                    self.allowed_programs_message()
                ));
            }
            let program = request.program.as_deref().ok_or_else(|| {
                format!(
                    "program is required when command_allowlist is configured; {}",
                    self.allowed_programs_message()
                )
            })?;
            if let Some(interpreter) = forbidden_program_interpreter(program) {
                return Err(format!(
                    "program is a forbidden command interpreter in allowlist mode: {interpreter}; forbidden interpreters: [{}]; {}",
                    FORBIDDEN_PROGRAM_INTERPRETERS.join(", "),
                    self.allowed_programs_message()
                ));
            }
            let candidate = command_candidate(request).expect("allowlist requests require program");
            if !self
                .command_allowlist
                .iter()
                .any(|rule| rule.matches(&candidate))
            {
                return Err(format!(
                    "program invocation is not allowed by command_allowlist; {}",
                    self.allowed_programs_message()
                ));
            }
        }
        if self.work_dirs.is_empty() {
            return self.pin_elevated_allowlisted_program(request);
        }
        let cwd = match request.cwd.as_deref() {
            Some(cwd) => self.resolve_existing(Path::new(cwd))?,
            None => {
                let path = self.default_work_dir.clone().ok_or_else(|| {
                    format!(
                        "cwd is required when work_dir is configured as an array; {}",
                        self.allowed_work_dirs_message()
                    )
                })?;
                self.guard_existing(path)?
            }
        };
        if !cwd.path.is_dir() {
            return Err(format!(
                "cwd must be an existing directory; {}",
                self.allowed_work_dirs_message()
            ));
        }
        request.cwd = Some(cwd.path.to_string_lossy().into_owned());
        request.cwd_guard = Some(cwd._guard);
        self.resolve_allowlisted_batch_program(request)?;
        self.pin_elevated_allowlisted_program(request)
    }

    pub fn resolve_download(&self, path: &Path) -> Result<GuardedDownload, String> {
        if self.work_dirs.is_empty() {
            let file = File::open(path)
                .map_err(|err| format!("failed to open download {}: {err}", path.display()))?;
            Ok(GuardedDownload {
                path: path.to_path_buf(),
                file,
                _guard: PathGuard::default(),
            })
        } else {
            let guarded = self.resolve_existing(path)?;
            let file = open_file_without_delete_sharing(&guarded.path).map_err(|err| {
                format!("failed to open download {}: {err}", guarded.path.display())
            })?;
            let actual = opened_file_path(&file, &guarded.path)?;
            self.ensure_within_root(&actual)?;
            Ok(GuardedDownload {
                path: actual,
                file,
                _guard: guarded._guard,
            })
        }
    }

    pub fn resolve_upload(&self, path: &Path) -> Result<GuardedPath, String> {
        if self.work_dirs.is_empty() {
            return Ok(GuardedPath {
                path: path.to_path_buf(),
                _guard: PathGuard::default(),
            });
        }
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.default_work_dir
                .as_ref()
                .ok_or_else(|| self.relative_path_with_multiple_roots())?
                .join(path)
        };
        let file_name = joined
            .file_name()
            .ok_or_else(|| "destination must include a file name".to_string())?;
        let parent = joined.parent().unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|err| {
            format!(
                "destination directory does not exist: {err}; {}",
                self.allowed_work_dirs_message()
            )
        })?;
        self.ensure_within_root(&parent)?;
        let parent = self.guard_existing(parent)?;
        Ok(GuardedPath {
            path: parent.path.join(file_name),
            _guard: parent._guard,
        })
    }

    fn resolve_existing(&self, path: &Path) -> Result<GuardedPath, String> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.default_work_dir
                .as_ref()
                .ok_or_else(|| self.relative_path_with_multiple_roots())?
                .join(path)
        };
        let resolved = fs::canonicalize(&joined).map_err(|err| {
            format!(
                "path does not exist: {}: {err}; {}",
                joined.display(),
                self.allowed_work_dirs_message()
            )
        })?;
        self.ensure_within_root(&resolved)?;
        self.guard_existing(resolved)
    }

    fn resolve_allowlisted_batch_program(&self, request: &mut ExecRequest) -> Result<(), String> {
        if self.command_allowlist.is_empty() || self.work_dirs.is_empty() {
            return Ok(());
        }
        let Some(program) = request.program.as_deref() else {
            return Ok(());
        };
        let path = Path::new(program);
        let mut components = path.components();
        let Some(Component::Normal(_)) = components.next() else {
            return Ok(());
        };
        if components.next().is_some()
            || !path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            })
        {
            return Ok(());
        }

        let cwd = request
            .cwd
            .as_deref()
            .expect("configured work_dir always resolves cwd");
        let resolved = fs::canonicalize(Path::new(cwd).join(path)).map_err(|err| {
            format!(
                "allowlisted batch program does not exist in cwd: {}: {err}",
                path.display()
            )
        })?;
        self.ensure_within_root(&resolved)?;
        let guarded = self.guard_existing(resolved)?;
        if !guarded.path.is_file() {
            return Err(format!(
                "allowlisted batch program is not a file in cwd: {}",
                path.display()
            ));
        }
        request.program = Some(guarded.path.to_string_lossy().into_owned());
        request.program_guard = Some(guarded._guard);
        Ok(())
    }

    fn pin_elevated_allowlisted_program(&self, request: &mut ExecRequest) -> Result<(), String> {
        if !request.require_admin
            || self.command_allowlist.is_empty()
            || request.program_guard.is_some()
        {
            return Ok(());
        }
        let program = request
            .program
            .as_deref()
            .expect("allowlisted requests require program");
        let path = Path::new(program);
        if !path.is_absolute() {
            return Err(
                "elevated allowlisted programs must resolve to an absolute path; configure work_dir for bare .cmd/.bat names or send an absolute program path"
                    .to_string(),
            );
        }
        let resolved = fs::canonicalize(path).map_err(|err| {
            format!("elevated allowlisted program does not exist: {program}: {err}")
        })?;
        if !resolved.is_file() {
            return Err(format!(
                "elevated allowlisted program is not a file: {program}"
            ));
        }
        let guard = PathGuard::lock(&resolved).map_err(|err| {
            format!(
                "failed to lock elevated program {}: {err}",
                resolved.display()
            )
        })?;
        let verified = fs::canonicalize(&resolved).map_err(|err| {
            format!(
                "elevated program changed during validation: {}: {err}",
                resolved.display()
            )
        })?;
        request.program = Some(verified.to_string_lossy().into_owned());
        request.program_guard = Some(guard);
        Ok(())
    }

    fn guard_existing(&self, path: PathBuf) -> Result<GuardedPath, String> {
        let guard = PathGuard::lock(&path)
            .map_err(|err| format!("failed to lock path {}: {err}", path.display()))?;
        let resolved = fs::canonicalize(&path)
            .map_err(|err| format!("path changed during validation: {}: {err}", path.display()))?;
        self.ensure_within_root(&resolved)?;
        Ok(GuardedPath {
            path: resolved,
            _guard: guard,
        })
    }

    fn ensure_within_root(&self, path: &Path) -> Result<(), String> {
        if self
            .work_dirs
            .iter()
            .any(|root| path_starts_with(path, root))
        {
            Ok(())
        } else {
            Err(format!(
                "path is outside configured work_dir: {}; {}",
                path.display(),
                self.allowed_work_dirs_message()
            ))
        }
    }

    fn allowed_programs_message(&self) -> String {
        let rules = self
            .command_allowlist
            .iter()
            .map(CommandRule::description)
            .collect::<Vec<_>>();
        format!("allowed program rules: [{}]", rules.join(", "))
    }

    fn allowed_work_dirs_message(&self) -> String {
        let paths = self
            .work_dirs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        format!("allowed work directories: [{}]", paths.join(", "))
    }

    fn relative_path_with_multiple_roots(&self) -> String {
        format!(
            "relative paths are not allowed when work_dir is an array; use an absolute path; {}",
            self.allowed_work_dirs_message()
        )
    }
}

impl PathGuard {
    #[cfg(windows)]
    fn lock(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, GetFileInformationByHandle,
        };

        let mut handles = Vec::new();
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::RootDir | Component::Normal(_)) {
                continue;
            }
            let handle = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&current)?;
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as _, &mut info) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("reparse points are not allowed: {}", current.display()),
                ));
            }
            handles.push(handle);
        }
        Ok(Self { _handles: handles })
    }

    #[cfg(not(windows))]
    fn lock(_path: &Path) -> std::io::Result<Self> {
        Ok(Self::default())
    }
}

#[cfg(windows)]
fn open_file_without_delete_sharing(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
}

#[cfg(not(windows))]
fn open_file_without_delete_sharing(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

#[cfg(windows)]
fn opened_file_path(file: &File, _requested: &Path) -> Result<PathBuf, String> {
    use std::{
        ffi::OsString,
        os::windows::{ffi::OsStringExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

    let handle = file.as_raw_handle() as _;
    let required = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
    if required == 0 {
        return Err(format!(
            "failed to resolve opened download: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0u16; required as usize + 1];
    let written =
        unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if written == 0 || written as usize >= buffer.len() {
        return Err(format!(
            "failed to resolve opened download: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(
        &buffer[..written as usize],
    )))
}

#[cfg(not(windows))]
fn opened_file_path(file: &File, requested: &Path) -> Result<PathBuf, String> {
    let _ = file;
    fs::canonicalize(requested).map_err(|err| format!("failed to resolve opened download: {err}"))
}

#[cfg(windows)]
fn path_starts_with(path: &Path, root: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

    let mut path_components = path.components();
    root.components().all(|root_component| {
        let Some(path_component) = path_components.next() else {
            return false;
        };
        let left = path_component.as_os_str().encode_wide().collect::<Vec<_>>();
        let right = root_component.as_os_str().encode_wide().collect::<Vec<_>>();
        (unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            )
        }) == CSTR_EQUAL
    })
}

#[cfg(not(windows))]
fn path_starts_with(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

fn command_candidate(request: &ExecRequest) -> Option<String> {
    if let Some(command) = &request.command {
        return Some(command.clone());
    }
    let program = request.program.as_ref()?;
    let mut candidate = program.clone();
    for argument in &request.args {
        candidate.push(' ');
        candidate
            .push_str(&serde_json::to_string(argument).expect("serializing a string cannot fail"));
    }
    Some(candidate)
}

fn forbidden_program_interpreter(program: &str) -> Option<&'static str> {
    let name = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    FORBIDDEN_PROGRAM_INTERPRETERS
        .iter()
        .copied()
        .find(|candidate| *candidate == name)
        .or_else(|| {
            let suffix = name.strip_prefix("python")?;
            (!suffix.is_empty()
                && suffix
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.'))
            .then_some("python")
        })
}

impl CommandRule {
    fn matches(&self, command: &str) -> bool {
        match self {
            Self::Prefix(prefix) => command.to_lowercase().starts_with(&prefix.to_lowercase()),
            Self::Regex(regex) => regex.is_match(command.as_bytes()),
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Prefix(prefix) => prefix.clone(),
            Self::Regex(regex) => format!("/{}/", regex.as_str()),
        }
    }
}

fn parse_command_rule(value: String) -> Result<CommandRule, String> {
    if value.len() >= 2 && value.starts_with('/') && value.ends_with('/') {
        let pattern = &value[1..value.len() - 1];
        if pattern.is_empty() {
            return Err("command_allowlist regex cannot be empty".to_string());
        }
        return RegexBuilder::new(pattern)
            .unicode(false)
            .case_insensitive(true)
            .build()
            .map(CommandRule::Regex)
            .map_err(|err| format!("invalid command_allowlist regex {value:?}: {err}"));
    }
    if value.is_empty() {
        return Err("command_allowlist prefix cannot be empty".to_string());
    }
    Ok(CommandRule::Prefix(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_prefix_and_regex_are_case_insensitive() {
        let prefix = parse_command_rule("echo ".to_string()).unwrap();
        let regex = parse_command_rule("/^git (status|diff)$/".to_string()).unwrap();
        assert!(prefix.matches("ECHO hello"));
        assert!(!prefix.matches("xecho hello"));
        assert!(regex.matches("GIT STATUS"));
        assert!(!regex.matches("git push"));
    }

    #[test]
    fn allowlist_regex_wildcards_match_unicode_arguments_as_utf8_bytes() {
        let regex = parse_command_rule(r#"/^tool \".*\"$/"#.to_string()).unwrap();
        assert!(regex.matches(r#"tool "中文参数""#));
    }

    #[test]
    fn configured_paths_cannot_escape_the_root() {
        let root = fs::canonicalize(".").unwrap();
        let policy = RuntimePolicy {
            work_dirs: vec![root.clone()],
            default_work_dir: Some(root.clone()),
            command_allowlist: Vec::new(),
            allow_elevation: false,
        };
        assert_eq!(policy.resolve_existing(Path::new(".")).unwrap().path, root);
        let outside = root.parent().unwrap();
        assert!(policy.resolve_existing(outside).is_err());
    }

    #[test]
    fn relative_paths_are_resolved_from_the_root() {
        let root = std::env::temp_dir().join(format!(
            "lcr-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("file.bin");
        fs::write(&file, b"test").unwrap();
        let policy = RuntimePolicy {
            work_dirs: vec![fs::canonicalize(&root).unwrap()],
            default_work_dir: Some(fs::canonicalize(&root).unwrap()),
            command_allowlist: Vec::new(),
            allow_elevation: false,
        };
        assert_eq!(
            policy
                .resolve_download(Path::new("nested/file.bin"))
                .unwrap()
                .path,
            fs::canonicalize(&file).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loads_an_explicit_config_and_resolves_its_relative_work_dir() {
        let root = std::env::temp_dir().join(format!(
            "lcr-config-load-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let work_dir = root.join("workspace");
        fs::create_dir_all(&work_dir).unwrap();
        let config_path = root.join("custom.toml");
        fs::write(
            &config_path,
            "work_dir = 'workspace'\ncommand_allowlist = ['echo ', '/^cd$/']\n",
        )
        .unwrap();

        let (policy, loaded_path) = RuntimePolicy::load(Some(&config_path)).unwrap();
        assert_eq!(loaded_path, Some(config_path));
        assert_eq!(policy.work_dirs, vec![fs::canonicalize(&work_dir).unwrap()]);
        assert_eq!(
            policy.default_work_dir,
            Some(fs::canonicalize(work_dir).unwrap())
        );
        assert_eq!(policy.command_allowlist.len(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_config_loads_in_order_and_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "lcr-config-discovery-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let current_dir = root.join("current");
        let exe_dir = root.join("bin");
        fs::create_dir_all(&current_dir).unwrap();
        fs::create_dir_all(&exe_dir).unwrap();
        let current_config = current_dir.join("lcr.toml");
        let exe_config = exe_dir.join("lcr.toml");
        let exe = exe_dir.join("lcr.exe");

        fs::write(&exe_config, "command_allowlist = ['exe-rule']").unwrap();
        let (exe_policy, loaded_path) = RuntimePolicy::load_default(&current_dir, &exe).unwrap();
        assert_eq!(loaded_path, Some(exe_config));
        assert_eq!(exe_policy.command_allowlist.len(), 1);

        fs::write(&current_config, "command_allowlist = ['cwd-rule']").unwrap();
        let (cwd_policy, loaded_path) = RuntimePolicy::load_default(&current_dir, &exe).unwrap();
        assert_eq!(loaded_path, Some(current_config.clone()));
        assert_eq!(cwd_policy.command_allowlist.len(), 1);

        fs::remove_file(&current_config).unwrap();
        fs::create_dir(&current_config).unwrap();
        let error = RuntimePolicy::load_default(&current_dir, &exe).unwrap_err();
        assert!(error.contains("failed to read config"));
        assert!(error.contains(&current_config.to_string_lossy().to_string()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_an_invalid_allowlist_regex() {
        let error = parse_command_rule("/[unterminated/".to_string()).unwrap_err();
        assert!(error.contains("invalid command_allowlist regex"));
    }

    #[test]
    fn rejects_unicode_allowlist_regex_features() {
        let error = parse_command_rule(r"/^\p{Greek}+$/".to_string()).unwrap_err();
        assert!(error.contains("invalid command_allowlist regex"));
    }

    #[test]
    fn work_dir_array_requires_cwd_and_accepts_each_root() {
        let root = fs::canonicalize(".").unwrap();
        let other = fs::canonicalize(std::env::temp_dir()).unwrap();
        let policy = RuntimePolicy {
            work_dirs: vec![root.clone(), other.clone()],
            default_work_dir: None,
            command_allowlist: Vec::new(),
            allow_elevation: false,
        };
        let mut missing = serde_json::from_str::<ExecRequest>(r#"{"command":"echo hi"}"#).unwrap();
        let error = policy.prepare_exec(&mut missing).unwrap_err();
        assert!(error.contains("cwd is required when work_dir is configured as an array"));
        assert!(error.contains(&root.to_string_lossy().to_string()));
        assert!(error.contains(&other.to_string_lossy().to_string()));
        let mut explicit = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "command": "echo hi",
            "cwd": other,
        }))
        .unwrap();
        policy.prepare_exec(&mut explicit).unwrap();
        assert_eq!(explicit.cwd, Some(other.to_string_lossy().into_owned()));
        assert!(policy.resolve_download(Path::new("relative.bin")).is_err());
    }

    #[test]
    fn loads_a_work_dir_array_without_a_default() {
        let config: FileConfig = toml::from_str("work_dir = ['one', 'two']").unwrap();
        assert!(matches!(config.work_dir, Some(WorkDirConfig::Many(values)) if values.len() == 2));
    }

    #[test]
    fn repository_example_is_valid_toml() {
        let _: FileConfig = toml::from_str(include_str!("../lcr.toml.example")).unwrap();
    }

    #[test]
    fn elevation_requires_an_explicit_config_opt_in() {
        let mut blocked = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "command": "echo blocked",
            "require_admin": true
        }))
        .unwrap();
        let error = RuntimePolicy::default()
            .prepare_exec(&mut blocked)
            .unwrap_err();
        assert!(error.contains("allow_elevation = true"));

        let policy = RuntimePolicy {
            allow_elevation: true,
            ..RuntimePolicy::default()
        };
        let mut allowed = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "command": "echo allowed",
            "require_admin": true
        }))
        .unwrap();
        policy.prepare_exec(&mut allowed).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn work_dir_boundary_comparison_is_case_insensitive_on_windows() {
        assert!(path_starts_with(
            Path::new(r"C:\WORKSPACE\Child"),
            Path::new(r"c:\workspace")
        ));
        assert!(!path_starts_with(
            Path::new(r"C:\WORKSPACE-other"),
            Path::new(r"c:\workspace")
        ));
    }

    #[test]
    fn direct_program_allowlist_candidate_includes_quoted_arguments() {
        let request = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "program": "git.exe",
            "args": ["status", "path with spaces", "quote\"value"]
        }))
        .unwrap();
        assert_eq!(
            command_candidate(&request).unwrap(),
            r#"git.exe "status" "path with spaces" "quote\"value""#
        );
    }

    #[test]
    fn direct_program_arguments_participate_in_allowlist_matching() {
        let policy = RuntimePolicy {
            work_dirs: Vec::new(),
            default_work_dir: None,
            command_allowlist: vec![
                parse_command_rule(r#"/^git\.exe "status"$/"#.to_string()).unwrap(),
            ],
            allow_elevation: false,
        };
        let mut allowed = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "program": "git.exe",
            "args": ["status"]
        }))
        .unwrap();
        policy.prepare_exec(&mut allowed).unwrap();
        let mut blocked = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "program": "git.exe",
            "args": ["push"]
        }))
        .unwrap();
        let error = policy.prepare_exec(&mut blocked).unwrap_err();
        assert!(error.contains("program invocation is not allowed by command_allowlist"));
        assert!(error.contains(r#"/^git\.exe "status"$/"#));
    }

    #[test]
    fn allowlisted_batch_filename_is_resolved_from_the_work_dir() {
        let root = std::env::temp_dir().join(format!(
            "lcr-config-batch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let batch = root.join("WimBuilder.cmd");
        fs::write(&batch, "@echo off\r\n").unwrap();
        let root = fs::canonicalize(&root).unwrap();
        let policy = RuntimePolicy {
            work_dirs: vec![root.clone()],
            default_work_dir: Some(root.clone()),
            command_allowlist: vec![parse_command_rule("WimBuilder.cmd".to_string()).unwrap()],
            allow_elevation: true,
        };
        let mut request = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "program": "WimBuilder.cmd",
            "args": ["build"],
            "require_admin": true
        }))
        .unwrap();

        policy.prepare_exec(&mut request).unwrap();
        assert_eq!(
            request.program,
            Some(
                fs::canonicalize(&batch)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(request.cwd, Some(root.to_string_lossy().into_owned()));

        drop(request);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn elevated_allowlist_rejects_an_unresolved_bare_executable() {
        let policy = RuntimePolicy {
            work_dirs: Vec::new(),
            default_work_dir: None,
            command_allowlist: vec![parse_command_rule("git.exe".to_string()).unwrap()],
            allow_elevation: true,
        };
        let mut request = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "program": "git.exe",
            "require_admin": true
        }))
        .unwrap();

        let error = policy.prepare_exec(&mut request).unwrap_err();
        assert!(error.contains("must resolve to an absolute path"));
    }

    #[test]
    fn allowlist_disables_command_requests() {
        let policy = RuntimePolicy {
            work_dirs: Vec::new(),
            default_work_dir: None,
            command_allowlist: vec![parse_command_rule("echo ".to_string()).unwrap()],
            allow_elevation: false,
        };
        let mut request = serde_json::from_value::<ExecRequest>(serde_json::json!({
            "command": "echo allowed text"
        }))
        .unwrap();
        let error = policy.prepare_exec(&mut request).unwrap_err();
        assert!(error.contains("command is disabled when command_allowlist is configured"));
        assert!(error.contains("allowed program rules: [echo ]"));
    }

    #[test]
    fn allowlist_rejects_direct_command_interpreters() {
        let policy = RuntimePolicy {
            work_dirs: Vec::new(),
            default_work_dir: None,
            command_allowlist: vec![parse_command_rule("/".to_string()).unwrap()],
            allow_elevation: false,
        };
        for program in [
            r"C:\Windows\System32\CMD.EXE",
            "pwsh.exe",
            "powershell.exe",
            "python3.13.exe",
            "node.exe",
            "wscript.exe",
        ] {
            let mut request = serde_json::from_value::<ExecRequest>(serde_json::json!({
                "program": program,
            }))
            .unwrap();
            let error = policy.prepare_exec(&mut request).unwrap_err();
            assert!(
                error.contains("program is a forbidden command interpreter in allowlist mode"),
                "unexpected error for {program}: {error}"
            );
        }
    }
}
