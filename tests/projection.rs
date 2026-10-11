use std::sync::Arc;

use anyhow::Result;
use futures::{executor::block_on, io::Cursor};
use jj_lib::backend::{CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use jj_workfuse::filesystem::ReadOnlyFs;
use jj_workfuse::projection::{ObjectKey, ObjectKind, Projection, SourceRepo, checked_path};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    repo: Arc<ReadonlyRepo>,
}

impl Fixture {
    fn new(git: bool) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let mut config = StackedConfig::with_defaults();
        config.add_layer(ConfigLayer::parse(
            ConfigSource::User,
            "user.name = 'Test'\nuser.email = 'test@example.invalid'",
        )?);
        let settings = UserSettings::from_config_and_home_dir(config, None)?;
        let (_ws, repo) = if git {
            block_on(Workspace::init_internal_git(
                &settings,
                dir.path(),
                gix::hash::Kind::Sha1,
            ))?
        } else {
            block_on(Workspace::init_simple(&settings, dir.path()))?
        };
        Ok(Self { dir, repo })
    }
    fn file(&self, path: &str, data: &[u8], executable: bool) -> Result<TreeValue> {
        let id = block_on(
            self.repo
                .store()
                .write_file(&checked_path(path)?, &mut Cursor::new(data)),
        )?;
        Ok(TreeValue::File {
            id,
            executable,
            copy_id: CopyId::placeholder(),
        })
    }
    fn link(&self, path: &str, target: &str) -> Result<TreeValue> {
        Ok(TreeValue::Symlink(block_on(
            self.repo
                .store()
                .write_symlink(&checked_path(path)?, target),
        )?))
    }
    fn tree(&self, base: MergedTree, edits: Vec<(&str, Option<TreeValue>)>) -> Result<MergedTree> {
        let mut builder = MergedTreeBuilder::new(base);
        for (path, value) in edits {
            builder.set_or_remove(checked_path(path)?, Merge::resolved(value));
        }
        Ok(block_on(builder.write_tree())?)
    }
    fn commit(&mut self, parents: &[Commit], tree: MergedTree) -> Result<Commit> {
        let mut tx = self.repo.start_transaction();
        let commit = block_on(
            tx.repo_mut()
                .new_commit(parents.iter().map(|c| c.id().clone()).collect(), tree)
                .set_description("fixture")
                .write(),
        )?;
        self.repo = block_on(tx.commit("fixture"))?;
        Ok(commit)
    }
    fn projection(&self, commit: &Commit, path: &str) -> Result<Projection> {
        SourceRepo::open(self.dir.path(), None)?.project(&commit.id().hex(), path)
    }
}

#[test]
fn native_git_and_simple_subtrees_are_unmaterialized_and_filtered() -> Result<()> {
    for git in [false, true] {
        let mut f = Fixture::new(git)?;
        let inside = f.file("plugin/nested/index.js", b"export default 42;\n", false)?;
        let outside = f.file("secrets/secret", b"NOT IN VIEW", false)?;
        let tree = f.tree(
            f.repo.store().empty_merged_tree(),
            vec![
                ("plugin/nested/index.js", Some(inside)),
                ("secrets/secret", Some(outside.clone())),
                (
                    "plugin/run",
                    Some(f.file("plugin/run", b"#!/bin/sh\n", true)?),
                ),
                (
                    "plugin/link",
                    Some(f.link("plugin/link", "../secrets/secret")?),
                ),
                (
                    "plugin/é",
                    Some(f.file("plugin/é", &[0, 255, 13, 10], false)?),
                ),
            ],
        )?;
        let root_id = tree.tree_ids().as_resolved().unwrap().hex();
        let commit = f.commit(&[f.repo.store().root_commit()], tree)?;
        assert!(!f.dir.path().join("plugin").exists());
        assert!(!f.dir.path().join("secrets").exists());
        let p = f.projection(&commit, "plugin")?;
        assert_eq!(p.read("nested/index.js")?, b"export default 42;\n");
        assert_eq!(p.read("é")?, [0, 255, 13, 10]);
        assert_eq!(p.read("link")?, b"../secrets/secret");
        assert!(p.entry("secrets/secret").is_err());
        assert!(p.entry("../secrets/secret").is_err());
        let objects = p.objects()?;
        assert!(!objects.contains(&ObjectKey {
            kind: ObjectKind::Tree,
            id: root_id
        }));
        let TreeValue::File { id, .. } = outside else {
            unreachable!()
        };
        let forbidden = ObjectKey {
            kind: ObjectKind::File,
            id: id.hex(),
        };
        assert!(!objects.contains(&forbidden));
        assert!(p.read_object(&forbidden).is_err());
        for object in &objects {
            assert!(!p.read_object(object)?.is_empty());
        }
        assert!(p.provenance().projected_tree.starts_with("wf-tree-v1:"));
        assert_ne!(p.provenance().projected_tree, p.provenance().source_commit);
        assert!(!f.dir.path().join("plugin").exists());
    }
    Ok(())
}

