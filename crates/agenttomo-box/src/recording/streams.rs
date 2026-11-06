//! A run-wide sliding window. Payloads and their indexes expire together.
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};

pub const FILE: &str = "stream-messages.sqlite3";
pub const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;
pub const MAX_MESSAGES: u32 = 10_000;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub message_bytes: u32,
    pub window_bytes: u64,
    pub window_messages: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            message_bytes: 65536,
            window_bytes: 4 * 1024 * 1024,
            window_messages: 1000,
        }
    }
}

pub struct Message {
    pub request_id: u64,
    pub data: Value,
    pub payload: Vec<u8>,
}
enum Command {
    Message(Message),
    Flush(oneshot::Sender<io::Result<()>>),
}
pub struct Store {
    pub limits: Limits,
    sender: mpsc::Sender<Command>,
    dropped: Arc<AtomicU64>,
}

impl Store {
    pub async fn create(directory: &Path, limits: Limits) -> io::Result<Arc<Self>> {
        // Reserve the file with the same 0600/create_new contract as other logs.
        drop(super::create_log(directory, FILE).await?);
        let path = directory.join(FILE);
        let connection = tokio::task::spawn_blocking(move || {
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
            db.busy_timeout(std::time::Duration::from_secs(2))?;
            db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=NORMAL;
                PRAGMA secure_delete=ON; PRAGMA auto_vacuum=INCREMENTAL;
                CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT,
                    request_id INTEGER NOT NULL, data TEXT NOT NULL, payload BLOB NOT NULL, cost INTEGER NOT NULL);
                CREATE INDEX by_request ON messages(request_id, id);
                CREATE TABLE state (id INTEGER PRIMARY KEY CHECK(id=1), received INTEGER NOT NULL,
                    evicted INTEGER NOT NULL, dropped INTEGER NOT NULL, bytes INTEGER NOT NULL, limits TEXT NOT NULL);")?;
            db.execute("INSERT INTO state VALUES (1,0,0,0,0,?1)", [json!({"message_bytes":limits.message_bytes,"window_bytes":limits.window_bytes,"window_messages":limits.window_messages}).to_string()])?;
            Ok::<_, rusqlite::Error>(db)
        }).await.map_err(io::Error::other)?.map_err(io::Error::other)?;
        let (sender, mut receiver) = mpsc::channel(64);
        let dropped = Arc::new(AtomicU64::new(0));
        let store = Arc::new(Self {
            limits,
            sender,
            dropped: dropped.clone(),
        });
        tokio::task::spawn_blocking(move || {
            let mut db = connection;
            let mut error: Option<String> = None;
            while let Some(command) = receiver.blocking_recv() {
                let mut commands = vec![command];
                while commands.len() < 64 {
                    let Ok(command) = receiver.try_recv() else {
                        break;
                    };
                    commands.push(command);
                }
                let mut barriers = Vec::new();
                let mut messages = Vec::new();
                for command in commands {
                    match command {
                        Command::Message(message) => messages.push(message),
                        Command::Flush(barrier) => barriers.push(barrier),
                    }
                }
                if error.is_none()
                    && let Err(failure) =
                        save(&mut db, messages, limits, dropped.load(Ordering::Relaxed))
                {
                    error = Some(failure.to_string());
                }
                for barrier in barriers {
                    let result = error
                        .as_ref()
                        .map_or(Ok(()), |error| Err(io::Error::other(error.clone())));
                    let _ = barrier.send(result);
                }
            }
        });
        Ok(store)
    }

    // This runs in the forwarding path: never block it on SQLite or a slow UI.
    pub fn record(&self, mut message: Message) {
        message.payload.truncate(self.limits.message_bytes as usize);
        message.data["captured_bytes"] = message.payload.len().into();
        if self.sender.try_send(Command::Message(message)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn flush(&self) -> io::Result<()> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .send(Command::Flush(sender))
            .await
            .map_err(io::Error::other)?;
        receiver.await.map_err(io::Error::other)?
    }
}

fn save(
    db: &mut Connection,
    messages: Vec<Message>,
    limits: Limits,
    dropped: u64,
) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    let mut bytes: u64 =
        tx.query_row("SELECT bytes FROM state WHERE id=1", [], |row| row.get(0))?;
    let mut count: u32 = tx.query_row("SELECT count(*) FROM messages", [], |row| row.get(0))?;
    let mut received = 0;
    let mut evicted = 0;
    for message in messages {
        received += 1;
        let data = message.data.to_string();
        let cost = (data.len() + message.payload.len()) as u64;
        while count > 0
            && (bytes.saturating_add(cost) > limits.window_bytes || count >= limits.window_messages)
        {
            let (id, cost): (u64, u64) = tx.query_row(
                "SELECT id,cost FROM messages ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            tx.execute("DELETE FROM messages WHERE id=?1", [id])?;
            bytes -= cost;
            count -= 1;
            evicted += 1;
        }
        if limits.window_bytes == 0 || limits.message_bytes == 0 || cost > limits.window_bytes {
            evicted += 1;
            continue;
        }
        tx.execute(
            "INSERT INTO messages(request_id,data,payload,cost) VALUES (?1,?2,?3,?4)",
            params![message.request_id, data, message.payload, cost],
        )?;
        bytes += cost;
        count += 1;
    }
    tx.execute(
        "UPDATE state SET received=received+?1,evicted=evicted+?2,dropped=?3,bytes=?4 WHERE id=1",
        params![received, evicted, dropped, bytes],
    )?;
    tx.commit()
}

