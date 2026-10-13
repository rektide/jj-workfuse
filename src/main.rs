use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use fuser::MountOption;
use jj_workfuse::filesystem::ReadOnlyFs;
use jj_workfuse::projection::{ObjectKey, ObjectKind, Projection, SourceRepo};

#[derive(Parser)]
#[command(
    version,
    about = "Read-only lazy subtree views of pinned Jujutsu revisions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Selection {
    /// A jj workspace/path beneath it, .jj, or .jj/repo metadata directory
    #[arg(env = "JJ_WORKFUSE_REPOSITORY")]
    repository: PathBuf,
    /// Built-in jj revset selecting exactly one commit; no snapshot is taken
    #[arg(long, default_value = "@", env = "JJ_WORKFUSE_REVISION")]
    revision: String,
    /// Normalized source directory relative to the repository root
    #[arg(long, default_value = ".", env = "JJ_WORKFUSE_SUBDIR")]
    subdir: String,
    /// Workspace identity for @ when opening metadata without a workspace
    #[arg(long, env = "JJ_WORKFUSE_WORKSPACE")]
    workspace: Option<String>,
}

impl Selection {
    fn project(&self) -> Result<Projection> {
        SourceRepo::open(&self.repository, self.workspace.as_deref())?
            .project(&self.revision, &self.subdir)
    }
}

#[derive(Subcommand)]
enum Command {
    /// Mount an immutable directory view in the foreground (Ctrl-C to unmount)
    Mount {
        #[command(flatten)]
        selection: Selection,
        /// Existing empty directory, separate from source repository metadata
        #[arg(env = "JJ_WORKFUSE_MOUNTPOINT")]
        mountpoint: PathBuf,
        /// Maximum retained file-content cache in MiB (not jj metadata caches)
        #[arg(long, default_value_t = 64, env = "JJ_WORKFUSE_CACHE_MIB")]
        cache_mib: usize,
    },
    /// Print pinned provenance and selected-root state as JSON, without blobs
    Inspect {
        #[command(flatten)]
        selection: Selection,
    },
    /// List one projected directory as JSON, without reading file blobs
    Ls {
        #[command(flatten)]
        selection: Selection,
        #[arg(default_value = ".")]
        path: String,
    },
    /// Read a resolved projected file (or the recorded symlink target)
    Cat {
        #[command(flatten)]
        selection: Selection,
        path: String,
    },
    /// Enumerate only objects reachable inside this view, including conflict terms
    Objects {
        #[command(flatten)]
        selection: Selection,
    },
    /// Read a reachable object; tree objects are JSON, file/symlink objects bytes
    Object {
        #[command(flatten)]
        selection: Selection,
        #[arg(long, value_enum)]
        kind: Kind,
        #[arg(long)]
        id: String,
    },
    /// Project all ancestors, retaining unchanged vertices and merge topology
    History {
        #[command(flatten)]
        selection: Selection,
        /// Explicit guard; exceeding it fails rather than returning a partial graph
        #[arg(long, default_value_t = 10_000)]
        max_commits: usize,
    },
}

#[derive(Clone, ValueEnum)]
enum Kind {
    Tree,
    File,
    Symlink,
}

fn json(value: &impl serde::Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Inspect { selection } => json(selection.project()?.provenance()),
        Command::Ls { selection, path } => json(&selection.project()?.list(&path)?),
        Command::Cat { selection, path } => {
            io::stdout()
                .lock()
                .write_all(&selection.project()?.read(&path)?)?;
            Ok(())
        }
        Command::Objects { selection } => json(&selection.project()?.objects()?),
        Command::Object {
            selection,
            kind,
            id,
        } => {
            let kind = match kind {
                Kind::Tree => ObjectKind::Tree,
                Kind::File => ObjectKind::File,
                Kind::Symlink => ObjectKind::Symlink,
            };
            io::stdout()
                .lock()
                .write_all(&selection.project()?.read_object(&ObjectKey { kind, id })?)?;
            Ok(())
        }
        Command::History {
            selection,
            max_commits,
        } => json(&selection.project()?.history(max_commits)?),
        Command::Mount {
            selection,
            mountpoint,
            cache_mib,
        } => {
            let projection = selection.project()?;
            let mountpoint = mountpoint
                .canonicalize()
                .context("mountpoint must be an existing empty directory")?;
            ensure!(mountpoint.is_dir(), "mountpoint must be a directory");
            ensure!(
                !mountpoint.starts_with(&projection.provenance().repository),
                "mountpoint must not hide source repository metadata"
            );
            ensure!(
                mountpoint.read_dir()?.next().is_none(),
                "mountpoint must be empty"
            );
            let budget = cache_mib
                .checked_mul(1024 * 1024)
                .context("cache budget overflow")?;
            let uid = nix::unistd::geteuid().as_raw();
            let gid = nix::unistd::getegid().as_raw();
            // Flattened for the sd_notify STATUS line; newlines would corrupt
            // the key=value protocol, and arbitrary paths choose the rest.
            let flat = |value: String| value.replace('\n', " ");
            let pinned = format!(
                "pinned {}:{}",
                projection.provenance().source_commit,
                flat(projection.provenance().subdir.clone()),
            );
            let source_operation = flat(projection.provenance().source_operation.clone());
            eprintln!(
                "{pinned} at source operation {source_operation}\nProjected tree {}\nMounting {} (read-only); Ctrl-C or fusermount3 -u to unmount",
                projection.provenance().projected_tree,
                mountpoint.display()
            );
            let fs = ReadOnlyFs::new(projection, budget, uid, gid)?;
            let options = [
                MountOption::RO,
                MountOption::NoDev,
                MountOption::NoSuid,
                MountOption::NoAtime,
                MountOption::DefaultPermissions,
                MountOption::FSName("jj-workfuse".into()),
                MountOption::Subtype("jj-workfuse".into()),
            ];
            let mut signals = signal_hook::iterator::Signals::new([
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
            ])?;
            let handle = signals.handle();
            let mut session = fuser::Session::new(fs, &mountpoint, &options).context(
                "FUSE mount failed; check /dev/fuse, fusermount3, and host mount permissions",
            )?;
            // The mount is established once Session::new returns, so this is
            // the correct readiness point for Type=notify service units. A
            // delivery failure is loud but not fatal: the mount works.
            let status = format!(
                "READY=1\nSTATUS={pinned} at {} (source operation {source_operation})",
                flat(mountpoint.display().to_string()),
            );
            if let Err(error) = jj_workfuse::notify::notify(&status) {
                eprintln!("warning: service-manager readiness notification failed: {error}");
            }
            let mut unmounter = session.unmount_callable();
            let signal_thread = std::thread::spawn(move || {
                if signals.forever().next().is_some()
                    && let Err(error) = unmounter.unmount()
                {
                    eprintln!("unmount failed: {error}");
                }
            });
            let result = session.run();
            handle.close();
            signal_thread
                .join()
                .map_err(|_| anyhow::anyhow!("signal handler thread panicked"))?;
            result.context("FUSE session failed")
        }
    }
}
