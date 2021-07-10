//! Operations on the Git repository. This module exists for a few reasons:
//!
//! - To ensure that every call to a Git operation has an associated `context`
//! for use with `Try`.
//! - To improve the interface in some cases. In particular, some operations in
//! `git2` return an `Error` with code `ENOTFOUND`, but we should really return
//! an `Option` in those cases.
//! - To make it possible to audit all the Git operations carried out in the
//! codebase.
//! - To collect some different helper Git functions.

use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::Context;
use cursive::theme::BaseColor;
use cursive::utils::markup::StyledString;
use fn_error_context::context;
use os_str_bytes::{OsStrBytes, OsStringBytes};

use crate::core::config::get_main_branch_name;
use crate::core::metadata::{render_commit_metadata, CommitMessageProvider, CommitOidProvider};

use super::Config;

/// Convert a `git2::Error` into an `anyhow::Error` with an auto-generated message.
pub fn wrap_git_error(error: git2::Error) -> anyhow::Error {
    anyhow::anyhow!("Git error {:?}: {}", error.code(), error.message())
}

/// Wrapper around `git2::Repository`.
pub struct Repo {
    inner: git2::Repository,
}

/// Represents the ID of a Git object.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NonZeroOid {
    inner: git2::Oid,
}

impl Display for NonZeroOid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.inner)
    }
}

impl TryFrom<MaybeZeroOid> for NonZeroOid {
    type Error = anyhow::Error;

    fn try_from(value: MaybeZeroOid) -> Result<Self, Self::Error> {
        match value {
            MaybeZeroOid::NonZero(non_zero_oid) => Ok(non_zero_oid),
            MaybeZeroOid::Zero => anyhow::bail!("Expected a non-zero OID"),
        }
    }
}

impl TryFrom<OsString> for NonZeroOid {
    type Error = anyhow::Error;

    fn try_from(value: OsString) -> Result<Self, Self::Error> {
        let value: &OsStr = &value;
        value.try_into()
    }
}

impl TryFrom<&OsStr> for NonZeroOid {
    type Error = anyhow::Error;

    fn try_from(value: &OsStr) -> Result<Self, Self::Error> {
        let oid: MaybeZeroOid = value.try_into()?;
        match oid {
            MaybeZeroOid::Zero => anyhow::bail!("OID was zero, but expected to be non-zero"),
            MaybeZeroOid::NonZero(oid) => Ok(oid),
        }
    }
}

impl FromStr for NonZeroOid {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let oid: MaybeZeroOid = value.parse()?;
        match oid {
            MaybeZeroOid::NonZero(non_zero_oid) => Ok(non_zero_oid),
            MaybeZeroOid::Zero => anyhow::bail!("Expected a non-zero OID, but got: {:?}", value),
        }
    }
}

fn make_non_zero_oid(oid: git2::Oid) -> NonZeroOid {
    assert_ne!(oid, git2::Oid::zero());
    NonZeroOid { inner: oid }
}

/// Represents an OID which may be zero or non-zero. This exists because Git
/// often represents the absence of an object using the zero OID. We want to
/// statically check for those cases by using a more descriptive type.
///
/// This type is isomorphic to `Option<NonZeroOid>`. It should be used primarily
/// when converting to and from string representations of OID values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MaybeZeroOid {
    /// The zero OID (i.e. 40 `0`s).
    Zero,

    /// A non-zero OID.
    NonZero(NonZeroOid),
}

impl std::fmt::Debug for MaybeZeroOid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl Display for MaybeZeroOid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let zero = git2::Oid::zero();
        write!(
            f,
            "{:?}",
            match self {
                MaybeZeroOid::NonZero(NonZeroOid { inner }) => inner,
                MaybeZeroOid::Zero => &zero,
            }
        )
    }
}

impl FromStr for MaybeZeroOid {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.parse() {
            Ok(oid) if oid == git2::Oid::zero() => Ok(MaybeZeroOid::Zero),
            Ok(oid) => Ok(MaybeZeroOid::NonZero(NonZeroOid { inner: oid })),
            Err(err) => Err(wrap_git_error(err))
                .with_context(|| format!("Could not parse OID from string: {:?}", s)),
        }
    }
}

impl TryFrom<&OsStr> for MaybeZeroOid {
    type Error = anyhow::Error;

    fn try_from(value: &OsStr) -> Result<Self, Self::Error> {
        match value.to_str() {
            None => anyhow::bail!("OID value was not a simple ASCII value: {:?}", value),
            Some(value) => value.parse(),
        }
    }
}

