use ct_shim::{read_response, write_json, Response, ShimVerdict, HOLD_WAIT, REPORT_ACK_WAIT};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::process::{self, Command};

const DAEMON_UNREACHABLE: &str =
    "Tractus daemon unreachable; command not executed. Start chaosd or unset SHELL.";
const USAGE: &str = "usage: ct-shim -c <command>";
const PROMPT: &str = "tractus ▸ ";

fn main() {
    process::exit(run_from_args(env::args().collect()));
}

fn run_from_args(args: Vec<String>) -> i32 {
    if args.len() == 1 {
        return run_repl();
    }

    let Some(command) = shell_command(&args) else {
        println!("{USAGE}");
        return 1;
    };

    run_command(&command)
}

fn run_repl() -> i32 {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let mut input = stdin.lock();
    let mut stdout = io::stdout().lock();

    loop {
        if interactive {
            let _ = write!(stdout, "{PROMPT}");
            let _ = stdout.flush();
        }

        let mut line = String::new();
        let Ok(bytes_read) = input.read_line(&mut line) else {
            return 1;
        };
        if bytes_read == 0 {
            return 0;
        }

        let command = line.trim();
        if command.is_empty() {
            continue;
        }
        if matches!(command, "exit" | "quit") {
            return 0;
        }

        let _ = run_command(command);
    }
}

fn run_command(command: &str) -> i32 {
    match request_verdict(&command) {
        Ok(ShimVerdict::Allow { mut connection, id }) => {
            execute_and_report(&command, &id, &mut connection)
        }
        Ok(ShimVerdict::Block(message)) => {
            println!("{message}");
            1
        }
        Ok(ShimVerdict::Hold {
            mut connection, id, ..
        }) => match read_response(&mut connection, HOLD_WAIT) {
            Ok(Response::Allow) => execute_and_report(&command, &id, &mut connection),
            Ok(Response::Block(message)) => {
                println!("{message}");
                1
            }
            _ => {
                println!("{DAEMON_UNREACHABLE}");
                1
            }
        },
        Err(()) => {
            println!("{DAEMON_UNREACHABLE}");
            1
        }
    }
}

fn shell_command(args: &[String]) -> Option<String> {
    (args.len() == 3 && args[1] == "-c").then(|| args[2].clone())
}

fn request_verdict(command: &str) -> Result<ShimVerdict, ()> {
    let cwd = env::current_dir().map_err(|_| ())?;
    let environment = env::vars().collect::<HashMap<_, _>>();
    ct_shim::request_verdict(command, &cwd, "ct-shim", environment)
}

fn execute_and_report(command: &str, id: &str, connection: &mut UnixStream) -> i32 {
    let status = match Command::new("/bin/sh").arg("-c").arg(command).status() {
        Ok(status) => status,
        Err(_) => return 1,
    };
    let exit_code = status.code().unwrap_or(1);
    let report = json!({
        "type": "report",
        "id": id,
        "exit_code": exit_code,
    });
    // Report failures cannot authorize a fallback execution: the approved command
    // has already completed, so preserve its real exit status.
    let _ =
        write_json(connection, &report).and_then(|_| read_response(connection, REPORT_ACK_WAIT));
    exit_code
}
