mod search;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use arc_swap::ArcSwap;
use im::{HashMap, OrdSet};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use self::search::{FullIndex, TextView};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub shop: String,
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub product_ids: Vec<String>,
    pub scope: String,
    #[serde(default)]
    pub sku: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub source: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub shop: String,
    pub id: String,
    // Require an explicit null to delete. A missing item often means a malformed feed.
    #[serde(deserialize_with = "Option::deserialize")]
    pub item: Option<Item>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id == id.trim() && id.len() <= 2048 && !id.chars().any(char::is_control)
}

impl Item {
    fn validate(&mut self) -> Result<()> {
        ensure!(
            valid_id(&self.shop) && valid_id(&self.id),
            "invalid shop or record id"
        );
        ensure!(
            self.kind == "sku" || self.kind == "answer",
            "invalid item kind"
        );
        ensure!(
            self.product_ids.iter().all(|id| valid_id(id)),
            "invalid product id"
        );
        self.product_ids.sort();
        self.product_ids.dedup();
        match self.scope.as_str() {
            "product" => ensure!(
                !self.product_ids.is_empty(),
                "product scope requires a product id"
            ),
            "shop" => ensure!(
                self.kind == "answer" && self.product_ids.is_empty(),
                "shop scope requires an answer without product bindings"
            ),
            _ => anyhow::bail!("invalid item scope"),
        }
        Ok(())
    }
}

fn normalized(value: &str) -> String {
    value.trim().to_lowercase()
}

type RecordKey = (String, String);
type SkuKey = (String, String, Selector);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Selector {
    Product,
    Sku(String),
    Model(String),
}

impl Item {
    fn sku_keys(&self) -> Vec<SkuKey> {
        if !self.enabled || self.kind != "sku" {
            return Vec::new();
        }
        let mut selectors = vec![Selector::Product];
        if !self.sku.trim().is_empty() {
            selectors.push(Selector::Sku(normalized(&self.sku)));
        }
        if !self.model.trim().is_empty() {
            selectors.push(Selector::Model(normalized(&self.model)));
        }
        self.product_ids
            .iter()
            .flat_map(|product| {
                selectors
                    .iter()
                    .map(move |selector| (self.shop.clone(), product.clone(), selector.clone()))
            })
            .collect()
    }
}

#[derive(Clone, Default)]
struct Catalog {
    items: HashMap<RecordKey, Arc<Item>>,
    exact: HashMap<SkuKey, OrdSet<String>>,
}

impl Catalog {
    fn replace(&mut self, key: &RecordKey, next: Option<Arc<Item>>) {
        if let Some(old) = self.items.remove(key) {
            for selector in old.sku_keys() {
                if let Some(ids) = self.exact.get_mut(&selector) {
                    ids.remove(&old.id);
                    if ids.is_empty() {
                        self.exact.remove(&selector);
                    }
                }
            }
        }
        if let Some(item) = next {
            for selector in item.sku_keys() {
                self.exact
                    .entry(selector)
                    .or_default()
                    .insert(item.id.clone());
            }
            self.items.insert(key.clone(), item);
        }
    }
}

struct View {
    version: u64,
    catalog: Catalog,
    text: TextView,
}

struct Writer {
    db: Connection,
    index: Option<FullIndex>,
}

pub struct Engine {
    writer: Mutex<Writer>,
    view: ArcSwap<View>,
}

#[cfg(test)]
mod tests;

