//! Just enough Python pickle (protocol 2) to write mapproxy-seed's `--progress-file`, which
//! tnt-mapproxy's admin app reads with `pickle.load` (it only looks at the dict keys).

pub enum Py {
    Str(String),
    Int(i64),
    Tuple(Vec<Py>),
}

fn put(out: &mut Vec<u8>, v: &Py) {
    match v {
        Py::Str(s) => {
            out.push(b'X'); // BINUNICODE
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Py::Int(i) if (0..256).contains(i) => {
            out.push(b'K'); // BININT1
            out.push(*i as u8);
        }
        Py::Int(i) if i32::try_from(*i).is_ok() => {
            out.push(b'J'); // BININT
            out.extend_from_slice(&(*i as i32).to_le_bytes());
        }
        Py::Int(i) => {
            out.push(0x8a); // LONG1
            out.push(8);
            out.extend_from_slice(&i.to_le_bytes());
        }
        Py::Tuple(items) => {
            out.push(b'('); // MARK
            for it in items {
                put(out, it);
            }
            out.push(b't'); // TUPLE
        }
    }
}

/// `pickle.dumps({k: v, ...}, protocol=2)`.
pub fn dumps_dict(items: &[(Py, Py)]) -> Vec<u8> {
    let mut out = vec![0x80, 2, b'}']; // PROTO 2, EMPTY_DICT
    if !items.is_empty() {
        out.push(b'(');
        for (k, v) in items {
            put(&mut out, k);
            put(&mut out, v);
        }
        out.push(b'u'); // SETITEMS
    }
    out.push(b'.');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_can_load_it() {
        let key = Py::Tuple(vec![
            Py::Str("topo_norway_2x".into()),
            Py::Str("topo_cache_2x".into()),
            Py::Str("norway_25833_2x".into()),
            Py::Tuple((0..15).map(Py::Int).collect()),
        ]);
        let val = Py::Tuple(vec![Py::Str("rmapproxy".into()), Py::Int(1234), Py::Int(1 << 40)]);
        let data = dumps_dict(&[(key, val)]);
        let path = std::env::temp_dir().join(format!("rmp-pickle-{}", std::process::id()));
        std::fs::write(&path, &data).unwrap();
        let out = std::process::Command::new("python3")
            .args(["-I", "-c", "import pickle,sys; d=pickle.load(open(sys.argv[1],'rb')); print(repr(d))"])
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(&path);
        let Ok(out) = out else { return }; // no python: nothing to check against
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "{('topo_norway_2x', 'topo_cache_2x', 'norway_25833_2x', (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14)): ('rmapproxy', 1234, 1099511627776)}"
        );
    }
}
