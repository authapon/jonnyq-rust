use super::*;
use crate::llm::{Message, Role, ToolCall};
use crate::team::review::{parse_verdict, Verdict};
use crate::testsupport::{
    content_step, thinking_then_prose_step, EchoTool, SequenceProvider, StubResponse, TempDir,
    TestLog, TestServer,
};
use crate::tools::Registry;
use crate::ui::Writer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A bare agent carrying `name`, for the tests that only need an identity
/// (naming, persistence) rather than a whole roster.
fn solo(name: AgentName) -> Arc<crate::agent::Agent> {
    let log = TestLog::new("team-solo");
    let a = Arc::new(crate::agent::Agent::new(
        SequenceProvider::new(Vec::new()),
        "test-model",
        Registry::new(),
        false,
        0,
        0,
        Arc::new(Writer::new(&log.file).unwrap()),
        Vec::new(),
    ));
    a.set_name(name);
    a
}

fn team(log: &TestLog, responses: Vec<Vec<crate::llm::ChatEvent>>) -> Team {
    let ui = Arc::new(Writer::new(&log.file).unwrap());
    let mut reg = Registry::new();
    reg.register(Arc::new(EchoTool::new()));
    Team::build(
        &crate::config::Config::default(),
        SequenceProvider::new(responses),
        &reg,
        ui,
    )
}

#[test]
fn every_agent_has_its_own_history_file() {
    let keys: Vec<&str> = AgentName::ALL.iter().map(|a| a.key()).collect();
    assert_eq!(keys, vec!["jonny", "jacky", "joey", "joy"]);
    for a in AgentName::ALL {
        assert_eq!(
            a.context_file(),
            format!(".context/{}.md", a.key()),
            "{a} history file"
        );
    }
}

#[test]
fn agent_names_parse_the_forms_a_person_types() {
    for a in AgentName::ALL {
        assert_eq!(AgentName::parse(a.key()), Some(a));
        assert_eq!(AgentName::parse(&a.key().to_uppercase()), Some(a));
        assert_eq!(AgentName::parse(&format!("/{a}")), Some(a));
        assert_eq!(AgentName::parse(&format!(" {a} ")), Some(a));
    }
    // JACKY was JACKSON: the old name is a name for the same agent, not a
    // fifth one and not an unknown one.
    assert_eq!(AgentName::parse("jackson"), Some(AgentName::Jacky));
    assert_eq!(AgentName::parse("/JACKSON"), Some(AgentName::Jacky));
    assert_eq!(AgentName::Jacky.legacy_key(), Some("jackson"));
    assert_eq!(AgentName::Joey.legacy_key(), None);
    assert_eq!(AgentName::parse("someone-else"), None);
    assert_eq!(AgentName::parse(""), None);
}

#[test]
fn slash_commands_map_to_the_agent_that_owns_them() {
    assert_eq!(Team::owner_of("/plan"), AgentName::Jacky);
    assert_eq!(Team::owner_of("/plan_for_human"), AgentName::Jacky);
    assert_eq!(Team::owner_of("/coding"), AgentName::Joey);
    assert_eq!(Team::owner_of("/coding_phase"), AgentName::Joey);
    assert_eq!(Team::owner_of("/autocoding"), AgentName::Joey);
    assert_eq!(Team::owner_of("/help"), AgentName::Jonny);
    assert_eq!(Team::owner_of("/model"), AgentName::Jonny);
}

#[test]
fn each_agent_is_told_which_lane_it_is_in() {
    for a in AgentName::ALL {
        assert!(
            a.system_prompt().contains(&a.key().to_uppercase()),
            "{a} prompt must name itself"
        );
    }
    // The split only holds if the reviewer is told not to do the work and the
    // doers are told not to redesign.
    assert!(AgentName::Joy.system_prompt().contains("never implement"));
    assert!(AgentName::Joy.system_prompt().contains("APPROVE"));
    assert!(AgentName::Joy.system_prompt().contains("NEEDS FIX"));
    assert!(AgentName::Jacky.system_prompt().contains("do not build"));
    assert!(AgentName::Joey.system_prompt().contains("Do not redesign"));
    // jonny must know the specialists exist, or it will redo their work.
    assert!(AgentName::Jonny.system_prompt().contains("JACKY"));
    assert!(AgentName::Jonny.system_prompt().contains("JOY"));
}

#[test]
fn each_agent_carries_its_character_into_its_system_message() {
    // The roster's personalities are part of how the agents work, so they have
    // to reach the model - and never override the lane rules that come first.
    for who in AgentName::ALL {
        assert!(!who.persona().trim().is_empty(), "{who} has no persona");
        let msg = solo(who).build_system_message();
        assert!(
            msg.content.contains(who.persona()),
            "{who}'s persona is missing from its system message: {}",
            msg.content
        );
        assert!(
            msg.content.contains(who.system_prompt()),
            "{who}'s lane rules must come with it"
        );
        assert!(
            msg.content.find(who.system_prompt()) < msg.content.find(who.persona()),
            "{who}: the lane rules must be stated before the character"
        );
    }
    // JOY's character must not be licence to stop giving a verdict.
    let joy = AgentName::Joy.system_prompt();
    assert!(joy.contains("not only in your reasoning"), "{joy}");
    assert!(joy.contains("unverified"), "{joy}");
}

