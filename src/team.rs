//! The multi-agent roster: four named agents, each with its **own history
//! context**, run one at a time.
//!
//! ```text
//! jonny    main agent - takes the user's prompts and does ordinary work
//! jacky    task planning & system design   - /plan, /plan_for_human
//! joey     coding & implementation         - /coding, /coding_phase
//! joy      testing & verification          - reviews jacky and joey
//! ```
//!
//! The point of the split is that a planning turn never pays for the coding
//! agent's history and vice versa: each agent's conversation lives in its own
//! [`Agent::history`](crate::agent::Agent::history) and is persisted to its
//! own Markdown file under the state directory (`.context/jonny.md`,
//! `.context/jacky.md`, `.context/joey.md`, `.context/joy.md`), which
//! replaced the old single `output.txt` transcript.
//!
//! Every agent is told who else is on the roster and what each one is for
//! ([`roster_note`]), and every agent may **read** any other agent's context
//! file - that is how the planner sees what the coder actually did without
//! spending its own context on it. Reading is one-way: the files themselves
//! belong to jonnyq, and no agent may write, edit, rename or delete one (the
//! refusal is enforced in the tools, see [`crate::tools::context_guard`]).
//!
//! Agents are **never** running inside each other's loop: [`Team::activate`]
//! is called immediately before an agent's turn and only one agent is ever
//! active, so a personal machine only ever has one model conversation in
//! flight. The handoff (jonny -> jacky -> joy -> jacky, or jonny -> joey
//! -> joy -> joey) is done by the REPL/coding layer, which awaits each turn in
//! sequence.
//!
//! The state directory itself is a setting ([`set_context_dir`], from
//! `-context-dir` / `JONNYQ_CONTEXT_DIR` / `.env`), and every path in the
//! program that names a state file is derived from [`context_dir`] so prompts
//! and real paths cannot drift apart.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::{Agent, ToolCallMode};
use crate::config::Config;
use crate::llm;
use crate::llm::{Message, Role};
use crate::tools::Registry;
use crate::ui::{
    Writer, COLOR_BRIGHT_BLUE, COLOR_BRIGHT_CYAN, COLOR_BRIGHT_MAGENTA, COLOR_BRIGHT_YELLOW,
};

pub mod review;

pub use crate::config::{
    agent_history_file, context_dir, progress_file, progress_hash_file, progress_retry_file,
    set_context_dir, transcript_file_in, DEFAULT_CONTEXT_DIR, DEFAULT_TRANSCRIPT_FILE,
};

/// The four agents, in the order `/help` and `/agents` list them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentName {
    Jonny,
    Jacky,
    Joey,
    Joy,
}

impl AgentName {
    pub const ALL: [AgentName; 4] = [
        AgentName::Jonny,
        AgentName::Jacky,
        AgentName::Joey,
        AgentName::Joy,
    ];

