//! The watcher's ring, written from Rust: gongoozler's `spec/ring.md`, byte for byte the format
//! `python/src/gongoozler/_ring.py` writes, so the engine's loops report a run's progress while
//! the run holds no Python lock (review H6).
//!
//! * **Ordered events** are records: `[u32 length][u32 crc32][payload]`, padded to 8 bytes, the
//!   payload one event as JSON (`spec/protocol.md`).
//! * **Progress and metrics** are latest-value slots, each guarded by a sequence lock and a crc32.
//!   Before an ordered event, every slot changed since the last record is written as a record of
//!   its own (with its `slot` and `sseq`), so progress never reads after the stage that made it.
//!
//! The writer never waits: a record that does not fit is dropped and counted in the header, and
//! nothing here starts a thread or makes a system call once the ring exists. A ring has one
//! writer: the engine makes its own, beside the Python emitter's, in the directory the supervisor
//! reads (`GONGOOZLER_RING`).

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering, fence};
use std::time::{SystemTime, UNIX_EPOCH};

use memmap2::MmapMut;

/// The ring's magic, written last.
pub const MAGIC: &[u8; 8] = b"GZRING01";
/// The event protocol's major version.
pub const PROTOCOL: u32 = 1;
const VERSION: u32 = 1;
const HEADER: usize = 4096;
const SKIP: u32 = 0xFFFF_FFFF;
const SLOT_SIZE: usize = 256;
const SLOT_KEY_MAX: usize = SLOT_SIZE - 80;

const O_VERSION: usize = 8;
const O_HEADER: usize = 12;
const O_DATA_SIZE: usize = 16;
const O_SLOTS_OFF: usize = 24;
const O_SLOT_COUNT: usize = 32;
const O_SLOT_SIZE: usize = 36;
const O_PID: usize = 40;
const O_CREATED: usize = 48;
const O_HEAD: usize = 64;
const O_TAIL: usize = 128;
const O_DROPPED: usize = 192;
const O_TRUNCATED: usize = 200;
const O_CLOSED: usize = 216;
const O_SLOTS_USED: usize = 224;
const O_SLOTS_FULL: usize = 232;

const KIND_PROGRESS: u8 = 1;
const KIND_METRIC: u8 = 2;
const F_B: u8 = 1;
const F_C: u8 = 2;
const F_D: u8 = 4;
const F_E: u8 = 8;
const F_A_INT: u8 = 32;

/// A field's value in an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A string.
    Str(String),
    /// A number (non-finite numbers become null).
    Num(f64),
    /// An integer.
    Int(i64),
    /// True or false.
    Bool(bool),
    /// Null.
    Null,
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Self::Num(x)
    }
}

impl From<i64> for Value {
    fn from(x: i64) -> Self {
        Self::Int(x)
    }
}

impl From<u64> for Value {
    fn from(x: u64) -> Self {
        Self::Int(i64::try_from(x).unwrap_or(i64::MAX))
    }
}

