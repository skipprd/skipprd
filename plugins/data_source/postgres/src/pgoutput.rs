/// Decoder for the PostgreSQL pgoutput logical replication protocol.
///
/// Reference: https://www.postgresql.org/docs/current/protocol-logicalrep-message-formats.html

#[derive(Debug, Clone)]
pub struct PgColumn {
    pub name: String,
    pub type_oid: u32,
}

pub type TupleData = Vec<Option<String>>;

#[derive(Debug, Clone)]
pub enum PgOutputMessage {
    Relation {
        oid: u32,
        schema: String,
        name: String,
        columns: Vec<PgColumn>,
    },
    Insert {
        oid: u32,
        new_row: TupleData,
    },
    Update {
        oid: u32,
        old_row: Option<TupleData>,
        new_row: TupleData,
    },
    Delete {
        oid: u32,
        old_row: TupleData,
    },
}

pub fn parse(data: &[u8]) -> Option<PgOutputMessage> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        b'R' => parse_relation(&data[1..]),
        b'I' => parse_insert(&data[1..]),
        b'U' => parse_update(&data[1..]),
        b'D' => parse_delete(&data[1..]),
        _ => None,
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Option<u8> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let value = self.buf[self.pos];
        self.pos += 1;
        Some(value)
    }

    fn read_u16(&mut self) -> Option<u16> {
        if self.remaining() < 2 {
            return None;
        }
        let value = u16::from_be_bytes([self.buf[self.pos], self.buf[self.pos + 1]]);
        self.pos += 2;
        Some(value)
    }

    fn read_u32(&mut self) -> Option<u32> {
        if self.remaining() < 4 {
            return None;
        }
        let value = u32::from_be_bytes([
            self.buf[self.pos],
            self.buf[self.pos + 1],
            self.buf[self.pos + 2],
            self.buf[self.pos + 3],
        ]);
        self.pos += 4;
        Some(value)
    }

    fn read_cstring(&mut self) -> Option<String> {
        let start = self.pos;
        while self.pos < self.buf.len() {
            if self.buf[self.pos] == 0 {
                let text = std::str::from_utf8(&self.buf[start..self.pos]).ok()?;
                self.pos += 1;
                return Some(text.to_string());
            }
            self.pos += 1;
        }
        None
    }

    fn read_bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.remaining() < len {
            return None;
        }
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Some(slice)
    }
}

fn parse_tuple(cur: &mut Cursor<'_>) -> Option<TupleData> {
    let n_cols = cur.read_u16()? as usize;
    let mut row = Vec::with_capacity(n_cols);
    for _ in 0..n_cols {
        let tag = cur.read_u8()?;
        match tag {
            b'n' => row.push(None),
            b't' => {
                let len = cur.read_u32()? as usize;
                let bytes = cur.read_bytes(len)?;
                let text = std::str::from_utf8(bytes).ok()?.to_string();
                row.push(Some(text));
            }
            _ => row.push(None),
        }
    }
    Some(row)
}

fn parse_relation(data: &[u8]) -> Option<PgOutputMessage> {
    let mut cur = Cursor::new(data);
    let oid = cur.read_u32()?;
    let schema = cur.read_cstring()?;
    let name = cur.read_cstring()?;
    let _replica_identity = cur.read_u8()?;
    let col_count = cur.read_u16()? as usize;
    let mut columns = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        let _flags = cur.read_u8()?;
        let col_name = cur.read_cstring()?;
        let type_oid = cur.read_u32()?;
        let _type_modifier = cur.read_u32()?;
        columns.push(PgColumn {
            name: col_name,
            type_oid,
        });
    }
    Some(PgOutputMessage::Relation {
        oid,
        schema,
        name,
        columns,
    })
}

fn parse_insert(data: &[u8]) -> Option<PgOutputMessage> {
    let mut cur = Cursor::new(data);
    let oid = cur.read_u32()?;
    let tag = cur.read_u8()?;
    if tag != b'N' {
        return None;
    }
    let new_row = parse_tuple(&mut cur)?;
    Some(PgOutputMessage::Insert { oid, new_row })
}

