mod priority_queue;

use crate::{
    app::priority_queue::PriorityQueue,
    git::{
        build_git_history_graph, Branch, Commit, Diff, HistoryGraph, ObjectId, ReferenceId, Repo,
    },
};

use anyhow::{bail, Context, Error, Result};
use log::{debug, error, info};
use notify::{self, RawEvent, RecommendedWatcher, RecursiveMode, Watcher};

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Eq, PartialEq, Default)]
pub struct RepoState {
    pub(crate) repo: PathBuf,
    pub(crate) head: ReferenceId,
    pub(crate) branches: Vec<Branch>,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct ViewState {
    pub(crate) selected_references: Vec<ReferenceId>,
}

impl ViewState {
    pub(crate) fn update_with_repo_state(&mut self, repo_state: &RepoState) {
        let selected_references = std::mem::take(&mut self.selected_references);
        let had_any_branches = !selected_references.is_empty();
        self.selected_references = selected_references
            .into_iter()
            .filter(|selected| repo_state.branches.iter().any(|b| &b.id == selected))
            .collect();

        if self.selected_references.is_empty() && had_any_branches {
            self.selected_references = vec![ReferenceId::head()]
        }
    }
}

pub enum AppRequest {
    OpenRepo(PathBuf),
    GetCommitGraph {
        expected_repo: PathBuf,
        // Unique ID from the UI for preempting
        viewer_id: String,
        view_state: ViewState,
    },
    Refresh,
    GetCommit {
        expected_repo: PathBuf,
        id: ObjectId,
    },
    GetDiff {
        expected_repo: PathBuf,
        from: ObjectId,
        to: ObjectId,
    },
    Checkout(RepoState, ReferenceId),
    ExecuteGitCommand(RepoState, String),
}

pub enum AppEvent {
    OutputLogged(String),
    RepoStateUpdated(RepoState),
    CommitGraphFetched(ViewState, HistoryGraph),
    CommitFetched { repo: PathBuf, commit: Commit },
    DiffFetched { repo: PathBuf, diff: Diff },
    Error(String),
}

pub struct App {
    tx: Sender<AppEvent>,
    rx: PriorityQueue,
    notifier: RecommendedWatcher,
    repo: Option<Repo>,
}

impl App {
    pub fn new(
        event_tx: Sender<AppEvent>,
        request_tx: Sender<AppRequest>,
        request_rx: Receiver<AppRequest>,
    ) -> Result<App> {
        Ok(App {
            tx: event_tx,
            rx: PriorityQueue::new(request_rx),
            notifier: spawn_watcher(request_tx)?,
            repo: None,
        })
    }

    pub fn run(&mut self) {
        while let Ok(req) = self.rx.recv() {
            if let Err(e) = self.handle_req(req) {
                let err_s = format!("{:?}", e);
                error!("{}", err_s);
                let _ = self.tx.send(AppEvent::Error(err_s));
            }
        }
    }

