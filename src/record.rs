//! `--record FILE`: watch intervals appended to a CSV file, for looking back
//! later at who was growing or swapping.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};

pub struct Recorder {
    path: String,
    file: File,
}

impl Recorder {
    /// Opens FILE for appending. A new or empty file gets `header`; an existing
    /// one must already start with it, so one file never mixes two layouts.
    pub fn open(path: &str, header: &str) -> Result<Recorder, String> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(|e| format!("--record {path}: {e}"))?;
        let mut first = String::new();
        BufReader::new(&file)
            .read_line(&mut first)
            .map_err(|e| format!("--record {path}: {e}"))?;
        if first.is_empty() {
            file.write_all(header.as_bytes())
                .map_err(|e| format!("--record {path}: {e}"))?;
        } else if first != header {
            return Err(format!(
                "--record {path}: its columns differ from this run's (another mode \
                 or -t/-a?); record into a new file\n  file:     {}\n  this run: {}",
                first.trim_end(),
                header.trim_end()
            ));
        }
        Ok(Recorder {
            path: path.to_string(),
            file,
        })
    }

    pub fn write(&mut self, rows: &str) -> Result<(), String> {
        self.file
            .write_all(rows.as_bytes())
            .map_err(|e| format!("--record {}: {e}", self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recorder_header_once_and_mismatch() {
        let dir = std::env::temp_dir().join(format!("macswapi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rec.csv");
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);

        let mut r = Recorder::open(path, "time,pid\n").unwrap();
        r.write("t1,1\n").unwrap();
        drop(r);
        let mut r = Recorder::open(path, "time,pid\n").unwrap(); // reopened: no 2nd header
        r.write("t2,2\n").unwrap();
        drop(r);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "time,pid\nt1,1\nt2,2\n"
        );
        assert!(Recorder::open(path, "time,pid,depth\n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