#[test]
fn every_agent_knows_the_whole_roster_and_how_to_share_it() {
    // The note quotes context-file paths, and the state directory is a
    // process-wide setting another test moves: hold the shared env lock.
    let _env = crate::testsupport::clean_env();
    // Knowing the others exists is what stops an agent redoing their work;
    // knowing their context file is what lets it build on what they learned.
    for who in AgentName::ALL {
        let note = roster_note(who);
        for a in AgentName::ALL {
            assert!(
                note.contains(&a.key().to_uppercase()),
                "{who} is not told about {a}: {note}"
            );
            assert!(
                note.contains(&a.context_file()),
                "{who} is not told where {a}'s context lives: {note}"
            );
            assert!(
                note.contains(a.duty()),
                "{who} is not told what {a} is for: {note}"
            );
        }
        // Which one it is, out of the four.
        assert!(
            note.contains("(you)"),
            "{who} is not told which entry is itself"
        );
        // Reading is offered, writing is refused - and the refusal names the
        // reason, not just the rule.
        let lower = note.to_lowercase();
        assert!(lower.contains("read_file"), "{who} is not told how to read");
        assert!(
            lower.contains("never write, edit"),
            "{who} is not told the files are read-only: {note}"
        );
        assert!(
            lower.contains("false memory"),
            "the reason must be in the note: {note}"
        );
    }
    // The system message an agent actually gets carries the roster: the note
    // is not written somewhere nobody sends.
    let sys = solo(AgentName::Joey).build_system_message();
    for a in AgentName::ALL {
        assert!(
            sys.content.contains(&a.context_file()),
            "joey's system message never mentions {a}: {}",
            sys.content
        );
    }
    // And the state-directory note agrees with the guard: reading is allowed,
    // and it does not contradict the roster note by forbidding it.
    assert!(
        sys.content.contains("Read anything in it freely"),
        "the state note must not forbid reading: {}",
        sys.content
    );
    assert!(
        !sys.content.to_lowercase().contains("never read"),
        "the state note forbids reading, which the guard does not: {}",
        sys.content
    );
}

#[test]
fn every_agent_gets_its_own_colour() {
    let colors: Vec<&str> = AgentName::ALL.iter().map(|a| a.color()).collect();
    assert_eq!(
        colors
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
}

#[test]
fn history_round_trips_through_the_markdown_form() {
    let history = vec![
        Message {
            role: Role::System,
            content: "[earlier conversation summarized: built the parser]".into(),
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: "make it work\nwith two lines".into(),
            ..Default::default()
        },
        Message {
            role: Role::Assistant,
            content: "on it".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "run_command".into(),
                arguments: "{\"command\":\"cargo test\"}".into(),
            }],
            ..Default::default()
        },
        Message {
            role: Role::Tool,
            content: "test result: ok. 12 passed".into(),
            name: "run_command".into(),
            tool_call_id: "c1".into(),
            ..Default::default()
        },
        Message {
            role: Role::User,
            content: "Tool result (run_command): ok".into(),
            ..Default::default()
        },
    ];
    let text = render_history(AgentName::Joey, &history);
    assert!(text.starts_with("# jonnyq history - joey"));
    assert!(text.contains(CONTEXT_FORMAT_TAG));
    assert!(text.contains("> tool: run_command({\"command\":\"cargo test\"})"));
    assert!(text.contains("```text\ntest result: ok"));

    let back = parse_history_file(&text);
    // The tool result is dropped: the call it answers no longer exists to
    // point at, and providers reject an orphaned tool_call_id.
    let roles: Vec<Role> = back.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::System, Role::User, Role::Assistant, Role::User],
        "{roles:?}"
    );
    assert_eq!(back[1].content, "make it work\nwith two lines");
    assert!(back.iter().all(|m| m.tool_calls.is_empty()));
    assert_eq!(back[3].content, "Tool result (run_command): ok");
}

#[test]
fn a_history_file_with_no_format_tag_is_not_read() {
    // A person's own Markdown in the state dir must never be mistaken for a
    // history and fed back to the model as memory.
    assert!(parse_history_file("# Notes\n\n## user\n\nhello\n").is_empty());
}

#[test]
fn a_heading_inside_a_fenced_block_does_not_end_a_message() {
    let text = format!(
        "{CONTEXT_FORMAT_TAG}\n## user\n\n```markdown\n## assistant\nnot a turn\n```\n\nstill mine\n"
    );
    let back = parse_history_file(&text);
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].role, Role::User);
    assert!(back[0].content.contains("## assistant"));
    assert!(back[0].content.ends_with("still mine"));
}

#[test]
fn save_and_load_a_history_file() {
    let tmp = TempDir::new("team-context");
    let a = solo(AgentName::Joey);
    a.history.lock().unwrap().push(Message {
        role: Role::User,
        content: "first ask".into(),
        ..Default::default()
    });
    a.history.lock().unwrap().push(Message {
        role: Role::Assistant,
        content: "first answer".into(),
        ..Default::default()
    });
    let path = agent_history_path(&tmp.str(), AgentName::Joey);
    a.save_context(&path).unwrap();
    assert!(path.exists(), "the state folder is created on demand");

    let fresh = solo(AgentName::Joey);
    assert!(fresh.load_context(&path), "the file is recognised");
    let h = fresh.history.lock().unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].content, "first ask");

    // A missing file is not an error, just nothing to restore.
    assert!(!solo(AgentName::Joy).load_context(&agent_history_path(&tmp.str(), AgentName::Joy)));
}