// Callers pin and validate the run directory before opening this fixed basename.
pub fn reader(path: &Path) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(std::time::Duration::from_millis(200))?;
    Ok(db)
}

pub fn snapshot(db: &mut Connection, request_id: u64) -> rusqlite::Result<Value> {
    let tx = db.transaction()?;
    let (received, evicted, dropped, bytes, limits): (u64, u64, u64, u64, String) = tx.query_row(
        "SELECT received,evicted,dropped,bytes,limits FROM state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let mut statement = tx
        .prepare("SELECT id,data FROM messages WHERE request_id=?1 ORDER BY id DESC LIMIT 10000")?;
    let mut rows = statement.query([request_id])?;
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let data: String = row.get(1)?;
        let mut data: Value = serde_json::from_str(&data).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        data["id"] = row.get::<_, u64>(0)?.into();
        messages.push(data);
    }
    messages.reverse();
    Ok(
        json!({"messages":messages,"received":received,"evicted":evicted,"dropped":dropped,"retained_bytes":bytes,"limits":serde_json::from_str::<Value>(&limits).unwrap_or(Value::Null)}),
    )
}

pub fn payload(
    db: &Connection,
    request_id: u64,
    message_id: u64,
) -> rusqlite::Result<Option<Vec<u8>>> {
    db.query_row(
        "SELECT payload FROM messages WHERE request_id=?1 AND id=?2 AND length(payload)<=?3",
        params![request_id, message_id, MAX_MESSAGE_BYTES],
        |row| row.get(0),
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn byte_budget_reuses_storage_and_zero_disables_retention() {
        let dir =
            std::env::temp_dir().join(format!("agenttomo-window-bytes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let store = Store::create(
            &dir,
            Limits {
                message_bytes: 128,
                window_bytes: 512,
                window_messages: 100,
            },
        )
        .await
        .unwrap();
        let mut peak = 0;
        for n in 0..100 {
            store.record(Message {
                request_id: 7,
                data: json!({"n":n}),
                payload: vec![b'x'; 128],
            });
            store.flush().await.unwrap();
            let mut db = reader(&dir.join(FILE)).unwrap();
            let state = snapshot(&mut db, 7).unwrap();
            assert!(state["retained_bytes"].as_u64().unwrap() <= 512);
            assert!(state["messages"].as_array().unwrap().len() <= 3);
            if n == 20 {
                peak = std::fs::metadata(dir.join(FILE)).unwrap().len();
            }
        }
        assert!(
            std::fs::metadata(dir.join(FILE)).unwrap().len() <= peak + 8192,
            "window must reuse pages, not grow per message"
        );
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        let store = Store::create(
            &dir,
            Limits {
                window_bytes: 0,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        store.record(Message {
            request_id: 7,
            data: json!({}),
            payload: b"not retained".to_vec(),
        });
        store.flush().await.unwrap();
        let mut db = reader(&dir.join(FILE)).unwrap();
        assert_eq!(snapshot(&mut db, 7).unwrap()["messages"], json!([]));
        drop(store);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn sliding_window_evicts_indexes_and_payloads_and_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("agenttomo-window-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let store = Store::create(
            &dir,
            Limits {
                message_bytes: 4,
                window_bytes: 1000,
                window_messages: 2,
            },
        )
        .await
        .unwrap();
        for n in 0..4 {
            store.record(Message {
                request_id: 7,
                data: json!({"n":n}),
                payload: b"abcdef".to_vec(),
            });
        }
        store.flush().await.unwrap();
        let mut db = reader(&dir.join(FILE)).unwrap();
        let snapshot = snapshot(&mut db, 7).unwrap();
        assert_eq!(snapshot["evicted"], 2);
        assert_eq!(snapshot["messages"].as_array().unwrap().len(), 2);
        assert_eq!(snapshot["messages"][0]["n"], 2);
        assert!(payload(&db, 7, 1).unwrap().is_none());
        assert!(payload(&db, 8, 4).unwrap().is_none());
        assert_eq!(payload(&db, 7, 4).unwrap().unwrap(), b"abcd");
        drop(store);
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