impl Engine {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).context("open knowledge database")?;
        db.busy_timeout(Duration::from_secs(3))?;
        // One API process owns this database. An independent writer cannot keep this
        // process's immutable in-memory view coherent, so reject a second owner.
        db.execute_batch("PRAGMA locking_mode = EXCLUSIVE;
                          PRAGMA journal_mode = WAL;
                          PRAGMA synchronous = FULL;
                          BEGIN IMMEDIATE;
                          CREATE TABLE IF NOT EXISTS kb_meta (
                            singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL
                          );
                          INSERT OR IGNORE INTO kb_meta VALUES(1,0);
                          CREATE TABLE IF NOT EXISTS kb_items (
                            shop TEXT NOT NULL, id TEXT NOT NULL, item TEXT NOT NULL,
                            PRIMARY KEY(shop,id)
                          ) WITHOUT ROWID;
                          COMMIT;")?;
        let (view, index) = Self::load(&db)?;
        Ok(Self {
            writer: Mutex::new(Writer {
                db,
                index: Some(index),
            }),
            view: ArcSwap::from_pointee(view),
        })
    }

    fn load(db: &Connection) -> Result<(View, FullIndex)> {
        let version: i64 =
            db.query_row("SELECT version FROM kb_meta WHERE singleton=1", [], |r| {
                r.get(0)
            })?;
        let version = u64::try_from(version).context("invalid stored version")?;
        let mut catalog = Catalog::default();
        let mut index = FullIndex::new()?;
        let mut statement = db.prepare("SELECT shop,id,item FROM kb_items")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let key: RecordKey = (row.get(0)?, row.get(1)?);
            let mut item: Item = serde_json::from_str(row.get_ref(2)?.as_str()?)?;
            item.validate()?;
            ensure!(
                key.0 == item.shop && key.1 == item.id,
                "stored record identity mismatch"
            );
            index.add(&item)?;
            catalog.replace(&key, Some(Arc::new(item)));
        }
        let text = index.publish()?;
        Ok((
            View {
                version,
                catalog,
                text,
            },
            index,
        ))
    }

    pub fn version(&self) -> u64 {
        self.view.load().version
    }

    pub fn apply(&self, changes: Vec<Change>) -> Result<u64> {
        let mut pending = BTreeMap::new();
        for change in changes {
            ensure!(
                valid_id(&change.shop) && valid_id(&change.id),
                "invalid change identity"
            );
            let mut item = change.item;
            if let Some(item) = item.as_mut() {
                ensure!(
                    item.shop == change.shop && item.id == change.id,
                    "change identity does not match item"
                );
                item.validate()?;
            }
            pending.insert((change.shop, change.id), item.map(Arc::new));
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("knowledge database writer unavailable"))?;
        if writer.index.is_none() {
            // A failed SQLite or index commit may have advanced the private index.
            // Rebuild only from committed SQLite rows before allowing another write.
            let (view, index) = Self::load(&writer.db)?;
            writer.index = Some(index);
            self.view.store(Arc::new(view));
        }
        let before = self.view.load_full();
        pending.retain(|key, item| before.catalog.items.get(key) != item.as_ref());
        if pending.is_empty() {
            return Ok(before.version);
        }
        let version = before
            .version
            .checked_add(1)
            .context("knowledge version overflow")?;
        let Writer { db, index } = &mut *writer;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut catalog = before.catalog.clone();
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO kb_items(shop,id,item) VALUES(?1,?2,?3)
                ON CONFLICT(shop,id) DO UPDATE SET item=excluded.item",
            )?;
            let mut delete = tx.prepare_cached("DELETE FROM kb_items WHERE shop=?1 AND id=?2")?;
            for (key, item) in &pending {
                if let Some(item) = item {
                    upsert.execute(params![key.0, key.1, serde_json::to_string(item.as_ref())?])?;
                } else {
                    delete.execute(params![key.0, key.1])?;
                }
                catalog.replace(key, item.clone());
            }
        }
        tx.execute(
            "UPDATE kb_meta SET version=?1 WHERE singleton=1",
            [i64::try_from(version).context("database version overflow")?],
        )?;
        // A private Tantivy commit is invisible to readers until SQLite commits and
        // we atomically publish the exact indexes and matching Searcher together.
        let mut working = index.take().context("knowledge index writer unavailable")?;
        let mut changed_text = false;
        for (key, item) in &pending {
            if before
                .catalog
                .items
                .get(key)
                .is_some_and(|i| search::eligible(i))
            {
                working.delete(&key.0, &key.1);
                changed_text = true;
            }
            if let Some(item) = item.as_ref().filter(|i| search::eligible(i)) {
                working.add(item)?;
                changed_text = true;
            }
        }
        let text = if changed_text {
            working.publish()?
        } else {
            before.text.clone()
        };
        tx.commit()?;
        *index = Some(working);
        self.view.store(Arc::new(View {
            version,
            catalog,
            text,
        }));
        Ok(version)
    }

    pub fn lookup(&self, shop: &str, product: &str, sku: &str, model: &str) -> (u64, Vec<Item>) {
        let view = self.view.load();
        let sku = normalized(sku);
        let model = normalized(model);
        let selector = if !sku.is_empty() {
            Selector::Sku(sku)
        } else if !model.is_empty() {
            Selector::Model(model.clone())
        } else {
            Selector::Product
        };
        let ids = view
            .catalog
            .exact
            .get(&(shop.to_owned(), product.to_owned(), selector));
        let mut result = Vec::new();
        if let Some(ids) = ids {
            for id in ids {
                let Some(item) = view.catalog.items.get(&(shop.to_owned(), id.clone())) else {
                    continue;
                };
                if model.is_empty() || model == normalized(&item.model) {
                    result.push(item.as_ref().clone());
                    if result.len() == 100 {
                        break;
                    }
                }
            }
        }
        (view.version, result)
    }

    pub fn search(&self, shop: &str, product: &str, q: &str, limit: usize) -> (u64, Vec<Item>) {
        let view = self.view.load();
        let mut result = Vec::new();
        if let Ok(ids) = view.text.find(shop, product, q, limit.min(100)) {
            for id in ids {
                if let Some(item) = view.catalog.items.get(&(shop.to_owned(), id)) {
                    result.push(item.as_ref().clone());
                }
            }
        }
        (view.version, result)
    }
}
