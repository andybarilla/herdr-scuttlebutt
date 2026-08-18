use anyhow::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub ts: String,
    pub from: String,
    pub text: String,
}

fn room_file(dir: &Path) -> std::path::PathBuf {
    dir.join("room.jsonl")
}

fn parse_lines(content: &str) -> Vec<Message> {
    content
        .lines()
        .filter_map(|l| serde_json::from_str::<Message>(l).ok())
        .collect()
}

pub fn append(dir: &Path, from: &str, text: &str) -> Result<Message> {
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(room_file(dir))?;
    file.lock_exclusive()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let content = String::from_utf8_lossy(&bytes).into_owned();
    let next_id = parse_lines(&content).last().map(|m| m.id).unwrap_or(0) + 1;
    let msg = Message {
        id: next_id,
        ts: chrono::Utc::now().to_rfc3339(),
        from: from.to_string(),
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&msg)?;
    // Recover from a torn trailing write: if the file doesn't end with \n, prefix one.
    if !content.is_empty() && !content.ends_with('\n') {
        line = format!("\n{line}");
    }
    writeln!(file, "{line}")?;
    fs2::FileExt::unlock(&file)?;
    Ok(msg)
}

pub fn read_since(dir: &Path, since_id: u64) -> Result<Vec<Message>> {
    let bytes = match std::fs::read(room_file(dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let content = String::from_utf8_lossy(&bytes).into_owned();
    Ok(parse_lines(&content)
        .into_iter()
        .filter(|m| m.id > since_id)
        .collect())
}

pub fn last_id(dir: &Path) -> Result<u64> {
    Ok(read_since(dir, 0)?.last().map(|m| m.id).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_assigns_increasing_ids() {
        let dir = tempfile::tempdir().unwrap();
        let m1 = append(dir.path(), "alice", "hello").unwrap();
        let m2 = append(dir.path(), "bob", "hi").unwrap();
        assert_eq!(m1.id, 1);
        assert_eq!(m2.id, 2);
        assert_eq!(m2.from, "bob");
    }

    #[test]
    fn read_since_filters_and_orders() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            append(dir.path(), "alice", &format!("msg{i}")).unwrap();
        }
        let msgs = read_since(dir.path(), 2).unwrap();
        assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn last_id_is_zero_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(last_id(dir.path()).unwrap(), 0);
    }

    #[test]
    fn torn_trailing_line_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), "alice", "ok").unwrap();
        // simulate a torn write
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("room.jsonl"))
            .unwrap();
        write!(f, "{{\"id\":2,\"ts\":\"tr").unwrap();
        drop(f);
        assert_eq!(last_id(dir.path()).unwrap(), 1);
        assert_eq!(read_since(dir.path(), 0).unwrap().len(), 1);
        // next append recovers: id continues from last valid line
        let m = append(dir.path(), "bob", "next").unwrap();
        assert_eq!(m.id, 2);
    }

    #[test]
    fn torn_multibyte_utf8_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let m1 = append(dir.path(), "alice", "hi \u{1F600} there").unwrap();
        assert_eq!(m1.text, "hi \u{1F600} there");

        // Simulate a crash mid-write that truncates inside a multi-byte
        // UTF-8 sequence: write a valid line prefix, then a raw fragment
        // whose id field is followed by a truncated 4-byte emoji (only the
        // first 2 bytes land on disk).
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("room.jsonl"))
            .unwrap();
        let emoji_bytes = "\u{1F600}".as_bytes();
        let mut fragment = b"{\"id\":2,\"ts\":\"t\",\"from\":\"x\",\"text\":\"".to_vec();
        fragment.extend_from_slice(&emoji_bytes[..2]); // truncated multi-byte sequence
        f.write_all(&fragment).unwrap();
        drop(f);

        // Reads must not error out on invalid UTF-8; the torn line is just
        // unparseable JSON and gets dropped.
        assert_eq!(last_id(dir.path()).unwrap(), 1);
        let msgs = read_since(dir.path(), 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], m1);

        // A subsequent append still succeeds and reuses id 2.
        let m2 = append(dir.path(), "bob", "recovered").unwrap();
        assert_eq!(m2.id, 2);

        let all = read_since(dir.path(), 0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], m1);
        assert_eq!(all[1], m2);
    }
}
