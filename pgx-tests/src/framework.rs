use std::process::{Command, Stdio};

use lazy_static::*;
use std::sync::{Arc, Mutex};

use colored::*;
use pgx::*;
use postgres::error::DbError;
use postgres::Client;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

type LogLines = Arc<Mutex<HashMap<String, Vec<String>>>>;

struct SetupState {
    installed: bool,
    loglines: LogLines,
    system_session_id: String,
}

lazy_static! {
    static ref TEST_MUTEX: Mutex<SetupState> = Mutex::new(SetupState {
        installed: false,
        loglines: Arc::new(Mutex::new(HashMap::new())),
        system_session_id: "NONE".to_string(),
    });
    static ref SHUTDOWN_HOOKS: Mutex<Vec<Box<dyn Fn() + Send>>> = Mutex::new(Vec::new());
}

fn register_shutdown_hook() {
    extern "C" fn run_shutdown_hooks() {
        for func in SHUTDOWN_HOOKS.lock().unwrap().iter() {
            func();
        }
    }
    shutdown_hooks::add_shutdown_hook(run_shutdown_hooks);
}

fn add_shutdown_hook<F: Fn()>(func: F)
where
    F: Send + 'static,
{
    SHUTDOWN_HOOKS.lock().unwrap().push(Box::new(func));
}

#[macro_export]
macro_rules! testmsg {
    ($($arg:tt)*) => (
        eprintln!("{}", format!("TMSG: {}:{}:{}:  {}", file!(), line!(), column!(), format!($($arg)*)));
    )
}

pub fn run_test<F: FnOnce(pg_sys::FunctionCallInfo) -> pg_sys::Datum>(
    _test_func: F,
    expected_error: Option<&str>,
) {
    let (loglines, system_session_id) = initialize_test_framework();

    let funcname = get_named_capture(
        &regex::Regex::new(".*::(?P<funcname>.*?)_wrapper").unwrap(),
        "funcname",
        std::any::type_name::<F>(),
    )
    .unwrap_or_else(|| {
        panic!(
            "couldn't extract function name from {}",
            std::any::type_name::<F>()
        )
    });
    let (mut client, session_id) = client();

    let schema = get_extension_schema();
    let result = match client.transaction() {
        // run the test function in a transaction
        Ok(mut tx) => {
            let result = tx.simple_query(&format!("SELECT {}.{}();", schema, funcname));

            if result.is_ok() {
                // and abort the transaction when complete
                tx.rollback().expect("test rollback didn't work");
            }

            result
        }

        Err(e) => panic!(e),
    };

    if let Err(e) = result {
        let error_as_string = format!("{}", e);

        let cause = e.into_source();
        if let Some(e) = cause {
            if let Some(dberror) = e.downcast_ref::<DbError>() {
                // we got an ERROR
                let received_error_message: &str = dberror.message();

                if let Some(expected_error_message) = expected_error {
                    // and we expected an error, so assert what we got is what we expect
                    assert_eq!(received_error_message, expected_error_message)
                } else {
                    // we weren't expecting an error
                    //
                    // wait a second for Postgres to get log messages written to stdoerr
                    std::thread::sleep(std::time::Duration::from_millis(1000));

                    let mut pg_location = String::new();
                    pg_location.push_str("Postgres location: ");
                    if dberror.file().is_some() {
                        pg_location.push_str(&dberror.file().unwrap());

                        if dberror.line().is_some() {
                            pg_location.push(':');
                            pg_location.push_str(&dberror.line().unwrap().to_string());
                        }
                    } else {
                        pg_location.push_str("<unknown>");
                    }

                    let mut rust_location = String::new();
                    rust_location.push_str("Rust location: ");
                    if dberror.where_().is_some() {
                        rust_location.push_str(&dberror.where_().unwrap());
                    } else {
                        rust_location.push_str("<unknown>");
                    }

                    // then we can panic with those messages plus those that belong to the system
                    panic!(
                        "\n{}...\n{}\n{}\n{}\n{}\n\n",
                        format_loglines(&system_session_id, &loglines),
                        format_loglines(&session_id, &loglines),
                        received_error_message.bold().red(),
                        pg_location.dimmed().white(),
                        rust_location.yellow()
                    );
                }
            } else {
                panic!(e)
            }
        } else {
            panic!(format!("{}", error_as_string.bold().red()))
        }
    } else if let Some(expected_error_message) = expected_error {
        // we expected an ERROR, but didn't get one
        panic!("Expected error: {}", expected_error_message);
    }
}

fn format_loglines(session_id: &str, loglines: &LogLines) -> String {
    let mut result = String::new();

    for line in loglines
        .lock()
        .unwrap()
        .entry(session_id.to_string())
        .or_default()
        .iter()
    {
        result.push_str(line);
        result.push('\n');
    }

    result
}

