use crate::{feishu::json_response, mapping::Snapshot};
use anyhow::{Context, Result, ensure};
use reqwest::{Url, blocking::Client};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

pub struct Admin {
    client: Client,
    pub url: String,
    token: String,
}
impl Admin {
    pub fn new(client: Client) -> Result<Self> {
        let root = std::env::var("KB_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
        let url = Url::parse(&root)?;
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "KB_API_URL must be a service base URL"
        );
        let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && local),
            "KB_API_URL requires HTTPS or loopback HTTP"
        );
        let url = format!("{}/v1/admin/changes", url.as_str().trim_end_matches('/'));
        Ok(Self {
            client,
            url,
            token: crate::required("KB_ADMIN_TOKEN")?,
        })
    }
    fn apply(&self, changes: &[Value]) -> Result<()> {
        let reply = json_response(
            self.client
                .post(&self.url)
                .bearer_auth(&self.token)
                .json(&json!({"changes":changes})),
        )?;
        ensure!(
            reply.get("version").and_then(Value::as_u64).is_some(),
            "server response missing version"
        );
        Ok(())
    }
}
pub struct State {
    db: Connection,
}
impl State {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(10))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS sync_destination (id INTEGER PRIMARY KEY CHECK(id=1),url TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS sync_owned (source TEXT NOT NULL,shop TEXT NOT NULL,id TEXT NOT NULL,item TEXT NOT NULL,PRIMARY KEY(shop,id));
        CREATE INDEX IF NOT EXISTS sync_owned_source ON sync_owned(source);
        CREATE TABLE IF NOT EXISTS sync_pending (source TEXT NOT NULL,shop TEXT NOT NULL,id TEXT NOT NULL,item TEXT,PRIMARY KEY(shop,id));
        CREATE INDEX IF NOT EXISTS sync_pending_source ON sync_pending(source);
        CREATE TABLE IF NOT EXISTS sync_creates (source TEXT NOT NULL,shop TEXT NOT NULL,id TEXT NOT NULL,token TEXT NOT NULL, PRIMARY KEY(source,shop,id));")?;
        Ok(Self { db })
    }
    pub fn bind(&self, url: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO sync_destination VALUES (1,?1)",
            [url],
        )?;
        let saved: String =
            self.db
                .query_row("SELECT url FROM sync_destination WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        ensure!(
            saved == url,
            "sync checkpoint belongs to a different KB_API_URL"
        );
        Ok(())
    }
    pub fn stage(&mut self, source: &str, current: &Snapshot) -> Result<usize> {
        let tx = self.db.transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM sync_pending WHERE source=?1",
            [source],
            |r| r.get(0),
        )?;
        ensure!(count == 0, "unacknowledged previous snapshot");
        let mut prior = BTreeMap::new();
        {
            let mut sel = tx.prepare("SELECT shop,id,item FROM sync_owned WHERE source=?1")?;
            for row in sel.query_map([source], |r| {
                Ok((
                    (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (key, item) = row?;
                prior.insert(key, item);
            }
        }
        let mut changes = 0;
        let mut bytes = b"{\"changes\":[]}".len();
        {
            let mut owner = tx.prepare(
                "SELECT source FROM sync_owned WHERE shop=?1 AND id=?2
              UNION SELECT source FROM sync_pending WHERE shop=?1 AND id=?2",
            )?;
            let mut add =
                tx.prepare("INSERT INTO sync_pending(source,shop,id,item) VALUES (?1,?2,?3,?4)")?;
            for ((shop, id), item) in current {
                let body = serde_json::to_string(item)?;
                let old = prior.remove(&(shop.clone(), id.clone()));
                if old.as_ref() == Some(&body) {
                    continue;
                }
                let other: Option<String> = owner
                    .query_row(params![shop, id], |r| r.get(0))
                    .optional()?;
                ensure!(
                    other.as_deref().is_none_or(|s| s == source),
                    "(shop, stable ID) already owned by another table"
                );
                bytes += serde_json::to_vec(&json!({"shop":shop,"id":id,"item":item}))?.len()
                    + usize::from(changes > 0);
                add.execute(params![source, shop, id, body])?;
                changes += 1;
            }
            for (shop, id) in prior.keys() {
                bytes += serde_json::to_vec(&json!({"shop":shop,"id":id,"item":null}))?.len()
                    + usize::from(changes > 0);
                add.execute(params![source, shop, id, None::<String>])?;
                changes += 1;
            }
        }
        ensure!(
            changes <= 50_000,
            "snapshot exceeds the admin API limit of 50,000 changes"
        );
        ensure!(
            bytes <= 32 * 1024 * 1024,
            "snapshot exceeds the admin API limit of 32 MiB"
        );
        tx.commit()?;
        Ok(changes)
    }
    fn pending(&self, source: &str) -> Result<Vec<Value>> {
        let mut sel = self
            .db
            .prepare("SELECT shop,id,item FROM sync_pending WHERE source=?1 ORDER BY shop,id")?;
        let mut changes = Vec::new();
        for entry in sel.query_map([source], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (shop, id, raw) = entry?;
            let item = raw.map(|s| serde_json::from_str::<Value>(&s)).transpose()?;
            changes.push(json!({"shop":shop,"id":id,"item":item}));
        }
        Ok(changes)
    }
    fn acknowledge(&mut self, source: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM sync_owned WHERE source=?1 AND (shop,id) IN (SELECT shop,id FROM sync_pending WHERE source=?1 AND item IS NULL)",[source])?;
        tx.execute("INSERT INTO sync_owned(source,shop,id,item) SELECT source,shop,id,item FROM sync_pending WHERE source=?1 AND item IS NOT NULL
            ON CONFLICT(shop,id) DO UPDATE SET item=excluded.item WHERE sync_owned.source=excluded.source",[source])?;
        tx.execute("DELETE FROM sync_pending WHERE source=?1", [source])?;
        tx.commit()?;
        Ok(())
    }
    pub fn publish(&mut self, source: &str, api: &Admin) -> Result<()> {
        let pending = self.pending(source)?;
        if pending.is_empty() {
            return Ok(());
        }
        api.apply(&pending)
            .context("publish complete table snapshot")?;
        self.acknowledge(source)
    }
    pub fn create_token(&self, source: &str, shop: &str, id: &str) -> Result<String> {
        // Keep Feishu's client_token across transport failures so an ambiguous POST can be retried.
        self.db.execute(
            "INSERT OR IGNORE INTO sync_creates VALUES (?1,?2,?3,
            lower(hex(randomblob(4))||'-'||hex(randomblob(2))||'-4'||substr(hex(randomblob(2)),2)
            ||'-8'||substr(hex(randomblob(2)),2)||'-'||hex(randomblob(6))))",
            params![source, shop, id],
        )?;
        Ok(self.db.query_row(
            "SELECT token FROM sync_creates WHERE source=?1 AND shop=?2 AND id=?3",
            params![source, shop, id],
            |r| r.get(0),
        )?)
    }
    pub fn clear_token(&self, source: &str, shop: &str, id: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM sync_creates WHERE source=?1 AND shop=?2 AND id=?3",
            params![source, shop, id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_survives_restart_and_tombstones_are_limited_to_the_owner() {
        let path =
            std::env::temp_dir().join(format!("kb-sync-{}-checkpoint.sqlite", std::process::id()));
        let key = ("shop".to_owned(), "001".to_owned());
        let items = Snapshot::from([(key.clone(), json!({"answer":"first"}))]);
        {
            let mut s = State::open(&path).unwrap();
            s.bind("http://local.example").unwrap();
            assert_eq!(s.stage("a", &items).unwrap(), 1);
            assert!(s.stage("b", &items).is_err());
        }
        {
            let mut s = State::open(&path).unwrap();
            assert!(s.bind("http://other.example").is_err());
            assert_eq!(s.pending("a").unwrap()[0]["item"]["answer"], "first");
            s.acknowledge("a").unwrap();
            assert_eq!(s.stage("a", &items).unwrap(), 0);
            assert!(s.stage("b", &items).is_err());
            let b = Snapshot::from([(
                ("shop".into(), "002".into()),
                json!({"answer":"other source"}),
            )]);
            s.stage("b", &b).unwrap();
            s.acknowledge("b").unwrap();
            assert_eq!(s.stage("a", &Snapshot::new()).unwrap(), 1);
            assert_eq!(
                s.pending("a").unwrap(),
                vec![json!({"shop":key.0,"id":key.1,"item":null})]
            );
            s.acknowledge("a").unwrap();
            assert_eq!(s.stage("b", &b).unwrap(), 0);
            let token = s.create_token("b", "shop", "002").unwrap();
            assert_eq!(token.len(), 36);
            assert_eq!(token, s.create_token("b", "shop", "002").unwrap());
        }
        std::fs::remove_file(path).unwrap();
    }
}
