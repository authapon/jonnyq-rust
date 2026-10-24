//! jonnyq - an interactive coding agent CLI.
//!
//! Port of Go `cmd/jonnyq/main.go`: load config, open the transcript log,
//! build the tool registry and agent, then hand control to the REPL on the
//! tokio runtime.

use std::sync::Arc;

use jonnyq::agent::Agent;
use jonnyq::config;
use jonnyq::llm;
use jonnyq::repl::Repl;
use jonnyq::tools;
use jonnyq::tools::run_command::RunCommandTool;
use jonnyq::ui::Writer;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let cfg = match config::load(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    let w = match Writer::new(&cfg.output_file) {
        Ok(w) => Arc::new(w),
        Err(e) => {
            eprintln!("cannot open output file: {e}");
            std::process::exit(1);
        }
    };

    let work_dir = match std::env::current_dir() {
        Ok(d) => d.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("cannot determine working directory: {e}");
            std::process::exit(1);
        }
    };

    let provider = match llm::new_provider(&cfg.provider_name, &cfg.provider_url, &cfg.key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("provider error: {e}");
            std::process::exit(1);
        }
    };

    let mut reg = tools::Registry::new();
    reg.register(Arc::new(tools::file::ReadFileTool { work_dir: work_dir.clone() }));
    reg.register(Arc::new(tools::file::WriteFileTool { work_dir: work_dir.clone() }));
    reg.register(Arc::new(tools::file::EditFileTool { work_dir: work_dir.clone() }));
    reg.register(Arc::new(tools::file::CreateFolderTool { work_dir: work_dir.clone() }));
    reg.register(Arc::new(tools::search::SearxngTool::new(
        &cfg.searxng_url,
        cfg.searxng_max_results,
        cfg.searxng_concurrency,
        cfg.searxng_timeout_sec,
    )));
    reg.register(Arc::new(tools::web_fetch::FetchTool::new(
        cfg.fetch_concurrency,
        cfg.fetch_timeout_sec,
    )));
    reg.register(Arc::new(tools::pdf::PdfTool::new(&work_dir, cfg.pdf_max_pages, cfg.pdf_dpi)));
    reg.register(Arc::new(tools::pic::PicTool::new(&work_dir)));
    let run_command = Arc::new(RunCommandTool::new(&work_dir, cfg.run_command_timeout_sec));
    reg.register(run_command.clone());
    reg.register(Arc::new(tools::skill_tool::SkillTool::new(cfg.skill_paths.clone())));
    // Always available, in every configuration - see Registry::install_always.
    reg.install_always(Arc::new(tools::ask_user::AskUserTool::new(w.clone(), &cfg.ntfy_url)));

    let agent = Agent::new(
        provider,
        &cfg.model,
        reg,
        cfg.thinking,
        cfg.context_size,
        cfg.max_tool_calls_per_turn,
        w.clone(),
        cfg.skill_paths.clone(),
    );
    agent.set_tool_call_mode(jonnyq::agent::ToolCallMode::from_config(&cfg.tool_call_mode));
    agent.set_output_file(&cfg.output_file);
    let agent = Arc::new(agent);

    let r = Repl::new(cfg, agent, w.clone(), run_command);
    let err = r.run().await.err();
    w.close();
    if let Some(e) = err {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
