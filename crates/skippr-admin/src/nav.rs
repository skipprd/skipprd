use std::sync::{Arc, Mutex};

use react_core::scope::{ProjectId, RequestScope, TenantId, WorkspaceId};
use react_core::storage::StorageAdapter;
use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

/// Shared state that both ShellState and ShellHelper can read.
#[derive(Default)]
pub struct Shared {
    depth: usize,
    children: Vec<String>,
}

pub struct ShellState {
    scope: Vec<String>,
    storage: Arc<dyn StorageAdapter>,
    shared: Arc<Mutex<Shared>>,
}

impl ShellState {
    pub fn new(storage: Arc<dyn StorageAdapter>) -> (Self, Arc<Mutex<Shared>>) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let state = Self {
            scope: Vec::new(),
            storage,
            shared: shared.clone(),
        };
        (state, shared)
    }

    pub fn depth(&self) -> usize {
        self.scope.len()
    }

    pub fn prompt(&self) -> String {
        if self.scope.is_empty() {
            "skippr> ".to_string()
        } else {
            format!("[{}]> ", self.scope.join("/"))
        }
    }

    pub fn cd_into(&mut self, name: String) {
        self.scope.push(name);
        self.sync_shared(&[]);
    }

    pub fn cd_up(&mut self) {
        self.scope.pop();
        self.sync_shared(&[]);
    }

    pub fn children(&self) -> Vec<String> {
        self.shared.lock().unwrap().children.clone()
    }

    pub fn tenant(&self) -> Option<&str> {
        self.scope.first().map(|s| s.as_str())
    }

    pub fn s3_prefix(&self) -> String {
        if self.scope.is_empty() {
            String::new()
        } else {
            format!("{}/", self.scope.join("/"))
        }
    }

    pub fn request_scope(&self) -> Option<RequestScope> {
        if self.scope.len() < 3 {
            return None;
        }
        let tenant = TenantId::parse(&self.scope[0]).ok()?;
        let workspace = WorkspaceId::parse(&self.scope[1]).ok()?;
        let project = ProjectId::parse(&self.scope[2]).ok()?;
        Some(RequestScope {
            tenant,
            workspace,
            project_id: project,
        })
    }

    pub async fn refresh_children(&mut self) {
        let prefix = self.s3_prefix();
        match self.storage.list_prefix(&prefix).await {
            Ok(keys) => {
                let mut children: Vec<String> = keys
                    .iter()
                    .filter_map(|k| {
                        let rest = k.strip_prefix(&prefix)?;
                        let name = rest.split('/').next()?;
                        if name.is_empty() {
                            return None;
                        }
                        Some(name.to_string())
                    })
                    .collect();
                children.sort();
                children.dedup();
                self.sync_shared(&children);
            }
            Err(_) => {
                self.sync_shared(&[]);
            }
        }
    }

    fn sync_shared(&self, children: &[String]) {
        let mut s = self.shared.lock().unwrap();
        s.depth = self.scope.len();
        s.children = children.to_vec();
    }
}

const COMMANDS: [[&str; 10]; 4] = [
    ["ls", "cd", "help", "quit", "", "", "", "", "", ""],
    [
        "ls", "cd", "..", "account", "ledger", "help", "quit", "", "", "",
    ],
    ["ls", "cd", "..", "help", "quit", "", "", "", "", ""],
    [
        "ls", "threads", "thread", "log", "debug", "feedback", "fdebug", "..", "help", "quit",
    ],
];

fn commands_for_depth(depth: usize) -> Vec<&'static str> {
    let idx = depth.min(3);
    COMMANDS[idx]
        .iter()
        .copied()
        .filter(|s| !s.is_empty())
        .collect()
}

pub struct ShellHelper {
    shared: Arc<Mutex<Shared>>,
}

impl ShellHelper {
    pub fn new(shared: Arc<Mutex<Shared>>) -> Self {
        Self { shared }
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let input = &line[..pos];
        let s = self.shared.lock().unwrap();

        // After "cd ", complete against children
        if let Some(arg) = input.strip_prefix("cd ") {
            let arg = arg.trim_start();
            let candidates: Vec<Pair> = s
                .children
                .iter()
                .filter(|c| c.starts_with(arg))
                .map(|c| Pair {
                    display: c.clone(),
                    replacement: c.clone(),
                })
                .collect();
            let start = pos - arg.len();
            return Ok((start, candidates));
        }

        // At the start of the line, complete commands
        if !input.contains(' ') {
            let candidates: Vec<Pair> = commands_for_depth(s.depth)
                .into_iter()
                .filter(|cmd| cmd.starts_with(input))
                .map(|cmd| Pair {
                    display: cmd.to_string(),
                    replacement: cmd.to_string(),
                })
                .collect();
            return Ok((0, candidates));
        }

        Ok((pos, vec![]))
    }
}

impl Hinter for ShellHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        // Inline hint for "cd " — show first matching child greyed out
        if let Some(arg) = line[..pos].strip_prefix("cd ") {
            let arg = arg.trim_start();
            if arg.is_empty() {
                return None;
            }
            let s = self.shared.lock().unwrap();
            for child in &s.children {
                if child.starts_with(arg) && child.len() > arg.len() {
                    return Some(child[arg.len()..].to_string());
                }
            }
        }
        None
    }
}

impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}
impl Helper for ShellHelper {}
