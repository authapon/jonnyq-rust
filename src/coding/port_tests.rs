#[cfg(test)]
mod tests {
    use crate::coding::*;
    use crate::llm::ChatEvent;
    use crate::testsupport::{
        content_step, content_step_with_usage, write_progress_step, CodingEnv, StubResponse,
        TestServer,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// One ".progress" body, written with real newlines (Rust multiline
    /// string literals) so the test source needs no escape sequences.
    const ONE_TASK: &str = "- [ ] task one
";
    const DONE_TASK: &str = "- [x] task one
";

    /// A project with requirements.md and a provider scripted to answer
    /// `responses`; the returned provider handle reports what was sent.
    fn env(
        tag: &str,
        responses: Vec<Vec<ChatEvent>>,
    ) -> (CodingEnv, Arc<crate::testsupport::SequenceProvider>) {
        let provider = crate::testsupport::SequenceProvider::strict(responses);
        let e = CodingEnv::with_provider(tag, provider.clone());
        (e, provider)
    }

    /// A stub ntfy endpoint: records everything posted to it.
    async fn ntfy() -> TestServer {
        TestServer::with_handler(|_r| StubResponse::status(200, "")).await
    }

    #[tokio::test]
    async fn plan_generates_progress_when_missing() {
        let (e, provider) = env(
            "plan-gen",
            vec![write_progress_step(ONE_TASK), content_step("ok")],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 2, "expected toolcall + final answer");
        assert_eq!(e.dir.read(&progress_file()), ONE_TASK);
        assert_eq!(
            e.dir.read(&hash_file()).len(),
            64,
            "sha256 hex of requirements"
        );
    }

    #[tokio::test]
    async fn plan_generate_prompt_asks_for_granular_english_phases() {
        let (e, provider) = env(
            "plan-prompt",
            vec![write_progress_step(ONE_TASK), content_step("ok")],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap();
        let prompt = provider.last_user_content(0);
        for want in [
            "in English",
            "existing code",
            "detailed and granular",
            "## Phase 1: <Phase Name>",
            "# <Project Title>",
            "REQ-012",
            "boundary is unambiguous",
        ] {
            assert!(prompt.contains(want), "prompt missing {want}: {prompt}");
        }
    }

    #[tokio::test]
    async fn plan_skips_when_requirements_unchanged() {
        let (e, provider) = env("plan-skip", Vec::new());
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.dir.write(&progress_file(), DONE_TASK);
        let hash = file_hash(e.dir.read(REQUIREMENTS_FILE).as_bytes());
        e.dir.write(&hash_file(), &hash);
        e.coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap();
        assert_eq!(
            provider.call_count(),
            0,
            "no model call when nothing changed"
        );
    }

    #[tokio::test]
    async fn plan_reconciles_when_requirements_changed() {
        let (e, provider) = env(
            "plan-recon",
            vec![
                write_progress_step(
                    "- [x] old task
- [ ] new task
",
                ),
                content_step("ok"),
            ],
        );
        e.dir
            .write(REQUIREMENTS_FILE, "build a thing, and also a new feature");
        e.dir.write(
            &progress_file(),
            "- [x] old task
",
        );
        e.dir.write(&hash_file(), "stale-hash");
        e.coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 2);
        assert_eq!(
            e.dir.read(&progress_file()),
            "- [x] old task
- [ ] new task
"
        );
        let prompt = provider.last_user_content(0);
        for want in ["in English", "detailed and granular", "## Phase N: <name>"] {
            assert!(prompt.contains(want), "reconcile prompt missing {want}");
        }
        let want = file_hash(e.dir.read(REQUIREMENTS_FILE).as_bytes());
        assert_eq!(
            e.dir.read(&hash_file()),
            want,
            "hash must track the new requirements"
        );
    }

    #[tokio::test]
    async fn plan_errors_without_requirements_file() {
        let (e, _p) = env("plan-noreq", Vec::new());
        let err = e
            .coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap_err();
        assert!(
            err.message.contains(REQUIREMENTS_FILE),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn plan_resets_coding_mode() {
        let (e, _p) = env(
            "plan-mode",
            vec![write_progress_step(ONE_TASK), content_step("ok")],
        );
        e.dir.write(REQUIREMENTS_FILE, "x");
        e.coding("")
            .plan(&CancellationToken::new(), false)
            .await
            .unwrap();
        assert!(!e.agent.coding_mode(), "coding mode must be restored");
    }

    // ---- /coding (one task) ----

    #[tokio::test]
    async fn run_one_task_does_exactly_one_task_then_stops() {
        let two = "- [ ] task one
- [ ] task two
";
        let one_done = "- [x] task one
- [ ] task two
";
        let (e, provider) = env(
            "one-one",
            vec![write_progress_step(one_done), content_step("done")],
        );
        e.dir.write(&progress_file(), two);
        e.coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 2, "one task = toolcall + final");
        assert_eq!(
            e.dir.read(&progress_file()),
            one_done,
            "only the first task touched"
        );
    }

    #[tokio::test]
    async fn run_one_task_errors_without_progress_file() {
        let (e, _p) = env("one-noprog", Vec::new());
        let err = e
            .coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            err.message.contains("/plan"),
            "error should point at /plan, got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn run_one_task_all_done_makes_no_model_call() {
        let (e, provider) = env("one-alldone", Vec::new());
        e.dir.write(&progress_file(), DONE_TASK);
        e.coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn run_one_task_clears_retry_state_when_task_completes() {
        let (e, provider) = env(
            "one-clear",
            vec![write_progress_step(DONE_TASK), content_step("done")],
        );
        e.dir.write(&progress_file(), ONE_TASK);
        e.dir.write(&retry_file(), "2\t0\ttask one");
        e.coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap();
        let prompt = provider.last_user_content(0);
        assert!(
            prompt.contains("already attempted 2 time(s)"),
            "prior count reused: {prompt}"
        );
        assert!(
            !e.dir.path().join(retry_file()).exists(),
            "retry state must clear on success"
        );
    }

    /// The over-implementation this guards: the task was "create `index.html`"
    /// and the coder wrote the whole app into it, then spent the turn testing it.
    /// The task prompt must name the boundary AND size the verification to the
    /// task - naming the boundary alone still left the old "run the project's
    /// build and test commands" demand pushing the coder to create behaviour
    /// that would be worth testing.
    #[test]
    fn task_prompt_bounds_the_task_and_its_verification() {
        let p = build_task_prompt(
            "Create the file `index.html`.",
            Status::Open,
            0,
            false,
            None,
        );
        for want in [
            "SCOPE",
            "nothing beyond it",
            "Later tasks are not yours to start now",
            "Verify only what THIS task delivers",
            "not build a test harness",
            "Do not write functionality just so there is something to test",
        ] {
            assert!(p.contains(want), "task prompt missing {want}: {p}");
        }
        // The old whole-project demand, which read as "build the app to test it".
        assert!(
            !p.contains("the real build and test commands for this project"),
            "task prompt still asks for the project's whole build: {p}"
        );
        // Ticking the box on proof is still required - the scope rule must not
        // have loosened the verification half.
        assert!(
            p.contains("this task's own verification pass in this turn"),
            "{p}"
        );
        assert!(p.contains("Create the file `index.html`."));
    }

    #[tokio::test]
    async fn run_one_task_persists_retry_state_when_stuck() {
        let (e, _p) = env("one-stuck", vec![content_step("I already did this")]);
        e.dir.write(&progress_file(), ONE_TASK);
        e.coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap();
        let state = e.dir.read(&retry_file());
        assert!(state.starts_with("1\t"), "expected count 1, got {state:?}");
        assert!(state.contains("task one"));
    }

    /// The case that used to end in silence: `/verify off`, the coder's turn is
    /// spent, the box never moves — so nothing was gated, nothing was refused,
    /// and the command simply returned. The person gets one message saying the
    /// task is still open and why nothing was checked.
    #[tokio::test]
    async fn verify_off_reports_when_the_task_did_not_move() {
        let srv = ntfy().await;
        let (e, _p) = env("one-open-off", vec![content_step("I already did this")]);
        e.dir.write(&progress_file(), ONE_TASK);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(false);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        team.review_after(
            &CancellationToken::new(),
            crate::team::AgentName::Joey,
            "/coding on .context/progress.md",
            c.run_one_task(&CancellationToken::new()),
        )
        .await
        .unwrap();
        let notes = srv.notifications();
        assert_eq!(notes.len(), 1, "one push for the whole command: {notes:?}");
        let (title, body) = &notes[0];
        assert!(
            title.contains("/coding"),
            "title names the command: {title}"
        );
        assert!(
            title.contains("still open"),
            "the title says the task did not finish: {title}"
        );
        for want in [
            "STILL OPEN",
            "task one",
            "1 attempt",
            "verify off",
            "Verify is off",
        ] {
            assert!(body.contains(want), "body missing {want}: {body}");
        }
    }

    /// The same ending with verification on: the run reports the open task, but
    /// the report waits for JOY's verdict and goes out under it — one message,
    /// the verdict in front.
    #[tokio::test]
    async fn verify_on_reports_the_open_task_under_the_verdict() {
        let srv = ntfy().await;
        let (e, provider) = env(
            "one-open-on",
            vec![
                content_step("I already did this"),
                content_step("NEEDS FIX\n1. the checklist still says the task is open"),
            ],
        );
        e.dir.write(&progress_file(), ONE_TASK);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        team.set_review_rounds(0);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        team.review_after(
            &CancellationToken::new(),
            crate::team::AgentName::Joey,
            "/coding on .context/progress.md",
            c.run_one_task(&CancellationToken::new()),
        )
        .await
        .unwrap();
        assert_eq!(provider.call_count(), 2, "one coding turn, one review");
        let notes = srv.notifications();
        assert_eq!(notes.len(), 1, "one push for the whole command: {notes:?}");
        let (title, body) = &notes[0];
        // The held report's own title is replaced by the verdict's: the review,
        // not the attempt, is what a person reads the outcome from.
        assert!(
            title.contains("/coding") && title.contains("did NOT approve"),
            "title: {title}"
        );
        assert!(
            body.starts_with("VERDICT:"),
            "the verdict comes first: {body}"
        );
        assert!(body.contains("STILL OPEN"), "body: {body}");
    }

    #[tokio::test]
    async fn run_one_task_stalls_after_max_attempts() {
        let (e, _p) = env("one-stall", vec![content_step("still done, trust me")]);
        e.dir.write(&progress_file(), ONE_TASK);
        e.dir.write(&retry_file(), "4\t0\ttask one");
        let err = e
            .coding("")
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            err.message.contains("made no progress after 5 attempts"),
            "got: {}",
            err.message
        );
    }

    // ---- /autocoding ----

    /// `/verify on` puts the plan in front of the coder: JOY reviews what JACKY
    /// just wrote and approves it BEFORE a single task is coded. The order of the
    /// provider's requests is the proof — the plan review must sit between the
    /// generation of the checklist and the first coding turn.
    #[tokio::test]
    async fn auto_run_with_verify_on_has_joy_approve_the_plan_before_any_coding() {
        let srv = ntfy().await;
        let (e, provider) = env(
            "auto-plan-gate-ok",
            vec![
                // jacky writes the plan.
                write_progress_step(ONE_TASK),
                content_step("planned it"),
                // joy reads the plan itself (a plan review, not a build).
                content_step("APPROVE\nthe checklist covers every requirement"),
                // only now does joey get his turn, and joy verifies his work.
                write_progress_step("- [?] task one\n"),
                content_step("coded it"),
                content_step("APPROVE"),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        team.set_review_rounds(1);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        c.auto_run(&CancellationToken::new()).await.unwrap();

        assert_eq!(provider.call_count(), 6, "plan(2) + plan review + task(2) + review");
        let reqs = provider.sent_reqs.lock().unwrap();
        let plan_review = reqs[2].messages.last().unwrap().content.clone();
        assert!(
            plan_review.contains("Review the work jacky just finished"),
            "the third turn is joy's review of the plan: {plan_review}"
        );
        assert!(
            plan_review.contains("You are reviewing a PLAN"),
            "a checklist gets the plan brief, not the code brief: {plan_review}"
        );
        let first_coding = reqs[3].messages.last().unwrap().content.clone();
        assert!(
            first_coding.contains("task one"),
            "the task turn follows the plan verdict: {first_coding}"
        );
        assert_eq!(e.dir.read(&progress_file()), "- [A] task one\n");
        // One push for the whole command, and it says the plan was approved.
        let notes = srv.notifications();
        assert_eq!(notes.len(), 1, "one push for the whole run: {notes:?}");
        let (title, body) = &notes[0];
        assert!(title.contains("/autocoding complete"), "title: {title}");
        assert!(
            body.contains("joy approved it"),
            "the report says the plan was verified first: {body}"
        );
        assert!(body.contains("1/1 tasks approved"), "body: {body}");
    }

    /// A plan JOY will not approve stops the run BEFORE any coding: the checklist
    /// is untouched, nothing was coded, and the one thing that goes out is the
    /// problem summary at high priority, carrying her findings.
    #[tokio::test]
    async fn auto_run_stops_at_a_refused_plan_and_codes_nothing() {
        let srv = ntfy().await;
        let (e, provider) = env(
            "auto-plan-gate-refused",
            vec![
                write_progress_step(ONE_TASK),
                content_step("planned it"),
                // joy refuses; jacky corrects the plan; joy still refuses, and
                // the round budget is spent.
                content_step("NEEDS FIX\n1. REQ-002 has no task at all"),
                content_step("added the missing task"),
                content_step("NEEDS FIX\n1. still no task for REQ-002"),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        team.set_review_rounds(1);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        let err = c.auto_run(&CancellationToken::new()).await.unwrap_err();

        assert!(
            err.message.contains("did not clear joy's review") && err.notified,
            "the stop is reported once: {err:?}"
        );
        assert_eq!(provider.call_count(), 5, "plan(2) + review + fix + review");
        // Nothing was coded: the checklist is exactly as it was generated.
        assert_eq!(e.dir.read(&progress_file()), ONE_TASK);
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1, "one push for the whole run: {reqs:?}");
        let alert = &reqs[0];
        assert_eq!(
            alert.header("priority").as_deref(),
            Some("high"),
            "a stopped run is an alert"
        );
        assert!(
            alert.header("title").unwrap_or_default().contains("did not approve the plan"),
            "title: {}",
            alert.header("title").unwrap_or_default()
        );
        // The findings that go out are the ones from the review that ended it —
        // a re-review restates what still stands, as in the task gate.
        for want in ["STOPPED AT THE PLAN", "NO TASK WAS CODED", "still no task for REQ-002"] {
            assert!(alert.body.contains(want), "body missing {want}: {}", alert.body);
        }
    }

    /// The requirements did not move, so the plan is not new work: JACKY runs no
    /// turn and JOY is not asked to re-read a checklist she already ruled on —
    /// the run goes straight to the tasks.
    #[tokio::test]
    async fn auto_run_does_not_re_plan_or_re_review_an_unchanged_plan() {
        let (e, provider) = env(
            "auto-plan-unchanged",
            vec![
                write_progress_step("- [?] task one\n"),
                content_step("coded it"),
                content_step("APPROVE"),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.dir.write(&progress_file(), ONE_TASK);
        let hash = file_hash(e.dir.read(REQUIREMENTS_FILE).as_bytes());
        e.dir.write(&hash_file(), &hash);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), "");
        c.auto_run(&CancellationToken::new()).await.unwrap();
        assert_eq!(
            provider.call_count(),
            3,
            "no planning turn and no plan review: only the task and its verdict"
        );
        let reqs = provider.sent_reqs.lock().unwrap();
        assert!(
            reqs[0].messages.last().unwrap().content.contains("task one"),
            "the first turn is the coder's"
        );
        assert_eq!(e.dir.read(&progress_file()), "- [A] task one\n");
    }

    #[tokio::test]
    async fn auto_run_generates_and_completes_all_tasks() {
        let (e, provider) = env(
            "auto-all",
            vec![
                write_progress_step(ONE_TASK),
                content_step("ok"),
                write_progress_step(DONE_TASK),
                content_step("done"),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.coding("")
            .auto_run(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 4, "generate(2) + one task(2)");
        assert_eq!(e.dir.read(&progress_file()), DONE_TASK);
    }

    #[tokio::test]
    async fn auto_run_resets_history_before_each_task() {
        let two = "- [ ] first task
- [ ] second task
";
        let first_done = "- [x] first task
- [ ] second task
";
        let both_done = "- [x] first task
- [x] second task
";
        let (e, provider) = env(
            "auto-hist",
            vec![
                write_progress_step(two),
                content_step("plan reply"),
                write_progress_step(first_done),
                content_step("task1 reply"),
                write_progress_step(both_done),
                content_step("task2 reply"),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build two things");
        e.coding("")
            .auto_run(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 6);
        // The second task's two requests must not carry earlier replies.
        let reqs = provider.sent_reqs.lock().unwrap();
        for idx in [4usize, 5] {
            let msgs = &reqs[idx].messages;
            let all: String = msgs.iter().map(|m| m.content.clone()).collect();
            assert!(
                !all.contains("plan reply"),
                "planning reply leaked into task 2"
            );
            assert!(
                !all.contains("task1 reply"),
                "task 1 reply leaked into task 2"
            );
        }
        let n = reqs[5].messages.len();
        assert!(
            (2..=4).contains(&n),
            "second task request should be self-contained, got {n}"
        );
    }

    // ---- ntfy notifications ----

    fn usage() -> crate::llm::Usage {
        crate::llm::Usage {
            prompt_tokens: 500,
            completion_tokens: 120,
            total_tokens: 620,
            load_duration: std::time::Duration::from_millis(200),
            prompt_eval_duration: std::time::Duration::from_millis(80),
            eval_duration: std::time::Duration::from_secs(4),
            has_timing: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn plan_notifies_on_generate_with_stats() {
        let srv = ntfy().await;
        let two = "- [ ] task one
- [x] task two
";
        let (e, _p) = env(
            "plan-notify",
            vec![
                write_progress_step(two),
                content_step_with_usage("ok", usage()),
            ],
        );
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.coding(&srv.url)
            .plan(&CancellationToken::new(), true)
            .await
            .unwrap();
        let notes = srv.notifications();
        assert_eq!(notes.len(), 1, "exactly one notification");
        let (title, body) = &notes[0];
        assert!(title.contains("/plan"), "title: {title}");
        for want in [
            "1/2 tasks approved",
            "Started:",
            "Finished:",
            "Duration:",
            "token_in=500",
            "token_out=120",
            "total_token=620",
            "preload=200ms",
            "prompt_eval=80ms",
            "generate=4s",
        ] {
            assert!(body.contains(want), "body missing {want}: {body}");
        }
    }

    /// `/verify` on, run the way the REPL runs it: the command under the
    /// review. One command, one notification — the run's summary goes out once,
    /// after JOY has ruled, with her verdict on the front. No push per task, and
    /// no second push for the review.
    #[tokio::test]
    async fn a_verified_run_notifies_once_with_the_verdict_on_the_front() {
        let srv = ntfy().await;
        let (e, provider) = env(
            "joy-gate-push",
            vec![
                // joey's turn: tick [?], then his final answer...
                write_progress_step("- [?] task one\n"),
                content_step("coded it"),
                // ...then joy's review of the task, one turn with no tool calls,
                // then her review of the command.
                content_step("APPROVE\n\nbuild and the tests for it both pass"),
                content_step("APPROVE\n\nthe task matches its line"),
            ],
        );
        e.dir.write(&progress_file(), ONE_TASK);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        // The REPL keeps the roster's topic and the command's copy in step; a
        // test that wants the report pushed has to do the same.
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        let cancel = CancellationToken::new();
        team.review_after(
            &cancel,
            crate::team::AgentName::Joey,
            "/coding on .context/progress.md",
            c.run_one_task(&cancel),
        )
        .await
        .unwrap();
        // joey's turn is two calls (the tick, then his answer), then two reviews.
        assert_eq!(provider.call_count(), 4);
        assert_eq!(e.dir.read(&progress_file()), "- [A] task one\n");
        let notes = srv.notifications();
        assert_eq!(notes.len(), 1, "one push for the whole command: {notes:?}");
        let (title, body) = &notes[0];
        assert!(
            title.contains("/coding"),
            "title names the command: {title}"
        );
        assert!(title.contains("approved"), "title: {title}");
        assert!(body.starts_with("VERDICT: joy: APPROVED"), "body: {body}");
        // The work summary rides along underneath the verdict.
        assert!(body.contains("Completed:"), "body: {body}");
        assert!(body.contains("1/1 tasks approved"), "body: {body}");
    }

    /// The other half of the contract: JOY keeps refusing after the fix rounds
    /// are spent, so the run stops rather than building on the refused task, and
    /// the one notification that goes out is the stop report at high priority,
    /// carrying her findings as the problem summary.
    #[tokio::test]
    async fn a_refused_run_stops_and_pushes_one_high_priority_report() {
        let srv = ntfy().await;
        let (e, _p) = env(
            "joy-gate-refused",
            vec![
                // joey codes it; joy refuses; joey re-ticks it [+] and answers;
                // joy refuses again and the round budget is spent.
                write_progress_step("- [?] task one\n"),
                content_step("coded it"),
                content_step("NEEDS FIX\n1. nothing builds\n2. no tests were run"),
                write_progress_step("- [+] task one\n"),
                content_step("fixed, it builds now"),
                content_step("NEEDS FIX\n1. still nothing builds"),
            ],
        );
        e.dir.write(&progress_file(), ONE_TASK);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(true);
        team.set_review_rounds(1);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        let cancel = CancellationToken::new();
        let err = team
            .review_after(
                &cancel,
                crate::team::AgentName::Joey,
                "/coding on .context/progress.md",
                c.run_one_task(&cancel),
            )
            .await
            .unwrap_err();
        assert!(err.notified, "the refusal was reported once: {err:?}");
        assert_eq!(e.dir.read(&progress_file()), "- [F] task one\n");
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1, "one push for the whole command: {reqs:?}");
        let refused = &reqs[0];
        assert_eq!(
            refused.header("priority").as_deref(),
            Some("high"),
            "a refusal is sent at high priority"
        );
        assert!(
            refused.body.contains("nothing builds"),
            "body: {}",
            refused.body
        );
        // The report says the work stopped and what is still open.
        assert!(refused.body.contains("STOPPED"), "body: {}", refused.body);
    }

    /// `/verify off`: nobody is reviewing, so there is no verdict to wait for.
    /// The command reports the moment its work is done — still exactly one push,
    /// and it is the plain completion summary, not a verdict.
    #[tokio::test]
    async fn verify_off_reports_once_when_the_work_is_done() {
        let srv = ntfy().await;
        let (e, provider) = env(
            "gate-off-once",
            vec![
                write_progress_step("- [?] task one\n"),
                content_step("coded it"),
            ],
        );
        e.dir.write(&progress_file(), ONE_TASK);
        let team = Arc::new(crate::team::Team::solo(e.agent.clone()));
        team.set_verify(false);
        team.set_ntfy_url(&srv.url);
        let c = crate::coding::Coding::new(team.clone(), e.ui.clone(), &e.dir.str(), &srv.url);
        let cancel = CancellationToken::new();
        team.review_after(
            &cancel,
            crate::team::AgentName::Joey,
            "/coding on .context/progress.md",
            c.run_one_task(&cancel),
        )
        .await
        .unwrap();
        // No review conversation at all.
        assert_eq!(provider.call_count(), 2, "joey's two calls, no joy");
        let notes = srv.notifications();
        assert_eq!(
            notes.len(),
            1,
            "one push, at the end of the work: {notes:?}"
        );
        assert!(
            notes[0].0.contains("/coding complete"),
            "title: {}",
            notes[0].0
        );
        assert!(
            !notes[0].1.contains("VERDICT"),
            "no verdict to report: {}",
            notes[0].1
        );
    }

    #[tokio::test]
    async fn all_done_notification_reports_na_not_stale_usage() {
        let srv = ntfy().await;
        let (e, _p) = env("plan-stale", Vec::new());
        e.dir.write(&progress_file(), DONE_TASK);
        // Leftover figures from an unrelated earlier turn must not leak in.
        *e.agent.last_usage.lock().unwrap() = crate::agent::UsageTotals {
            usage: usage(),
            turn_elapsed: std::time::Duration::from_secs(1),
        };
        e.coding(&srv.url)
            .run_one_task(&CancellationToken::new())
            .await
            .unwrap();
        let body = &srv.notifications()[0].1;
        assert!(
            body.contains("token_in=n/a"),
            "expected n/a stats, got: {body}"
        );
    }

    #[tokio::test]
    async fn plan_does_not_notify_when_already_up_to_date() {
        let srv = ntfy().await;
        let (e, _p) = env("plan-nonotify", Vec::new());
        e.dir.write(REQUIREMENTS_FILE, "build a thing");
        e.dir.write(&progress_file(), DONE_TASK);
        let hash = file_hash(e.dir.read(REQUIREMENTS_FILE).as_bytes());
        e.dir.write(&hash_file(), &hash);
        e.coding(&srv.url)
            .plan(&CancellationToken::new(), true)
            .await
            .unwrap();
        assert_eq!(
            srv.request_count(),
            0,
            "nothing happened, so nothing to report"
        );
    }
}
