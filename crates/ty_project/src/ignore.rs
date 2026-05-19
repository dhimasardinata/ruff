use ruff_db::system::{System, SystemPath};

/// Returns `true` if `path` would be filtered out by ignore files during a project walk.
///
/// This mirrors the ignore sources enabled by the walker when
/// `src.respect-ignore-files` is enabled: `.ignore`, repository-scoped
/// `.gitignore`, `.git/info/exclude`, and the global gitignore. The precedence
/// order follows
/// [`Ignore::matched_ignore`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L431).
pub(crate) fn is_path_ignored_by_ignore_files(
    system: &dyn System,
    path: &SystemPath,
    is_directory: bool,
) -> bool {
    IgnoreCheck {
        system,
        path,
        is_directory,
    }
    .is_ignored()
}

struct IgnoreCheck<'a> {
    system: &'a dyn System,
    /// The path to check for ignoredness.
    path: &'a SystemPath,
    /// Whether `path` is checked as a directory or file.
    is_directory: bool,
}

impl IgnoreCheck<'_> {
    fn is_ignored(&self) -> bool {
        if let Some(is_ignored) = self.ignored_by_parent_ignore_file(".ignore", None) {
            return is_ignored;
        }

        let Some(git_root) = self.git_repository_root() else {
            return false;
        };

        if let Some(is_ignored) = self.ignored_by_parent_ignore_file(".gitignore", Some(git_root)) {
            return is_ignored;
        }

        if let Some(is_ignored) = self.ignored_by_git_exclude(git_root) {
            return is_ignored;
        }

        self.ignored_by_global_gitignore().unwrap_or(false)
    }

    /// Mirrors the parent-directory matching performed by
    /// [`Ignore::matched_ignore`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L431).
    fn ignored_by_parent_ignore_file(
        &self,
        file_name: &str,
        stop_at: Option<&SystemPath>,
    ) -> Option<bool> {
        let parent = self.path.parent()?;

        parent
            .ancestors()
            .take_while(|directory| stop_at.is_none_or(|stop_at| directory.starts_with(stop_at)))
            .find_map(|directory| {
                let matcher =
                    ignore_file_matcher(self.system, &directory.join(file_name), directory);
                ignored_by_match(
                    &matcher
                        .matched_path_or_any_parents(self.path.as_std_path(), self.is_directory),
                )
            })
    }

    /// Mirrors the `.git/info/exclude` matcher setup in
    /// [`Ignore::add_child_path`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L258).
    fn ignored_by_git_exclude(&self, git_root: &SystemPath) -> Option<bool> {
        let git_dir = git_root.join(".git");
        self.system.is_directory(&git_dir).then_some(())?;

        let exclude_path = git_dir.join("info/exclude");
        let matcher = ignore_file_matcher(self.system, &exclude_path, git_root);

        ignored_by_match(
            &matcher.matched_path_or_any_parents(self.path.as_std_path(), self.is_directory),
        )
    }

    /// Mirrors global gitignore matcher construction in
    /// [`IgnoreBuilder::build_with_cwd`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L655)
    /// and
    /// [`GitignoreBuilder::build_global`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/gitignore.rs#L364).
    fn ignored_by_global_gitignore(&self) -> Option<bool> {
        let cwd = self.system.current_directory();
        let (matcher, error) =
            ignore::gitignore::GitignoreBuilder::new(cwd.as_std_path()).build_global();

        if let Some(err) = error {
            tracing::warn!("Failed to read global gitignore: {err}");
        }

        if self.path.starts_with(cwd) {
            ignored_by_match(
                &matcher.matched_path_or_any_parents(self.path.as_std_path(), self.is_directory),
            )
        } else {
            ignored_by_match(&matcher.matched(self.path.as_std_path(), self.is_directory))
        }
    }

    /// Mirrors the repository-presence checks in
    /// [`Ignore::add_parents`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L225-L230)
    /// and
    /// [`Ignore::add_child_path`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L259-L267).
    fn git_repository_root(&self) -> Option<&SystemPath> {
        self.path.ancestors().find(|ancestor| {
            self.system.path_exists(&ancestor.join(".git"))
                || self.system.path_exists(&ancestor.join(".jj"))
        })
    }
}

