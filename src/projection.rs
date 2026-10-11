//! Filtered revision, tree and object visibility, independent of FUSE.
//!
//! The source repository is private: object access is granted only by walking a
//! selected subtree. Source commit IDs are provenance, not projected identities.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, executor::block_on, io::AsyncReadExt};
use jj_lib::backend::{MergedTreeValue, MergedTreeValueExt, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::config::StackedConfig;
use jj_lib::default_backend_factories::{
    default_backend_factories, default_working_copy_factories, default_workspace_loader_factory,
};
use jj_lib::fileset::FilesetAliasesMap;
use jj_lib::merge::Merge;
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::repo::{ReadonlyRepo, Repo, RepoLoader};
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::revset::{
    self, RevsetAliasesMap, RevsetDiagnostics, RevsetExtensions, RevsetParseContext,
    RevsetWorkspaceContext, SymbolResolver,
};
use jj_lib::settings::UserSettings;
use jj_lib::tree::{ToTreeMergeExt, TreeMergeExt};
use jj_lib::ui_path::RepoPathUiConverter;
use jj_lib::workspace::Workspace;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A repository view pinned to one operation, never snapshotted or reconciled.
pub struct SourceRepo {
    repo: Arc<ReadonlyRepo>,
    repo_path: PathBuf,
    workspace: Option<String>,
    workspace_root: PathBuf,
}

impl SourceRepo {
    /// Open a workspace (or a path beneath it), `.jj`, or `.jj/repo` directly.
    /// A direct metadata path needs `workspace` for workspace-relative revsets.
    pub fn open(path: &Path, workspace: Option<&str>) -> Result<Self> {
        let path = path
            .canonicalize()
            .context("resolve source repository path")?;
        let settings =
            UserSettings::from_config_and_home_dir(StackedConfig::with_defaults(), None)?;
        let factories = default_backend_factories();
        let (repo_path, workspace_root, detected_workspace) =
            if path.join("store").is_dir() && path.join("op_store").is_dir() {
                (path.clone(), path.clone(), None)
            } else {
                let root = path
                    .ancestors()
                    .find(|p| p.join(".jj").is_dir())
                    .context("no jj workspace found; pass a workspace or its .jj/repo directory")?;
                let loader = default_workspace_loader_factory().create(root)?;
                let name = if workspace.is_none() && root.join(".jj/working_copy").is_dir() {
                    Some(
                        Workspace::load(
                            &settings,
                            root,
                            &factories,
                            &default_working_copy_factories(),
                        )?
                        .workspace_name()
                        .as_str()
                        .to_owned(),
                    )
                } else {
                    None
                };
                (loader.repo_path().canonicalize()?, root.to_owned(), name)
            };
        let loader = RepoLoader::init_from_file_system(&settings, &repo_path, &factories)?;
        // load_at_head() reconciles concurrent operations and writes commits!
        // Fail instead of turning a read-only mount into an implicit mutation.
        let heads = block_on(loader.op_heads_store().get_op_heads())?;
        if heads.len() != 1 {
            bail!(
                "expected one jj operation head, found {}; run jj to reconcile explicitly",
                heads.len()
            );
        }
        let op = block_on(loader.load_operation(&heads[0]))?;
        let repo = block_on(loader.load_at(&op))?;
        Ok(Self {
            repo,
            repo_path,
            workspace: workspace.map(str::to_owned).or(detected_workspace),
            workspace_root,
        })
    }

    /// Resolve exactly one commit, then pin its selected path without reading blobs.
    /// Built-in jj revset syntax is supported; user aliases/config are not loaded.
    pub fn project(&self, revision: &str, subdir: &str) -> Result<Projection> {
        let subdir = checked_path(subdir)?;
        let aliases = RevsetAliasesMap::new();
        let fileset_aliases = FilesetAliasesMap::new();
        let extensions = RevsetExtensions::default();
        let converter = RepoPathUiConverter::Fs {
            cwd: self.workspace_root.clone(),
            base: self.workspace_root.clone(),
        };
        let workspace_name = self.workspace.as_deref().map(WorkspaceName::new);
        let context = RevsetParseContext {
            aliases_map: &aliases,
            local_variables: HashMap::new(),
            user_email: self.repo.settings().user_email(),
            date_pattern_context: chrono::Utc::now().fixed_offset().into(),
            default_ignored_remote: Some("git".as_ref()),
            fileset_aliases_map: &fileset_aliases,
            extensions: &extensions,
            workspace: workspace_name.map(|workspace_name| RevsetWorkspaceContext {
                path_converter: &converter,
                workspace_name,
            }),
        };
        let expression = revset::parse(&mut RevsetDiagnostics::new(), revision, &context)?;
        let resolver = SymbolResolver::new(self.repo.as_ref(), extensions.symbol_resolvers());
        let resolved = expression.resolve_user_expression(self.repo.as_ref(), &resolver)?;
        let evaluated = resolved.evaluate()?;
        let mut ids = evaluated.stream();
        let id = block_on(ids.next())
            .transpose()?
            .context("revision selected no commits")?;
        if block_on(ids.next()).transpose()?.is_some() {
            bail!("revision must select exactly one commit");
        }
        let commit = self.repo.store().get_commit(&id)?;
        Projection::new(
            commit,
            subdir,
            self.repo_path.clone(),
            self.repo.op_id().hex(),
        )
    }
}

/// A strict repository-relative path. `.` names the root, never a traversal.
pub fn checked_path(path: &str) -> Result<RepoPathBuf> {
    if path == "." || path.is_empty() {
        return Ok(RepoPathBuf::root());
    }
    if path.contains('\\')
        || path.contains('\0')
        || path.starts_with('/')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        bail!("expected a normalized repository-relative path, got {path:?}");
    }
    Ok(RepoPathBuf::from_internal_string(path)?)
}

