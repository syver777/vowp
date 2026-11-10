//! In-memory [`WindowManager`] for core and CLI tests.
//!
//! It records sessions, windows and specs and enforces the same safety rules
//! as the real implementation: `vowp-` sessions only, validated names, tagged
//! windows only for kills, and no duplicate windows.
//!
//! Spec: `docs/design/crates/vowp-tmux.md` §2.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::TmuxError;
use crate::manager::{
    LayoutSpec, WindowInfo, WindowManager, WindowSpec, require_vowp_session, require_window_name,
    session_name_for_project,
};

/// In-memory window manager.
#[derive(Debug, Clone, Default)]
pub struct FakeWindows {
    inner: Arc<Mutex<State>>,
}

#[derive(Debug, Clone, Default)]
struct State {
    sessions: BTreeMap<String, Session>,
    killed: Vec<(String, String)>,
    next_pid: u32,
}

#[derive(Debug, Clone, Default)]
struct Session {
    windows: Vec<Window>,
    next_index: u32,
}

#[derive(Debug, Clone)]
struct Window {
    index: u32,
    name: String,
    spec: WindowSpec,
    tagged: bool,
    active: bool,
    pane_pid: u32,
}

impl FakeWindows {
    /// Create an empty fake manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an untagged window, simulating a foreign window in a `vowp-`
    /// session. Invalid names are ignored.
    pub fn untagged_window(&self, session: &str, name: &str) {
        if require_vowp_session(session).is_err() || require_window_name(name).is_err() {
            return;
        }
        let mut state = self.state();
        state.sessions.entry(session.to_owned()).or_default();
        push_window(&mut state, session, name, WindowSpec::default(), false);
    }

    /// Window specs for a session, in creation order.
    pub fn specs(&self, session: &str) -> Vec<(String, WindowSpec)> {
        let state = self.state();
        state
            .sessions
            .get(session)
            .map(|s| s.windows.iter().map(|w| (w.name.clone(), w.spec.clone())).collect())
            .unwrap_or_default()
    }

    /// Windows killed so far, as `(session, window name)`, in kill order.
    pub fn killed(&self) -> Vec<(String, String)> {
        self.state().killed.clone()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn create_board_session(
        &self,
        session: &str,
        argv: &[String],
        cwd: Option<std::path::PathBuf>,
    ) -> Result<(), TmuxError> {
        if argv.is_empty() {
            return Err(TmuxError::EmptyCommand);
        }
        if let Some(cwd) = &cwd {
            if !cwd.is_dir() {
                return Err(TmuxError::InvalidCwd(cwd.display().to_string()));
            }
        }
        let mut state = self.state();
        state.sessions.entry(session.to_owned()).or_default();
        push_window(
            &mut state,
            session,
            "board",
            WindowSpec {
                argv: argv.to_vec(),
                cwd,
                env: Vec::new(),
            },
            true,
        );
        Ok(())
    }
}

impl WindowManager for FakeWindows {
    fn ensure_session(&self, session: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        let mut state = self.state();
        if !state.sessions.contains_key(session) {
            state.sessions.insert(session.to_owned(), Session::default());
            push_window(
                &mut state,
                session,
                "scratch",
                WindowSpec {
                    argv: vec!["sh".to_owned(), "-c".to_owned(), "exec cat".to_owned()],
                    ..WindowSpec::default()
                },
                true,
            );
        }
        Ok(())
    }

