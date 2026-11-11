//! Core CLI tests: parsing, exit codes, help, streams and context construction.

mod common;

use clap::{CommandFactory, Parser};
use vowp_cli::cli::{Cli, Command, JobCommand, ScheduleArgs, V0_1_COMMANDS, V0_2_COMMANDS};
use vowp_cli::output::kind_name;
use vowp_cli::{BufferIo, CliError, Ctx, Global, IoStreams};
use vowp_protocol::ErrorKind;

#[test]
fn parse_global_flags_and_node() {
    let cli = Cli::try_parse_from([
        "vowp",
        "--json",
        "--node",
        "vps",
        "--project",
        "demo",
        "--home",
        "/tmp/vowp-home",
        "--no-color",
        "-q",
        "-vv",
        "job",
        "ls",
        "--all",
    ])
    .expect("parses");
    assert!(cli.global.json);
    assert_eq!(cli.global.node.as_deref(), Some("vps"));
    assert_eq!(cli.global.project.as_deref(), Some("demo"));
    assert_eq!(cli.global.home.as_deref(), Some(std::path::Path::new("/tmp/vowp-home")));
    assert!(cli.global.no_color);
    assert!(cli.global.quiet);
    assert_eq!(cli.global.verbose, 2);
    match cli.command {
        Command::Job {
            command: JobCommand::Ls(args),
        } => assert!(args.all),
        other => panic!("expected job ls, got {other:?}"),
    }

    // Global flags also work after the subcommand.
    let cli = Cli::try_parse_from(["vowp", "job", "ls", "--node", "vps"]).expect("parses");
    assert_eq!(cli.global.node.as_deref(), Some("vps"));
    let cli = Cli::try_parse_from(["vowp", "--node=other", "status"]).expect("parses");
    assert_eq!(cli.global.node.as_deref(), Some("other"));

    // The ssh argv strips both `--node name` and `--node=name`.
    let stripped = vowp_cli::remote::strip_node_flag(&[
        "--json".to_owned(),
        "--node".to_owned(),
        "vps".to_owned(),
        "job".to_owned(),
        "--node=second".to_owned(),
        "ls".to_owned(),
    ]);
    assert_eq!(stripped, vec!["--json", "job", "ls"]);
}

#[test]
fn exit_code_mapping_table() {
    let table = [
        (ErrorKind::InvalidInput, 2),
        (ErrorKind::NotFound, 3),
        (ErrorKind::Conflict, 4),
        (ErrorKind::Guard, 5),
        (ErrorKind::Refused, 5),
        (ErrorKind::Limit, 6),
        (ErrorKind::Auth, 6),
        (ErrorKind::Remote, 7),
        (ErrorKind::Internal, 1),
        (ErrorKind::Io, 1),
        (ErrorKind::Db, 1),
        (ErrorKind::Parse, 1),
        (ErrorKind::Config, 1),
        (ErrorKind::AdapterFailed, 1),
        (ErrorKind::AdapterUnavailable, 1),
        (ErrorKind::Timeout, 1),
    ];
    for (kind, expected) in table {
        assert_eq!(vowp_cli::errors::exit_code_for_kind(kind), expected, "{kind:?}");
        assert!(!kind_name(kind).is_empty());
    }
}

#[test]
fn exit_codes_follow_error_kind() {
    let cases: Vec<(CliError, i32)> = vec![
        (CliError::Invalid("bad".to_owned()), 2),
        (CliError::NotFound("not found: job n99".to_owned()), 3),
        (CliError::Conflict("exists".to_owned()), 4),
        (CliError::Guard("guard".to_owned()), 5),
        (CliError::Limited("limit".to_owned()), 6),
        (CliError::Remote("ssh".to_owned()), 7),
        (CliError::NotImplemented("job"), 1),
        (CliError::Io(std::io::Error::other("io")), 1),
        (CliError::Store(vowp_store::StoreError::Conflict("row".to_owned())), 4),
        (
            CliError::Protocol(vowp_protocol::ProtocolError::Config("cfg".to_owned())),
            1,
        ),
        (CliError::Core(vowp_core::CoreError::Invalid("core".to_owned())), 2),
    ];
    for (error, expected) in cases {
        assert_eq!(error.exit_code(), expected, "{error}");
    }
    assert_eq!(
        CliError::NotFound("not found: job n99".to_owned()).hint().as_deref(),
        Some("vowp job ls")
    );
}