    /// The lowercase name used everywhere: files, slash-command arguments,
    /// log lines.
    pub fn key(self) -> &'static str {
        match self {
            AgentName::Jonny => "jonny",
            AgentName::Jacky => "jacky",
            AgentName::Joey => "joey",
            AgentName::Joy => "joy",
        }
    }

    /// One-line description shown in banners, `/agents` and `/help`.
    pub fn role(self) -> &'static str {
        match self {
            AgentName::Jonny => "main agent (general work)",
            AgentName::Jacky => "task planning & system design (/plan, /plan_for_human)",
            AgentName::Joey => "coding & implementation (/coding, /coding_phase)",
            AgentName::Joy => "testing & verification (reviews jacky and joey)",
        }
    }

    /// The one-line "what this agent is for" as it is written into the other
    /// agents' system prompts (see [`roster_note`]) - the role and its limits,
    /// without the command list, which [`Self::commands`] adds separately.
    pub fn duty(self) -> &'static str {
        match self {
            AgentName::Jonny => {
                "the main agent - takes the person's prompts and does ordinary work \
itself"
            }
            AgentName::Jacky => {
                "the planner - task planning and system design; owns the plan in \
the task file; designs, never implements"
            }
            AgentName::Joey => {
                "the coder - coding and implementation; works the checklist task by \
task, never redesigns the plan"
            }
            AgentName::Joy => {
                "the reviewer - testing and verification of what the others produced; \
reads, runs and checks, never edits or fixes anything herself"
            }
        }
    }

    /// The commands this agent owns, as listed in [`roster_note`] and in the
    /// parenthetical of [`Self::role`].
    pub fn commands(self) -> &'static str {
        match self {
            AgentName::Jonny => "every command not named below",
            AgentName::Jacky => "/plan, /plan_for_human",
            AgentName::Joey => "/coding, /coding_phase, /autocoding",
            AgentName::Joy => "the review that follows /plan and /coding (see /verify)",
        }
    }

    /// Colour of this agent's banner, so a scrolled-back terminal shows at a
    /// glance who produced which block of output.
    pub fn color(self) -> &'static str {
        match self {
            AgentName::Jonny => COLOR_BRIGHT_CYAN,
            AgentName::Jacky => COLOR_BRIGHT_BLUE,
            AgentName::Joey => COLOR_BRIGHT_YELLOW,
            AgentName::Joy => COLOR_BRIGHT_MAGENTA,
        }
    }

    /// `<dir>/<name>.md` - this agent's persisted history context, relative to
    /// the working directory.
    pub fn context_file(self) -> String {
        agent_history_file(self)
    }

    /// The name this agent answered to in an earlier version, if it was ever
    /// renamed. Only the name changes - the agent, its lane and its role are
    /// the same one - so a history file written under the old name is still
    /// that agent's memory and must come back with it (see
    /// `Team::migrate_legacy_contexts`).
    pub fn legacy_key(self) -> Option<&'static str> {
        match self {
            AgentName::Jacky => Some("jackson"),
            AgentName::Jonny | AgentName::Joey | AgentName::Joy => None,
        }
    }

    /// Accepts `joey`, `Joey`, `/joey`, `agent:joey` - whatever a person is
    /// likely to type at `/agent <name>` or in a slash-command argument. An
    /// agent's former name is accepted too, and means the same agent.
    pub fn parse(s: &str) -> Option<AgentName> {
        let s = s.trim().trim_start_matches('/').trim_start_matches('@');
        let s = s.strip_prefix("agent:").unwrap_or(s).trim();
        AgentName::ALL.into_iter().find(|a| {
            a.key().eq_ignore_ascii_case(s)
                || a.legacy_key()
                    .is_some_and(|old| old.eq_ignore_ascii_case(s))
        })
    }

    /// The role instructions appended to the shared base system message.
    /// Kept deliberately short and binding: the four agents share one
    /// codebase and one task list, and the split only holds if each one stays
    /// in its lane. Who else is on the roster - and that their context may be
    /// read but never written - is added by [`roster_note`], not here, so the
    /// roster text stays in one place.
    pub fn system_prompt(self) -> &'static str {
        match self {
            AgentName::Jonny => {
                "You are JONNY, the main jonnyq agent: you receive the user's instructions and do the \
work yourself with the tools.\n\
Specialist agents take over for the slash commands that belong to them - JACKY plans (/plan, \
/plan_for_human), JOEY implements (/coding, /coding_phase) and JOY verifies. When one of those \
commands runs, jonnyq hands the turn to that agent and hands it back when it is finished: never \
redo their work, and never claim it is done. When you relay their result to the user, say which \
agent produced it.\n"
            }
            AgentName::Jacky => {
                "You are JACKY, the task-planning and system-design agent. You are called for /plan \
and /plan_for_human and you own the plan: read the requirements and the existing code, decide the \
architecture and the phasing, and write that plan into the task file.\n\
You design, you do not build: never implement a feature, fix application code or change a test - \
that is JOEY's job. If a reviewer (JOY) returns findings on the plan, fix the plan itself: make it \
complete, unambiguous, granular and traceable back to the requirements.\n"
            }
            AgentName::Joey => {
                "You are JOEY, the coding and implementation agent. You are called for /coding and \
/coding_phase: take the current task from the task file, implement it, and prove it by running the \
real build and tests before you tick its checkbox.\n\
Work on the task you were given, nothing else: implement exactly what the task line asks and stop \
there. Do not start later tasks early, do not add features, files or refactors it did not ask for, \
and do not write code just so there is something to test - when the task only creates a file or a \
skeleton, proving that the file exists with the required content is the whole verification. Do not \
redesign the plan, do not remove or reword unrelated tasks, and do not tick a box you have not \
verified with real command output in this turn. If the plan itself looks wrong, say so in your reply \
and leave the correction to JACKY.\n"
            }
            AgentName::Joy => {
                "You are JOY, the testing and verification agent. You review what JACKY (the plan) \
and JOEY (the implementation) produced - you never implement, edit or fix anything yourself.\n\
The two reviews are different jobs. A PLAN is reviewed for coverage and completeness: read the \
requirements and walk them one by one against the checklist, and report any requirement, edge case \
or constraint no task delivers, anything a coder would have to guess, any task too broad to verify \
in one sitting, and any task that could be ticked without proving anything - a plan that can be \
followed to the letter yet yields an incomplete app is a failed plan. An IMPLEMENTATION is reviewed \
for correctness and for scope: does the change deliver exactly the task line it was ticked against, \
and nothing that belongs to a later task.\n\
Verify with evidence, not with trust: read the files that were changed, re-run the build and test \
commands with run_command, and check the result against the requirements and against the task \
description. A claim in the reply text is not proof; tool output is.\n\
Finish every review with a verdict on its own first line: exactly APPROVE when everything is \
correct and verified, otherwise NEEDS FIX followed by numbered findings - each one naming the file \
(or the task), what is actually wrong, and what the other agent must change - so it can be handed \
straight back to the agent that has to fix it.\n\
The verdict must be in your ANSWER, not only in your reasoning. jonnyq reads the answer you leave \
behind at the end of the turn and nothing else: work out as much as you like in your thinking, but \
a review whose answer holds no APPROVE or NEEDS FIX leaves the work unverified, and a defect you \
found and never stated is a defect you let through.\n"
            }
        }
    }

    /// Who this agent is, beyond its lane: the character the person gave the
    /// roster, written into the top of its own system message (see
    /// [`Self::system_prompt`]).
    ///
    /// This is temperament, not capability - it changes how an agent approaches
    /// its work (JOY does not let a doubtful thing pass, JOEY will not leave a
    /// known flaw in his own), never what it is allowed to touch. The lane rules
    /// in [`Self::system_prompt`] stay binding and come first.
    pub fn persona(self) -> &'static str {
        match self {
            AgentName::Jonny => {
                "You are a man, and you are cheerful: brisk, good-humoured and easy to work with - \
the person should never have to drag an answer out of you."
            }
            AgentName::Jacky => {
                "You are a man, meticulous to the letter: you read the requirement twice, you name \
the edge case before anyone has to ask about it, and a plan of yours leaves nothing to guess at."
            }
            AgentName::Joey => {
                "You are a man, careful and self-critical: you do not allow a flaw to survive in \
work that carries your name, so you run the build and the tests before you claim anything, and you \
say plainly what you have not verified."
            }
            AgentName::Joy => {
                "You are a woman, striking and strict in equal measure: evidence or it did not \
happen. You are fair - you say what checks out and you never invent a finding to look thorough - \
but you do not let a doubtful thing pass, and you always land on a verdict."
            }
        }
    }
}

