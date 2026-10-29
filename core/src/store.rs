//! An append-only envelope log per space, and small files replaced whole.
//! Records: `len u32 ‖ checked u8 ‖ bytes`; a torn tail is cut on open.
//! `checked`: 0 not yet, else the signature's version (1 or 2).

use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct Log {
    f: File,
    end: u64,
}

pub type Rec = (u64, u8, Vec<u8>);

impl Log {
    pub fn open(path: &Path) -> Result<(Self, Vec<Rec>)> {
        fs::create_dir_all(path.parent().expect("in a dir"))?;
        let mut o = OpenOptions::new();
        let f =
            o.read(true).write(true).create(true).truncate(false).open(path)?;
        let (mut recs, mut end, mut r) = (vec![], 0, BufReader::new(&f));
        let mut head = [0; 5];
        while r.read_exact(&mut head).is_ok() {
            // A damaged length ends the log.
            let len = u32::from_le_bytes(head[..4].try_into()?) as usize;
            if len > crate::op::MAX_ENV {
                break;
            }
            let mut b = vec![0; len];
            if r.read_exact(&mut b).is_err() {
                break;
            }
            recs.push((end, head[4], b));
            end = r.stream_position()?;
        }
        f.set_len(end)?;
        Ok((Self { f, end }, recs))
    }

    pub fn append(&mut self, checked: u8, b: &[u8], sync: bool) -> Result<u64> {
        let at = self.end;
        let len = (b.len() as u32).to_le_bytes();
        self.f.seek(SeekFrom::Start(at))?;
        self.f.write_all(&[&len[..], &[checked], b].concat())?;
        if sync {
            self.f.sync_data()?;
        }
        self.end += 5 + b.len() as u64;
        Ok(at)
    }

    pub fn read(&mut self, at: u64, len: u32) -> Result<Vec<u8>> {
        let mut b = vec![0; len as usize];
        self.f.seek(SeekFrom::Start(at + 5))?;
        self.f.read_exact(&mut b)?;
        Ok(b)
    }
}

pub fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    let b = fs::read(path).ok();
    b.and_then(|b| postcard::from_bytes(&b).ok()).unwrap_or_default()
}

pub fn save<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    write(path, &postcard::to_allocvec(v)?)
}

/// Whole or not at all, readable only by us.
pub fn write(path: &Path, b: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut o = OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(&tmp)?;
    f.write_all(b)?;
    f.sync_all()?;
    Ok(fs::rename(tmp, path)?)
}

#[test]
fn torn_tail() {
    let p = std::env::temp_dir().join(format!("irch-{}.log", crate::now()));
    let (mut l, _) = Log::open(&p).unwrap();
    l.append(0, b"one", false).unwrap();
    l.append(2, b"two", true).unwrap();
    drop(l);
    let len = fs::metadata(&p).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&p)
        .unwrap()
        .write_all(&[9, 0, 0])
        .unwrap();
    let (mut l, recs) = Log::open(&p).unwrap();
    assert_eq!(recs, vec![(0, 0, b"one".to_vec()), (8, 2, b"two".to_vec())]);
    assert_eq!(fs::metadata(&p).unwrap().len(), len);
    assert_eq!(l.read(8, 3).unwrap(), b"two");
    fs::remove_file(p).unwrap();
}