#[test]
fn io_streams_buffer_impl_captures_output() {
    let mut io = BufferIo::new("first line\nsecond\n");
    write!(io.out(), "hello").expect("stdout");
    writeln!(io.err(), "oops").expect("stderr");
    let mut line = String::new();
    io.input().read_line(&mut line).expect("read stdin");
    assert_eq!(line, "first line\n");
    assert_eq!(io.stdout(), "hello");
    assert_eq!(io.stderr(), "oops\n");
    assert!(!io.is_tty());
    assert!(BufferIo::with_tty("", true).is_tty());
}

#[test]
fn help_lists_every_v0_1_command() {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|command| command.get_name().to_owned())
        .filter(|name| name != "help")
        .collect();
    names.sort();
    let mut expected: Vec<String> = V0_1_COMMANDS
        .iter()
        .chain(V0_2_COMMANDS.iter())
        .map(|name| (*name).to_owned())
        .collect();
    expected.sort();
    assert_eq!(names, expected);

    let help = Cli::command().render_help().to_string();
    for command in V0_1_COMMANDS
        .iter()
        .chain(V0_2_COMMANDS.iter())
        .filter(|name| !name.starts_with("__"))
    {
        assert!(help.contains(command), "help does not mention {command}:\n{help}");
    }
}

#[test]
fn parse_schedule_add_examples_from_cli_md() {
    use vowp_cli::cli::{ScheduleActionArg, ScheduleCommand};

    let cli = Cli::try_parse_from([
        "vowp",
        "schedule",
        "add",
        "0 7 * * 1-5",
        "loop",
        "l4-planner",
        "--name",
        "morning-report",
        "--tz",
        "+02:00",
        "--missed",
        "run_once",
    ])
    .expect("cli.md loop example parses");
    match cli.command {
        Command::Schedule(ScheduleArgs {
            command: ScheduleCommand::Add(args),
            ..
        }) => {
            assert_eq!(args.spec, "0 7 * * 1-5");
            match args.action {
                ScheduleActionArg::Loop(loop_) => {
                    assert_eq!(loop_.loop_name, "l4-planner");
                    assert_eq!(loop_.common.name, "morning-report");
                    assert_eq!(loop_.common.tz.as_deref(), Some("+02:00"));
                    assert_eq!(loop_.common.missed.as_deref(), Some("run_once"));
                }
                other => panic!("expected loop action, got {other:?}"),
            }
        }
        other => panic!("expected schedule add, got {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "vowp",
        "schedule",
        "add",
        "every 30m",
        "command",
        "--name",
        "hb",
        "--",
        "vowp",
        "status",
        "--check",
    ])
    .expect("cli.md command example parses");
    match cli.command {
        Command::Schedule(ScheduleArgs {
            command: ScheduleCommand::Add(args),
            ..
        }) => match args.action {
            ScheduleActionArg::Command(command) => {
                assert_eq!(command.common.name, "hb");
                assert_eq!(command.argv, vec!["vowp", "status", "--check"])
            }
            other => panic!("expected command action, got {other:?}"),
        },
        other => panic!("expected schedule add, got {other:?}"),
    }

    let cli = Cli::try_parse_from([
        "vowp",
        "schedule",
        "add",
        "0 1 * * *",
        "job-template",
        "--name",
        "dep-update",
        "--template-file",
        "deps.json",
    ])
    .expect("cli.md job-template example parses");
    match cli.command {
        Command::Schedule(ScheduleArgs {
            command: ScheduleCommand::Add(args),
            ..
        }) => match args.action {
            ScheduleActionArg::JobTemplate(template) => {
                assert_eq!(template.common.name, "dep-update");
                assert_eq!(template.template_file, std::path::Path::new("deps.json"))
            }
            other => panic!("expected job-template action, got {other:?}"),
        },
        other => panic!("expected schedule add, got {other:?}"),
    }
}