#[test]
fn the_system_message_names_the_agent_and_its_state_folder() {
    let a = solo(AgentName::Joy);
    let sys = a.build_system_message();
    assert!(sys.content.contains("You are JOY"), "{}", sys.content);
    assert!(
        sys.content.contains("testing and verification"),
        "{}",
        sys.content
    );
    assert!(
        sys.content.contains(&format!("{}/", context_dir())),
        "the state folder must be named: {}",
        sys.content
    );
    // jonny's own message must not read like joy's.
    assert!(!solo(AgentName::Jonny)
        .build_system_message()
        .content
        .contains("You are JOY"));
}

#[test]
fn the_history_file_of_an_agent_is_named_in_its_prompt() {
    let tmp = TempDir::new("team-prompt");
    let a = solo(AgentName::Jacky);
    a.set_context_path(&agent_history_path(&tmp.str(), AgentName::Jacky).to_string_lossy());
    let sys = a.build_system_message();
    assert!(sys.content.contains("jacky.md"), "{}", sys.content);
    // Without a work dir (a bare Agent) nothing is claimed about a file.
    assert!(!solo(AgentName::Joey)
        .build_system_message()
        .content
        .contains("is mirrored to"));
}

#[test]
fn activating_an_agent_labels_the_ui_and_its_banner() {
    let log = TestLog::new("team-banner");
    let t = team(&log, Vec::new());
    assert_eq!(
        t.active_name(),
        AgentName::Jonny,
        "unset means the main agent"
    );
    t.start(AgentName::Joy, "/verify after joey");
    assert_eq!(t.active_name(), AgentName::Joy);
    let text = log.contents();
    assert!(text.contains("=== agent joy |"), "{text}");
    assert!(text.contains("/verify after joey"), "{text}");
    assert!(text.contains("testing & verification"), "{text}");
}

#[tokio::test]
async fn headers_carry_the_active_agent_name() {
    let log = TestLog::new("team-header");
    let t = team(&log, vec![content_step("planned")]);
    t.set_model("test-model");
    let jacky = t.start(AgentName::Jacky, "/plan");
    jacky
        .run_turn(CancellationToken::new(), "plan the thing")
        .await
        .unwrap();
    let text = log.contents();
    assert!(text.contains("[jacky] Answer ("), "{text}");
    // A later agent's turn re-labels the headers - the prefix is not frozen
    // to whoever ran first.
    let t2 = TestLog::new("team-header2");
    let ui = Arc::new(Writer::new(&t2.file).unwrap());
    let t = Team::build(
        &crate::config::Config::default(),
        SequenceProvider::new(vec![content_step("verified")]),
        &Registry::new(),
        ui,
    );
    t.set_model("test-model");
    t.start(AgentName::Joy, "/verify")
        .run_turn(CancellationToken::new(), "check it")
        .await
        .unwrap();
    assert!(
        t2.contents().contains("[joy] Answer ("),
        "{}",
        t2.contents()
    );
}

#[test]
fn the_team_shares_tools_but_not_histories() {
    let log = TestLog::new("team-isolation");
    let t = team(&log, Vec::new());
    t.jacky.history.lock().unwrap().push(Message {
        role: Role::User,
        content: "only jacky knows this".into(),
        ..Default::default()
    });
    assert!(t.joey.history.lock().unwrap().is_empty());
    assert!(t.joy.history.lock().unwrap().is_empty());
    assert!(t.jonny.history.lock().unwrap().is_empty());
    // Four distinct conversations - no agent is a second handle on one.
    let ptrs: std::collections::HashSet<_> = t.all().iter().map(|(_, a)| Arc::as_ptr(a)).collect();
    assert_eq!(ptrs.len(), 4, "each agent is its own conversation");
    for (name, a) in t.all() {
        assert_eq!(a.name(), name);
    }
}

#[test]
fn settings_reach_every_agent() {
    let log = TestLog::new("team-config");
    let t = team(&log, Vec::new());
    t.set_model("shared-model");
    t.set_context_size(32_000);
    t.set_thinking(false);
    t.set_max_tool_calls_per_turn(7);
    for (name, a) in t.all() {
        assert_eq!(a.model(), "shared-model", "{name}");
        assert_eq!(
            a.context_size.load(std::sync::atomic::Ordering::SeqCst),
            32_000,
            "{name}"
        );
        assert_eq!(a.max_tool_calls_per_turn(), 7, "{name}");
        assert!(!a.thinking(), "{name}");
    }
    // The default compaction tuning is the one the docs promise.
    let tun = t.jonny.compact_tunables();
    assert_eq!(
        tun.headroom_percent,
        crate::config::DEFAULT_COMPACT_HEADROOM_PERCENT
    );
    assert_eq!(
        tun.target_percent,
        crate::config::DEFAULT_COMPACT_TARGET_PERCENT
    );
    assert_eq!(tun.keep_recent, crate::config::DEFAULT_COMPACT_KEEP_RECENT);
}