impl TryFrom<OsString> for MaybeZeroOid {
    type Error = anyhow::Error;

    fn try_from(value: OsString) -> Result<Self, Self::Error> {
        MaybeZeroOid::try_from(value.as_os_str())
    }
}

impl From<NonZeroOid> for MaybeZeroOid {
    fn from(oid: NonZeroOid) -> Self {
        Self::NonZero(oid)
    }
}

impl From<Option<NonZeroOid>> for MaybeZeroOid {
    fn from(oid: Option<NonZeroOid>) -> Self {
        match oid {
            Some(oid) => MaybeZeroOid::NonZero(oid),
            None => MaybeZeroOid::Zero,
        }
    }
}

impl From<MaybeZeroOid> for Option<NonZeroOid> {
    fn from(oid: MaybeZeroOid) -> Self {
        match oid {
            MaybeZeroOid::Zero => None,
            MaybeZeroOid::NonZero(oid) => Some(oid),
        }
    }
}

/// Information about the current `HEAD` of the repository.
///
/// `HEAD` is typically a symbolic reference, which means that it's a reference
/// that points to another reference. Usually, the other reference is a branch.
/// In this way, you can check out a branch and move the branch (e.g. by
/// committing) and `HEAD` is also effectively updated (you can traverse the
/// pointed-to reference and get the current commit OID).
///
/// There are a couple of interesting edge cases to worry about:
///
/// - `HEAD` is detached. This means that it's pointing directly to a commit and
/// is not a symbolic reference for the time being. This is uncommon in normal
/// Git usage, but very common in `git-branchless` usage.
/// - `HEAD` is unborn. This means that it doesn't even exist yet. This happens
/// when a repository has been freshly initialized, but no commits have been
/// made, for example.
pub struct HeadInfo<'repo> {
    repo: &'repo Repo,

    /// The OID of the commit that `HEAD` points to. If `HEAD` is unborn, then
    /// this is `None`.
    pub oid: Option<NonZeroOid>,

    /// The name of the reference that `HEAD` points to symbolically. If `HEAD`
    /// is detached, then this is `None`.
    reference_name: Option<String>,
}

impl<'repo> HeadInfo<'repo> {
    /// Get the name of the branch, if any. Returns `None` if `HEAD` is
    /// detached.  The `refs/heads/` prefix, if any, is stripped.
    pub fn get_branch_name(&self) -> Option<&str> {
        self.reference_name
            .as_ref()
            .map(|name| match name.strip_prefix("refs/heads/") {
                Some(branch_name) => branch_name,
                None => &name,
            })
    }

    /// Detach `HEAD` by making it point directly to its current OID, rather
    /// than to a branch. If `HEAD` is already detached, this has no effect.
    pub fn detach_head(&self) -> anyhow::Result<()> {
        match self.oid {
            Some(oid) => self
                .repo
                .inner
                .set_head_detached(oid.inner)
                .map_err(wrap_git_error),
            None => {
                log::warn!("Attempted to detach `HEAD` while `HEAD` is unborn");
                Ok(())
            }
        }
    }
}

/// The parsed version of Git.
#[derive(Debug, PartialEq, PartialOrd, Eq)]
pub struct GitVersion(pub isize, pub isize, pub isize);

impl FromStr for GitVersion {
    type Err = anyhow::Error;

    #[context("Parsing Git version from string: {:?}", output)]
    fn from_str(output: &str) -> anyhow::Result<GitVersion> {
        let output = output.trim();
        let words = output.split(' ').collect::<Vec<&str>>();
        let version_str = match &words.as_slice() {
            [_git, _version, version_str, ..] => version_str,
            _ => anyhow::bail!("Could not parse Git version output: {:?}", output),
        };
        match version_str.split('.').collect::<Vec<&str>>().as_slice() {
            [major, minor, patch, ..] => {
                let major = major.parse()?;
                let minor = minor.parse()?;
                let patch = patch.parse()?;
                Ok(GitVersion(major, minor, patch))
            }
            _ => anyhow::bail!("Could not parse Git version string: {}", version_str),
        }
    }
}

impl Repo {
    /// Get the Git repository associated with the given directory.
    #[context("Getting Git repository for directory: {:?}", &path)]
    pub fn from_dir(path: &Path) -> anyhow::Result<Self> {
        let repo = git2::Repository::discover(path).map_err(wrap_git_error)?;
        Ok(Repo { inner: repo })
    }

