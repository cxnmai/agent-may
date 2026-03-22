mod auth;
mod openai;

use std::io::{self, Write};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use crate::openai::{ChatClient, ChatTurn};

#[derive(Debug, Parser)]
#[command(name = "agent-may", version, about = "A minimal terminal agent with Codex-style login")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Login,
    Chat(ChatArgs),
    Logout,
    Status,
}

#[derive(Debug, Args)]
struct ChatArgs {
    #[arg(long, default_value = "gpt-5")]
    model: String,
    #[arg(long, default_value = "You are a concise terminal-based AI assistant.")]
    system_prompt: String,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Chat(ChatArgs {
        model: "gpt-5".to_string(),
        system_prompt: "You are a concise terminal-based AI assistant.".to_string(),
    })) {
        Command::Login => login(),
        Command::Chat(args) => chat(args),
        Command::Logout => logout(),
        Command::Status => status(),
    }
}

fn login() -> Result<()> {
    let summary = auth::login_with_chatgpt()?;
    println!();
    println!("Login succeeded.");
    if let Some(email) = summary.email {
        println!("Account: {email}");
    }
    if let Some(plan) = summary.plan_type {
        println!("Plan: {plan}");
    }
    println!("Saved auth to {}", summary.auth_path.display());
    Ok(())
}

fn chat(args: ChatArgs) -> Result<()> {
    let session = auth::load_auth_session()
        .context("no stored Codex-style login found; run `agent-may login` first")?;
    let profile = auth::user_profile(&session.auth)?;
    let client = ChatClient::new(args.model.clone(), args.system_prompt.clone())?;

    println!("Model: {}", args.model);
    if let Some(email) = profile.email {
        println!("Signed in as: {email}");
    }
    if let Some(plan) = profile.plan_type {
        println!("Plan: {plan}");
    }
    println!("Type `/exit` to quit.\n");

    let mut turns = Vec::new();

    let stdin = io::stdin();
    loop {
        print!("you> ");
        io::stdout().flush().context("failed to flush stdout")?;

        let mut input = String::new();
        stdin
            .read_line(&mut input)
            .context("failed to read a line from stdin")?;
        let input = input.trim();

        if input.is_empty() {
            continue;
        }
        if matches!(input, "/exit" | "/quit") {
            break;
        }

        turns.push(ChatTurn {
            role: "user".to_string(),
            content: input.to_string(),
        });

        let answer = client.send(&turns)?;
        println!("\nassistant> {answer}\n");

        turns.push(ChatTurn {
            role: "assistant".to_string(),
            content: answer,
        });
    }

    Ok(())
}

fn logout() -> Result<()> {
    if auth::logout()? {
        println!("Removed stored auth.");
    } else {
        println!("No stored auth was present.");
    }
    Ok(())
}

fn status() -> Result<()> {
    let session = auth::load_auth_session()
        .context("no stored Codex-style login found; run `agent-may login` first")?;
    let profile = auth::user_profile(&session.auth)?;

    println!("Auth file: {}", session.auth_path.display());
    println!(
        "Mode: {}",
        session.auth.auth_mode.as_deref().unwrap_or("unknown")
    );
    println!(
        "API key present: {}",
        session.auth.openai_api_key.as_ref().is_some()
    );
    if let Some(email) = profile.email {
        println!("Account: {email}");
    }
    if let Some(plan) = profile.plan_type {
        println!("Plan: {plan}");
    }
    if let Some(account_id) = profile.account_id {
        println!("Workspace: {account_id}");
    }

    Ok(())
}
