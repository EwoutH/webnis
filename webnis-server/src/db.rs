use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::Path;
use std::str::FromStr;
use std::time::SystemTime;

use gdbm;
use serde::{self, Deserialize, Deserializer};
use serde_json;

use crate::errors::*;

struct GdbmDb {
    #[allow(unused)]
    file_name: String,
    modified: Option<SystemTime>,
    lastcheck: SystemTime,
    handle: gdbm::Gdbm,
}

// Unfortunately `gdbm' is not thread-safe.
thread_local! {
    static MAPS: RefCell<HashMap<String, GdbmDb>> = RefCell::new(HashMap::new());
}

pub fn gdbm_lookup(db_path: impl AsRef<str>, key: &str) -> Result<String, WnError> {
    MAPS.with(|maps| {
        // do we have an open handle.
        let m = &mut *maps.borrow_mut();
        let path = db_path.as_ref();
        if let Some(mut db) = m.get_mut(path) {
            // yes. now, every 5 secs, see if database file has changed.
            let mut reopen = false;
            let now = SystemTime::now();
            if let Ok(d) = now.duration_since(db.lastcheck) {
                if d.as_secs() > 5 {
                    if let Ok(metadata) = fs::metadata(path) {
                        reopen = match (metadata.modified(), db.modified) {
                            (Ok(m1), Some(m2)) => m1 != m2,
                            _ => true,
                        };
                    }
                }
            }

            // no change, look up and return.
            if !reopen {
                db.lastcheck = now;
                return db.handle.fetch(key).map_err(|_| WnError::KeyNotFound);
            }

            m.remove(path);
        }

        // try to open, then lookup, and save handle.
        let metadata = fs::metadata(path).map_err(|_| WnError::MapNotFound)?;
        let handle =
            gdbm::Gdbm::new(Path::new(path), 0, gdbm::READER, 0).map_err(|_| WnError::MapNotFound)?;
        let db = GdbmDb {
            file_name: path.to_string(),
            handle:    handle,
            modified:  metadata.modified().ok(),
            lastcheck: SystemTime::now(),
        };
        let res = db.handle.fetch(key).map_err(|_| WnError::KeyNotFound);
        m.insert(path.to_owned(), db);
        res
    })
}

pub fn json_lookup(
    db_path: impl AsRef<str>,
    keyname: &str,
    keyval: &str,
) -> Result<serde_json::Value, WnError>
{
    let file = File::open(db_path.as_ref()).map_err(|_| WnError::MapNotFound)?;
    let entries: serde_json::Value = serde_json::from_reader(file).map_err(|_| WnError::DbOther)?;
    let mut idx: usize = 0;
    let keyval = match keyval.parse::<u64>() {
        Ok(num) => json!(num),
        Err(_) => json!(keyval),
    };
    loop {
        let obj = match entries.get(idx) {
            None => break,
            Some(obj) => obj,
        };
        if obj.get(keyname) == Some(&keyval) {
            return Ok(obj.to_owned());
        }
        idx += 1;
    }
    Err(WnError::KeyNotFound)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum MapType {
    Gdbm,
    Json,
    Lua,
    None,
}

impl FromStr for MapType {
    type Err = WnError;

    fn from_str(s: &str) -> Result<MapType, WnError> {
        let f = match s {
            "gdbm" => MapType::Gdbm,
            "json" => MapType::Json,
            "lua" => MapType::Lua,
            _ => return Err(WnError::UnknownMapType),
        };
        Ok(f)
    }
}

// Serde helper
pub fn deserialize_map_type<'de, D>(deserializer: D) -> Result<MapType, D::Error>
where D: Deserializer<'de> {
    let s = String::deserialize(deserializer)?;
    MapType::from_str(&s).map_err(serde::de::Error::custom)
}

impl Default for MapType {
    fn default() -> MapType {
        MapType::None
    }
}