fn get_named_capture(regex: &regex::Regex, name: &'static str, against: &str) -> Option<String> {
    match regex.captures(against) {
        Some(cap) => Some(cap[name].to_string()),
        None => None,
    }
}

#[inline]
fn initialize_test_framework() -> (LogLines, String) {
    let mut state = TEST_MUTEX.lock().expect("lock was poisoned");

    if !state.installed {
        register_shutdown_hook();

        install_extension();
        initdb();

        let system_session_id = start_pg(state.loglines.clone());
        dropdb();
        createdb();
        create_extension();

        state.installed = true;
        state.system_session_id = system_session_id;
    }

    (state.loglines.clone(), state.system_session_id.clone())
}

pub fn client() -> (postgres::Client, String) {
    fn determine_session_id(client: &mut Client) -> String {
        let result = client.query("SELECT to_hex(trunc(EXTRACT(EPOCH FROM backend_start))::integer) || '.' || to_hex(pid) AS sid FROM pg_stat_activity WHERE pid = pg_backend_pid();", &[]).expect("failed to determine session id");

        match result.get(0) {
            Some(row) => row.get::<&str, &str>("sid").to_string(),
            None => panic!("No session id returned from query"),
        }
    }

    let mut client = postgres::Config::new()
        .host(&get_pg_host())
        .port(get_pg_port())
        .user(&get_pg_user())
        .dbname(&get_pg_dbname())
        .connect(postgres::NoTls)
        .unwrap();

    let session_id = determine_session_id(&mut client);
    client
        .simple_query("SET log_min_messages TO 'INFO';")
        .expect("FAILED: SET log_min_messages TO 'INFO'");

    client
        .simple_query("SET log_min_duration_statement TO 1000;")
        .expect("FAILED: SET log_min_duration_statement TO 1000");

    client
        .simple_query("SET log_statement TO 'all';")
        .expect("FAILED: SET log_statement TO 'all'");

    (client, session_id)
}

fn install_extension() {
    let mut command = Command::new("cargo-pgx")
        .arg("pgx")
        .arg("install")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("PATH", get_pgbin_envpath())
        .env("PGX_TEST_MODE", "1")
        .env("PGX_MANIFEST_DIR", std::env::var("PWD").unwrap())
        .env(
            "PGX_BUILD_FLAGS",
            format!(
                "--features pgx/pg{} --no-default-features",
                pg_sys::get_pg_major_version_string().to_string()
            ),
        )
        .spawn()
        .unwrap();

    let status = command.wait().unwrap();
    if !status.success() {
        panic!("failed to install extension");
    }
}

fn initdb() {
    let pgdata = get_pgdata_path();

    if !pgdata.is_dir() {
        let status = Command::new("initdb")
            .arg("-D")
            .arg(pgdata.to_str().unwrap())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .env("PATH", get_pgbin_envpath())
            .status()
            .unwrap();

        if !status.success() {
            panic!("initdb failed");
        }
    }

    modify_postgresql_conf(pgdata);
}

fn modify_postgresql_conf(pgdata: PathBuf) {
    let mut postgresql_conf = std::fs::OpenOptions::new()
        .append(true)
        .open(format!("{}/postgresql.conf", pgdata.display()))
        .expect("couldn't open postgresql.conf");
    postgresql_conf
        .write_all("log_line_prefix='[%m] [%p] [%c]: '\n".as_bytes())
        .expect("couldn't append log_line_prefix");
}

fn start_pg(loglines: LogLines) -> String {
    let mut command = Command::new("postmaster");
    command
        .arg("-D")
        .arg(get_pgdata_path().to_str().unwrap())
        .arg("-h")
        .arg(get_pg_host())
        .arg("-p")
        .arg(get_pg_port().to_string())
        .arg("-i")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .env("PATH", get_pgbin_envpath());

    let cmd_string = format!("{:?}", command);

    // start Postgres and monitor its stderr in the background
    // also notify the main thread when it's ready to accept connections
    let (pgpid, session_id) = monitor_pg(command, cmd_string, loglines);

    // add a shutdown hook so we can terminate it when the test framework exits
    add_shutdown_hook(move || unsafe {
        let message_string =
            std::ffi::CString::new("Stopping Postgres\n\n".bold().blue().to_string()).unwrap();
        libc::printf(message_string.as_ptr());
        libc::kill(pgpid as libc::pid_t, libc::SIGTERM);
    });

    session_id
}

