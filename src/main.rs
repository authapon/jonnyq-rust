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

/// The one-line session description the web console shows: model, context
/// usage and window. Built from the agent rather than a snapshot, so the page
/// shows what the session is using *now* - after `/model`, `/context` or a
/// compaction - not what it started with.
fn status_line(agent: Arc<Agent>) -> jonnyq::web::StatusFn {
    Arc::new(move || {
        let cs = agent.ui.state.read().unwrap().context_status();
        let model = if agent.model().is_empty() {
            "(no model)".to_string()
        } else {
            agent.model()
        };
        format!("{model} | {cs} | {}", agent.tool_call_mode().as_str())
    })
}

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

    // Install the proxy before anything talks HTTP: `-socks-proxy` was
    // validated during config loading, and the value is what every client
    // built from here on (provider, web tools, ntfy) is constructed from.
    if let Err(e) = jonnyq::net::set_current(cfg.proxy_config_or_default()) {
        eprintln!("config error: {e}");
        std::process::exit(1);
    }

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
    agent.configure(&cfg);
    let agent = Arc::new(agent);

    let r = Repl::new(cfg, agent.clone(), w.clone(), run_command);

    // The web console (-web <port>): bind before the REPL starts so a port
    // already in use is reported now, not after a session has begun. It mirrors
    // the terminal into a browser page at http://127.0.0.1:<port> and feeds
    // prompts typed there back through the REPL's own loop - see crate::web.
    if r.cfg.lock().await.web_port > 0 {
        let port = r.cfg.lock().await.web_port as u16;
        let status = status_line(agent.clone());
        match jonnyq::web::WebConsole::bind(port, &work_dir, w.clone(), status) {
            Ok(web) => {
                if let Err(e) = web.start() {
                    eprintln!("web console: {e}");
                } else {
                    r.set_web(web.clone()).await;
                    w.plainln(&format!("[web] console at {}", web.url()));
                }
            }
            Err(e) => eprintln!("web console: {e}"),
        }
    }

    let err = r.run().await.err();
    w.close();
    if let Some(e) = err {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