#[test]
fn lazy_filesystem_operations_modes_links_offsets_and_cache() -> Result<()> {
    let mut f = Fixture::new(true)?;
    let tree = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![
            ("plugin/a", Some(f.file("plugin/a", b"abcdefgh", false)?)),
            (
                "plugin/nested/run",
                Some(f.file("plugin/nested/run", b"run", true)?),
            ),
            ("plugin/link", Some(f.link("plugin/link", "a")?)),
            ("outside", Some(f.file("outside", &[0; 100_000], false)?)),
        ],
    )?;
    let commit = f.commit(&[f.repo.store().root_commit()], tree)?;
    let mut fs = ReadOnlyFs::new(f.projection(&commit, "plugin")?, 8, 1000, 1001)?;
    assert_eq!(fs.stats().directory_loads, 0);
    assert_eq!(fs.readdir(1).unwrap().len(), 5);
    assert_eq!(fs.stats().blob_reads, 0);
    assert_eq!(fs.stats().size_scans, 0);
    let a = fs.lookup(1, "a".as_ref()).unwrap();
    assert_eq!(fs.getattr(a).unwrap().size, 8);
    assert_eq!(fs.getattr(a).unwrap().perm, 0o444);
    assert_eq!(fs.read(a, 2, 3).unwrap(), b"cde");
    assert_eq!(fs.read(a, 7, 20).unwrap(), b"h");
    assert_eq!(fs.read(a, i64::MAX, 20).unwrap(), b"");
    assert_eq!(fs.read(a, -1, 20), Err(libc::EINVAL));
    assert_eq!(fs.stats().blob_reads, 1);
    assert_eq!(fs.open(a, libc::O_WRONLY), Err(libc::EROFS));
    assert_eq!(fs.open(a, libc::O_RDONLY | libc::O_TRUNC), Err(libc::EROFS));
    assert_eq!(fs.access(a, libc::X_OK), Err(libc::EACCES));
    assert_eq!(fs.lookup(1, "outside".as_ref()), Err(libc::ENOENT));
    assert_eq!(fs.lookup(1, "..".as_ref()), Ok(1));
    let link = fs.lookup(1, "link".as_ref()).unwrap();
    assert_eq!(fs.readlink(link).unwrap(), b"a");
    assert_eq!(fs.read(link, 0, 1), Err(libc::ELOOP));
    let nested = fs.lookup(1, "nested".as_ref()).unwrap();
    let run = fs.lookup(nested, "run".as_ref()).unwrap();
    assert_eq!(fs.getattr(run).unwrap().perm, 0o555);
    assert_eq!(fs.read(run, 0, 3).unwrap(), b"run");
    assert!(fs.stats().cached_bytes <= 8);
    assert_eq!(fs.lookup(nested, "..".as_ref()), Ok(1));
    assert_eq!(fs.readdir(a).unwrap_err(), libc::ENOTDIR);
    assert_eq!(fs.getattr(999).unwrap_err(), libc::ENOENT);
    Ok(())
}

#[test]
fn oversized_files_use_uncached_ranges() -> Result<()> {
    let mut f = Fixture::new(false)?;
    let bytes: Vec<_> = (0..100_000).map(|n| (n % 256) as u8).collect();
    let tree = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![("plugin/big", Some(f.file("plugin/big", &bytes, false)?))],
    )?;
    let commit = f.commit(&[f.repo.store().root_commit()], tree)?;
    let mut fs = ReadOnlyFs::new(f.projection(&commit, "plugin")?, 64, 1, 1)?;
    let ino = fs.lookup(1, "big".as_ref()).unwrap();
    assert_eq!(fs.read(ino, 65_530, 20).unwrap(), bytes[65_530..65_550]);
    assert_eq!(fs.read(ino, 99_995, 20).unwrap(), bytes[99_995..]);
    assert_eq!(fs.stats().cached_bytes, 0);
    assert_eq!(fs.stats().size_scans, 1);
    Ok(())
}