#[tokio::test]
async fn a_finished_turn_persists_only_its_own_agent() {
    let tmp = TempDir::new("team-persist");
    let log = TestLog::new("team-persist");
    let ui = Arc::new(Writer::new(&log.file).unwrap());
    let provider = SequenceProvider::new(vec![
        content_step("done"),
        content_step("also done"),
        content_step("third"),
    ]);
    let t = Team::build(
        &crate::config::Config::default(),
        provider,
        &Registry::new(),
        ui.clone(),
    );
    t.set_work_dir(&tmp.str());
    t.set_model("test-model");

    t.joey
        .run_turn(CancellationToken::new(), "implement it")
        .await
        .unwrap();

    let joey = std::fs::read_to_string(agent_history_path(&tmp.str(), AgentName::Joey))
        .expect("joey's history file");
    assert!(joey.starts_with("# jonnyq history - joey"), "{joey}");
    assert!(joey.contains("implement it"), "{joey}");
    assert!(
        !agent_history_path(&tmp.str(), AgentName::Joy).exists(),
        "an agent that never ran writes nothing"
    );
    // save_contexts covers the rest on shutdown.
    t.save_contexts(&tmp.str());
    assert!(agent_history_path(&tmp.str(), AgentName::Joy).exists());
    // ...and load_contexts reads them back. joy's file is a header with no
    // messages in it, so nothing is restored for an agent that never spoke.
    assert_eq!(
        t.load_contexts(&tmp.str()),
        1,
        "only joey's file holds a conversation"
    );
    assert_eq!(t.joey.history.lock().unwrap().len(), 2);
    assert!(t.joy.history.lock().unwrap().is_empty());
}

#[test]
fn a_history_file_written_under_the_old_name_comes_back() {
    let _env = crate::testsupport::clean_env();
    let tmp = TempDir::new("team-rename");
    // The session that built this memory ran when the planner was JACKSON.
    let old = agent_history_path(&tmp.str(), AgentName::Jacky)
        .parent()
        .unwrap()
        .join("jackson.md");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(
        &old,
        format!(
            "{}\n## user\n\nplan the API in three phases\n",
            CONTEXT_FORMAT_TAG
        ),
    )
    .unwrap();

    let log = TestLog::new("team-rename");
    let t = team(&log, Vec::new());
    assert_eq!(
        t.load_contexts(&tmp.str()),
        1,
        "the planner's memory is recognised under its new name"
    );
    assert!(!t.jacky.history.lock().unwrap().is_empty());
    assert!(
        !old.exists(),
        "the old file is renamed, not copied: two files would claim to be one agent's memory"
    );
    assert!(agent_history_path(&tmp.str(), AgentName::Jacky).exists());

    // A second start has nothing left to move, and nothing to complain about.
    let t2 = team(&TestLog::new("team-rename2"), Vec::new());
    assert_eq!(t2.load_contexts(&tmp.str()), 1);

    // An existing jacky.md wins: the migration never overwrites a history.
    let tmp2 = TempDir::new("team-rename-clash");
    std::fs::create_dir_all(state_dir(&tmp2.str())).unwrap();
    std::fs::write(
        state_dir(&tmp2.str()).join("jackson.md"),
        format!("{}\n## user\n\nOLD\n", CONTEXT_FORMAT_TAG),
    )
    .unwrap();
    std::fs::write(
        agent_history_path(&tmp2.str(), AgentName::Jacky),
        format!("{}\n## user\n\nNEW\n", CONTEXT_FORMAT_TAG),
    )
    .unwrap();
    let t3 = team(&TestLog::new("team-rename3"), Vec::new());
    t3.load_contexts(&tmp2.str());
    assert_eq!(t3.jacky.history.lock().unwrap()[0].content, "NEW");
    assert!(state_dir(&tmp2.str()).join("jackson.md").exists());
}

#[test]
fn the_state_dir_setting_moves_every_state_path() {
    let _env = crate::testsupport::clean_env();
    set_context_dir("");
    assert_eq!(context_dir(), DEFAULT_CONTEXT_DIR);
    assert_eq!(agent_history_file(AgentName::Jonny), ".context/jonny.md");
    assert_eq!(progress_file(), ".context/progress.md");

    set_context_dir(".state/");
    assert_eq!(context_dir(), ".state");
    assert_eq!(agent_history_file(AgentName::Joy), ".state/joy.md");
    assert_eq!(progress_hash_file(), ".state/progress.hash");
    assert_eq!(progress_retry_file(), ".state/progress.retry");
    assert_eq!(transcript_file_in(".state"), ".state/transcript.md");
    set_context_dir("");
    assert_eq!(context_dir(), DEFAULT_CONTEXT_DIR);
}

// ---------------------------------------------------------------------------
// the hand-off: work -> joy -> findings -> the author again
// ---------------------------------------------------------------------------

