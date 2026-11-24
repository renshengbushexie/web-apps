#[path = "../sync/events.rs"]
mod events;
#[path = "../sync/feishu.rs"]
mod feishu;
#[path = "../sync/import.rs"]
mod import;
#[path = "../sync/mapping.rs"]
mod mapping;
#[path = "../sync/state.rs"]
mod state;

use anyhow::{Context, Result, ensure};
use feishu::Feishu;
use mapping::Table;
use reqwest::blocking::Client;
use std::{env, fs, path::PathBuf, time::Duration};

fn required(name: &str) -> Result<String> {
    let value = env::var(name).with_context(|| format!("missing {name}"))?;
    ensure!(!value.trim().is_empty(), "{name} is empty");
    Ok(value)
}

fn cycle(
    feishu: &Feishu,
    tables: &[Table],
    state: &mut state::State,
    api: &state::Admin,
) -> Result<()> {
    let mut failures = 0;
    for table in tables {
        let result = (|| -> Result<()> {
            let source = feishu.source_key(table);
            state.publish(&source, api)?;
            let token = feishu.token()?;
            let rows = feishu.records(table, &token)?;
            let snapshot = mapping::snapshot(table, &feishu.base, &rows)?;
            let count = state.stage(&source, &snapshot)?;
            state.publish(&source, api)?;
            eprintln!(
                "{}: {} published entries, {} changes",
                table.name,
                snapshot.len(),
                count
            );
            Ok(())
        })();
        if let Err(err) = result {
            failures += 1;
            eprintln!("{}: {err:#}", table.name);
        }
    }
    ensure!(failures == 0, "{failures} table(s) failed");
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!(
            "kb-sync once | watch | import-xiaoduo <table-name> <file.csv|file.jsonl>\nSet FEISHU_APP_ID, FEISHU_APP_SECRET, FEISHU_BASE_TOKEN, KB_TABLES_JSON and KB_SYNC_DB. Synchronization also uses KB_API_URL, KB_ADMIN_TOKEN and optionally KB_POLL_SECONDS (default 60)."
        );
        return Ok(());
    }
    let importing = matches!(args.as_slice(), [cmd, _, _] if cmd == "import-xiaoduo");
    ensure!(
        importing || args == ["once"] || args == ["watch"],
        "invalid arguments (try --help)"
    );
    let tables: Vec<Table> =
        serde_json::from_str(&required("KB_TABLES_JSON")?).context("invalid KB_TABLES_JSON")?;
    mapping::validate_tables(&tables)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(35))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let feishu = Feishu::new(client.clone())?;
    let path =
        PathBuf::from(env::var_os("KB_SYNC_DB").unwrap_or_else(|| "data/kb-sync.sqlite3".into()));
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    let mut state = state::State::open(&path)?;
    if importing {
        lock.try_lock()
            .context("another kb-sync operation is active; retry after it completes")?;
        let table = tables
            .iter()
            .find(|t| t.name == args[1])
            .context("unknown table name")?;
        return import::run(&feishu, table, &args[2], &state);
    }
    let api = state::Admin::new(client)?;
    let seconds = env::var("KB_POLL_SECONDS")
        .unwrap_or_else(|_| "60".into())
        .parse::<u64>()?;
    ensure!(
        (1..=86400).contains(&seconds),
        "KB_POLL_SECONDS must be in 1..86400"
    );
    let events = if args == ["watch"] {
        events::start(&tables, &feishu.base)?
    } else {
        None
    };
    loop {
        let result = (|| -> Result<()> {
            lock.try_lock()
                .context("another kb-sync operation is active")?;
            let result = state
                .bind(&api.url)
                .and_then(|()| cycle(&feishu, &tables, &mut state, &api));
            lock.unlock()?;
            result
        })();
        if args == ["once"] {
            return result;
        }
        if let Err(err) = result {
            eprintln!("sync cycle: {err:#}");
        }
        events::wait(&events, Duration::from_secs(seconds));
    }
}
