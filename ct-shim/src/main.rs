use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{self, Command};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DAEMON_UNREACHABLE: &str =
    "Chaos Twin daemon unreachable; command not executed. Start chaosd or unset SHELL.";
const USAGE: &str = "usage: ct-shim -c <command>";
const HOLD_WAIT: Duration = Duration::from_secs(65);
const REPORT_ACK_WAIT: Duration = Duration::from_secs(2);

fn main() {
    process::exit(run_from_args(env::args().collect()));
}

fn run_from_args(args: Vec<String>) -> i32 {
    let Some(command) = shell_command(&args) else {
        println!("{USAGE}");
        return 1;
    };

    match request_verdict(&command) {
        Ok(ShimVerdict::Allow { mut connection, id }) => {
            execute_and_report(&command, &id, &mut connection)
        }
        Ok(ShimVerdict::Block(message)) => {
            println!("{message}");
            1
        }
        Ok(ShimVerdict::Hold { mut connection, id }) => match read_response(&mut connection, HOLD_WAIT) {
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

enum ShimVerdict {
    Allow { connection: UnixStream, id: String },
    Block(String),
    Hold { connection: UnixStream, id: String },
}

enum Response {
    Allow,
    Block(String),
    Hold,
}

fn request_verdict(command: &str) -> Result<ShimVerdict, ()> {
    let socket_path = socket_path();
    let mut connection = UnixStream::connect(socket_path).map_err(|_| ())?;
    connection.set_read_timeout(Some(HOLD_WAIT)).map_err(|_| ())?;
    connection.set_write_timeout(Some(Duration::from_secs(5))).map_err(|_| ())?;

    let cwd = env::current_dir().map_err(|_| ())?;
    let env = env::vars().collect::<HashMap<_, _>>();
    let id = command_id();
    let proposal = json!({
        "type": "propose",
        "id": id,
        "cmd": command,
        "cwd": cwd,
        "env": env,
        "agent_session": "ct-shim",
    });
    write_json(&mut connection, &proposal)?;

    match read_response(&mut connection, HOLD_WAIT)? {
        Response::Allow => Ok(ShimVerdict::Allow { connection, id }),
        Response::Block(message) => Ok(ShimVerdict::Block(message)),
        Response::Hold => Ok(ShimVerdict::Hold { connection, id }),
    }
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
    let _ = write_json(connection, &report).and_then(|_| read_response(connection, REPORT_ACK_WAIT));
    exit_code
}

fn read_response(connection: &mut UnixStream, timeout: Duration) -> Result<Response, ()> {
    connection.set_read_timeout(Some(timeout)).map_err(|_| ())?;
    let mut line = String::new();
    BufReader::new(connection.try_clone().map_err(|_| ())?)
        .read_line(&mut line)
        .map_err(|_| ())?;
    if line.is_empty() {
        return Err(());
    }
    let value: Value = serde_json::from_str(&line).map_err(|_| ())?;
    match value.get("action").and_then(Value::as_str) {
        Some("allow") => Ok(Response::Allow),
        Some("hold") => Ok(Response::Hold),
        Some("block") => value
            .get("synthetic_stdout")
            .and_then(Value::as_str)
            .map(|message| Response::Block(message.to_owned()))
            .ok_or(()),
        _ => Err(()),
    }
}

fn write_json(connection: &mut UnixStream, value: &Value) -> Result<(), ()> {
    let encoded = serde_json::to_string(value).map_err(|_| ())?;
    connection.write_all(encoded.as_bytes()).map_err(|_| ())?;
    connection.write_all(b"\n").map_err(|_| ())?;
    connection.flush().map_err(|_| ())
}

fn socket_path() -> PathBuf {
    env::var_os("CHAOSTWIN_SOCK")
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path)
}

fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("chaostwin.sock");
    }
    let uid = env::var("UID").unwrap_or_else(|_| "0".to_owned());
    PathBuf::from(format!("/tmp/chaostwin-{uid}.sock"))
}

fn command_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("shim-{}-{timestamp}", process::id())
}