#[test]
fn a_verdict_is_read_from_the_opening_of_the_review() {
    assert_eq!(parse_verdict("APPROVE\n\nall good"), Verdict::Approve);
    assert_eq!(parse_verdict("verdict: approve"), Verdict::Approve);
    let v = parse_verdict("NEEDS FIX\n1. src/a.rs never compiles");
    assert_eq!(
        v,
        Verdict::NeedsFix("1. src/a.rs never compiles".into()),
        "the findings are what the next agent gets"
    );
    // A review that never settles is not a pass.
    assert_eq!(parse_verdict("I looked at it a bit"), Verdict::Unresolved);
    assert_eq!(parse_verdict(""), Verdict::Unresolved);
    // Quoting the rule inside the body is not a verdict, and a body line far
    // below the opening is not read as one either.
    assert_eq!(
        parse_verdict("Everything checks out.\n\nAs the rule says, this is not a NEEDS FIX case."),
        Verdict::Unresolved
    );
    // NEEDS FIX wins when both appear up top: a reviewer who found something
    // has found something.
    assert!(matches!(
        parse_verdict("APPROVE-ish, actually NEEDS FIX\n1. broken"),
        Verdict::NeedsFix(_)
    ));
}

#[test]
fn a_review_of_real_shape_without_a_verdict_line_is_unresolved() {
    // The session that showed the bug: every finding written, evidence quoted,
    // and the verdict left in her reasoning. Nothing in the answer states it,
    // so this must stay Unresolved - which is what makes the review ask for the
    // verdict rather than reporting a verdict it never got.
    let answer = "Verified the revision against the requirements and re-measured every new \
claim. The five defects from round 1 are genuinely fixed (evidence: Phase 1 now mandates \
`CC := gcc`, reproduced with `origin=file` under a hostile env). Nothing was implemented (tree = \
`requirements.md` only), 111 checkbox tasks, every task traceable. Three new defects:\n\n\
1. **`.context/progress.md` Phase 9, task \"Cover tiny-literal acceptance\"** - the expected \
value is factually wrong and will produce a guaranteed false failure: `1e-10` prints \
`0.0000000001`.\n\
2. **Phase 3, Phase 8, Phase 9** - the guard fixtures do not exercise the ERANGE-underflow path \
they claim to guard.\n\
3. **Phase 10, `tools/audit.sh` pattern task** - the pattern is narrower than the requirements it \
is declared to gate: `%e`/`%E` → 0 hits.\n";
    assert_eq!(parse_verdict(answer), Verdict::Unresolved);
}

#[test]
fn a_verdict_stated_on_its_own_line_is_read_wherever_it_sits() {
    // The real failure: she wrote the whole review, evidence and all, and put
    // the verdict below it instead of at the top. Prose that merely mentions
    // the words must still not count (see the case above).
    let review = "Verified the revision against the requirements and re-measured every claim. \
The five defects from round 1 are genuinely fixed (evidence: `CC := gcc` reproduces the strict \
flags under a hostile env).\n\n1. **Phase 9** - the expected value is wrong: `1e-10` prints \
`0.0000000001`, so that task is a guaranteed false failure.\n2. **Phase 3** - the guard fixture \
never reaches ERANGE.\n\nNEEDS FIX\n";
    match parse_verdict(review) {
        Verdict::NeedsFix(_) => {}
        other => panic!("a verdict below the findings must be read: {other:?}"),
    }
    // Decoration, and both orders.
    assert_eq!(parse_verdict("Notes.\n**APPROVE**"), Verdict::Approve);
    assert_eq!(
        parse_verdict("Notes.\n> Verdict: NEEDS FIX\n1. broken"),
        Verdict::NeedsFix("1. broken".into())
    );
    // A verdict with nothing but prose around it is still a verdict.
    assert_eq!(
        parse_verdict("All checks ran clean.\napprove."),
        Verdict::Approve
    );
    // And a reply that only ever *talks about* verdicts remains unresolved.
    assert_eq!(
        parse_verdict(
            "I could not decide whether this was APPROVE or NEEDS FIX.\nThe build is fine."
        ),
        Verdict::Unresolved
    );
    // Below the opening lines, only a line that says *nothing but* the verdict
    // counts: a sentence that happens to begin with the word is prose.
    assert_eq!(
        parse_verdict(
            "Notes on the build.\n\nThe build is clean.\n\nAPPROVE all three phases is what the \
plan asks me to conclude, and it is not what I conclude."
        ),
        Verdict::Unresolved
    );
    // Below the opening, NEEDS FIX wins over APPROVE wherever each sits - so a
    // reviewer who wrote `APPROVE`, then found something, is not read as a pass.
    let changed_mind = "Notes on the run.\n\nThe first two phases check out, and I re-ran both \
suites.\n\nThere is nothing further to add beyond what follows.\n\nAPPROVE\n\nWait - one more \
thing.\n1. the audit pattern misses %e.\n\nNEEDS FIX";
    match parse_verdict(changed_mind) {
        Verdict::NeedsFix(f) => assert!(f.contains("%e"), "{f}"),
        other => panic!("a late NEEDS FIX must outrank an earlier APPROVE: {other:?}"),
    }
}

/// Builds a roster whose provider replays `replies` one turn at a time, so a
/// test can see exactly which agent spoke in which order.
async fn roster(
    tag: &str,
    replies: Vec<String>,
) -> (Team, Arc<SequenceProvider>, TempDir, TestLog) {
    roster_steps(
        tag,
        replies
            .into_iter()
            .map(|t| content_step(&t))
            .collect::<Vec<_>>(),
    )
    .await
}