impl From<bool> for Value {
    fn from(x: bool) -> Self {
        Self::Bool(x)
    }
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn json_value(out: &mut String, v: &Value) {
    match v {
        Value::Str(s) => json_str(out, s),
        Value::Num(x) if x.is_finite() => {
            // Integral floats keep a decimal point, as Python's json writes them.
            if x.fract() == 0.0 && x.abs() < 1e16 {
                let _ = write!(out, "{x:.1}");
            } else {
                let _ = write!(out, "{x}");
            }
        }
        Value::Num(_) | Value::Null => out.push_str("null"),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
    }
}

/// One event as JSON: `fields` in order, keys as given.
#[must_use]
pub fn encode(fields: &[(&str, Value)]) -> String {
    let mut out = String::with_capacity(160);
    out.push('{');
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(&mut out, k);
        out.push(':');
        json_value(&mut out, v);
    }
    out.push('}');
    out
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// A slot's latest value, waiting to be written as a record ahead of the next ordered event.
#[derive(Clone, Debug)]
struct Pending {
    kind: u8,
    fields: Vec<(&'static str, Value)>,
    t: f64,
    sseq: u64,
}

/// A ring writer. Not thread-safe by itself: one writer, as the format needs.
pub struct Ring {
    map: MmapMut,
    path: PathBuf,
    data_size: u64,
    slots_off: usize,
    slot_count: usize,
    head: u64,
    seq: u64,
    pid: u32,
    run: Option<String>,
    slots: HashMap<(u8, String), usize>,
    slot_seq: Vec<u64>,
    pending: Vec<Option<Pending>>,
    units: HashMap<String, String>,
    closed: bool,
}

impl Ring {
    /// A new ring in `dir`, named `PID-RANDOM.ring`, mode 0600, with `data_size` bytes of records
    /// (a power of two, at least 64 KiB) and `slot_count` slots.
    ///
    /// # Errors
    /// The file cannot be made or mapped, or the sizes break the format's rules.
    pub fn create(dir: &Path, data_size: u64, slot_count: usize, run: Option<String>) -> io::Result<Self> {
        if !data_size.is_power_of_two() || data_size < 1 << 16 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "data_size must be a power of two of at least 64 KiB"));
        }
        let pid = std::process::id();
        let mut h = RandomState::new().build_hasher();
        h.write_u64(now().to_bits());
        let path = dir.join(format!("{pid}-{:08x}.ring", h.finish() as u32));
        let slots_off = HEADER + data_size as usize;
        let size = slots_off + slot_count * SLOT_SIZE;
        let file = OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        file.set_len(size as u64)?;
        // SAFETY: the file is ours, just made; the supervisor maps it to read.
        let mut map = unsafe { MmapMut::map_mut(&file)? };
        map[O_VERSION..O_VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
        map[O_HEADER..O_HEADER + 4].copy_from_slice(&(HEADER as u32).to_le_bytes());
        map[O_DATA_SIZE..O_DATA_SIZE + 8].copy_from_slice(&data_size.to_le_bytes());
        map[O_SLOTS_OFF..O_SLOTS_OFF + 8].copy_from_slice(&(slots_off as u64).to_le_bytes());
        map[O_SLOT_COUNT..O_SLOT_COUNT + 4].copy_from_slice(&(slot_count as u32).to_le_bytes());
        map[O_SLOT_SIZE..O_SLOT_SIZE + 4].copy_from_slice(&(SLOT_SIZE as u32).to_le_bytes());
        map[O_PID..O_PID + 4].copy_from_slice(&pid.to_le_bytes());
        let created = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        map[O_CREATED..O_CREATED + 8].copy_from_slice(&created.to_le_bytes());
        // The magic last: a reader seeing it sees a whole header.
        fence(Ordering::Release);
        map[0..8].copy_from_slice(MAGIC);
        Ok(Self {
            map,
            path,
            data_size,
            slots_off,
            slot_count,
            head: 0,
            seq: 0,
            pid,
            run,
            slots: HashMap::new(),
            slot_seq: vec![0; slot_count],
            pending: vec![None; slot_count],
            units: HashMap::new(),
            closed: false,
        })
    }

