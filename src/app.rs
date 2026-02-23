use crate::flame::{FlameGraph, SearchPattern};
#[cfg(feature = "python")]
use crate::py_spy::{record_samples, ProfilerOutput, SamplerState, SamplerStatus};
use crate::state::FlameGraphState;
use crate::view::FlameGraphView;
#[cfg(feature = "python")]
use remoteprocess;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::error;
use std::sync::{Arc, Mutex};
#[cfg(feature = "python")]
use std::thread;
use std::time::Duration;

/// Application result type.
pub type AppResult<T> = std::result::Result<T, Box<dyn error::Error>>;

#[derive(Debug)]
pub enum FlameGraphInput {
    File(String),
    Pid(u64, Option<String>),
}

#[derive(Debug)]
pub struct ParsedFlameGraph {
    pub flamegraph: FlameGraph,
    pub elapsed: Duration,
}

#[derive(Debug)]
pub struct InputBuffer {
    pub buffer: tui_input::Input,
    pub cursor: Option<(u16, u16)>,
}

/// Application.
#[derive(Debug)]
pub struct App {
    /// Is the application running?
    pub running: bool,
    /// Flamegraph view
    pub flamegraph_view: FlameGraphView,
    /// Flamegraph input information
    pub flamegraph_input: FlameGraphInput,
    /// User input buffer
    pub input_buffer: Option<InputBuffer>,
    /// Timing information for debugging
    pub elapsed: HashMap<String, Duration>,
    /// Transient message
    pub transient_message: Option<String>,
    /// Debug mode
    pub debug: bool,
    /// Next flamegraph to swap in
    next_flamegraph: Arc<Mutex<Option<ParsedFlameGraph>>>,
    #[cfg(feature = "python")]
    sampler_state: Option<Arc<Mutex<SamplerState>>>,
    pub log_messages: VecDeque<String>,
    pub show_log_panel: bool,
    pub has_log_channel: bool,
    pub log_scroll_offset: usize,
    pub log_auto_scroll: bool,
    pub log_search_pattern: Option<regex::Regex>,
    pub log_search_text: Option<String>,
    pub log_input_buffer: Option<InputBuffer>,
    pub log_max_capacity: usize,
    pub log_current_match_line: Option<usize>,
    pub log_visible_lines: usize,
}

impl App {
    /// Constructs a new instance of [`App`].
    pub fn with_flamegraph(filename: &str, flamegraph: FlameGraph) -> Self {
        Self {
            running: true,
            flamegraph_view: FlameGraphView::new(flamegraph),
            flamegraph_input: FlameGraphInput::File(filename.to_string()),
            input_buffer: None,
            elapsed: HashMap::new(),
            transient_message: None,
            debug: false,
            next_flamegraph: Arc::new(Mutex::new(None)),
            #[cfg(feature = "python")]
            sampler_state: None,
            log_messages: VecDeque::new(),
            show_log_panel: false,
            has_log_channel: false,
            log_scroll_offset: 0,
            log_auto_scroll: true,
            log_search_pattern: None,
            log_search_text: None,
            log_input_buffer: None,
            log_max_capacity: 1000,
            log_current_match_line: None,
            log_visible_lines: 8,
        }
    }

