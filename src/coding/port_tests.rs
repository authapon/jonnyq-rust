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
        let p = build_task_prompt("Create the file `index.html`.", 0, false);
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
            "1/2 tasks done",
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