    /// The ring's file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The process that made it.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn atomic(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: the offsets are 8-byte aligned within a page-aligned mapping that outlives the
        // reference, and the other process touches these words only as whole u64s.
        unsafe { AtomicU64::from_ptr(self.map.as_ptr().add(offset).cast::<u64>().cast_mut()) }
    }

    fn count(&self, offset: usize) {
        self.atomic(offset).fetch_add(1, Ordering::Relaxed);
    }

    /// Appends one record; false (and counted) when it does not fit.
    fn write(&mut self, payload: &[u8]) -> bool {
        let need = (8 + payload.len() as u64 + 7) & !7;
        if need > self.data_size / 4 {
            self.count(O_TRUNCATED);
            return false;
        }
        let tail = self.atomic(O_TAIL).load(Ordering::Acquire);
        let pos = self.head % self.data_size;
        let wrap = if pos + need > self.data_size { self.data_size - pos } else { 0 };
        if self.data_size - (self.head - tail) < wrap + need {
            self.count(O_DROPPED);
            return false;
        }
        let mut pos = pos as usize;
        if wrap > 0 {
            self.map[HEADER + pos..HEADER + pos + 4].copy_from_slice(&SKIP.to_le_bytes());
            self.head += wrap;
            pos = 0;
        }
        let base = HEADER + pos;
        self.map[base + 8..base + 8 + payload.len()].copy_from_slice(payload);
        self.map[base..base + 4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        self.map[base + 4..base + 8].copy_from_slice(&crc32fast::hash(payload).to_le_bytes());
        self.head += need;
        self.atomic(O_HEAD).store(self.head, Ordering::Release);
        true
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn envelope(&mut self, kind: &str, t: f64, fields: &[(&str, Value)], extra: &[(&str, Value)]) -> String {
        let seq = self.next_seq();
        let mut all: Vec<(&str, Value)> = Vec::with_capacity(fields.len() + extra.len() + 5);
        all.push(("kind", kind.into()));
        all.push(("t", t.into()));
        all.extend(fields.iter().cloned());
        all.extend(extra.iter().cloned());
        all.push(("seq", seq.into()));
        all.push(("v", i64::from(PROTOCOL).into()));
        all.push(("pid", i64::from(self.pid).into()));
        if let Some(r) = &self.run {
            all.push(("run", r.clone().into()));
        }
        encode(&all)
    }

    /// The slots changed since the last record, written as records ahead of the next one.
    fn flush_pending(&mut self) {
        for i in 0..self.pending.len() {
            if let Some(p) = self.pending[i].take() {
                let kind = if p.kind == KIND_PROGRESS { "progress" } else { "metric" };
                let extra = [("slot", Value::Int(i as i64)), ("sseq", Value::Int(p.sseq as i64))];
                let payload = self.envelope(kind, p.t, &p.fields, &extra);
                self.write(payload.as_bytes());
            }
        }
    }

    /// An ordered event: `kind` and its `fields`, in the envelope (`t`, `seq`, `v`, `pid`, `run`).
    pub fn event(&mut self, kind: &str, fields: &[(&str, Value)]) {
        if self.closed {
            return;
        }
        self.flush_pending();
        let payload = self.envelope(kind, now(), fields, &[]);
        self.write(payload.as_bytes());
    }

    fn slot(&mut self, kind: u8, key: &str) -> Option<usize> {
        if let Some(i) = self.slots.get(&(kind, key.to_owned())) {
            return Some(*i);
        }
        if self.slots.len() >= self.slot_count {
            self.count(O_SLOTS_FULL);
            return None;
        }
        let i = self.slots.len();
        self.slots.insert((kind, key.to_owned()), i);
        self.atomic(O_SLOTS_USED).store(self.slots.len() as u64, Ordering::Release);
        Some(i)
    }

    #[allow(clippy::too_many_arguments)]
    fn set(&mut self, i: usize, kind: u8, key: &[u8], a: (f64, bool), b: Option<f64>, c: Option<i64>, d: Option<f64>, e: Option<f64>, t: f64) -> u64 {
        let mut flags = 0u8;
        if a.1 {
            flags |= F_A_INT;
        }
        for (present, f) in [(b.is_some(), F_B), (c.is_some(), F_C), (d.is_some(), F_D), (e.is_some(), F_E)] {
            if present {
                flags |= f;
            }
        }
        let key = &key[..key.len().min(SLOT_KEY_MAX)];
        let mut body = Vec::with_capacity(68 + key.len());
        body.push(kind);
        body.push(flags);
        body.extend_from_slice(&(key.len() as u16).to_le_bytes());
        for x in [a.0, b.unwrap_or(0.0)] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        body.extend_from_slice(&c.unwrap_or(0).to_le_bytes());
        for x in [d.unwrap_or(0.0), e.unwrap_or(0.0), t] {
            body.extend_from_slice(&x.to_le_bytes());
        }
        body.extend_from_slice(&self.seq.to_le_bytes());
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(key);
        let crc = crc32fast::hash(&body);
        let off = self.slots_off + i * SLOT_SIZE;
        let seq = self.slot_seq[i] + 1;
        self.atomic(off).store(seq, Ordering::Release); // odd: being written
        fence(Ordering::Release);
        self.map[off + 12..off + 12 + body.len()].copy_from_slice(&body);
        self.map[off + 8..off + 12].copy_from_slice(&crc.to_le_bytes());
        fence(Ordering::Release);
        self.atomic(off).store(seq + 1, Ordering::Release); // even: stable
        self.slot_seq[i] = seq + 1;
        seq + 1
    }

    /// How far `key` has got: `done` of `total` (in `unit`), at simulated time `sim`
    /// (nanoseconds), at `rate` units a second with `eta` seconds to go. Only the latest value is
    /// kept.
    #[allow(clippy::too_many_arguments)]
    pub fn progress(&mut self, key: &str, done: f64, total: Option<f64>, unit: Option<&str>, sim: Option<i64>, rate: Option<f64>, eta: Option<f64>) {
        if self.closed {
            return;
        }
        if let Some(u) = unit
            && self.units.get(key).map(String::as_str) != Some(u)
        {
            self.units.insert(key.to_owned(), u.to_owned());
            self.event("progress.unit", &[("key", key.into()), ("unit", u.into())]);
        }
        let Some(i) = self.slot(KIND_PROGRESS, key) else { return };
        let t = now();
        let fin = |x: Option<f64>| x.filter(|v| v.is_finite());
        let (total, rate, eta) = (fin(total), fin(rate), fin(eta));
        let sseq = self.set(i, KIND_PROGRESS, key.as_bytes(), (done, false), total, sim, rate, eta, t);
        let mut fields: Vec<(&'static str, Value)> = vec![("key", key.into()), ("done", done.into())];
        if let Some(x) = total {
            fields.push(("total", x.into()));
        }
        if let Some(x) = sim {
            fields.push(("sim", x.into()));
        }
        if let Some(x) = rate {
            fields.push(("rate", x.into()));
        }
        if let Some(x) = eta {
            fields.push(("eta", x.into()));
        }
        self.pending[i] = Some(Pending { kind: KIND_PROGRESS, fields, t, sseq });
    }

    /// A named number, with the simulated time it is for. Only the latest value is kept.
    pub fn metric(&mut self, name: &str, value: f64, at: Option<i64>) {
        if self.closed {
            return;
        }
        let Some(i) = self.slot(KIND_METRIC, name) else { return };
        let t = now();
        let sseq = self.set(i, KIND_METRIC, name.as_bytes(), (value, false), None, at, None, None, t);
        let mut fields: Vec<(&'static str, Value)> = vec![("name", name.into()), ("value", value.into())];
        if let Some(x) = at {
            fields.push(("at", x.into()));
        }
        self.pending[i] = Some(Pending { kind: KIND_METRIC, fields, t, sseq });
    }

    /// Writes what is pending and marks the ring closed cleanly.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.flush_pending();
        self.closed = true;
        self.atomic(O_CLOSED).store(1, Ordering::Release);
        let _ = self.map.flush();
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u64_at(m: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(m[o..o + 8].try_into().unwrap())
    }

    fn u32_at(m: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(m[o..o + 4].try_into().unwrap())
    }

    /// Reads every record back, as the supervisor does.
    fn records(m: &[u8]) -> Vec<String> {
        let size = u64_at(m, O_DATA_SIZE);
        let head = u64_at(m, O_HEAD);
        let mut pos = 0u64;
        let mut out = Vec::new();
        while pos < head {
            let at = HEADER + (pos % size) as usize;
            let len = u32_at(m, at);
            if len == SKIP {
                pos += size - pos % size;
                continue;
            }
            let payload = &m[at + 8..at + 8 + len as usize];
            assert_eq!(u32_at(m, at + 4), crc32fast::hash(payload), "crc");
            out.push(String::from_utf8(payload.to_vec()).unwrap());
            pos += (8 + u64::from(len) + 7) & !7;
        }
        out
    }

    #[test]
    fn a_ring_reads_back_as_the_format_says() {
        let dir = std::env::temp_dir().join(format!("fi-ring-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut r = Ring::create(&dir, 1 << 16, 8, Some("run1".into())).unwrap();
        r.event("hello", &[("producer", "fi-engine".into())]);
        r.progress("fi.ledger", 10.0, Some(100.0), Some("steps"), Some(1_700_000_000_000_000_000), Some(5.0), Some(18.0));
        r.progress("fi.ledger", 20.0, Some(100.0), Some("steps"), None, None, None);
        r.event("stage", &[("name", "fi.ledger".into()), ("phase", "end".into()), ("ok", true.into())]);
        r.metric("nav", f64::NAN, None);
        r.close();
        let m = std::fs::read(r.path()).unwrap();
        assert_eq!(&m[0..8], MAGIC);
        assert_eq!(u64_at(&m, O_CLOSED), 1);
        assert_eq!(u64_at(&m, O_SLOTS_USED), 2);
        let recs = records(&m);
        // hello, the unit, the progress written ahead of the stage, the stage, the metric at close.
        assert_eq!(recs.len(), 5, "{recs:#?}");
        assert!(recs[0].starts_with("{\"kind\":\"hello\",\"t\":") && recs[0].contains("\"producer\":\"fi-engine\"") && recs[0].contains("\"run\":\"run1\""));
        assert!(recs[1].contains("\"kind\":\"progress.unit\"") && recs[1].contains("\"unit\":\"steps\""));
        assert!(recs[2].contains("\"kind\":\"progress\"") && recs[2].contains("\"done\":20.0") && recs[2].contains("\"slot\":0"));
        assert!(recs[3].contains("\"kind\":\"stage\""));
        assert!(recs[4].contains("\"value\":null"), "non-finite numbers are null");
        let seqs: Vec<u64> = recs.iter().map(|r| r.split("\"seq\":").nth(1).unwrap().split(',').next().unwrap().parse().unwrap()).collect();
        assert!(seqs.windows(2).all(|w| w[1] > w[0]), "seq strictly increases: {seqs:?}");
        // The slot holds the latest value, its seq even, its crc whole.
        let off = u64_at(&m, O_SLOTS_OFF) as usize;
        assert_eq!(u64_at(&m, off) % 2, 0);
        let keylen = u16::from_le_bytes(m[off + 14..off + 16].try_into().unwrap()) as usize;
        assert_eq!(&m[off + 80..off + 80 + keylen], b"fi.ledger");
        assert_eq!(u32_at(&m, off + 8), crc32fast::hash(&m[off + 12..off + 80 + keylen]));
        assert_eq!(f64::from_le_bytes(m[off + 16..off + 24].try_into().unwrap()), 20.0);
        std::fs::remove_file(r.path()).unwrap();
    }

    #[test]
    fn a_full_ring_drops_and_counts_never_waiting() {
        let dir = std::env::temp_dir().join(format!("fi-ring-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut r = Ring::create(&dir, 1 << 16, 4, None).unwrap();
        let long = "x".repeat(1000);
        for _ in 0..200 {
            r.event("log", &[("msg", long.as_str().into())]);
        }
        let m = std::fs::read(r.path()).unwrap();
        assert!(u64_at(&m, O_DROPPED) > 0, "nobody consumed: the ring filled and counted");
        r.event("log", &[("msg", "y".repeat(20_000).into())]);
        assert_eq!(u64_at(&std::fs::read(r.path()).unwrap(), O_TRUNCATED), 1, "a record over a quarter of the ring is refused");
        let p = r.path().to_owned();
        drop(r);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(
            encode(&[("a", "q\"\\\n\u{1}".into()), ("b", Value::Int(-3)), ("c", 1.5.into()), ("d", Value::Null)]),
            r#"{"a":"q\"\\\n\u0001","b":-3,"c":1.5,"d":null}"#
        );
    }
}
