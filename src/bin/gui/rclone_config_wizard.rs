use serde::Deserialize;
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RcloneQuestion {
    #[serde(rename = "State")]
    pub state: String,
    #[serde(rename = "Option")]
    pub option: Option<RcloneOption>,
    #[serde(rename = "Error")]
    pub error: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RcloneOption {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Help")]
    pub help: String,
    #[serde(rename = "Examples")]
    pub examples: Option<Vec<RcloneExample>>,
    #[serde(rename = "IsPassword")]
    pub is_password: bool,
    #[serde(rename = "Required")]
    pub required: bool,
    #[serde(rename = "DefaultStr")]
    pub default_str: String,
    #[serde(rename = "Exclusive")]
    pub exclusive: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RcloneExample {
    #[serde(rename = "Value")]
    pub value: String,
    #[serde(rename = "Help")]
    pub help: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Init,
    Working,
    Question(RcloneQuestion),
    WaitingOAuth { url: String },
    Done,
    Error(String),
}

type Pending = Arc<Mutex<Option<Result<RcloneQuestion, String>>>>;

#[derive(Clone)]
struct HistoryEntry {
    rclone_state: String,
    question: RcloneQuestion,
}

pub struct Wizard {
    pub remote_name: String,
    pub remote_type: String,
    pub step: WizardStep,
    pub current_answer: String,
    rclone_state: String,
    pending: Option<Pending>,
    history: Vec<HistoryEntry>,
}

impl Wizard {
    pub fn new() -> Self {
        Self {
            remote_name: String::new(),
            remote_type: "onedrive".into(),
            step: WizardStep::Init,
            current_answer: String::new(),
            rclone_state: String::new(),
            pending: None,
            history: Vec::new(),
        }
    }

    pub fn can_go_back(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn go_back(&mut self) {
        if let Some(entry) = self.history.pop() {
            self.rclone_state = entry.rclone_state.clone();
            self.current_answer = entry
                .question
                .option
                .as_ref()
                .map(|o| o.default_str.clone())
                .unwrap_or_default();
            self.step = WizardStep::Question(entry.question);
        }
    }

    pub fn start(&mut self) {
        self.history.clear();
        let name = self.remote_name.clone();
        let remote_type = self.remote_type.clone();
        self.spawn(move || run_rclone_step(&name, &remote_type, "", ""));
    }

    pub fn submit_answer(&mut self) {
        if let WizardStep::Question(q) = &self.step {
            self.history.push(HistoryEntry {
                rclone_state: self.rclone_state.clone(),
                question: q.clone(),
            });
        }
        let answer = std::mem::take(&mut self.current_answer);
        let state = self.rclone_state.clone();
        let name = self.remote_name.clone();
        let remote_type = self.remote_type.clone();
        self.spawn(move || run_rclone_step(&name, &remote_type, &state, &answer));
    }

    pub fn poll(&mut self) {
        let result = match &self.pending {
            Some(arc) => arc.lock().unwrap().clone(),
            None => return,
        };
        if let Some(result) = result {
            self.pending = None;
            match result {
                Ok(q) => self.handle_question(q),
                Err(e) => self.step = WizardStep::Error(e),
            }
        }
    }

    fn handle_question(&mut self, question: RcloneQuestion) {
        if !question.error.is_empty() {
            self.step = WizardStep::Error(question.error.clone());
            return;
        }

        if question.state.is_empty() {
            self.step = WizardStep::Done;
            return;
        }

        self.rclone_state = question.state.clone();

        let option = match &question.option {
            Some(o) => o,
            None => {
                self.step = WizardStep::Done;
                return;
            }
        };

        if option.name == "config_is_local" {
            let state = self.rclone_state.clone();
            let name = self.remote_name.clone();
            let remote_type = self.remote_type.clone();
            let url = extract_oauth_url(&option.help)
                .unwrap_or_else(|| "http://127.0.0.1:53682/auth".into());
            self.step = WizardStep::WaitingOAuth { url };
            let slot: Pending = Arc::new(Mutex::new(None));
            self.pending = Some(slot.clone());
            std::thread::spawn(move || {
                let result = run_rclone_step(&name, &remote_type, &state, "true");
                *slot.lock().unwrap() = Some(result);
            });
            return;
        }

        self.current_answer = option.default_str.clone();
        self.step = WizardStep::Question(question);
    }

    fn spawn(&mut self, f: impl FnOnce() -> Result<RcloneQuestion, String> + Send + 'static) {
        let slot: Pending = Arc::new(Mutex::new(None));
        self.pending = Some(slot.clone());
        self.step = WizardStep::Working;
        std::thread::spawn(move || {
            *slot.lock().unwrap() = Some(f());
        });
    }
}

fn extract_oauth_url(help: &str) -> Option<String> {
    help.split_whitespace()
        .find(|word| word.starts_with("http://127.0.0.1") || word.starts_with("http://localhost"))
        .map(|url| url.trim_end_matches('.').to_string())
}

fn run_rclone_step(
    name: &str,
    remote_type: &str,
    state: &str,
    result: &str,
) -> Result<RcloneQuestion, String> {
    let mut cmd = Command::new("rclone");

    if state.is_empty() {
        cmd.args(["config", "create", "--non-interactive", name, remote_type]);
    } else {
        cmd.args([
            "config",
            "update",
            "--continue",
            "--state",
            state,
            "--result",
            result,
            name,
        ]);
    }

    let output = cmd.output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    if stdout.trim().is_empty() {
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(stderr
                .lines()
                .find(|l| l.contains("Error:"))
                .unwrap_or(stderr.trim())
                .to_string());
        }
        return Ok(RcloneQuestion {
            state: String::new(),
            option: None,
            error: String::new(),
        });
    }

    serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse rclone response: {e}\nraw: {stdout}"))
}