/// As [`roster`], but each turn is scripted as raw events - for the turns that
/// are not plain text answers (a reasoning model's thinking-only reply, say).
async fn roster_steps(
    tag: &str,
    steps: Vec<Vec<crate::llm::ChatEvent>>,
) -> (Team, Arc<SequenceProvider>, TempDir, TestLog) {
    let dir = TempDir::new(tag);
    let log = TestLog::new(tag);
    let ui = Arc::new(Writer::new(&log.file).unwrap());
    let provider = SequenceProvider::strict(steps);
    let mut reg = Registry::new();
    reg.register(Arc::new(EchoTool::new()));
    let cfg = crate::config::Config {
        model: "test-model".into(),
        ..Default::default()
    };
    let t = Team::build(&cfg, provider.clone(), &reg, ui);
    t.set_work_dir(&dir.str());
    t.set_verify(true);
    t.set_review_rounds(1);
    (t, provider, dir, log)
}

#[tokio::test]
async fn the_verdict_ends_the_hand_off() {
    let (t, provider, _dir, log) = roster(
        "review-approve",
        vec!["I wrote the plan".into(), "APPROVE".into()],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert_eq!(v, Verdict::Approve);
    assert_eq!(provider.call_count(), 2, "work, then one review");
    let text = log.contents();
    assert!(text.contains("=== agent jacky |"), "{text}");
    assert!(text.contains("=== agent joy |"), "{text}");
    assert!(text.contains("joy verified"), "{text}");
}

#[tokio::test]
async fn findings_go_back_to_the_agent_that_produced_them() {
    let (t, provider, _dir, log) = roster(
        "review-fix",
        vec![
            "I wrote the plan".into(),
            "NEEDS FIX\n1. phase 2 has no tests".into(),
            "I fixed the plan".into(),
            "APPROVE".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert_eq!(v, Verdict::Approve);
    assert_eq!(provider.call_count(), 4, "work, review, fix, re-review");
    let text = log.contents();
    assert!(text.contains("round 1/1"), "{text}");
    // The fix turn belongs to the author, not to the reviewer.
    assert!(
        text.contains("joy's findings on /plan"),
        "the fix turn was not announced as jacky's: {text}"
    );
    // And the findings themselves reach the author's prompt.
    assert!(text.contains("phase 2 has no tests"), "{text}");
}

#[tokio::test]
async fn the_loop_is_bounded_and_reports_the_disagreement() {
    // joy never relents: one fix round, then the findings are reported.
    let (t, provider, _dir, log) = roster(
        "review-stuck",
        vec![
            "worked".into(),
            "NEEDS FIX\n1. still broken".into(),
            "tried again".into(),
            "NEEDS FIX\n1. still broken".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let joey = t.start(AgentName::Joey, "/coding");
    let work = joey.run_turn(cancel.clone(), "implement it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Joey,
            "/coding on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::NeedsFix(_)), "{v:?}");
    assert_eq!(
        provider.call_count(),
        4,
        "the loop must stop at the round limit"
    );
    let text = log.contents();
    assert!(
        text.contains("still unresolved after 1 fix round"),
        "{text}"
    );
}

#[tokio::test]
async fn a_review_without_a_verdict_says_the_work_is_unverified() {
    // Two shots at it - the review, then the verdict-only follow-up - and she
    // commits to neither: the work must come back unverified, not approved.
    let (t, provider, _dir, log) = roster(
        "review-noverdict",
        vec![
            "worked".into(),
            "hmm, I looked at some of it".into(),
            "I am not sure either way".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let joey = t.start(AgentName::Joey, "/coding");
    let work = joey.run_turn(cancel.clone(), "implement it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Joey,
            "/coding on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert_eq!(v, Verdict::Unresolved);
    assert_eq!(
        provider.call_count(),
        3,
        "the verdict is asked for once, and only once"
    );
    let text = log.contents();
    assert!(text.contains("came back without a verdict"), "{text}");
    assert!(text.contains("NOT verified"), "{text}");
}

/// The failure a real session showed: joy did the whole review, but the answer
/// streamed to the terminal opened with prose - the verdict line lived in her
/// reasoning, which jonnyq deliberately does not treat as what she said. The
/// review must not be thrown away for that: she is asked once, in her own
/// context, for nothing but the verdict.
#[tokio::test]
async fn a_verdict_left_in_the_reasoning_channel_is_asked_for_once() {
    let review = "Verified the revision against the requirements and re-measured every new \
claim. The five defects from round 1 are genuinely fixed.\n\n1. Phase 9 asserts `mmath \
\"1*0.0000000001\"` prints `0`, but it prints `0.0000000001` - a guaranteed false failure.\n\
2. Phase 8's tiny literal never sets ERANGE, so the guard test is vacuous.";
    let (t, provider, _dir, log) = roster_steps(
        "review-thinking-verdict",
        vec![
            content_step("I wrote the plan"),
            thinking_then_prose_step(
                "Findings 1 and 2 are real, so the verdict is NEEDS FIX with 2 findings.",
                review,
            ),
            content_step(
                "NEEDS FIX\n1. Phase 9's expected value must be `0.0000000001`.\n2. Use the \
400-digit underflow literal as the ERANGE fixture.",
            ),
            content_step("I fixed the plan"),
            content_step("APPROVE"),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::Approve), "{v:?}");
    // work, review, verdict-ask, fix, re-review
    assert_eq!(provider.call_count(), 5, "{:?}", provider.call_count());
    let ask = provider.last_user_content(2);
    assert!(
        ask.contains("could not act on it") && ask.contains("a guaranteed false failure"),
        "the follow-up does not ask for the verdict, or does not quote her review: {ask}"
    );
    // The findings she then states reach the author as instructions.
    let fix = provider.last_user_content(3);
    assert!(
        fix.contains("0.0000000001"),
        "the author was not sent the verdict's findings: {fix}"
    );
    // The review's substance is not lost on the way - the person can read it.
    let text = log.contents();
    assert!(text.contains("false failure"), "{text}");
    assert!(text.contains("came back without a verdict"), "{text}");
}

/// A follow-up that answers `NEEDS FIX` on one line carries the verdict but
/// none of the substance; the author must still be told what to fix.
#[tokio::test]
async fn a_bare_verdict_follow_up_hands_back_the_review_itself() {
    let (t, provider, _dir, _log) = roster(
        "review-bare-verdict",
        vec![
            "I wrote the plan".into(),
            "The plan looks complete, and every claim I re-measured holds up. One thing is \
wrong though: Phase 10's audit pattern does not match `%e`.\n\nHmm. Let me think about that."
                .into(),
            "NEEDS FIX".into(),
            "I fixed the plan".into(),
            "APPROVE".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::Approve), "{v:?}");
    let fix = provider.last_user_content(3);
    assert!(
        fix.contains("Phase 10's audit pattern"),
        "the author was not sent the findings: {fix}"
    );
}

#[tokio::test]
async fn verify_off_runs_the_work_alone() {
    let (t, provider, _dir, log) = roster("review-off", vec!["worked".into()]).await;
    t.set_verify(false);
    let cancel = CancellationToken::new();
    let joey = t.start(AgentName::Joey, "/coding");
    let work = joey.run_turn(cancel.clone(), "implement it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Joey,
            "/coding on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert_eq!(v, Verdict::Skipped);
    assert_eq!(provider.call_count(), 1, "no review conversation at all");
    assert!(
        !log.contents().contains("=== agent joy |"),
        "{}",
        log.contents()
    );
}

/// A plan review and a code review are different jobs: the same code-shaped
/// brief used to send joy off to re-run "the real build and test commands" over
/// a checklist that has no build, and never asked her whether the plan covers
/// every requirement - which is the one thing a plan can fail at.
#[tokio::test]
async fn a_plan_is_reviewed_for_coverage_and_a_change_for_scope() {
    let _env = crate::testsupport::clean_env();
    let (t, provider, _dir, _log) = roster(
        "review-brief",
        vec![
            "I wrote the plan".into(),
            "NEEDS FIX\n1. no task delivers the currency conversion in requirements.md".into(),
            "I fixed the plan".into(),
            "APPROVE".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::Approve), "{v:?}");
    let plan_brief = provider.last_user_content(1);
    for want in [
        "reviewing a PLAN, not code",
        "COVERAGE",
        "COMPLETENESS",
        "GRANULARITY AND SCOPE",
        "VERIFIABILITY",
        "FALSE WORK",
        "requirements.md",
        ".context/progress.md",
    ] {
        assert!(
            plan_brief.contains(want),
            "the plan brief is missing {want}: {plan_brief}"
        );
    }
    // She must not be sent looking for a build that a plan does not have.
    assert!(
        !plan_brief.contains("re-run the real build and test commands"),
        "the plan brief still asks for a build: {plan_brief}"
    );

    // The same reviewer, reviewing an implementation, gets the code brief
    // instead - and it names scope as a reviewable defect.
    let (t, provider, _dir, _log) = roster(
        "review-brief-code",
        vec!["I implemented it".into(), "APPROVE".into()],
    )
    .await;
    let joey = t.start(AgentName::Joey, "/coding");
    let work = joey.run_turn(cancel.clone(), "implement it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Joey,
            "/coding on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::Approve), "{v:?}");
    let code_brief = provider.last_user_content(1);
    for want in [
        "re-run the real build and test commands",
        "the SCOPE of the change",
        "belongs to a later",
        "ticked without real",
    ] {
        assert!(
            code_brief.contains(want),
            "the code brief is missing {want}: {code_brief}"
        );
    }
    assert!(!code_brief.contains("reviewing a PLAN"));
}

/// A finding of "you went beyond the task" must not be answered with more code:
/// the fix turn tells the coder to remove the surplus.
#[tokio::test]
async fn a_scope_finding_is_fixed_by_removing_not_adding() {
    let (t, provider, _dir, _log) = roster(
        "review-scope-fix",
        vec![
            "I implemented it".into(),
            "NEEDS FIX\n1. index.html already implements the whole evaluator, which is task 6's \
scope"
                .into(),
            "I removed it".into(),
            "APPROVE".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let joey = t.start(AgentName::Joey, "/coding");
    let work = joey.run_turn(cancel.clone(), "implement it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Joey,
            "/coding on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert!(matches!(v, Verdict::Approve), "{v:?}");
    let fix = provider.last_user_content(2);
    assert!(
        fix.contains("remove the surplus"),
        "the coder was not told to shrink: {fix}"
    );
    assert!(fix.contains("stay inside the task's own scope"), "{fix}");
}

/// The review is the roster's own doing, so the person can see which of the
/// four is responsible for the state of the code - including when verification
/// is switched off.
#[tokio::test]
async fn team_shows_whether_verification_is_on() {
    let log = TestLog::new("team-verify");
    let t = team(&log, Vec::new());
    t.set_verify(false);
    t.set_review_rounds(3);
    let _ = &t;
    // Asserted through the REPL command in repl::tests, where the roster is
    // wired to a Repl; here the two settings simply have to be readable.
    assert!(!t.verify());
    assert_eq!(t.review_rounds(), 3);
}

#[tokio::test]
async fn a_hand_off_names_the_context_file_to_read() {
    // The reviewer's whole advantage is that she can read the author's
    // history instead of trusting its reply; the fix turn gets the same
    // pointer back into joy's findings. Both prompts must say the path.
    let _env = crate::testsupport::clean_env();
    let (t, provider, _dir, _log) = roster(
        "review-prompt",
        vec![
            "I wrote the plan".into(),
            "NEEDS FIX\n1. phase 2 has no tests".into(),
            "I fixed the plan".into(),
            "APPROVE".into(),
        ],
    )
    .await;
    let cancel = CancellationToken::new();
    let jacky = t.start(AgentName::Jacky, "/plan");
    let work = jacky.run_turn(cancel.clone(), "plan it");
    let v = t
        .review_after(
            &cancel,
            AgentName::Jacky,
            "/plan on .context/progress.md",
            work,
        )
        .await
        .unwrap();
    assert_eq!(v, Verdict::Approve);

    let review = provider.last_user_content(1);
    assert!(
        review.contains(&AgentName::Jacky.context_file()),
        "joy is not told where jacky's history is: {review}"
    );
    let fix = provider.last_user_content(2);
    assert!(
        fix.contains(&AgentName::Joy.context_file()),
        "jacky is not told where joy's findings are recorded: {fix}"
    );
    // And neither hand-off is a licence to rewrite it.
    for p in [review, fix] {
        assert!(
            !p.to_lowercase().contains("write your findings into"),
            "a hand-off asks for a history to be written: {p}"
        );
    }
}

#[tokio::test]
async fn joy_s_verdict_is_pushed_to_ntfy() {
    let srv = TestServer::canned(vec![StubResponse::json("ok")]).await;
    let log = TestLog::new("joy-notify");
    let t = team(&log, Vec::new());
    t.set_ntfy_url(&srv.url);
    t.notify_verdict(
        "joy reviewed /plan",
        &Verdict::NeedsFix("1. nothing builds".into()),
        "Work reviewed: /plan\n\nJOY'S FINDINGS\n1. nothing builds\n2. tests skipped",
    )
    .await;
    let notes = srv.notifications();
    assert_eq!(notes.len(), 1, "one push per verdict");
    let (title, body) = &notes[0];
    assert!(title.contains("joy"), "title: {title}");
    assert!(body.contains("NEEDS FIX"), "body: {body}");
    assert!(body.contains("/plan"), "body: {body}");
    // Her findings arrive as a list, not as one wrapped line.
    assert!(
        body.contains("1. nothing builds\n2. tests skipped"),
        "body: {body}"
    );
    // A refusal is the one verdict worth waking someone for.
    assert_eq!(
        srv.requests()[0].header("priority").as_deref(),
        Some("high"),
        "a refusal is sent at high priority"
    );
}

#[tokio::test]
async fn joy_approves_quietly_and_no_topic_sends_nothing() {
    let srv = TestServer::canned(vec![StubResponse::json("ok")]).await;
    let log = TestLog::new("joy-notify-ok");
    let t = team(&log, Vec::new());
    t.set_ntfy_url(&srv.url);
    t.notify_verdict(
        "joy verified one task of /autocoding",
        &Verdict::Approve,
        "Task: make it so",
    )
    .await;
    let notes = srv.notifications();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].1.contains("APPROVED"), "body: {}", notes[0].1);
    assert_eq!(
        srv.requests()[0].header("priority").as_deref(),
        None,
        "an approval is not an alert"
    );
    // /ntfy off: the same call is a no-op rather than an error.
    t.set_ntfy_url("");
    t.notify_verdict("joy reviewed /plan", &Verdict::Unresolved, "")
        .await;
    assert_eq!(srv.request_count(), 1, "no topic, no push");
}

#[tokio::test]
async fn a_verdict_that_was_never_given_is_not_pushed() {
    // /verify off: the loop reports "verification off" on the terminal, but a
    // push per skipped task would be noise on every run.
    let srv = TestServer::canned(vec![StubResponse::json("ok")]).await;
    let log = TestLog::new("joy-notify-skipped");
    let t = team(&log, Vec::new());
    t.set_ntfy_url(&srv.url);
    t.notify_verdict(
        "joy reviewed /plan",
        &Verdict::Skipped,
        "Work reviewed: /plan",
    )
    .await;
    assert_eq!(srv.request_count(), 0, "a skipped review is not news");
}
