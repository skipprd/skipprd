use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Spinner frames (Unicode)
const FRAMES: &[&str] = &["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Pending,
    Active,
    Completed,
}

struct Task {
    name: String,
    state: TaskState,
}

struct UiState {
    tasks: Vec<Task>,
    frame_idx: usize,
    running: bool,
    last_lines: usize,
    enabled: bool,
}

/// Minimal TTY progress UI with spinner/checkmarks.
/// All methods are no-ops when `enabled` is false.
#[derive(Clone)]
pub struct ProgressUi {
    state: Arc<Mutex<UiState>>,
    ticker: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl ProgressUi {
    pub fn new(enabled: bool) -> Self {
        let ui = ProgressUi {
            state: Arc::new(Mutex::new(UiState {
                tasks: Vec::new(),
                frame_idx: 0,
                running: enabled,
                last_lines: 0,
                enabled,
            })),
            ticker: Arc::new(Mutex::new(None)),
        };
        if enabled {
            let state = ui.state.clone();
            let handle = thread::spawn(move || {
                // Hide cursor while rendering
                print!("\x1b[?25l");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                loop {
                    {
                        let mut st = state.lock().unwrap();
                        if !st.running { break; }
                        st.frame_idx = (st.frame_idx + 1) % FRAMES.len();
                    }
                    Self::render(&state);
                    thread::sleep(Duration::from_millis(120));
                }
                // Final render with checks
                Self::render(&state);
                // Show cursor
                print!("\x1b[?25h");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            });
            *ui.ticker.lock().unwrap() = Some(handle);
        }
        ui
    }

    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().enabled
    }

    pub fn add_tasks(&self, tasks: &[&str]) {
        let mut st = self.state.lock().unwrap();
        if !st.enabled { return; }
        for &t in tasks {
            st.tasks.push(Task { name: t.to_string(), state: TaskState::Pending });
        }
    }

    pub fn start(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        if !st.enabled { return; }
        for t in st.tasks.iter_mut() {
            if t.name == name {
                t.state = TaskState::Active;
                break;
            }
        }
    }

    pub fn complete(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        if !st.enabled { return; }
        for t in st.tasks.iter_mut() {
            if t.name == name {
                t.state = TaskState::Completed;
                break;
            }
        }
    }

    pub fn finish(&self) {
        let mut st = self.state.lock().unwrap();
        if !st.enabled { return; }
        st.running = false;
        drop(st);
        if let Some(handle) = self.ticker.lock().unwrap().take() {
            let _ = handle.join();
        }
    }

    fn render(state: &Arc<Mutex<UiState>>) {
        let mut st = state.lock().unwrap();
        if !st.enabled { return; }
        // Move cursor to the start of block
        if st.last_lines > 0 {
            print!("\x1b[{}F", st.last_lines); // move to beginning of N lines up
        }
        let mut lines_rendered = 0usize;
        for task in st.tasks.iter() {
            print!("\x1b[2K"); // clear line
            match task.state {
                TaskState::Pending => {
                    print!("○ {}", task.name);
                }
                TaskState::Active => {
                    let frame = FRAMES[st.frame_idx];
                    print!("{} {}", frame, task.name);
                }
                TaskState::Completed => {
                    print!("✔ {}", task.name);
                }
            }
            print!("\n");
            lines_rendered += 1;
        }
        st.last_lines = lines_rendered;
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}