/// The roster as every agent is told about it: who is on the team, what each
/// one is for, which commands each owns, where each one's context lives - and
/// the rule that those contexts are **readable by everyone and writable by
/// nobody**.
///
/// Two things the split needs and a per-agent prompt cannot say by itself:
/// an agent that does not know the others exist redoes their work, and an
/// agent that cannot see what another one learned has to re-derive it (or
/// guesses). Reading another agent's history file is the cheap way across, and
/// it costs nothing the other agent's context: the file is on disk, and the
/// reader spends only the tokens it actually pulls in.
///
/// The write half is not a suggestion. Each file is the one agent's memory as
/// jonnyq persists it; an edited or deleted one restores a lie or amnesia at
/// the next start, so the tools refuse it outright (see
/// [`crate::tools::context_guard`]) rather than trusting the wording here.
pub fn roster_note(for_agent: AgentName) -> String {
    let mut out = String::from(
        "You are one of four jonnyq agents sharing one project, one task file and one set of tools, \
one running at a time. Know what each can do and use the right one instead of doing its work:\n",
    );
    for a in AgentName::ALL {
        let mine = if a == for_agent { " (you)" } else { "" };
        out.push_str(&format!(
            "- {}{}: {}; {}. Context file (yours to read): {}.\n",
            a.key().to_uppercase(),
            mine,
            a.duty(),
            a.commands(),
            a.context_file()
        ));
    }
    out.push_str(
        "Read any context file above with read_file (or grep via run_command) whenever another \
agent's findings, decisions or verification would inform your work - that is what makes the team \
one memory instead of four, and it costs only the tokens you actually read. Never write, edit, \
append, rename or delete one, another agent's or your own: jonnyq owns those files, the write \
tools refuse such a call, and an edited history file returns as a false memory at the next start. \
Say what another agent did from what its file or its tool output actually says.\n",
    );
    out
}