#[test]
fn json_error_shape_not_found() {
    let env = common::TestEnv::new();
    let (code, stdout, stderr) = env.run(&["--json", "job", "show", "n99"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON on stdout");
    assert_eq!(value["error"]["kind"], "not_found");
    assert!(
        value["error"]["message"].as_str().unwrap_or_default().contains("n99"),
        "{value}"
    );
    assert_eq!(value["error"]["hint"], "vowp job ls");
}

#[test]
fn ctx_merges_node_profile_defaults() {
    let env = common::TestEnv::new();
    // The project omits [profiles]; the node config supplies them.
    std::fs::write(
        env.project_path().join("vowp.toml"),
        "[node]\nname = \"local\"\n\n[project]\nname = \"test\"\n",
    )
    .expect("minimal project config");
    let node_file = vowp_protocol::NodeFile {
        profiles: env.config.profiles.clone(),
        ..vowp_protocol::NodeFile::default()
    };
    std::fs::write(
        env.home_path().join("config.toml"),
        toml::to_string_pretty(&node_file).expect("node config"),
    )
    .expect("write node config");

    let io = BufferIo::new("");
    let ctx = Ctx::build(&env.global(false), &io).expect("node defaults fill the profiles");
    assert_eq!(ctx.config().profiles.len(), 4);
    assert!(ctx.config().profiles.contains_key("worker"));

    // Without the node defaults the same project must fail validation.
    std::fs::remove_file(env.home_path().join("config.toml")).expect("remove node config");
    match Ctx::build(&env.global(false), &io) {
        Err(CliError::Protocol(vowp_protocol::ProtocolError::Config(message))) => {
            assert!(message.contains("missing required profile"), "{message}");
        }
        Err(other) => panic!("expected config error, got {other}"),
        Ok(_) => panic!("missing profiles must fail validation"),
    }
}

#[test]
fn ctx_build_requires_init() {
    let env = common::TestEnv::new();
    // A second, uninitialised home next to the project.
    let uninitialised = tempfile::tempdir().expect("temp home");
    let global = Global {
        home: Some(uninitialised.path().to_path_buf()),
        project: Some(env.project_path().display().to_string()),
        ..Global::default()
    };
    let io = BufferIo::new("");
    match Ctx::build(&global, &io) {
        Err(CliError::NotFound(message)) => assert!(message.contains("vowp init"), "{message}"),
        Err(other) => panic!("expected not found, got {other}"),
        Ok(_) => panic!("Ctx::build must fail before init"),
    }
}

#[test]
fn chat_flags_are_no_longer_frozen() {
    let env = common::TestEnv::new();
    let cases: &[&[&str]] = &[
        &["chat", "nope00", "--node", "laptop"],
        &["chat", "nope00", "--follow"],
        &["chat", "nope00", "--takeover"],
        &["session", "chat", "nope00", "--follow"],
    ];
    for args in cases {
        let (code, _stdout, stderr) = env.run(args);
        assert_eq!(code, 3, "{args:?}: {stderr}");
        assert!(stderr.contains("session nope00"), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("not implemented in this build"),
            "{args:?} was still refused as not implemented: {stderr}"
        );
    }

    // The legacy single-file form still works.
    let legacy = env.home_path().join("legacy.db");
    let legacy = legacy.to_str().expect("legacy path");
    let (code, _stdout, stderr) = env.run(&["backup", "--to", legacy]);
    assert_eq!(code, 0, "legacy .db backup failed: {stderr}");
    assert!(
        std::path::Path::new(legacy).is_file(),
        "legacy .db backup was not written"
    );
}