    /// Get the Git repository associated with the current directory.
    #[context("Getting Git repository for current directory")]
    pub fn from_current_dir() -> anyhow::Result<Self> {
        let path = std::env::current_dir().with_context(|| "Getting working directory")?;
        Repo::from_dir(&path)
    }

    /// Get the path to the `.git` directory for the repository.
    pub fn get_path(&self) -> &Path {
        self.inner.path()
    }

    /// Get the path to the directory inside the `.git` directory which contains
    /// state used for the current rebase (if any).
    pub fn get_rebase_state_dir_path(&self) -> PathBuf {
        self.inner.path().join("rebase-merge")
    }

    /// Get the path to the working copy for this repository. If the repository
    /// is bare (has no working copy), returns `None`.
    pub fn get_working_copy_path(&self) -> Option<&Path> {
        self.inner.workdir()
    }

    /// Get the configuration object for the repository.
    #[context("Looking up config for repo at: {:?}", self.get_path())]
    pub fn get_config(&self) -> anyhow::Result<Config> {
        let config = self
            .inner
            .config()
            .map_err(wrap_git_error)
            .with_context(|| "Creating `git2::Config` object")?;
        Ok(config.into())
    }

    /// Get the connection to the SQLite database for this repository.
    #[context("Getting connection to SQLite database for repository at: {:?}", self.get_path())]
    pub fn get_db_conn(&self) -> anyhow::Result<rusqlite::Connection> {
        let dir = self.inner.path().join("branchless");
        std::fs::create_dir_all(&dir).with_context(|| "Creating .git/branchless dir")?;
        let path = dir.join("db.sqlite3");
        let conn = rusqlite::Connection::open(&path)
            .with_context(|| format!("Opening database connection at {:?}", &path))?;
        Ok(conn)
    }

    /// Get the OID for the repository's `HEAD` reference.
    #[context("Getting `HEAD` info for repository at: {:?}", self.get_path())]
    pub fn get_head_info(&self) -> anyhow::Result<HeadInfo> {
        let head_reference = match self.inner.find_reference("HEAD") {
            Err(err) if err.code() == git2::ErrorCode::NotFound => None,
            Err(err) => return Err(wrap_git_error(err)),
            Ok(result) => Some(result),
        };
        let (head_oid, reference_name) = match &head_reference {
            Some(head_reference) => {
                let head_oid = head_reference
                    .peel_to_commit()
                    .with_context(|| "Resolving `HEAD` reference")?
                    .id();
                let reference_name = match head_reference.kind() {
                    Some(git2::ReferenceType::Direct) => None,
                    Some(git2::ReferenceType::Symbolic) => match head_reference.symbolic_target() {
                        Some(name) => Some(name.to_string()),
                        None => anyhow::bail!(
                            "`HEAD` reference was resolved to OID: {:?}, but its name could not be decoded: {:?}",
                            head_oid, head_reference.name_bytes()
                        ),
                    }
                    None => anyhow::bail!("Unknown `HEAD` reference type")
                };
                (
                    MaybeZeroOid::NonZero(make_non_zero_oid(head_oid)),
                    reference_name,
                )
            }
            None => (MaybeZeroOid::Zero, None),
        };
        Ok(HeadInfo {
            repo: self,
            oid: head_oid.into(),
            reference_name,
        })
    }

    /// Get the `Reference` for the main branch for the repository.
    pub fn get_main_branch_reference(&self) -> anyhow::Result<Reference> {
        let main_branch_name = get_main_branch_name(self)?;
        match self.find_branch(&main_branch_name, git2::BranchType::Local)? {
            Some(branch) => Ok(branch.into_reference()),
            None => match self.find_branch(&main_branch_name, git2::BranchType::Remote)? {
                Some(branch) => Ok(branch.into_reference()),
                None => anyhow::bail!(
                    r"
The main branch {:?} could not be found in your repository.
Either create it, or update the main branch setting by running:

    git config branchless.core.mainBranch <branch>
",
                    get_main_branch_name(self)?,
                ),
            },
        }
    }

    /// Get the OID corresponding to the main branch.
    #[context("Getting main branch OID for repository at: {:?}", self.get_path())]
    pub fn get_main_branch_oid(&self) -> anyhow::Result<NonZeroOid> {
        let main_branch_reference = self.get_main_branch_reference()?;
        let commit = main_branch_reference.peel_to_commit()?;
        match commit {
            Some(commit) => Ok(commit.get_oid()),
            None => anyhow::bail!(
                "Could not find commit pointed to by main branch: {:?}",
                main_branch_reference.get_name()?
            ),
        }
    }