/// Mirrors ignore-file matcher construction in
/// [`create_gitignore`](https://github.com/BurntSushi/ripgrep/blob/57c190d56eedac90c061a238b63dbfed434fee50/crates/ignore/src/dir.rs#L847).
fn ignore_file_matcher(
    system: &dyn System,
    ignore_file: &SystemPath,
    root: &SystemPath,
) -> ignore::gitignore::Gitignore {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root.as_std_path());

    match system.read_to_string(ignore_file) {
        Ok(contents) => {
            const UTF8_BOM: &str = "\u{feff}";
            let contents = contents.trim_start_matches(UTF8_BOM);

            for (line_number, line) in contents.lines().enumerate() {
                let result = builder.add_line(Some(ignore_file.as_std_path().to_path_buf()), line);

                if let Some(err) = result.err() {
                    tracing::warn!(
                        "Failed to parse ignore file `{ignore_file}` at line {}: {err}",
                        line_number + 1
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!("Failed to read ignore file `{ignore_file}`: {error}"),
    }

    match builder.build() {
        Ok(matcher) => matcher,
        Err(error) => {
            tracing::warn!("Failed to build ignore matcher for `{ignore_file}`: {error}");
            ignore::gitignore::Gitignore::empty()
        }
    }
}

fn ignored_by_match<T>(match_result: &ignore::Match<T>) -> Option<bool> {
    match match_result {
        ignore::Match::None => None,
        ignore::Match::Ignore(_) => Some(true),
        ignore::Match::Whitelist(_) => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::system::{InMemorySystem, System};

    use super::is_path_ignored_by_ignore_files;

    #[test]
    fn nested_ignore_file_allowlist_overrides_parent_ignore_file() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("pkg/keep.py");

        system
            .fs()
            .write_files_all([
                (root.join(".ignore"), "pkg/keep.py\n"),
                (root.join("pkg/.ignore"), "!keep.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(!is_path_ignored_by_ignore_files(&system, &path, false));
    }

    #[test]
    fn ignore_file_takes_precedence_over_gitignore_allowlist() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("ignored.py");

        system
            .fs()
            .write_files_all([
                (root.join(".git/HEAD"), "ref: refs/heads/main\n"),
                (root.join(".ignore"), "ignored.py\n"),
                (root.join(".gitignore"), "!ignored.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(is_path_ignored_by_ignore_files(&system, &path, false));
    }

    #[test]
    fn ignore_file_allowlist_takes_precedence_over_gitignore_ignore() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("included.py");

        system
            .fs()
            .write_files_all([
                (root.join(".git/HEAD"), "ref: refs/heads/main\n"),
                (root.join(".ignore"), "!included.py\n"),
                (root.join(".gitignore"), "included.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(!is_path_ignored_by_ignore_files(&system, &path, false));
    }

    #[test]
    fn ignore_file_strips_utf8_bom() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("ignored.py");

        system
            .fs()
            .write_files_all([
                (root.join(".ignore"), "\u{feff}ignored.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(is_path_ignored_by_ignore_files(&system, &path, false));
    }

    #[test]
    fn gitignore_requires_repository() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("ignored.py");

        system
            .fs()
            .write_files_all([
                (root.join(".gitignore"), "ignored.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(!is_path_ignored_by_ignore_files(&system, &path, false));
    }

    #[test]
    fn git_exclude_ignores_files() {
        let system = InMemorySystem::new("/project".into());
        let root = system.current_directory().to_path_buf();
        let path = root.join("ignored.py");

        system
            .fs()
            .write_files_all([
                (root.join(".git/HEAD"), "ref: refs/heads/main\n"),
                (root.join(".git/info/exclude"), "ignored.py\n"),
                (path.clone(), ""),
            ])
            .unwrap();

        assert!(is_path_ignored_by_ignore_files(&system, &path, false));
    }
}