    fn handle_req(&mut self, req: AppRequest) -> Result<()> {
        match req {
            AppRequest::Checkout(repo_state, reference_id) => {
                if self.get_repo_state()? != repo_state {
                    bail!(
                        "Repo state has changed since checkout {} request",
                        reference_id
                    );
                }

                let repo = match &self.repo {
                    Some(repo) => repo,
                    None => bail!("Invalid repo"),
                };

                let ref_unescaped = reference_id.to_string();
                let ref_escaped = shell_escape::escape((&ref_unescaped).into());
                let git_dir = repo.git_dir();

                let command = format!("git checkout {}", ref_escaped);

                self.tx
                    .send(AppEvent::OutputLogged(command.clone()))
                    .context("Failed to send response to gui")?;

                let parsed = execute_command_in_repo(git_dir, &command)?;

                self.tx
                    .send(AppEvent::OutputLogged(parsed))
                    .context("Failed to send response to gui")?;
            }
            AppRequest::ExecuteGitCommand(repo_state, cmd) => {
                if !cmd.starts_with("git ") {
                    bail!("Invalid git command: {}", cmd);
                }

                if self.get_repo_state()? != repo_state {
                    bail!("Repo state has changed since {} was executed", cmd);
                }

                let repo = match &self.repo {
                    Some(repo) => repo,
                    None => bail!("Invalid repo"),
                };

                let git_dir = repo.git_dir();

                let cmd = cmd.trim();

                self.tx
                    .send(AppEvent::OutputLogged(cmd.to_string()))
                    .context("Failed to send response to gui")?;

                let parsed = execute_command_in_repo(git_dir, cmd)?;

                self.tx
                    .send(AppEvent::OutputLogged(parsed))
                    .context("Failed to send response to gui")?;
            }
            AppRequest::GetCommit { expected_repo, id } => match &mut self.repo {
                Some(repo) => {
                    if repo.git_dir() != expected_repo {
                        debug!(
                            "Ignoring commit request for {}, {} is no longer open",
                            id,
                            expected_repo.display()
                        );
                        return Ok(());
                    }

                    self.tx
                        .send(AppEvent::CommitFetched {
                            repo: expected_repo,
                            commit: repo.get_commit(&id)?,
                        })
                        .context("Failed to send commit fetched")?;
                }
                None => {
                    bail!("Commit requested without valid repo");
                }
            },
            AppRequest::GetDiff {
                expected_repo,
                from,
                to,
            } => {
                let repo = self
                    .repo
                    .as_mut()
                    .ok_or_else(|| Error::msg("Commit requested without valid repo"))?;

                if expected_repo != repo.git_dir() {
                    debug!(
                        "Ignoring diff request for {} -> {}, {} is no longer open",
                        from,
                        to,
                        expected_repo.display()
                    );
                    return Ok(());
                }

                let diff = repo
                    .diff(&from, &to)
                    .with_context(|| format!("Failed to retrieve diff for {} -> {}", from, to))?;

                self.tx.send(AppEvent::DiffFetched {
                    repo: expected_repo,
                    diff,
                })?;
            }
            AppRequest::OpenRepo(path) => {
                let mut repo = Repo::new(path.clone()).context("Failed to load git history")?;

                let repo_state = get_repo_state(&mut repo)?;

                self.tx
                    .send(AppEvent::RepoStateUpdated(repo_state))
                    .context("Failed to send response branches")?;

                self.repo = Some(repo);
                // FIXME: There is a race here where if a new object is created between when we
                // fetched the repo state and now we will not update the repo, however if we move
                // this up and changing repos fails the old path will not be watched anymore, and
                // we may miss an update in the old repo.
                self.notifier.watch(path, RecursiveMode::Recursive)?;
            }
            AppRequest::GetCommitGraph {
                expected_repo,
                view_state,
                ..
            } => match &mut self.repo {
                Some(repo) => {
                    if repo.git_dir() != expected_repo {
                        bail!(
                            "Current repo does not match expected repo: {}, {}",
                            repo.git_dir().display(),
                            expected_repo.display()
                        );
                    }

                    let heads = view_state
                        .selected_references
                        .iter()
                        .map(|id| repo.find_reference_object(id))
                        .collect::<Result<Vec<_>>>()?;

                    let graph = build_git_history_graph(repo, &heads)?;

                    self.tx
                        .send(AppEvent::CommitGraphFetched(view_state, graph))
                        .context("Failed to send response commit log")?;
                }
                None => {
                    bail!("Branches selected without valid repo");
                }
            },
            AppRequest::Refresh => {
                let repo_state = self.get_repo_state()?;

                self.tx
                    .send(AppEvent::RepoStateUpdated(repo_state))
                    .context("Failed to send response branches")?;
            }
        }

        Ok(())
    }