    /// Get a mapping from OID to the names of branches which point to that OID.
    ///
    /// The returned branch names include the `refs/heads/` prefix, so it must
    /// be stripped if desired.
    #[context("Getting branch-OID-to-names map for repository at: {:?}", self.get_path())]
    pub fn get_branch_oid_to_names(
        &self,
    ) -> anyhow::Result<HashMap<NonZeroOid, HashSet<OsString>>> {
        let branches = self
            .inner
            .branches(Some(git2::BranchType::Local))
            .with_context(|| "Reading branches")?;

        let mut result: HashMap<NonZeroOid, HashSet<OsString>> = HashMap::new();
        for branch_info in branches {
            let branch_info = branch_info.with_context(|| "Iterating over branches")?;
            let branch = match branch_info {
                (branch, git2::BranchType::Remote) => anyhow::bail!(
                    "Unexpectedly got a remote branch in local branch iterator: {:?}",
                    branch.name()
                ),
                (branch, git2::BranchType::Local) => branch,
            };

            let reference = branch.into_reference();
            let reference_name = match reference.name() {
                None => {
                    log::warn!(
                        "Could not decode branch name, skipping: {:?}",
                        reference.name_bytes()
                    );
                    continue;
                }
                Some(reference_name) => reference_name,
            };

            let branch_oid = reference
                .resolve()
                .with_context(|| format!("Resolving branch into commit: {}", reference_name))?
                .target()
                .unwrap();
            result
                .entry(make_non_zero_oid(branch_oid))
                .or_insert_with(HashSet::new)
                .insert(OsString::from(reference_name.to_owned()));
        }

        // The main branch may be a remote branch, in which case it won't be
        // returned in the iteration above.
        let main_branch_name = self.get_main_branch_reference()?.get_name()?;
        let main_branch_oid = self.get_main_branch_oid()?;
        result
            .entry(main_branch_oid)
            .or_insert_with(HashSet::new)
            .insert(main_branch_name);

        Ok(result)
    }

    /// Detect if an interactive rebase has started but not completed.
    ///
    /// Git will send us spurious `post-rewrite` events marked as `amend` during an
    /// interactive rebase, indicating that some of the commits have been rewritten
    /// as part of the rebase plan, but not all of them. This function attempts to
    /// detect when an interactive rebase is underway, and if the current
    /// `post-rewrite` event is spurious.
    ///
    /// There are two practical issues for users as a result of this Git behavior:
    ///
    ///   * During an interactive rebase, we may see many "processing 1 rewritten
    ///   commit" messages, and then a final "processing X rewritten commits" message
    ///   once the rebase has concluded. This is potentially confusing for users, since
    ///   the operation logically only rewrote the commits once, but we displayed the
    ///   message multiple times.
    ///
    ///   * During an interactive rebase, we may warn about abandoned commits, when the
    ///   next operation in the rebase plan fixes up the abandoned commit. This can
    ///   happen even if no conflict occurred and the rebase completed successfully
    ///   without any user intervention.
    #[context("Determining if rebase is underway for repository at: {:?}", self.get_path())]
    pub fn is_rebase_underway(&self) -> anyhow::Result<bool> {
        use git2::RepositoryState::*;
        match self.inner.state() {
            Rebase | RebaseInteractive | RebaseMerge => Ok(true),

            // Possibly some of these states should also be treated as `true`?
            Clean | Merge | Revert | RevertSequence | CherryPick | CherryPickSequence | Bisect
            | ApplyMailbox | ApplyMailboxOrRebase => Ok(false),
        }
    }

    /// Get the type current multi-step operation (such as `rebase` or
    /// `cherry-pick`) which is underway. Returns `None` if there is no such
    /// operation.
    pub fn get_current_operation_type(&self) -> Option<&str> {
        use git2::RepositoryState::*;
        match self.inner.state() {
            Clean | Bisect => None,
            Merge => Some("merge"),
            Revert | RevertSequence => Some("revert"),
            CherryPick | CherryPickSequence => Some("cherry-pick"),
            Rebase | RebaseInteractive | RebaseMerge => Some("rebase"),
            ApplyMailbox | ApplyMailboxOrRebase => Some("am"),
        }
    }