#[test]
fn history_retains_outside_only_vertices_and_boundary_deletions() -> Result<()> {
    let mut f = Fixture::new(true)?;
    let initial = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![("plugin/x", Some(f.file("plugin/x", b"x", false)?))],
    )?;
    let a = f.commit(&[f.repo.store().root_commit()], initial)?;
    let outside = f.tree(
        a.tree(),
        vec![("unrelated", Some(f.file("unrelated", b"outside", false)?))],
    )?;
    let b = f.commit(std::slice::from_ref(&a), outside)?;
    let moved = f.tree(
        b.tree(),
        vec![
            ("plugin/x", None),
            ("moved-x", Some(f.file("moved-x", b"x", false)?)),
        ],
    )?;
    let c = f.commit(std::slice::from_ref(&b), moved)?;
    let p = f.projection(&c, "plugin")?;
    assert_eq!(p.provenance().root.kind, "absent");
    assert!(p.objects()?.is_empty());
    let history = p.history(100)?;
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].root.kind, "absent");
    assert_eq!(history[1].tree, history[2].tree);
    assert_eq!(history[2].changed_from_parents, [false]);
    assert_eq!(history[3].changed_from_parents, [true]);
    assert_ne!(history[1].id, history[2].id);
    assert_eq!(history[3].parents, [history[2].id.clone()]);
    assert!(p.history(3).is_err());
    assert_eq!(
        p.history(100)?.last().unwrap().id,
        history.last().unwrap().id
    );
    Ok(())
}

#[test]
fn merges_preserve_topology_and_conflict_evidence_without_selecting_a_side() -> Result<()> {
    for git in [false, true] {
        let mut f = Fixture::new(git)?;
        let initial = f.tree(
            f.repo.store().empty_merged_tree(),
            vec![("plugin/x", Some(f.file("plugin/x", b"base", false)?))],
        )?;
        let base = f.commit(&[f.repo.store().root_commit()], initial)?;
        let left_tree = f.tree(
            base.tree(),
            vec![("plugin/x", Some(f.file("plugin/x", b"left", false)?))],
        )?;
        let left = f.commit(std::slice::from_ref(&base), left_tree)?;
        let right_tree = f.tree(
            base.tree(),
            vec![("plugin/x", Some(f.file("plugin/x", b"right", false)?))],
        )?;
        let right = f.commit(std::slice::from_ref(&base), right_tree)?;
        let ids = Merge::from_vec(vec![
            left.tree_ids().as_resolved().unwrap().clone(),
            base.tree_ids().as_resolved().unwrap().clone(),
            right.tree_ids().as_resolved().unwrap().clone(),
        ]);
        let merge_tree = MergedTree::new(f.repo.store().clone(), ids, ConflictLabels::unlabeled());
        let merge = f.commit(&[left, right], merge_tree)?;
        let p = f.projection(&merge, "plugin")?;
        assert_eq!(p.entry("x")?.kind, "conflict");
        assert_eq!(p.entry("x")?.terms.len(), 3);
        assert!(p.read("x").is_err());
        let file_objects: Vec<_> = p
            .objects()?
            .into_iter()
            .filter(|o| o.kind == ObjectKind::File)
            .collect();
        assert_eq!(file_objects.len(), 3);
        let sides: std::collections::BTreeSet<_> = file_objects
            .iter()
            .map(|o| p.read_object(o))
            .collect::<Result<_>>()?;
        assert_eq!(
            sides,
            [b"base".to_vec(), b"left".to_vec(), b"right".to_vec()].into()
        );
        let history = p.history(100)?;
        assert_eq!(history.len(), 5);
        assert_eq!(history.last().unwrap().parents.len(), 2);
        let mut fs = ReadOnlyFs::new(p, 4096, 1, 1)?;
        let ino = fs.lookup(1, "x".as_ref()).unwrap();
        assert_eq!(fs.getattr(ino).unwrap().size, 0);
        assert_eq!(fs.read(ino, 0, 100), Err(libc::EIO));
    }
    Ok(())
}