fn monitor_pg(mut command: Command, cmd_string: String, loglines: LogLines) -> (u32, String) {
    let (sender, receiver) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let mut child = command.spawn().expect("postmaster didn't spawn");

        let pid = child.id();

        eprintln!(
            "{}, pid={}",
            cmd_string.bold().blue(),
            pid.to_string().yellow()
        );
        eprintln!("{}", pg_sys::get_pg_version_string().bold().purple());

        // wait for the database to say its ready to start up
        let reader = BufReader::new(
            child
                .stderr
                .take()
                .expect("couldn't take postmaster stdout"),
        );

        let regex = regex::Regex::new(r#"\[.*?\] \[.*?\] \[(?P<session_id>.*?)\]"#).unwrap();
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let session_id = match get_named_capture(&regex, "session_id", &line) {
                        Some(sid) => sid,
                        None => "NONE".to_string(),
                    };

                    if line.contains("database system is ready to accept connections") {
                        // Postgres says it's ready to go
                        sender.send((pid, session_id.clone())).unwrap();
                    }

                    if line.contains("TMSG: ") {
                        eprintln!("{}", line.cyan());
                    }

                    //                    if line.contains("INFO: ") {
                    //                        eprintln!("{}", line.cyan());
                    //                    } else if line.contains("WARNING: ") {
                    //                        eprintln!("{}", line.bold().yellow());
                    //                    } else if line.contains("ERROR: ") {
                    //                        eprintln!("{}", line.bold().red());
                    //                    } else if line.contains("statement: ") || line.contains("duration: ") {
                    //                        eprintln!("{}", line.bold().blue());
                    //                    } else if line.contains("LOG: ") {
                    //                        eprintln!("{}", line.dimmed().white());
                    //                    } else {
                    //                        eprintln!("{}", line.bold().purple());
                    //                    }

                    let mut loglines = loglines.lock().unwrap();
                    let session_lines = loglines.entry(session_id).or_insert_with(Vec::new);
                    session_lines.push(line);
                }
                Err(e) => panic!(e),
            }
        }

        // wait for Postgres to really finish
        match child.try_wait() {
            Ok(status) => {
                if let Some(_status) = status {
                    // we exited normally
                }
            }
            Err(e) => panic!(e),
        }
    });

    // wait for Postgres to indicate it's ready to accept connection
    // and return its pid when it is
    receiver.recv().unwrap()
}

fn dropdb() {
    let output = Command::new("dropdb")
        .arg("--if-exists")
        .arg("-h")
        .arg(get_pg_host())
        .arg("-p")
        .arg(get_pg_port().to_string())
        .arg(get_pg_dbname())
        .env("PATH", get_pgbin_envpath())
        .output()
        .unwrap();

    if !output.status.success() {
        // maybe the database didn't exist, and if so that's okay
        let stderr = String::from_utf8_lossy(output.stderr.as_slice());
        if !stderr.contains(&format!(
            "ERROR:  database \"{}\" does not exist",
            get_pg_dbname()
        )) {
            // got some error we didn't expect
            eprintln!("{}", String::from_utf8_lossy(output.stdout.as_slice()));
            eprintln!("{}", stderr);
            panic!("failed to drop test database");
        }
    }
}

fn createdb() {
    let status = Command::new("createdb")
        .arg("-h")
        .arg(get_pg_host())
        .arg("-p")
        .arg(get_pg_port().to_string())
        .arg(get_pg_dbname())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("PATH", get_pgbin_envpath())
        .status()
        .unwrap();

    if !status.success() {
        panic!("failed to create testing database");
    }
}

fn create_extension() {
    let (mut client, _) = client();

    client
        .simple_query(&format!("CREATE EXTENSION {};", get_extension_name()))
        .unwrap();
}

fn get_extension_name() -> String {
    std::env::var("CARGO_PKG_NAME")
        .unwrap_or_else(|_| panic!("CARGO_PKG_NAME is not an envvar"))
        .replace("-", "_")
}

fn get_pg_path() -> String {
    format!(
        "/tmp/pgx-build/REL_{}_STABLE/install/",
        pg_sys::get_pg_major_version_string(),
    )
}

fn get_pgbin_envpath() -> String {
    format!("{}/bin:{}", get_pg_path(), std::env::var("PATH").unwrap())
}

fn get_pgdata_path() -> PathBuf {
    PathBuf::from(format!("{}/data", get_pg_path()))
}

fn get_pg_host() -> String {
    "localhost".to_string()
}

fn get_pg_port() -> u16 {
    8800 + pg_sys::get_pg_major_version_num()
}

fn get_pg_dbname() -> String {
    "pgx_tests".to_string()
}

fn get_pg_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| panic!("USER is not an envvar"))
}

fn get_extension_schema() -> String {
    let output = Command::new("cargo")
        .arg("pgx")
        .arg("get")
        .arg("schema")
        .output();

    match output {
        Ok(output) => match String::from_utf8(output.stdout).unwrap().trim() {
            "" => "public".to_string(),
            value => value.to_string(),
        },
        Err(e) => panic!("failed to get extension schema property: {:?}", e),
    }
}