    /// Find the merge-base between two commits. Returns `None` if a merge-base
    /// could not be found.
    #[context(
        "Looking up merge-base between {:?} and {:?} for repository at: {:?}",
        lhs, rhs, self.get_path()
    )]
    pub fn find_merge_base(
        &self,
        lhs: NonZeroOid,
        rhs: NonZeroOid,
    ) -> anyhow::Result<Option<NonZeroOid>> {
        match self.inner.merge_base(lhs.inner, rhs.inner) {
            Ok(merge_base_oid) => Ok(Some(make_non_zero_oid(merge_base_oid))),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Attempt to parse the user-provided object descriptor.
    pub fn revparse_single_commit(&self, spec: &str) -> anyhow::Result<Option<Commit>> {
        match self.inner.revparse_single(spec) {
            Ok(object) => match object.into_commit() {
                Ok(commit) => Ok(Some(Commit { inner: commit })),
                Err(_) => Ok(None),
            },
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Find all references in the repository.
    #[context("Looking up all references in repository at: {:?}", self.get_path())]
    pub fn get_all_references(&self) -> anyhow::Result<Vec<Reference>> {
        let mut all_references = Vec::new();
        for reference in self
            .inner
            .references()
            .map_err(wrap_git_error)
            .with_context(|| "Iterating through references")?
        {
            let reference = reference.with_context(|| "Accessing individual reference")?;
            all_references.push(Reference { inner: reference });
        }
        Ok(all_references)
    }

    /// Create a new reference or update an existing one.
    #[context(
        "Creating new reference {:?} pointing to {:?} (force={:?}, log_message={:?})",
        name,
        oid,
        force,
        log_message
    )]
    pub fn create_reference(
        &self,
        name: &OsStr,
        oid: NonZeroOid,
        force: bool,
        log_message: &str,
    ) -> anyhow::Result<Reference> {
        let name = match name.to_str() {
            Some(name) => name,
            None => anyhow::bail!(
                "Reference name is not a UTF-8 string (libgit2 limitation): {:?}",
                name
            ),
        };
        let reference = self
            .inner
            .reference(name, oid.inner, force, log_message)
            .map_err(wrap_git_error)?;
        Ok(Reference { inner: reference })
    }

    /// Look up a reference with the given name. Returns `None` if not found.
    #[context("Looking up reference {:?} in repository at: {:?}", name, self.get_path())]
    pub fn find_reference(&self, name: &OsStr) -> anyhow::Result<Option<Reference>> {
        let name = match name.to_str() {
            Some(name) => name,
            None => anyhow::bail!(
                "Reference name is not a UTF-8 string (libgit2 limitation): {:?}",
                name
            ),
        };
        match self.inner.find_reference(name) {
            Ok(reference) => Ok(Some(Reference { inner: reference })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Get all local branches in the repository.
    #[context("Looking up all local branches for repository at: {:?}", self.get_path())]
    pub fn get_all_local_branches(&self) -> anyhow::Result<Vec<Branch>> {
        let mut all_branches = Vec::new();
        for branch in self
            .inner
            .branches(Some(git2::BranchType::Local))
            .map_err(wrap_git_error)
            .with_context(|| "Iterating over all local branches")?
        {
            let (branch, _branch_type) = branch.with_context(|| "Accessing individual branch")?;
            all_branches.push(Branch { inner: branch });
        }
        Ok(all_branches)
    }

    /// Look up the branch with the given name. Returns `None` if not found.
    #[context("Looking up branch {:?} for repository at: {:?}", name, self.get_path())]
    pub fn find_branch(
        &self,
        name: &str,
        branch_type: BranchType,
    ) -> anyhow::Result<Option<Branch>> {
        match self.inner.find_branch(name, branch_type) {
            Ok(branch) => Ok(Some(Branch { inner: branch })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Create a new branch or update an existing branch.
    #[context("Creating branch {:?} for repository at: {:?}", name, self.get_path())]
    pub fn create_branch(
        &self,
        name: &OsStr,
        target: &Commit,
        force: bool,
    ) -> anyhow::Result<git2::Branch> {
        let name = match name.to_str() {
            Some(name) => name,
            None => anyhow::bail!(
                "Branch name is not a UTF-8 string (libgit2 limitation): {:?}",
                name
            ),
        };
        self.inner
            .branch(name, &target.inner, force)
            .map_err(wrap_git_error)
    }

    /// Look up a commit with the given OID. Returns `None` if not found.
    #[context("Looking up commit {:?} for repository at: {:?}", oid, self.get_path())]
    pub fn find_commit(&self, oid: NonZeroOid) -> anyhow::Result<Option<Commit>> {
        match self.inner.find_commit(oid.inner) {
            Ok(commit) => Ok(Some(Commit { inner: commit })),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Look up the commit with the given OID and render a friendly description
    /// of it, or render an error message if not found.
    pub fn friendly_describe_commit_from_oid(
        &self,
        oid: NonZeroOid,
    ) -> anyhow::Result<StyledString> {
        match self.find_commit(oid)? {
            Some(commit) => Ok(commit.friendly_describe()?),
            None => Ok(StyledString::styled(
                format!("<commit not found: {:?}>", oid),
                BaseColor::Red.light(),
            )),
        }
    }

    /// Create a new commit.
    #[context("Making commit {:?} for repository at: {:?}", message, self.get_path())]
    pub fn create_commit(
        &self,
        update_ref: Option<&str>,
        author: &Signature,
        committer: &Signature,
        message: &str,
        tree: &git2::Tree,
        parents: &[&Commit],
    ) -> anyhow::Result<NonZeroOid> {
        let parents = parents
            .iter()
            .map(|commit| &commit.inner)
            .collect::<Vec<_>>();
        let oid = self
            .inner
            .commit(
                update_ref,
                &author.inner,
                &committer.inner,
                message,
                tree,
                parents.as_slice(),
            )
            .map_err(wrap_git_error)?;
        Ok(make_non_zero_oid(oid))
    }

    /// Cherry-pick a commit in memory and return the resulting index.
    #[context(
        "Cherry-picking commit {:?} onto {:?} for repository at: {:?}",
        cherrypick_commit, our_commit, self.get_path()
    )]
    pub fn cherrypick_commit(
        &self,
        cherrypick_commit: &Commit,
        our_commit: &Commit,
        mainline: u32,
        options: Option<&git2::MergeOptions>,
    ) -> anyhow::Result<git2::Index> {
        self.inner
            .cherrypick_commit(
                &cherrypick_commit.inner,
                &our_commit.inner,
                mainline,
                options,
            )
            .map_err(wrap_git_error)
    }

    /// Look up the tree with the given OID. Returns `None` if not found.
    #[context("Looking up tree {:?} for repository at: {:?}", oid, self.get_path())]
    pub fn find_tree(&self, oid: NonZeroOid) -> anyhow::Result<Option<git2::Tree>> {
        match self.inner.find_tree(oid.inner) {
            Ok(tree) => Ok(Some(tree)),
            Err(err) if err.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(err) => Err(wrap_git_error(err)),
        }
    }

    /// Write the provided in-memory index as a tree into Git`s object database.
    /// There must be no merge conflicts in the index.
    #[context("Writing index file to disk for repository at: {:?}", self.get_path())]
    pub fn write_index_to_tree(&self, index: &mut git2::Index) -> anyhow::Result<NonZeroOid> {
        let oid = index.write_tree_to(&self.inner).map_err(wrap_git_error)?;
        Ok(make_non_zero_oid(oid))
    }
}

/// The signature of a commit, identifying who it was made by and when it was made.
pub struct Signature<'repo> {
    inner: git2::Signature<'repo>,
}

impl<'repo> Signature<'repo> {
    /// Update the timestamp of this signature to a new time.
    #[context("Updating commit timestamp")]
    pub fn update_timestamp(self, now: SystemTime) -> anyhow::Result<Signature<'repo>> {
        let seconds: i64 = now
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
            .try_into()?;
        let time = git2::Time::new(seconds, self.inner.when().offset_minutes());
        let name = match self.inner.name() {
            Some(name) => name,
            None => anyhow::bail!(
                "Could not decode signature name: {:?}",
                self.inner.name_bytes()
            ),
        };
        let email = match self.inner.email() {
            Some(email) => email,
            None => anyhow::bail!(
                "Could not decode signature email: {:?}",
                self.inner.email_bytes()
            ),
        };
        let signature = git2::Signature::new(name, email, &time)?;
        Ok(Signature { inner: signature })
    }

    /// Get the time when this signature was applied.
    pub fn get_time(&self) -> git2::Time {
        self.inner.when()
    }
}

/// Represents a commit object in the Git object database.
#[derive(Clone, Debug)]
pub struct Commit<'repo> {
    inner: git2::Commit<'repo>,
}

impl<'repo> Commit<'repo> {
    /// Get the object ID of the commit.
    pub fn get_oid(&self) -> NonZeroOid {
        NonZeroOid {
            inner: self.inner.id(),
        }
    }

    /// Get the object IDs of the parents of this commit.
    pub fn get_parent_oids(&self) -> Vec<NonZeroOid> {
        self.inner.parent_ids().map(make_non_zero_oid).collect()
    }

    /// Get the number of parents of this commit.
    pub fn get_parent_count(&self) -> usize {
        self.inner.parent_count()
    }

    /// Get the parent commits of this commit.
    pub fn get_parents(&self) -> Vec<Commit<'repo>> {
        self.inner
            .parents()
            .map(|commit| Commit { inner: commit })
            .collect()
    }

    /// Get the commit time of this commit.
    pub fn get_time(&self) -> git2::Time {
        self.inner.time()
    }

    /// Get the summary (first line) of the commit message.
    pub fn get_summary(&self) -> anyhow::Result<OsString> {
        match self.inner.summary_bytes() {
            Some(summary) => Ok(OsString::from_raw_vec(summary.into())?),
            None => anyhow::bail!("Could not read summary for commit: {:?}", self.get_oid()),
        }
    }

    /// Get the commit message with some whitespace trimmed.
    pub fn get_message_pretty(&self) -> anyhow::Result<OsString> {
        let message = OsString::from_raw_vec(self.inner.message_bytes().into())?;
        Ok(message)
    }

    /// Get the commit message, without any whitespace trimmed.
    pub fn get_message_raw(&self) -> anyhow::Result<OsString> {
        let message = OsString::from_raw_vec(self.inner.message_raw_bytes().into())?;
        Ok(message)
    }

    /// Get the author of this commit.
    pub fn get_author(&self) -> Signature {
        Signature {
            inner: self.inner.author(),
        }
    }

    /// Get the committer of this commit.
    pub fn get_committer(&self) -> Signature {
        Signature {
            inner: self.inner.committer(),
        }
    }

    /// Print a one-line description of this commit containing its OID and
    /// summary.
    #[context("Describing commit {}", self.get_oid())]
    pub fn friendly_describe(&self) -> anyhow::Result<StyledString> {
        let description = render_commit_metadata(
            self,
            &mut [
                &mut CommitOidProvider::new(true)?,
                &mut CommitMessageProvider::new()?,
            ],
        )?;
        Ok(description)
    }

    /// Determine if the current commit is empty (has no changes compared to its
    /// parent).
    pub fn is_empty(&self) -> bool {
        match self.get_parents().as_slice() {
            [] => false,
            [parent_commit] => self.inner.tree_id() == parent_commit.inner.tree_id(),
            _ => false,
        }
    }
}

/// Represents a reference to an object.
pub struct Reference<'repo> {
    inner: git2::Reference<'repo>,
}

impl<'repo> Reference<'repo> {
    /// Determine if the given name is a valid name for a reference.
    pub fn is_valid_name(name: &str) -> bool {
        git2::Reference::is_valid_name(name)
    }

    /// Given a reference name which is an OID, convert the string into an `Oid`
    /// object. If the `Oid` was zero, returns `None`.
    #[context("Converting reference name to OID: {:?}", ref_name)]
    pub fn name_to_oid(ref_name: &OsStr) -> anyhow::Result<Option<NonZeroOid>> {
        let oid: MaybeZeroOid = ref_name.try_into()?;
        match oid {
            MaybeZeroOid::NonZero(oid) => Ok(Some(oid)),
            MaybeZeroOid::Zero => Ok(None),
        }
    }

    /// Get the name of this reference.
    #[context("Getting name of reference: {:?}", self.inner.name_bytes())]
    pub fn get_name(&self) -> anyhow::Result<OsString> {
        let name = OsStringBytes::from_raw_vec(self.inner.name_bytes().into())
            .with_context(|| format!("Decoding reference name: {:?}", self.inner.name_bytes()))?;
        Ok(name)
    }

    /// Get the commit object pointed to by this reference. Returns `None` if
    /// the object pointed to by the reference is a different kind of object.
    #[context("Finding commit object pointed to by reference: {:?}", self.inner.name())]
    pub fn peel_to_commit(&self) -> anyhow::Result<Option<Commit<'repo>>> {
        let object = self.inner.peel(git2::ObjectType::Commit)?;
        match object.into_commit() {
            Ok(commit) => Ok(Some(Commit { inner: commit })),
            Err(_) => Ok(None),
        }
    }

    /// Delete the reference.
    #[context("Deleting reference: {:?}", self.inner.name())]
    pub fn delete(&mut self) -> anyhow::Result<()> {
        let reference_name = self.get_name()?;
        self.inner
            .delete()
            .with_context(|| format!("Deleting reference: {:?}", reference_name))?;
        Ok(())
    }
}

/// Determine what kind of branch a reference is, given its name. The returned
/// `suffix` value is converted to a `String` to be rendered to the screen, so
/// it may have lost some information if the reference name had unusual
/// characters.
#[derive(Debug)]
pub enum CategorizedReferenceName<'a> {
    /// The reference represents a local branch.
    LocalBranch {
        /// The full name of the reference.
        name: &'a OsStr,

        /// The string `refs/heads/`.
        prefix: &'static str,
    },

    /// The reference represents a remote branch.
    RemoteBranch {
        /// The full name of the reference.
        name: &'a OsStr,

        /// The string `refs/remotes/`.
        prefix: &'static str,
    },

    /// Some other kind of reference which isn't a branch at all.
    OtherRef {
        /// The full name of the reference.
        name: &'a OsStr,
    },
}