fn parse_update(data: &[u8]) -> Option<PgOutputMessage> {
    let mut cur = Cursor::new(data);
    let oid = cur.read_u32()?;
    let tag = cur.read_u8()?;
    let old_row = if tag == b'K' || tag == b'O' {
        let tuple = parse_tuple(&mut cur)?;
        let _new_tag = cur.read_u8()?;
        Some(tuple)
    } else {
        None
    };
    let new_row = parse_tuple(&mut cur)?;
    Some(PgOutputMessage::Update {
        oid,
        old_row,
        new_row,
    })
}

fn parse_delete(data: &[u8]) -> Option<PgOutputMessage> {
    let mut cur = Cursor::new(data);
    let oid = cur.read_u32()?;
    let tag = cur.read_u8()?;
    if tag != b'K' && tag != b'O' {
        return None;
    }
    let old_row = parse_tuple(&mut cur)?;
    Some(PgOutputMessage::Delete { oid, old_row })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_relation_msg() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(b'R');
        buf.extend_from_slice(&1234u32.to_be_bytes());
        buf.extend_from_slice(b"public\0");
        buf.extend_from_slice(b"users\0");
        buf.push(b'd');
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.push(0);
        buf.extend_from_slice(b"id\0");
        buf.extend_from_slice(&23u32.to_be_bytes());
        buf.extend_from_slice(&(-1i32 as u32).to_be_bytes());
        buf.push(0);
        buf.extend_from_slice(b"name\0");
        buf.extend_from_slice(&25u32.to_be_bytes());
        buf.extend_from_slice(&(-1i32 as u32).to_be_bytes());
        buf
    }

    #[test]
    fn test_parse_relation() {
        let msg = build_relation_msg();
        let parsed = parse(&msg).unwrap();
        match parsed {
            PgOutputMessage::Relation {
                oid,
                schema,
                name,
                columns,
            } => {
                assert_eq!(oid, 1234);
                assert_eq!(schema, "public");
                assert_eq!(name, "users");
                assert_eq!(columns.len(), 2);
                assert_eq!(columns[0].name, "id");
                assert_eq!(columns[1].name, "name");
            }
            _ => panic!("expected Relation"),
        }
    }

    #[test]
    fn test_parse_insert() {
        let mut buf = Vec::new();
        buf.push(b'I');
        buf.extend_from_slice(&1234u32.to_be_bytes());
        buf.push(b'N');
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.push(b't');
        let val = b"42";
        buf.extend_from_slice(&(val.len() as u32).to_be_bytes());
        buf.extend_from_slice(val);
        buf.push(b't');
        let val2 = b"alice";
        buf.extend_from_slice(&(val2.len() as u32).to_be_bytes());
        buf.extend_from_slice(val2);

        let parsed = parse(&buf).unwrap();
        match parsed {
            PgOutputMessage::Insert { oid, new_row } => {
                assert_eq!(oid, 1234);
                assert_eq!(new_row.len(), 2);
                assert_eq!(new_row[0], Some("42".to_string()));
                assert_eq!(new_row[1], Some("alice".to_string()));
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn test_parse_delete() {
        let mut buf = Vec::new();
        buf.push(b'D');
        buf.extend_from_slice(&1234u32.to_be_bytes());
        buf.push(b'K');
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.push(b't');
        let val = b"42";
        buf.extend_from_slice(&(val.len() as u32).to_be_bytes());
        buf.extend_from_slice(val);

        let parsed = parse(&buf).unwrap();
        match parsed {
            PgOutputMessage::Delete { oid, old_row } => {
                assert_eq!(oid, 1234);
                assert_eq!(old_row.len(), 1);
                assert_eq!(old_row[0], Some("42".to_string()));
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn test_parse_null_column() {
        let mut buf = Vec::new();
        buf.push(b'I');
        buf.extend_from_slice(&99u32.to_be_bytes());
        buf.push(b'N');
        buf.extend_from_slice(&2u16.to_be_bytes());
        buf.push(b't');
        let val = b"1";
        buf.extend_from_slice(&(val.len() as u32).to_be_bytes());
        buf.extend_from_slice(val);
        buf.push(b'n');

        let parsed = parse(&buf).unwrap();
        match parsed {
            PgOutputMessage::Insert { new_row, .. } => {
                assert_eq!(new_row[0], Some("1".to_string()));
                assert!(new_row[1].is_none());
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn test_parse_unknown_type_returns_none() {
        assert!(parse(&[b'X', 0, 0, 0, 1]).is_none());
    }

    #[test]
    fn test_parse_empty_returns_none() {
        assert!(parse(&[]).is_none());
    }
}
