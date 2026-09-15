//! The `flyco` binary: argument parsing, credential resolution, dispatch.
//!
//! Everything past this file lives in the library — `main` is the thin
//! edge that turns a process invocation into a [`flyco_cli::Exit`].

use std::process::ExitCode;

use clap::Parser as _;
use flyco_cli::cli::{AuthCommand, Cli, Command, SessionCommand};
use flyco_cli::client::Api;
use flyco_cli::out::Mode;
use flyco_cli::{Exit, Failure, Outcome, auth, creds, discover, handoff, human, run, session};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    match runtime.block_on(dispatch(cli)) {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(failure) => {
            // The failure's text is the command's output — the server's
            // problem document verbatim where there is one — so it is
            // written to stderr itself, not logged.
            let _ = flyco_cli::out::raw_err(format!("{}\n", failure.text).as_bytes());
            ExitCode::from(failure.code.code())
        }
    }
}

/// One parsed invocation: resolve the credential, build the client, run
/// the command.
async fn dispatch(cli: Cli) -> Outcome<Exit> {
    let mode = Mode::resolve(cli.json);
    let base = std::env::var(flyco_cli::API_URL_ENV)
        .map_or_else(
            |_| flyco_cli::DEFAULT_API_URL.parse(),
            |value| value.parse(),
        )
        .map_err(|error| {
            Failure::usage(format!("{} is not a URL: {error}", flyco_cli::API_URL_ENV))
        })?;
    // `login` alone runs unauthenticated — asking it for a credential would
    // be circular. Everything else resolves one, and its absence is the
    // auth error the contract reserves 3 for.
    let token = creds::resolve().map(|credentials| credentials.key);
    if token.is_none() && !matches!(cli.command, Some(Command::Login { .. })) {
        return Err(Failure::problem(
            Exit::Auth,
            "not signed in — `flyco login`, `--token`, or FLYCO_TOKEN",
        ));
    }
    let api = Api::new(base, token);

    match cli.command {
        None => human::pick_harness(&api, mode).await,
        Some(Command::Login { token }) => auth::login(&api, token, mode).await.map(|()| Exit::Ok),
        Some(Command::Logout) => auth::logout(&api, mode).await.map(|()| Exit::Ok),
        Some(Command::Auth {
            command: AuthCommand::Status,
        }) => auth::status(&api, mode).await.map(|()| Exit::Ok),
        Some(Command::Catalog) => discover::catalog(&api, mode).await.map(|()| Exit::Ok),
        Some(Command::Repos) => discover::repos(&api, mode).await.map(|()| Exit::Ok),
        Some(Command::Branches { repo }) => discover::branches(&api, &repo, mode)
            .await
            .map(|()| Exit::Ok),
        Some(Command::Harnesses) => discover::harnesses(&api, mode).await.map(|()| Exit::Ok),
        Some(Command::Session { command }) => session_command(&api, command, mode).await,
        Some(Command::Run {
            harness,
            spec,
            detach,
            stop,
            archive,
            timeout,
        }) => run::run(&api, harness, &spec, detach, stop, archive, timeout).await,
        Some(Command::Claude { repo, spec }) => {
            human::launch(&api, flyco_core::HarnessKind::ClaudeCode, repo, &spec, mode).await
        }
        Some(Command::Codex { repo, spec }) => {
            human::launch(&api, flyco_core::HarnessKind::Codex, repo, &spec, mode).await
        }
        Some(Command::Devin { repo, spec }) => {
            human::launch(&api, flyco_core::HarnessKind::Devin, repo, &spec, mode).await
        }
        Some(Command::Handoff {
            from,
            session,
            harness,
            message,
            no_summary,
            include_untracked,
            spec,
        }) => {
            handoff::handoff(
                &api,
                handoff::Args {
                    from,
                    session,
                    harness,
                    message,
                    no_summary,
                    include_untracked,
                    spec,
                },
                mode,
            )
            .await
        }
        Some(Command::Resume { id, last }) => human::resume(&api, id, last, mode).await,
    }
}

/// The `session …` subtree.
async fn session_command(api: &Api, command: SessionCommand, mode: Mode) -> Outcome<Exit> {
    let done = |result: Outcome<()>| result.map(|()| Exit::Ok);
    match command {
        SessionCommand::List { state } => done(session::list(api, state, mode).await),
        SessionCommand::Get { id } => done(session::get(api, &id, mode).await),
        SessionCommand::Create { harness, spec } => {
            done(session::create(api, harness, &spec, mode).await.map(|_| ()))
        }
        SessionCommand::Send {
            id,
            text,
            file,
            stdin_flag,
        } => done(session::send(api, &id, text, file, stdin_flag).await),
        SessionCommand::Exec { id, command } => session::exec(api, &id, &command).await,
        SessionCommand::Events { id, after, follow } => {
            done(session::events(api, &id, after.unwrap_or(0), follow).await)
        }
        SessionCommand::Wait {
            id,
            conditions,
            timeout,
        } => session::wait(api, &id, &conditions, timeout, mode).await,
        SessionCommand::Set {
            id,
            title,
            budget,
            model,
            effort,
            permission_mode,
        } => done('set: {
            let model = match (model, effort) {
                (Some(model), effort) => Some(flyco_core::ModelChoice { model, effort }),
                (None, Some(_)) => {
                    break 'set Err(Failure::usage("`--effort` needs `--model`"));
                }
                (None, None) => None,
            };
            session::set(
                api,
                &id,
                flyco_core::UpdateSession {
                    title,
                    budget_limit: budget,
                    model,
                    permission_mode,
                },
                mode,
            )
            .await
        }),
        SessionCommand::Stop { id } => done(session::stop(api, &id).await),
        SessionCommand::Resume { id } => done(session::resume(api, &id, mode).await.map(|_| ())),
        SessionCommand::Interrupt { id } => done(session::interrupt(api, &id).await),
        SessionCommand::Archive {
            id,
            discard_uncommitted,
        } => done(session::archive(api, &id, discard_uncommitted, mode).await),
        SessionCommand::Approvals { id, pending } => {
            done(session::approvals(api, &id, pending, mode).await)
        }
        SessionCommand::Approve {
            id: _,
            approval,
            allow,
            deny,
        } => {
            if allow == deny {
                return Err(Failure::usage(
                    "`approve` needs exactly one of --allow or --deny",
                ));
            }
            done(session::approve(api, &approval, allow, mode).await)
        }
        SessionCommand::Diff { id } => done(session::diff(api, &id, mode).await),
        SessionCommand::Files { id, path } => {
            done(session::files(api, &id, path.as_deref(), mode).await)
        }
        SessionCommand::Read { id, path } => done(session::read(api, &id, &path, mode).await),
        SessionCommand::Env { id, set } => done(session::env(api, &id, &set, mode).await),
    }
}