impl<'a> CategorizedReferenceName<'a> {
    /// Categorize the provided reference name.
    pub fn new(name: &'a OsStr) -> Self {
        let bytes = name.to_raw_bytes();
        if bytes.starts_with(b"refs/heads/") {
            Self::LocalBranch {
                name,
                prefix: "refs/heads/",
            }
        } else if bytes.starts_with(b"refs/remotes/") {
            Self::RemoteBranch {
                name,
                prefix: "refs/remotes/",
            }
        } else {
            Self::OtherRef { name }
        }
    }

    /// Remove the prefix from the reference name. May raise an error if the
    /// result couldn't be encoded as an `OsString` (probably shouldn't
    /// happen?).
    #[context("Removing prefix from reference name: {:?}", self)]
    pub fn remove_prefix(&self) -> anyhow::Result<OsString> {
        let (name, prefix): (_, &'static str) = match self {
            Self::LocalBranch { name, prefix } => (name, prefix),
            Self::RemoteBranch { name, prefix } => (name, prefix),
            Self::OtherRef { name } => (name, ""),
        };
        let bytes = name.to_raw_bytes();
        let bytes = match bytes.strip_prefix(prefix.as_bytes()) {
            Some(bytes) => Vec::from(bytes),
            None => Vec::from(bytes),
        };
        let result = OsString::from_raw_vec(bytes)?;
        Ok(result)
    }