    fn new_window(&self, session: &str, name: &str, spec: &WindowSpec) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        if spec.argv.is_empty() {
            return Err(TmuxError::EmptyCommand);
        }
        if let Some(cwd) = &spec.cwd {
            if !cwd.is_dir() {
                return Err(TmuxError::InvalidCwd(cwd.display().to_string()));
            }
        }
        let mut state = self.state();
        let exists = state
            .sessions
            .get(session)
            .is_some_and(|s| s.windows.iter().any(|w| w.name == name));
        if exists {
            return Err(TmuxError::WindowExists {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        }
        state.sessions.entry(session.to_owned()).or_default();
        push_window(&mut state, session, name, spec.clone(), true);
        Ok(())
    }

    fn kill_window(&self, session: &str, name: &str) -> Result<(), TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        let mut state = self.state();
        let Some(found) = state.sessions.get_mut(session) else {
            return Err(TmuxError::WindowNotFound {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        };
        let Some(position) = found.windows.iter().position(|w| w.name == name) else {
            return Err(TmuxError::WindowNotFound {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        };
        if !found.windows[position].tagged {
            return Err(TmuxError::NotTagged {
                session: session.to_owned(),
                name: name.to_owned(),
            });
        }
        let removed = found.windows.remove(position);
        if removed.active {
            if let Some(next) = found.windows.first_mut() {
                next.active = true;
            }
        }
        state.killed.push((session.to_owned(), name.to_owned()));
        Ok(())
    }

    fn list_windows(&self, session: &str) -> Result<Vec<WindowInfo>, TmuxError> {
        require_vowp_session(session)?;
        let state = self.state();
        Ok(state
            .sessions
            .get(session)
            .map(|s| {
                s.windows
                    .iter()
                    .map(|w| WindowInfo {
                        index: w.index,
                        name: w.name.clone(),
                        tagged: w.tagged,
                        pane_pid: Some(w.pane_pid),
                        active: w.active,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn window_exists(&self, session: &str, name: &str) -> Result<bool, TmuxError> {
        require_vowp_session(session)?;
        require_window_name(name)?;
        let state = self.state();
        Ok(state
            .sessions
            .get(session)
            .is_some_and(|s| s.windows.iter().any(|w| w.name == name)))
    }

    fn ensure_layout(&self, project: &str, spec: &LayoutSpec) -> Result<(), TmuxError> {
        let session = session_name_for_project(project);
        require_vowp_session(&session)?;
        let exists = {
            let state = self.state();
            state.sessions.contains_key(&session)
        };
        if !exists {
            self.create_board_session(&session, &spec.board, spec.cwd.clone())?;
        } else {
            if !self.window_exists(&session, "board")? {
                self.new_window(
                    &session,
                    "board",
                    &WindowSpec {
                        argv: spec.board.clone(),
                        cwd: spec.cwd.clone(),
                        env: Vec::new(),
                    },
                )?;
            }
            if self.window_exists(&session, "scratch")? {
                self.kill_window(&session, "scratch")?;
            }
        }
        let windows = [
            ("supervisor", &spec.supervisor),
            ("planner", &spec.planner),
            ("keeper", &spec.keeper),
            ("inbox", &spec.inbox),
        ];
        for (name, argv) in windows {
            if !self.window_exists(&session, name)? {
                self.new_window(
                    &session,
                    name,
                    &WindowSpec {
                        argv: argv.clone(),
                        cwd: spec.cwd.clone(),
                        env: Vec::new(),
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn push_window(state: &mut State, session_name: &str, name: &str, spec: WindowSpec, tagged: bool) {
    let pane_pid = 1000 + state.next_pid;
    state.next_pid += 1;
    let session = state.sessions.entry(session_name.to_owned()).or_default();
    let index = session.next_index;
    session.next_index += 1;
    let active = session.windows.is_empty();
    session.windows.push(Window {
        index,
        name: name.to_owned(),
        spec,
        tagged,
        active,
        pane_pid,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::WindowManager;

    fn sleep_spec() -> WindowSpec {
        WindowSpec {
            argv: vec!["sleep".to_owned(), "30".to_owned()],
            ..WindowSpec::default()
        }
    }

    #[test]
    fn fake_enforces_same_rules() {
        let fake = FakeWindows::new();
        fake.ensure_session("vowp-demo").unwrap();
        fake.new_window("vowp-demo", "job-1", &sleep_spec()).unwrap();

        assert!(matches!(
            fake.new_window("other", "job-2", &sleep_spec()),
            Err(TmuxError::ForeignSession(_))
        ));
        assert!(matches!(
            fake.new_window("vowp-demo", "bad name", &sleep_spec()),
            Err(TmuxError::InvalidName { .. })
        ));
        assert!(matches!(
            fake.new_window("vowp-demo", "job-1", &sleep_spec()),
            Err(TmuxError::WindowExists { .. })
        ));
        assert!(matches!(
            fake.new_window("vowp-demo", "job-empty", &WindowSpec::default()),
            Err(TmuxError::EmptyCommand)
        ));

        let windows = fake.list_windows("vowp-demo").unwrap();
        assert!(windows.iter().all(|w| w.tagged));
        assert!(windows.iter().any(|w| w.name == "job-1"));

        fake.untagged_window("vowp-demo", "foreign");
        assert!(matches!(
            fake.kill_window("vowp-demo", "foreign"),
            Err(TmuxError::NotTagged { .. })
        ));
        assert!(fake.window_exists("vowp-demo", "foreign").unwrap());

        fake.kill_window("vowp-demo", "job-1").unwrap();
        assert_eq!(fake.killed(), vec![("vowp-demo".to_owned(), "job-1".to_owned())]);
        assert!(!fake.window_exists("vowp-demo", "job-1").unwrap());
    }

    #[test]
    fn fake_layout_rejects_bad_cwd() {
        let fake = FakeWindows::new();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let spec = LayoutSpec {
            board: vec!["sleep".to_owned(), "30".to_owned()],
            supervisor: vec!["sleep".to_owned(), "30".to_owned()],
            planner: vec!["sleep".to_owned(), "30".to_owned()],
            keeper: vec!["sleep".to_owned(), "30".to_owned()],
            inbox: vec!["sleep".to_owned(), "30".to_owned()],
            cwd: Some(missing),
        };
        assert!(matches!(
            fake.ensure_layout("bad-cwd", &spec),
            Err(TmuxError::InvalidCwd(_))
        ));
        assert!(fake.list_windows("vowp-bad-cwd").unwrap().is_empty());
    }
}