/// Typed object identity. Git submodule commits and copy provenance are not
/// granted repository-object access by a tree entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Tree,
    File,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectKey {
    pub kind: ObjectKind,
    pub id: String,
}

/// One side of an entry. Absence is represented by `None` in `Entry::terms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Value {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<bool>,
}

fn value_record(value: &TreeValue) -> Value {
    let (kind, id, executable) = match value {
        TreeValue::File { id, executable, .. } => ("file", id.hex(), Some(*executable)),
        TreeValue::Symlink(id) => ("symlink", id.hex(), None),
        TreeValue::Tree(id) => ("tree", id.hex(), None),
        TreeValue::GitSubmodule(id) => ("git_submodule", id.hex(), None),
    };
    Value {
        kind: kind.to_owned(),
        id,
        executable,
    }
}

/// Alternating positive/negative/positive jj merge terms, not a chosen side.
/// Nonrecursive entries report directories, conflicts and submodules explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: String,
    pub terms: Vec<Option<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_at: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub path: RepoPathBuf,
    pub value: MergedTreeValue,
    pub blocked_at: Option<String>,
}

impl Node {
    pub fn kind(&self) -> &'static str {
        if self.blocked_at.is_some() {
            return "blocked";
        }
        if self.value.is_tree() {
            return "directory";
        }
        match self.value.as_resolved() {
            Some(None) => "absent",
            Some(Some(TreeValue::File { .. })) => "file",
            Some(Some(TreeValue::Symlink(_))) => "symlink",
            Some(Some(TreeValue::GitSubmodule(_))) => "git_submodule",
            Some(Some(TreeValue::Tree(_))) => "directory",
            None => "conflict",
        }
    }
    fn record(&self, relative_path: String) -> Entry {
        Entry {
            path: relative_path,
            kind: self.kind().to_owned(),
            terms: self
                .value
                .iter()
                .map(|v| v.as_ref().map(value_record))
                .collect(),
            blocked_at: self.blocked_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub format: String,
    pub repository: PathBuf,
    pub source_operation: String,
    pub source_commit: String,
    pub subdir: String,
    pub projected_tree: String,
    pub root: Entry,
}

/// An immutable scope/capability; no arbitrary source-object getter is exposed.
pub struct Projection {
    commit: Commit,
    subdir: RepoPathBuf,
    root: Node,
    provenance: Provenance,
}

impl Projection {
    fn new(
        commit: Commit,
        subdir: RepoPathBuf,
        repository: PathBuf,
        source_operation: String,
    ) -> Result<Self> {
        let root = block_on(select_root(&commit, &subdir))?;
        let record = root.record(String::new());
        let projected_tree = digest("tree", &(subdir.as_internal_file_string(), &record));
        let provenance = Provenance {
            format: "jj-workfuse-v1".into(),
            repository,
            source_operation,
            source_commit: commit.id().hex(),
            subdir: subdir.as_internal_file_string().to_owned(),
            projected_tree,
            root: record,
        };
        Ok(Self {
            commit,
            subdir,
            root,
            provenance,
        })
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    pub(crate) fn root(&self) -> &Node {
        &self.root
    }

    pub(crate) fn node(&self, path: &str) -> Result<Node> {
        let relative = checked_path(path)?;
        let mut node = self.root.clone();
        for component in relative.components() {
            let trees = block_on(node.value.to_tree_merge(self.commit.store(), &node.path))?
                .context("path component is not a directory (possibly conflicted)")?;
            let value = trees.value(component).cloned();
            node = Node {
                path: node.path.join(component),
                value,
                blocked_at: None,
            };
            if node.value.is_absent() {
                bail!("path not present in projection: {path}");
            }
        }
        Ok(node)
    }

    /// Lookup without following symlinks; the path cannot escape this projection.
    pub fn entry(&self, path: &str) -> Result<Entry> {
        Ok(self
            .node(path)?
            .record(checked_path(path)?.as_internal_file_string().to_owned()))
    }

    pub(crate) fn children(&self, node: &Node) -> Result<Vec<Node>> {
        let trees = block_on(node.value.to_tree_merge(self.commit.store(), &node.path))?
            .context("not a directory")?;
        let names: BTreeSet<_> = trees
            .iter()
            .flat_map(|t| t.entries_non_recursive().map(|e| e.name().to_owned()))
            .collect();
        let mut children = Vec::new();
        for name in names {
            let value = trees.value(&name).cloned();
            if value.is_present() {
                children.push(Node {
                    path: node.path.join(&name),
                    value,
                    blocked_at: None,
                });
            }
        }
        Ok(children)
    }

    /// List just one directory. This reads tree objects, never file contents.
    pub fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let node = self.node(path)?;
        self.children(&node)?
            .iter()
            .map(|n| Ok(n.record(self.relative(&n.path))))
            .collect()
    }

    fn relative(&self, path: &RepoPath) -> String {
        path.strip_prefix(&self.subdir)
            .expect("projection node is within selected subtree")
            .as_internal_file_string()
            .to_owned()
    }

    /// Read a resolved file or symlink only. Conflicts fail closed.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        self.read_node(&self.node(path)?)
    }

    pub(crate) fn read_node(&self, node: &Node) -> Result<Vec<u8>> {
        block_on(async {
            match node.value.as_resolved() {
                Some(Some(TreeValue::File { id, .. })) => {
                    let mut reader = self.commit.store().read_file(&node.path, id).await?;
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).await?;
                    Ok(bytes)
                }
                Some(Some(TreeValue::Symlink(id))) => Ok(self
                    .commit
                    .store()
                    .read_symlink(&node.path, id)
                    .await?
                    .into_bytes()),
                _ => bail!(
                    "not a resolved file or symlink: {} ({})",
                    self.relative(&node.path),
                    node.kind()
                ),
            }
        })
    }

    pub(crate) fn file_size(&self, node: &Node) -> Result<u64> {
        block_on(async {
            let Some(Some(TreeValue::File { id, .. })) = node.value.as_resolved() else {
                bail!("not a resolved file");
            };
            let mut reader = self.commit.store().read_file(&node.path, id).await?;
            let mut buffer = [0; 65536];
            let mut size = 0u64;
            loop {
                let count = reader.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                size += count as u64;
            }
            Ok(size)
        })
    }

    pub(crate) fn read_range(&self, node: &Node, offset: u64, size: u32) -> Result<Vec<u8>> {
        block_on(async {
            let Some(Some(TreeValue::File { id, .. })) = node.value.as_resolved() else {
                bail!("not a resolved file");
            };
            let mut reader = self.commit.store().read_file(&node.path, id).await?;
            let mut buffer = [0; 65536];
            let mut remaining = offset;
            while remaining > 0 {
                let limit = remaining.min(buffer.len() as u64) as usize;
                let n = reader.read(&mut buffer[..limit]).await?;
                if n == 0 {
                    return Ok(Vec::new());
                }
                remaining -= n as u64;
            }
            let mut data = Vec::with_capacity(size as usize);
            reader.take(size as u64).read_to_end(&mut data).await?;
            Ok(data)
        })
    }

    /// Inventory of source objects reachable within the selected root, including
    /// all conflict terms. No full source commit or ancestor/root trees escape.
    /// Recursive inventory reads trees but never file/symlink contents.
    pub fn objects(&self) -> Result<Vec<ObjectKey>> {
        Ok(self.object_locations()?.into_keys().collect())
    }

    fn object_locations(&self) -> Result<BTreeMap<ObjectKey, (RepoPathBuf, TreeValue)>> {
        let mut objects = BTreeMap::new();
        let mut stack: Vec<_> = self
            .root
            .value
            .iter()
            .filter_map(|v| v.clone())
            .map(|v| (self.root.path.clone(), v))
            .collect();
        while let Some((path, value)) = stack.pop() {
            let kind = match &value {
                TreeValue::Tree(_) => ObjectKind::Tree,
                TreeValue::File { .. } => ObjectKind::File,
                TreeValue::Symlink(_) => ObjectKind::Symlink,
                TreeValue::GitSubmodule(_) => continue, // External repository, not a reachable source commit.
            };
            let key = ObjectKey {
                kind,
                id: value_record(&value).id,
            };
            if objects.contains_key(&key) {
                continue;
            }
            objects.insert(key, (path.clone(), value.clone()));
            if let TreeValue::Tree(id) = value {
                let tree = block_on(self.commit.store().get_tree(path.clone(), &id))?;
                for entry in tree.entries_non_recursive() {
                    stack.push((path.join(entry.name()), entry.value().clone()));
                }
            }
        }
        Ok(objects)
    }

    /// Access only an object proved reachable by this view. Trees are returned
    /// as filtered-core JSON entries, blobs as bytes; this is not a raw Git wire API.
    pub fn read_object(&self, key: &ObjectKey) -> Result<Vec<u8>> {
        let locations = self.object_locations()?;
        let (path, value) = locations
            .get(key)
            .context("object is not reachable within this projection")?;
        if let TreeValue::Tree(id) = value {
            let tree = block_on(self.commit.store().get_tree(path.clone(), id))?;
            let entries: Vec<_> = tree
                .entries_non_recursive()
                .map(|e| {
                    Node {
                        path: path.join(e.name()),
                        value: Merge::resolved(Some(e.value().clone())),
                        blocked_at: None,
                    }
                    .record(e.name().as_internal_str().to_owned())
                })
                .collect();
            Ok(serde_json::to_vec(&entries)?)
        } else {
            self.read_node(&Node {
                path: path.clone(),
                value: Merge::resolved(Some(value.clone())),
                blocked_at: None,
            })
        }
    }

    /// Project every ancestor, keeping source parent ordering and no-change
    /// vertices. Parents precede children; the root is retained. An explicit
    /// guard fails instead of returning a silently truncated graph.
    pub fn history(&self, max_commits: usize) -> Result<Vec<ProjectedCommit>> {
        let mut ordered = Vec::new();
        let mut visiting = BTreeSet::new();
        let mut done: BTreeMap<String, ProjectedCommit> = BTreeMap::new();
        let mut stack = vec![(self.commit.clone(), false)];
        while let Some((commit, expanded)) = stack.pop() {
            let source = commit.id().hex();
            if done.contains_key(&source) {
                continue;
            }
            if !expanded {
                if !visiting.insert(source.clone()) {
                    bail!("cycle in source commit graph");
                }
                if visiting.len() + done.len() > max_commits {
                    bail!("history exceeds --max-commits {max_commits}; raise the explicit guard");
                }
                let parents = block_on(commit.parents())?;
                stack.push((commit, true));
                for parent in parents.into_iter().rev() {
                    if !done.contains_key(&parent.id().hex()) {
                        stack.push((parent, false));
                    }
                }
            } else {
                visiting.remove(&source);
                let projected = Self::new(
                    commit.clone(),
                    self.subdir.clone(),
                    self.provenance.repository.clone(),
                    self.provenance.source_operation.clone(),
                )?;
                let parents: Vec<_> = commit
                    .parent_ids()
                    .iter()
                    .map(|id| done[&id.hex()].id.clone())
                    .collect();
                let changed_from_parents = commit
                    .parent_ids()
                    .iter()
                    .map(|id| done[&id.hex()].tree != projected.provenance.projected_tree)
                    .collect();
                let id = digest(
                    "commit",
                    &(&source, &projected.provenance.projected_tree, &parents),
                );
                let vertex = ProjectedCommit {
                    id,
                    source_commit: source.clone(),
                    parents,
                    tree: projected.provenance.projected_tree.clone(),
                    root: projected.provenance.root.clone(),
                    changed_from_parents,
                };
                ordered.push(vertex.clone());
                done.insert(source, vertex);
            }
        }
        Ok(ordered)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectedCommit {
    pub id: String,
    pub source_commit: String,
    pub parents: Vec<String>,
    pub tree: String,
    pub root: Entry,
    /// One tree-equality comparison per source parent; not a merge-base diff.
    pub changed_from_parents: Vec<bool>,
}

fn digest(kind: &str, value: &impl Serialize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("jj-workfuse-v1/{kind}\0"));
    hasher.update(serde_json::to_vec(value).expect("projection identity serializes"));
    format!("wf-{kind}-v1:{:x}", hasher.finalize())
}

async fn select_root(commit: &Commit, subdir: &RepoPath) -> Result<Node> {
    let mut node = Node {
        path: RepoPathBuf::root(),
        value: commit
            .tree_ids()
            .map(|id| Some(TreeValue::Tree(id.clone()))),
        blocked_at: None,
    };
    for component in subdir.components() {
        if node.value.is_absent() {
            return Ok(Node {
                path: subdir.to_owned(),
                value: Merge::absent(),
                blocked_at: None,
            });
        }
        let Some(trees) = node.value.to_tree_merge(commit.store(), &node.path).await? else {
            // A non-directory ancestor obstructs this selection. Do not leak its
            // blob or turn a tree/file conflict into one arbitrarily chosen side.
            return Ok(Node {
                path: subdir.to_owned(),
                value: Merge::absent(),
                blocked_at: Some(node.path.as_internal_file_string().to_owned()),
            });
        };
        let value = trees.value(component).cloned();
        node = Node {
            path: node.path.join(component),
            value,
            blocked_at: None,
        };
    }
    Ok(node)
}
