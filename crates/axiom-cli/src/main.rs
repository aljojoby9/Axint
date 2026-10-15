//! `axiom` command line.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use axiom_git_bridge::{clone_repository, push_bookmark};
use axiom_storage::Repository;
use axiom_vfs::{watch, DirtyRing};
use clap::{Parser, Subcommand};
use eyre::WrapErr;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Terminal;

#[derive(Debug, Parser)]
#[command(name = "axiom", version, about = "Axiom version control")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create an `.axiom` repository in the current directory, or in `path`.
    Init {
        /// Directory to initialize.
        path: Option<PathBuf>,
        /// Author name stored in the repository config.
        #[arg(long, default_value = "Axiom User")]
        name: String,
        /// Author email stored in the repository config.
        #[arg(long, default_value = "user@localhost")]
        email: String,
    },
    /// Show working-tree changes.
    Status,
    /// Record the working tree as a signed commit.
    Commit {
        /// Commit message.
        #[arg(short, long)]
        message: String,
    },
    /// Move to another bookmark, keeping untracked files that do not collide.
    Switch {
        /// Bookmark to check out.
        bookmark: String,
    },
    /// Merge a bookmark into the current one. Conflicts stay in the commit.
    Merge {
        /// Bookmark to merge.
        bookmark: String,
        /// Merge message. A default is used when this is omitted.
        #[arg(short, long)]
        message: Option<String>,
    },
    /// Reverse the last journal entry.
    Undo,
    /// Reapply an undone journal entry.
    Redo,
    /// Show commit history. Uses a scrollable view on a terminal.
    Log {
        /// Print plain text even when stdout is a terminal.
        #[arg(long)]
        plain: bool,
    },
    /// Create a bookmark at the current commit.
    Bookmark {
        /// New bookmark name.
        name: String,
    },
    /// Clone a Git remote, including a GitHub SSH or HTTPS URL.
    Clone {
        /// Git URL, for example `git@github.com:org/repo.git`.
        url: String,
        /// Destination directory.
        dest: PathBuf,
        /// Imported author name.
        #[arg(long, default_value = "Axiom User")]
        name: String,
        /// Imported author email.
        #[arg(long, default_value = "user@localhost")]
        email: String,
    },
    /// Push a bookmark to a Git remote.
    Push {
        /// Remote name previously added with `axiom remote add`.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Bookmark to publish. Defaults to the current bookmark.
        #[arg(long)]
        bookmark: Option<String>,
    },
    /// Manage Git remotes.
    Remote {
        #[command(subcommand)]
        action: RemoteCommand,
    },
    /// Watch the working tree and record dirty paths for fast status.
    Watch,
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// Save a remote URL.
    Add {
        /// Remote name, usually `origin`.
        name: String,
        /// Git or Axiom URL.
        url: String,
    },
}

fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    match Cli::parse().command {
        Command::Init { path, name, email } => {
            let path = path.unwrap_or(std::env::current_dir()?);
            Repository::init(&path, &name, &email)?;
            println!("initialized axiom repository in {}", path.display());
        }
        Command::Status => print_status(&open_here()?)?,
        Command::Commit { message } => {
            let mut repo = open_here()?;
            let id = repo.commit(&message)?;
            println!("{id}");
        }
        Command::Switch { bookmark } => {
            let mut repo = open_here()?;
            repo.switch(&bookmark)?;
            println!("switched to {bookmark}");
        }
        Command::Merge { bookmark, message } => {
            let mut repo = open_here()?;
            let outcome = repo.merge(&bookmark, message.as_deref())?;
            if outcome.conflicts.is_empty() {
                println!("merged {bookmark}");
            } else {
                println!("merged {bookmark} with {} conflict(s)", outcome.conflicts.len());
            }
        }
        Command::Undo => {
            let mut repo = open_here()?;
            let head = repo.undo()?;
            println!("undo -> {head}");
        }
        Command::Redo => {
            let mut repo = open_here()?;
            let head = repo.redo()?;
            println!("redo -> {head}");
        }
        Command::Log { plain } => {
            let repo = open_here()?;
            let history = repo.log()?;
            if plain || !io::stdout().is_terminal() {
                for commit in history {
                    println!("{} {}", &commit.id.to_hex()[..12], commit.summary);
                }
            } else {
                run_log(&history)?;
            }
        }
        Command::Bookmark { name } => {
            let mut repo = open_here()?;
            repo.create_bookmark(&name)?;
            println!("created bookmark {name}");
        }
        Command::Clone { url, dest, name, email } => {
            clone_repository(&url, &dest, &name, &email)?;
            println!("cloned {url} into {}", dest.display());
        }
        Command::Push { remote, bookmark } => {
            let repo = open_here()?;
            let url = repo
                .remote_url(&remote)
                .ok_or_else(|| eyre::eyre!("remote `{remote}` is not configured"))?
                .to_owned();
            let bookmark = match bookmark {
                Some(name) => name,
                None => repo.current_bookmark()?,
            };
            let oid = push_bookmark(&repo, &bookmark, &url)?;
            println!("pushed {bookmark} to {remote} as {oid}");
        }
        Command::Remote { action } => match action {
            RemoteCommand::Add { name, url } => {
                let mut repo = open_here()?;
                repo.add_remote(&name, &url)?;
                println!("remote {name} -> {url}");
            }
        },
        Command::Watch => {
            let repo = open_here()?;
            let ring = DirtyRing::new(65_536);
            let _watcher = watch(repo.root(), ring.clone())?;
            println!("watching {}", repo.root().display());
            loop {
                std::thread::sleep(Duration::from_secs(2));
                match ring.drain() {
                    Ok(events) if events.is_empty() => {}
                    Ok(events) => {
                        for event in events {
                            println!("{:?} {}", event.kind, event.path);
                        }
                    }
                    Err(_) => println!("dirty ring overflowed; the next status will scan"),
                }
            }
        }
    }
    Ok(())
}

fn open_here() -> eyre::Result<Repository> {
    let cwd = std::env::current_dir()?;
    Repository::discover(&cwd).wrap_err("this directory is not inside an axiom repository")
}

fn print_status(repo: &Repository) -> eyre::Result<()> {
    let changes = repo.status_scan()?;
    if changes.is_empty() {
        println!("clean");
        return Ok(());
    }
    for change in changes {
        let label = match change.kind {
            axiom_storage::ChangeKind::Added => "added",
            axiom_storage::ChangeKind::Modified => "modified",
            axiom_storage::ChangeKind::Deleted => "deleted",
        };
        println!("{label}\t{}", change.path);
    }
    Ok(())
}

fn run_log(history: &[axiom_storage::CommitSummary]) -> eyre::Result<()> {
    let mut stdout = io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let items: Vec<ListItem> = history
        .iter()
        .map(|commit| ListItem::new(format!("{} {}", &commit.id.to_hex()[..12], commit.summary)))
        .collect();
    loop {
        terminal.draw(|frame| {
            let area = frame.size();
            let list = List::new(items.clone()).block(
                Block::default()
                    .title("axiom log  (q to quit)")
                    .borders(Borders::ALL),
            );
            frame.render_widget(list, area);
            let _ = Paragraph::new("");
        })?;
        if crossterm::event::poll(Duration::from_millis(250))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if matches!(
                    key.code,
                    crossterm::event::KeyCode::Char('q') | crossterm::event::KeyCode::Esc
                ) {
                    break;
                }
            }
        }
    }
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    Ok(())
}