#[test]
fn type_changes_and_blocked_nested_selections_are_explicit() -> Result<()> {
    let mut f = Fixture::new(true)?;
    let flat = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![("plugin", Some(f.file("plugin", b"flat", false)?))],
    )?;
    let a = f.commit(&[f.repo.store().root_commit()], flat)?;
    let nested = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![(
            "plugin/nested/x",
            Some(f.file("plugin/nested/x", b"nested", false)?),
        )],
    )?;
    let b = f.commit(std::slice::from_ref(&a), nested)?;
    let p = f.projection(&b, "plugin/nested")?;
    let history = p.history(100)?;
    assert_eq!(history[1].root.kind, "blocked");
    assert_eq!(history[1].root.blocked_at.as_deref(), Some("plugin"));
    let blocked = f.projection(&a, "plugin/nested")?;
    assert!(blocked.objects()?.is_empty());
    assert!(ReadOnlyFs::new(blocked, 100, 1, 1).is_err());
    assert_eq!(f.projection(&a, "plugin")?.provenance().root.kind, "file");
    let ids = Merge::from_vec(vec![
        b.tree_ids().as_resolved().unwrap().clone(),
        f.repo.store().empty_tree_id().clone(),
        a.tree_ids().as_resolved().unwrap().clone(),
    ]);
    let conflicted = f.commit(
        &[a, b],
        MergedTree::new(f.repo.store().clone(), ids, ConflictLabels::unlabeled()),
    )?;
    let p = f.projection(&conflicted, "plugin")?;
    assert_eq!(p.provenance().root.kind, "conflict");
    assert!(p.list("").is_err());
    assert!(p.objects()?.iter().any(|o| o.kind == ObjectKind::File));
    assert!(ReadOnlyFs::new(p, 100, 1, 1).is_err());
    let blocked = f.projection(&conflicted, "plugin/nested")?;
    assert_eq!(blocked.provenance().root.kind, "blocked");
    assert!(blocked.objects()?.is_empty());
    Ok(())
}

#[test]
fn direct_repo_and_linked_workspace_do_not_need_a_working_tree() -> Result<()> {
    let mut f = Fixture::new(true)?;
    let tree = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![("plugin/x", Some(f.file("plugin/x", b"pinned", false)?))],
    )?;
    let commit = f.commit(&[f.repo.store().root_commit()], tree)?;
    let source = SourceRepo::open(&f.dir.path().join(".jj/repo"), None)?;
    let p = source.project(&commit.id().hex(), "plugin")?;
    assert_eq!(p.read("x")?, b"pinned");
    assert!(source.project("@", "plugin").is_err());
    assert!(source.project("all()", "plugin").is_err());
    let linked = tempfile::tempdir()?;
    std::fs::create_dir(linked.path().join(".jj"))?;
    std::fs::write(
        linked.path().join(".jj/repo"),
        f.dir.path().join(".jj/repo").as_os_str().as_encoded_bytes(),
    )?;
    assert_eq!(
        SourceRepo::open(linked.path(), None)?
            .project(&commit.id().hex(), "plugin")?
            .read("x")?,
        b"pinned"
    );
    assert!(
        f.dir
            .path()
            .read_dir()?
            .all(|e| e.unwrap().file_name() == ".jj")
    );
    Ok(())
}

#[test]
fn submodule_is_an_opaque_boundary_not_a_commit_access_grant() -> Result<()> {
    let mut f = Fixture::new(true)?;
    let outside = f.tree(
        f.repo.store().empty_merged_tree(),
        vec![("outside", Some(f.file("outside", b"secret", false)?))],
    )?;
    let a = f.commit(&[f.repo.store().root_commit()], outside)?;
    let tree = f.tree(
        a.tree(),
        vec![(
            "plugin/vendor",
            Some(TreeValue::GitSubmodule(a.id().clone())),
        )],
    )?;
    let b = f.commit(std::slice::from_ref(&a), tree)?;
    let p = f.projection(&b, "plugin")?;
    assert_eq!(p.entry("vendor")?.kind, "git_submodule");
    assert_eq!(p.objects()?.len(), 1); // Only the plugin tree.
    assert!(p.read("vendor").is_err());
    let mut fs = ReadOnlyFs::new(p, 100, 1, 1)?;
    let vendor = fs.lookup(1, "vendor".as_ref()).unwrap();
    assert_eq!(fs.open(vendor, libc::O_RDONLY), Err(libc::EOPNOTSUPP));
    Ok(())
}

#[test]
fn reject_path_traversal_and_normalization_ambiguity() {
    for path in [
        "/plugin",
        "../plugin",
        "a/../b",
        "a/./b",
        "a//b",
        "a/",
        "a\\b",
        "nul\0",
    ] {
        assert!(checked_path(path).is_err(), "{path:?}");
    }
    assert_eq!(checked_path(".").unwrap(), RepoPathBuf::root());
    assert_eq!(checked_path("").unwrap(), RepoPathBuf::root());
}