impl std::fmt::Display for AgentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

/// Absolute path of `<work_dir>/<state dir>`.
pub fn state_dir(work_dir: &str) -> PathBuf {
    Path::new(work_dir).join(context_dir())
}

/// Absolute path of one agent's history file inside `<work_dir>/<state dir>`.
pub fn agent_history_path(work_dir: &str, name: AgentName) -> PathBuf {
    state_dir(work_dir).join(format!("{}.md", name.key()))
}

/// Creates the state directory if it is not there yet - every file jonnyq
/// writes for itself lives in it, so it has to exist before the first one
/// opens.
pub fn ensure_state_dir(work_dir: &str) -> std::io::Result<PathBuf> {
    let dir = state_dir(work_dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The four agents of one session, built from one tool registry so the tools
/// themselves (working directory, timeouts, ntfy topic) are shared while the
/// conversations are not.
pub struct Team {
    pub jonny: Arc<Agent>,
    pub jacky: Arc<Agent>,
    pub joey: Arc<Agent>,
    pub joy: Arc<Agent>,
    /// Whether joy reviews the planning and coding commands (`/verify`,
    /// `JONNYQ_VERIFY`). She is the reason the roster is split, so this is on
    /// by default; it exists because a quick experiment should not have to pay
    /// for two extra model conversations per command.
    verify: std::sync::atomic::AtomicBool,
    /// How many fix/re-review cycles one command may run
    /// (`/review_rounds`, `JONNYQ_REVIEW_ROUNDS`).
    review_rounds: std::sync::atomic::AtomicI64,
    /// The ntfy topic JOY's verdicts are posted to (`/ntfy`,
    /// `JONNYQ_NTFY_URL`). Kept here rather than read from the config per
    /// review because the roster outlives `/ntfy` changes and `/ntfy off` must
    /// silence the alerts immediately; it is the same URL the coding commands
    /// build their own notifier from.
    ntfy_url: std::sync::Mutex<String>,
}

impl Clone for Team {
    fn clone(&self) -> Self {
        Self {
            jonny: self.jonny.clone(),
            jacky: self.jacky.clone(),
            joey: self.joey.clone(),
            joy: self.joy.clone(),
            verify: std::sync::atomic::AtomicBool::new(self.verify()),
            review_rounds: std::sync::atomic::AtomicI64::new(self.review_rounds()),
            ntfy_url: std::sync::Mutex::new(self.ntfy_url()),
        }
    }
}

impl Team {
    /// Builds all four agents: same provider, model, tools, UI and window,
    /// separate histories and separate `<dir>/<name>.md` files.
    pub fn build(
        cfg: &Config,
        provider: Arc<dyn llm::Provider>,
        reg: &Registry,
        ui: Arc<Writer>,
    ) -> Team {
        let make = |name: AgentName| {
            let agent = Agent::new(
                provider.clone(),
                &cfg.model,
                reg.clone(),
                cfg.thinking,
                cfg.context_size,
                cfg.max_tool_calls_per_turn,
                ui.clone(),
                cfg.skill_paths.clone(),
            );
            agent.set_name(name);
            agent.set_tool_call_mode(ToolCallMode::from_config(&cfg.tool_call_mode));
            // One transcript log for the session (the role `output.txt` had),
            // labelled by agent; each agent's *own* memory is its separate
            // `<dir>/<name>.md`, pointed at by [`Team::set_work_dir`].
            agent.set_output_file(&cfg.output_file);
            agent.configure(cfg);
            agent
        };
        Team {
            jonny: Arc::new(make(AgentName::Jonny)),
            jacky: Arc::new(make(AgentName::Jacky)),
            joey: Arc::new(make(AgentName::Joey)),
            joy: Arc::new(make(AgentName::Joy)),
            verify: std::sync::atomic::AtomicBool::new(cfg.verify),
            review_rounds: std::sync::atomic::AtomicI64::new(cfg.review_rounds.max(0)),
            ntfy_url: std::sync::Mutex::new(cfg.ntfy_url.clone()),
        }
    }

    /// Whether joy reviews the work (`/verify`).
    pub fn verify(&self) -> bool {
        self.verify.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_verify(&self, on: bool) {
        self.verify.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// How many fix/re-review cycles a command may run (`/review_rounds`).
    pub fn review_rounds(&self) -> i64 {
        self.review_rounds.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_review_rounds(&self, n: i64) {
        self.review_rounds
            .store(n.max(0), std::sync::atomic::Ordering::SeqCst);
    }

    /// The ntfy topic JOY notifies on ("" when notifications are off).
    pub fn ntfy_url(&self) -> String {
        self.ntfy_url.lock().unwrap().clone()
    }

    /// Points the review lane at a topic (`/ntfy`), so `/ntfy off` silences
    /// JOY's verdict alerts with the same keystroke that silences the coding
    /// commands' completion notifications.
    pub fn set_ntfy_url(&self, url: &str) {
        *self.ntfy_url.lock().unwrap() = url.to_string();
    }

    /// A notifier for the roster's own notifications. Disabled (a no-op) when no
    /// topic is set, which is why every call site can call it unconditionally.
    pub fn notifier(&self) -> crate::notify::Notifier {
        crate::notify::Notifier::new(&self.ntfy_url())
    }

    /// A note from the roster itself (a hand-off, an unresolved review), in the
    /// plain style the UI uses for its own announcements.
    pub(crate) fn ui_note(&self, text: &str) {
        self.jonny.ui.plainln(text);
    }

    /// A one-agent roster: the same agent in all four slots. For tests and
    /// embeddings that only ever use the main agent - `/model` and friends
    /// still work because applying a setting four times to one agent is
    /// applying it once.
    #[cfg(test)]
    pub fn solo(agent: Arc<Agent>) -> Team {
        Team {
            jonny: agent.clone(),
            jacky: agent.clone(),
            joey: agent.clone(),
            joy: agent,
            verify: std::sync::atomic::AtomicBool::new(false),
            review_rounds: std::sync::atomic::AtomicI64::new(0),
            ntfy_url: std::sync::Mutex::new(String::new()),
        }
    }

    pub fn get(&self, name: AgentName) -> Arc<Agent> {
        match name {
            AgentName::Jonny => self.jonny.clone(),
            AgentName::Jacky => self.jacky.clone(),
            AgentName::Joey => self.joey.clone(),
            AgentName::Joy => self.joy.clone(),
        }
    }

    pub fn all(&self) -> [(AgentName, Arc<Agent>); 4] {
        [
            (AgentName::Jonny, self.jonny.clone()),
            (AgentName::Jacky, self.jacky.clone()),
            (AgentName::Joey, self.joey.clone()),
            (AgentName::Joy, self.joy.clone()),
        ]
    }

    /// The agent that owns a slash command, if any. `/plan*` belongs to
    /// jacky, `/coding*` to joey; everything else is jonny's.
    pub fn owner_of(cmd: &str) -> AgentName {
        match cmd {
            "/plan" | "/plan_for_human" => AgentName::Jacky,
            "/coding" | "/coding_phase" | "/autocoding" => AgentName::Joey,
            _ => AgentName::Jonny,
        }
    }

    /// Announces `name` as the one agent working now: prints its banner and
    /// re-seeds the shared UI snapshot with *its* context figures, so the
    /// `ctx ...` readout on the next header describes the agent that is about
    /// to run rather than the one that ran last.
    pub fn activate(&self, name: AgentName) -> Arc<Agent> {
        let agent = self.get(name);
        agent.activate();
        agent
    }

    /// Announces `name` with a reason (`"/plan"`, `"re-review after fixes"`)
    /// and returns its handle - what every command runs its turns under.
    pub fn start(&self, name: AgentName, note: &str) -> Arc<Agent> {
        let agent = self.activate(name);
        agent.announce(note);
        agent
    }

    /// Which agent is working right now (the last one [`Team::activate`]ed).
    pub fn active(&self) -> Arc<Agent> {
        self.get(self.active_name())
    }

    /// The name of the active agent, for the banner, `/team` and the web bar.
    pub fn active_name(&self) -> AgentName {
        AgentName::parse(&self.jonny.ui.current_agent()).unwrap_or(AgentName::Jonny)
    }

    // --- live configuration, applied to every agent -----------------------

    /// [`Agent::configure`] for all four (compaction tunables + tool-result
    /// ceiling), after a `/env reload`.
    pub fn configure(&self, cfg: &Config) {
        for (_, a) in self.all() {
            a.configure(cfg);
        }
    }

    pub fn set_model(&self, m: &str) {
        for (_, a) in self.all() {
            a.set_model(m);
        }
    }

    pub fn set_context_size(&self, n: i64) {
        for (_, a) in self.all() {
            a.set_context_size(n);
        }
    }

    pub fn set_thinking(&self, on: bool) {
        for (_, a) in self.all() {
            a.set_thinking(on);
        }
    }

    pub fn set_tool_call_mode(&self, m: ToolCallMode) {
        for (_, a) in self.all() {
            a.set_tool_call_mode(m);
        }
    }

    pub fn set_max_tool_calls_per_turn(&self, n: i64) {
        for (_, a) in self.all() {
            a.set_max_tool_calls_per_turn(n);
        }
    }

    pub fn set_skill_paths(&self, paths: Vec<String>) {
        for (_, a) in self.all() {
            a.set_skill_paths(paths.clone());
        }
    }

    /// The transcript log every agent appends to (they share one file, so the
    /// session reads as one conversation, labelled by agent).
    pub fn set_output_file(&self, path: &str) {
        for (_, a) in self.all() {
            a.set_output_file(path);
        }
    }

    /// Pushes a re-read config's provider settings into every agent.
    pub fn set_provider(&self, p: Arc<dyn llm::Provider>) {
        for (_, a) in self.all() {
            a.set_provider(p.clone());
        }
    }

    /// Points every agent at its own history file under `work_dir` (see
    /// [`agent_history_path`]). Called at startup and after a work-dir or
    /// state-dir change so a persisted history always lands next to the
    /// project it belongs to.
    pub fn set_work_dir(&self, work_dir: &str) {
        for (name, a) in self.all() {
            let path = agent_history_path(work_dir, name);
            a.set_context_path(&path.to_string_lossy());
        }
    }

    /// Reads every agent's history file back into its history, so a restarted
    /// session continues where it stopped. Returns how many agents got a
    /// history back.
    pub fn load_contexts(&self, work_dir: &str) -> usize {
        self.migrate_legacy_contexts(work_dir);
        let mut loaded = 0;
        for (name, agent) in self.all() {
            if agent.load_context(&agent_history_path(work_dir, name)) {
                loaded += 1;
            }
        }
        loaded
    }

    /// Renames a history file an agent left under a name it no longer uses
    /// (JACKSON became JACKY), so an upgrade does not quietly drop the memory
    /// a session built up.
    ///
    /// A rename rather than a copy: the new name is the only one jonnyq reads
    /// afterwards, and leaving the old file behind would leave two files
    /// claiming to be one agent's memory - one of them unreadable, both of them
    /// readable by the agents that are told to share them. If the new file
    /// already exists (the session ran once under the new name already), the
    /// old one is left alone rather than clobbering anything.
    fn migrate_legacy_contexts(&self, work_dir: &str) {
        for name in AgentName::ALL {
            let Some(old_key) = name.legacy_key() else {
                continue;
            };
            let old = state_dir(work_dir).join(format!("{old_key}.md"));
            let new = agent_history_path(work_dir, name);
            if !old.is_file() || new.exists() {
                continue;
            }
            match std::fs::rename(&old, &new) {
                Ok(_) => self.jonny.ui.meta(&format!(
                    "[team] renamed {old_key}.md to {} - the history of {name}, which used to be \
called {old_key}",
                    new.file_name().unwrap_or_default().to_string_lossy()
                )),
                Err(e) => self.jonny.ui.meta(&format!(
                    "warning: cannot rename {old_key}'s history file to {name}'s: {e}"
                )),
            }
        }
    }

    /// Writes every agent's history to its own file (used on shutdown; a
    /// finished turn persists itself).
    pub fn save_contexts(&self, work_dir: &str) {
        for (name, agent) in self.all() {
            // Best-effort, as during a turn: a full disk or a missing folder
            // must not stop the session from exiting.
            if let Err(e) = agent.save_context(&agent_history_path(work_dir, name)) {
                agent.ui.meta(&format!(
                    "warning: cannot save {name}'s history: {}",
                    e.message
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the Markdown form of a history (what <dir>/<name>.md holds)
// ---------------------------------------------------------------------------

/// Marks the file layout, so a future format change can refuse to misread an
/// older file rather than feeding garbage back to the model.
pub const CONTEXT_FORMAT_TAG: &str = "<!-- jonnyq-context: 1 -->";

/// Tool results are kept in the file for the human reader only, capped so one
/// `cargo build` cannot turn the history file into the biggest file in the
/// project.
const PERSISTED_TOOL_RESULT_CHARS: usize = 600;

/// Renders a history as Markdown: one `## <role>` heading per message, tool
/// calls as blockquotes, tool results as fenced blocks.
///
/// The file is both the durable record and the restore source - see
/// [`parse_history_file`] for what survives the round trip.
pub fn render_history(agent: AgentName, history: &[Message]) -> String {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let mut out = String::new();
    out.push_str(&format!("# jonnyq history - {agent}\n\n"));
    out.push_str(&format!("<!-- role: {} -->\n", agent.role()));
    out.push_str(&format!("{CONTEXT_FORMAT_TAG}\n"));
    out.push_str(&format!(
        "<!-- written: {stamp} | {} message(s) -->\n\n",
        history.len()
    ));
    for m in history {
        match m.role {
            Role::System => {
                out.push_str(&format!("## system\n\n{}\n\n", m.content.trim_end()));
            }
            Role::User => {
                out.push_str(&format!("## user\n\n{}\n\n", m.content.trim_end()));
            }
            Role::Assistant => {
                out.push_str(&format!("## assistant\n\n{}\n\n", m.content.trim_end()));
                for tc in &m.tool_calls {
                    out.push_str(&format!(
                        "> tool: {}({})\n\n",
                        tc.name,
                        tc.arguments.replace('\n', " ")
                    ));
                }
            }
            Role::Tool => {
                let name = if m.name.is_empty() { "tool" } else { &m.name };
                let body =
                    crate::tools::cap_tool_result_chars(&m.content, PERSISTED_TOOL_RESULT_CHARS);
                out.push_str(&format!(
                    "## tool {}\n\n```text\n{}\n```\n\n",
                    name,
                    body.trim_end()
                ));
            }
        }
    }
    out
}

/// Reads a history file back into the messages an agent can continue from.
///
/// Only the `user` / `assistant` / `system` turns come back: a restored tool
/// round would point at a call whose result nobody has any more, and providers
/// reject orphaned `tool_call_id`s outright.
pub fn parse_history_file(text: &str) -> Vec<Message> {
    if !text.contains(CONTEXT_FORMAT_TAG) {
        return Vec::new();
    }
    let mut out: Vec<Message> = Vec::new();
    let mut role: Option<Role> = None;
    let mut body = String::new();
    // Fenced blocks belong to the message they are inside: their ``` fences
    // and any `## heading` line inside them must not end the message.
    let mut in_fence = false;
    for line in text.split('\n') {
        let fence = line.trim_start().starts_with("```");
        if !in_fence && line.starts_with("## ") {
            flush_history_message(&mut role, &mut body, &mut out);
            let head = line[3..].trim();
            role = match head.split_whitespace().next().unwrap_or("") {
                "user" => Some(Role::User),
                "assistant" => Some(Role::Assistant),
                // System turns (a compaction summary, a prune note) are the
                // agent's own state and fold back in as context.
                "system" => Some(Role::System),
                _ => None, // "tool <name>" and anything else: not restorable
            };
            continue;
        }
        if fence {
            in_fence = !in_fence;
        }
        if role.is_some() {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush_history_message(&mut role, &mut body, &mut out);
    out
}

fn flush_history_message(role: &mut Option<Role>, body: &mut String, out: &mut Vec<Message>) {
    if let Some(r) = role.take() {
        let content = body.trim();
        if !content.is_empty() {
            out.push(Message {
                role: r,
                content: content.to_string(),
                ..Default::default()
            });
        }
    }
    body.clear();
}

#[cfg(test)]
mod tests;