    fn get_repo_state(&mut self) -> Result<RepoState> {
        let repo = self.repo.as_mut().ok_or_else(|| Error::msg("No repo"))?;
        get_repo_state(repo)
    }
}

fn execute_command_in_repo(repo: &Path, shell_string: &str) -> Result<String> {
    // NOTE: This looks really wrong, and that's because it is to some extent. We should not be
    // running bash commands for every git command we want to run. But this has the large benefit
    // that every action the program executes can be copy pasted by a user and run again. This
    // makes interop with command line users very nice and is worth the architectural incorrectness
    // of shelling out
    let mut bash_cmd = OsString::new();
    bash_cmd.push("cd '");
    bash_cmd.push(repo.as_os_str());
    bash_cmd.push("'; ");
    bash_cmd.push(shell_string);
    bash_cmd.push(" 2>&1");

    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg(bash_cmd)
        .output()
        .with_context(|| format!("Failed to run {}", shell_string))?;

    let parsed =
        String::from_utf8(output.stdout).context("Git response was not a valid utf8 string")?;

    Ok(parsed.trim().to_string())
}

fn get_repo_state(repo: &mut Repo) -> Result<RepoState> {
    let mut branches = vec![Ok(Branch {
        head: repo.find_reference_object(&ReferenceId::head())?,
        id: ReferenceId::head(),
    })];
    branches.extend(repo.branches().context("Failed to retrieve branches")?);
    let branches = branches.into_iter().collect::<Result<_>>()?;
    let head = repo.resolve_reference(&ReferenceId::head())?;

    Ok(RepoState {
        repo: repo.git_dir().to_path_buf(),
        head,
        branches,
    })
}

fn path_is_lock_file(path: Option<&Path>) -> bool {
    let path = match path {
        Some(p) => p,
        None => return false,
    };

    let extension = match path.extension() {
        Some(e) => e,
        None => return false,
    };

    extension == OsStr::new("lock")
}

fn debounce_event(notifier_rx: &Receiver<RawEvent>) {
    let debounce_max = Instant::now() + Duration::from_secs(2);
    let debounce_period = Duration::from_millis(500);

    while let Ok(_event) = notifier_rx.recv_timeout(debounce_period) {
        if Instant::now() > debounce_max {
            return;
        }
    }
}

fn spawn_watcher(app_tx: Sender<AppRequest>) -> Result<RecommendedWatcher> {
    let (notifier_tx, notifier_rx) = mpsc::channel();
    let notifier = notify::raw_watcher(notifier_tx)?;
    thread::spawn(move || {
        while let Ok(event) = notifier_rx.recv() {
            if path_is_lock_file(event.path.as_deref()) {
                continue;
            }

            // Debounce to avoid spam refreshing
            debounce_event(&notifier_rx);

            if let Err(_e) = app_tx.send(AppRequest::Refresh) {
                info!("App handle is no longer valid, closing watcher");
                return;
            }
        }
    });

    Ok(notifier)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_lock_file_check() {
        assert_eq!(path_is_lock_file(None), false);
        assert_eq!(path_is_lock_file(Some(&Path::new("test.test"))), false);
        assert_eq!(path_is_lock_file(Some(&Path::new("test.lock"))), true);
        // I don't know what I think this should be, but lets at least prove that we know how it
        // works
        assert_eq!(path_is_lock_file(Some(&Path::new(".lock"))), false);
        assert_eq!(path_is_lock_file(Some(&Path::new("lock"))), false);
        assert_eq!(path_is_lock_file(Some(&Path::new("test/test.lock"))), true);
    }

    #[test]
    fn view_state_deleted_branch() -> Result<()> {
        let mut view_state = ViewState {
            selected_references: vec![
                ReferenceId::head(),
                ReferenceId::RemoteBranch("Test".to_string()),
                ReferenceId::LocalBranch("Test".to_string()),
            ],
        };

        view_state.update_with_repo_state(&RepoState {
            repo: PathBuf::new(),
            head: ReferenceId::Unknown,
            branches: vec![
                Branch {
                    id: ReferenceId::head(),
                    head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse()?,
                },
                Branch {
                    id: ReferenceId::RemoteBranch("Test".to_string()),
                    head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse()?,
                },
            ],
        });

        assert_eq!(view_state.selected_references.len(), 2);
        assert_eq!(
            view_state.selected_references,
            &[
                ReferenceId::head(),
                ReferenceId::RemoteBranch("Test".to_string())
            ]
        );
        Ok(())
    }

    #[test]
    fn view_state_preserve_no_selection() -> Result<()> {
        let mut view_state = ViewState {
            selected_references: vec![],
        };

        view_state.update_with_repo_state(&RepoState {
            repo: PathBuf::new(),
            head: ReferenceId::Unknown,
            branches: vec![Branch {
                id: ReferenceId::head(),
                head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse()?,
            }],
        });

        assert_eq!(view_state.selected_references.len(), 0);
        assert_eq!(view_state.selected_references, &[]);

        Ok(())
    }

    #[test]
    fn view_state_swap_to_head() -> Result<()> {
        let mut view_state = ViewState {
            selected_references: vec![ReferenceId::LocalBranch("master".into())],
        };

        view_state.update_with_repo_state(&RepoState {
            repo: PathBuf::new(),
            head: ReferenceId::Unknown,
            branches: vec![Branch {
                id: ReferenceId::head(),
                head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".parse()?,
            }],
        });

        // Only selected branch remove, swap to HEAD
        assert_eq!(view_state.selected_references.len(), 1);
        assert_eq!(view_state.selected_references, &[ReferenceId::head()]);

        Ok(())
    }
}