    /// Render the full name of the reference, including its prefix, lossily as
    /// a `String`.
    pub fn render_full(&self) -> String {
        let name = match self {
            Self::LocalBranch { name, prefix: _ } => name,
            Self::RemoteBranch { name, prefix: _ } => name,
            Self::OtherRef { name } => name,
        };
        name.to_string_lossy().into_owned()
    }

    /// Render only the suffix of the reference name lossily as a `String`. The
    /// caller will usually check the type of reference and add additional
    /// information to the reference name.
    pub fn render_suffix(&self) -> String {
        let (name, prefix): (_, &'static str) = match self {
            Self::LocalBranch { name, prefix } => (name, prefix),
            Self::RemoteBranch { name, prefix } => (name, prefix),
            Self::OtherRef { name } => (name, ""),
        };
        let name = name.to_string_lossy();
        match name.strip_prefix(prefix) {
            Some(name) => name.to_string(),
            None => name.into_owned(),
        }
    }

    /// Render the reference name lossily, and prepend a helpful string like
    /// `branch` to the description.
    pub fn friendly_describe(&self) -> String {
        let name = self.render_suffix();
        let name = match self {
            CategorizedReferenceName::LocalBranch { .. } => {
                format!("branch {}", name)
            }
            CategorizedReferenceName::RemoteBranch { .. } => {
                format!("remote branch {}", name)
            }
            CategorizedReferenceName::OtherRef { .. } => format!("ref {}", name),
        };
        name
    }
}

type BranchType = git2::BranchType;

/// Represents a Git branch.
pub struct Branch<'repo> {
    inner: git2::Branch<'repo>,
}

impl<'repo> Branch<'repo> {
    /// Get the OID pointed to by the branch. Returns `None` if the branch is
    /// not a direct reference (which is unusual).
    pub fn get_oid(&self) -> anyhow::Result<Option<NonZeroOid>> {
        Ok(self.inner.get().target().map(make_non_zero_oid))
    }

    /// Convert the branch into its underlying `Reference`.
    pub fn into_reference(self) -> Reference<'repo> {
        Reference {
            inner: self.inner.into_reference(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_version_output() {
        assert_eq!(
            "git version 12.34.56".parse::<GitVersion>().unwrap(),
            GitVersion(12, 34, 56)
        );
        assert_eq!(
            "git version 12.34.56\n".parse::<GitVersion>().unwrap(),
            GitVersion(12, 34, 56)
        );
        assert_eq!(
            "git version 12.34.56.78.abcdef"
                .parse::<GitVersion>()
                .unwrap(),
            GitVersion(12, 34, 56)
        );
    }
}