    #[cfg(feature = "python")]
    pub fn with_pid(pid: u64, py_spy_args: Option<String>) -> Self {
        let next_flamegraph: Arc<Mutex<Option<ParsedFlameGraph>>> = Arc::new(Mutex::new(None));
        let pyspy_data: Arc<Mutex<Option<ProfilerOutput>>> = Arc::new(Mutex::new(None));
        let sampler_state = Arc::new(Mutex::new(SamplerState::default()));

        // Thread to poll data from pyspy and construct the next flamegraph
        {
            let next_flamegraph = next_flamegraph.clone();
            let pyspy_data = pyspy_data.clone();
            let _handle = thread::spawn(move || loop {
                if let Some(output) = pyspy_data.lock().unwrap().take() {
                    let tic = std::time::Instant::now();
                    let flamegraph = FlameGraph::from_string(output.data, true);
                    let parsed = ParsedFlameGraph {
                        flamegraph,
                        elapsed: tic.elapsed(),
                    };
                    *next_flamegraph.lock().unwrap() = Some(parsed);
                }
                thread::sleep(std::time::Duration::from_millis(250));
            });
        }

        // pyspy live sampler thread
        {
            let pyspy_data = pyspy_data.clone();
            let sampler_state = sampler_state.clone();
            let _handle = thread::spawn(move || {
                // Note: mimic a record command's invocation vs simply getting default Config as
                // from_args does a lot of heavy lifting
                let mut args = [
                    "py-spy",
                    "record",
                    "--pid",
                    pid.to_string().as_str(),
                    "--format",
                    "raw",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<String>>();
                if let Some(py_spy_args) = py_spy_args {
                    args.extend(py_spy_args.split_whitespace().map(|s| s.to_string()));
                }
                let config = py_spy::Config::from_args(&args).unwrap();
                let pid = pid as remoteprocess::Pid;
                record_samples(pid, &config, pyspy_data, sampler_state);
            });
        }

        let flamegraph = FlameGraph::from_string("".to_string(), true);
        let process_info = remoteprocess::Process::new(pid as remoteprocess::Pid)
            .and_then(|p| p.cmdline())
            .ok()
            .map(|c| c.join(" "));
        Self {
            running: true,
            flamegraph_view: FlameGraphView::new(flamegraph),
            flamegraph_input: FlameGraphInput::Pid(pid, process_info),
            next_flamegraph: next_flamegraph.clone(),
            input_buffer: None,
            elapsed: HashMap::new(),
            transient_message: None,
            debug: false,
            sampler_state: Some(sampler_state),
            log_messages: VecDeque::new(),
            show_log_panel: false,
            has_log_channel: false,
            log_scroll_offset: 0,
            log_auto_scroll: true,
            log_search_pattern: None,
            log_search_text: None,
            log_input_buffer: None,
            log_max_capacity: 1000,
            log_current_match_line: None,
            log_visible_lines: 8,
        }
    }

    /// Handles the tick event of the terminal.
    pub fn tick(&mut self) {
        // Replace flamegraph
        if !self.flamegraph_view.state.freeze {
            if let Some(parsed) = self.next_flamegraph.lock().unwrap().take() {
                self.elapsed
                    .insert("flamegraph".to_string(), parsed.elapsed);
                let tic = std::time::Instant::now();
                self.flamegraph_view.replace_flamegraph(parsed.flamegraph);
                self.elapsed
                    .insert("replacement".to_string(), tic.elapsed());
            }
        }

        // Exit if fatal error in sampler
        #[cfg(feature = "python")]
        if let Some(SamplerStatus::Error(s)) = self
            .sampler_state
            .as_ref()
            .map(|s| s.lock().unwrap().status.clone())
        {
            panic!("py-spy sampler exited with error: {}\n\nYou likely need to rerun this program with sudo.", s);
        }
    }

    /// Set running to false to quit the application.
    pub fn quit(&mut self) {
        self.running = false;
    }

    pub fn flamegraph(&self) -> &FlameGraph {
        &self.flamegraph_view.flamegraph
    }

    pub fn flamegraph_state(&self) -> &FlameGraphState {
        &self.flamegraph_view.state
    }

    #[cfg(feature = "python")]
    pub fn sampler_state(&self) -> Option<SamplerState> {
        self.sampler_state
            .as_ref()
            .map(|s| s.lock().unwrap().clone())
    }

    pub fn add_elapsed(&mut self, name: &str, elapsed: Duration) {
        self.elapsed.insert(name.to_string(), elapsed);
    }

    pub fn search_selected(&mut self) {
        if self.flamegraph_view.is_root_selected() {
            return;
        }
        let short_name = self.flamegraph_view.get_selected_stack().map(|s| {
            self.flamegraph()
                .get_stack_short_name_from_info(s)
                .to_string()
        });
        if let Some(short_name) = short_name {
            self.set_manual_search_pattern(short_name.as_str(), false);
        }
    }

    pub fn search_selected_row(&mut self) {
        let short_name = self
            .flamegraph_view
            .get_selected_row_name()
            .map(|s| s.to_string());
        if let Some(short_name) = short_name {
            self.set_manual_search_pattern(short_name.as_str(), false);
        }
        self.flamegraph_view.state.toggle_view_kind();
    }

    pub fn set_manual_search_pattern(&mut self, pattern: &str, is_regex: bool) {
        match SearchPattern::new(pattern, is_regex, true) {
            Ok(p) => self.flamegraph_view.set_search_pattern(p),
            Err(_) => {
                self.set_transient_message(&format!("Invalid regex: {}", pattern));
            }
        }
    }

    pub fn set_transient_message(&mut self, message: &str) {
        self.transient_message = Some(message.to_string());
    }

    pub fn clear_transient_message(&mut self) {
        self.transient_message = None;
    }

    pub fn toggle_debug(&mut self) {
        self.debug = !self.debug;
    }

    pub fn push_log_message(&mut self, msg: String) {
        self.log_messages.push_back(msg);
        if !self.log_auto_scroll {
            self.log_scroll_offset += 1;
        }
        if self.log_messages.len() > self.log_max_capacity {
            self.log_messages.pop_front();
            if self.log_scroll_offset > 0 {
                self.log_scroll_offset -= 1;
            }
        }
    }

    pub fn toggle_log_panel(&mut self) {
        if self.has_log_channel {
            self.show_log_panel = !self.show_log_panel;
        }
    }

    pub fn log_scroll_up(&mut self, lines: usize) {
        self.log_scroll_offset = self
            .log_scroll_offset
            .saturating_add(lines)
            .min(self.log_messages.len().saturating_sub(self.log_visible_lines));
        self.log_auto_scroll = false;
    }

    pub fn log_scroll_down(&mut self, lines: usize) {
        self.log_scroll_offset = self.log_scroll_offset.saturating_sub(lines);
        if self.log_scroll_offset == 0 {
            self.log_auto_scroll = true;
        }
    }

    pub fn log_scroll_to_bottom(&mut self) {
        self.log_scroll_offset = 0;
        self.log_auto_scroll = true;
    }

    pub fn set_log_search_pattern(&mut self, pattern: &str) {
        match regex::Regex::new(pattern) {
            Ok(re) => {
                let len = self.log_messages.len();
                let initial_match = (0..len)
                    .rev()
                    .find(|&i| re.is_match(&self.log_messages[i]));
                self.log_search_pattern = Some(re);
                self.log_search_text = Some(pattern.to_string());
                self.log_current_match_line = initial_match;
                if let Some(i) = initial_match {
                    self.scroll_to_log_line(i);
                }
            }
            Err(_) => {
                self.set_transient_message(&format!("Invalid regex: {}", pattern));
            }
        }
    }

    pub fn clear_log_search(&mut self) {
        self.log_search_pattern = None;
        self.log_search_text = None;
        self.log_current_match_line = None;
    }

    pub fn log_next_match(&mut self) {
        if let Some(re) = &self.log_search_pattern {
            let len = self.log_messages.len();
            if len == 0 {
                return;
            }
            let start = self.log_current_match_line.map_or(0, |i| i + 1);
            for i in start..len {
                if re.is_match(&self.log_messages[i]) {
                    self.log_current_match_line = Some(i);
                    self.scroll_to_log_line(i);
                    return;
                }
            }
        }
    }

    pub fn log_prev_match(&mut self) {
        if let Some(re) = &self.log_search_pattern {
            let len = self.log_messages.len();
            if len == 0 {
                return;
            }
            let start = self
                .log_current_match_line
                .unwrap_or(len)
                .saturating_sub(1);
            for i in (0..=start).rev() {
                if re.is_match(&self.log_messages[i]) {
                    self.log_current_match_line = Some(i);
                    self.scroll_to_log_line(i);
                    return;
                }
            }
        }
    }

    fn scroll_to_log_line(&mut self, line: usize) {
        let len = self.log_messages.len();
        let end = len.saturating_sub(self.log_scroll_offset);
        let start = end.saturating_sub(self.log_visible_lines);
        if line >= start && line < end {
            return;
        }
        let half = self.log_visible_lines / 2;
        self.log_scroll_offset = len
            .saturating_sub(line + half + 1)
            .min(len.saturating_sub(self.log_visible_lines));
        self.log_auto_scroll = self.log_scroll_offset == 0;
    }

    pub fn set_log_max_capacity(&mut self, capacity: usize) {
        self.log_max_capacity = capacity;
    }
}
