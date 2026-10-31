//! Minimal IPP (RFC 8010) message codec.
//!
//! Attributes keep their raw entries so that anything we don't touch (collections,
//! octet strings, unknown tags) round-trips byte for byte.

pub mod tag {
    pub const OPERATION: u8 = 0x01;
    pub const END: u8 = 0x03;
    pub const PRINTER: u8 = 0x04;

    pub const BOOLEAN: u8 = 0x22;
    pub const NAME: u8 = 0x42;
    pub const KEYWORD: u8 = 0x44;
    pub const URI: u8 = 0x45;
    pub const CHARSET: u8 = 0x47;
    pub const LANGUAGE: u8 = 0x48;
    pub const MIME: u8 = 0x49;
}

pub const OP_GET_PRINTER_ATTRIBUTES: u16 = 0x000B;

#[derive(Debug)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid IPP message: {}", self.0)
    }
}

impl std::error::Error for Error {}

/// One attribute: its name plus every (value-tag, value) entry that follows it.
/// Additional values and collection members are the entries with an empty name on the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub values: Vec<(u8, Vec<u8>)>,
}

impl Attribute {
    pub fn new<V: Into<Vec<u8>>>(name: &str, tag: u8, values: impl IntoIterator<Item = V>) -> Self {
        Attribute {
            name: name.to_string(),
            values: values.into_iter().map(|v| (tag, v.into())).collect(),
        }
    }

    /// String values (text, name, keyword, uri, mime, …). Only meaningful for non-collection attributes.
    pub fn strings(&self) -> Vec<String> {
        self.values
            .iter()
            .filter(|(t, _)| *t >= 0x40)
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
            .collect()
    }

    pub fn first_string(&self) -> Option<String> {
        self.strings().into_iter().next()
    }

    pub fn boolean(&self) -> Option<bool> {
        match self.values.first() {
            Some((tag::BOOLEAN, v)) if v.len() == 1 => Some(v[0] != 0),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub tag: u8,
    pub attrs: Vec<Attribute>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub version: [u8; 2],
    /// Operation id in requests, status code in responses.
    pub code: u16,
    pub request_id: u32,
    pub groups: Vec<Group>,
}

impl Message {
    pub fn new(version: [u8; 2], code: u16, request_id: u32) -> Self {
        Message { version, code, request_id, groups: Vec::new() }
    }

    pub fn find(&self, group: u8, name: &str) -> Option<&Attribute> {
        self.groups
            .iter()
            .filter(|g| g.tag == group)
            .flat_map(|g| g.attrs.iter())
            .find(|a| a.name == name)
    }

    /// The first group with this tag, appended if there is none.
    pub fn group_mut(&mut self, group: u8) -> &mut Group {
        let i = match self.groups.iter().position(|g| g.tag == group) {
            Some(i) => i,
            None => {
                self.groups.push(Group { tag: group, attrs: Vec::new() });
                self.groups.len() - 1
            }
        };
        &mut self.groups[i]
    }

    /// Replaces the attribute in the first group with this tag, or appends it.
    pub fn set(&mut self, group: u8, attr: Attribute) {
        let g = self.group_mut(group);
        match g.attrs.iter_mut().find(|a| a.name == attr.name) {
            Some(existing) => *existing = attr,
            None => g.attrs.push(attr),
        }
    }

    pub fn attrs_mut(&mut self) -> impl Iterator<Item = &mut Attribute> {
        self.groups.iter_mut().flat_map(|g| g.attrs.iter_mut())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(256);
        out.extend_from_slice(&self.version);
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.request_id.to_be_bytes());
        for g in &self.groups {
            out.push(g.tag);
            for a in &g.attrs {
                for (i, (t, v)) in a.values.iter().enumerate() {
                    let name = if i == 0 { a.name.as_bytes() } else { &[] };
                    out.push(*t);
                    out.extend_from_slice(&(name.len() as u16).to_be_bytes());
                    out.extend_from_slice(name);
                    out.extend_from_slice(&(v.len() as u16).to_be_bytes());
                    out.extend_from_slice(v);
                }
            }
        }
        out.push(tag::END);
        out
    }
}

/// Parses the message header and attributes from the start of `buf`.
/// Returns `Ok(None)` if `buf` doesn't hold the complete attribute section yet,
/// otherwise the message and the offset where the document data starts.
pub fn parse_header(buf: &[u8]) -> Result<Option<(Message, usize)>, Error> {
    if buf.len() < 8 {
        return Ok(None);
    }
    let mut msg = Message::new(
        [buf[0], buf[1]],
        u16::from_be_bytes([buf[2], buf[3]]),
        u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
    );
    let mut pos = 8;
    loop {
        let Some(&t) = buf.get(pos) else { return Ok(None) };
        if t == tag::END {
            return Ok(Some((msg, pos + 1)));
        }
        if t < 0x10 {
            msg.groups.push(Group { tag: t, attrs: Vec::new() });
            pos += 1;
            continue;
        }
        let Some(name_len) = read_u16(buf, pos + 1) else { return Ok(None) };
        let name_end = pos + 3 + name_len;
        let Some(value_len) = read_u16(buf, name_end) else { return Ok(None) };
        let value_end = name_end + 2 + value_len;
        if buf.len() < value_end {
            return Ok(None);
        }
        let name = &buf[pos + 3..name_end];
        let value = buf[name_end + 2..value_end].to_vec();
        let group = msg.groups.last_mut().ok_or(Error("attribute outside a group"))?;
        if name.is_empty() {
            let attr = group.attrs.last_mut().ok_or(Error("additional value without attribute"))?;
            attr.values.push((t, value));
        } else {
            let name = std::str::from_utf8(name).map_err(|_| Error("attribute name is not UTF-8"))?;
            group.attrs.push(Attribute { name: name.to_string(), values: vec![(t, value)] });
        }
        pos = value_end;
    }
}

/// Parses a complete message; returns it with its document data.
pub fn parse(buf: &[u8]) -> Result<(Message, &[u8]), Error> {
    match parse_header(buf)? {
        Some((msg, offset)) => Ok((msg, &buf[offset..])),
        None => Err(Error("truncated")),
    }
}

fn read_u16(buf: &[u8], pos: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*buf.get(pos)?, *buf.get(pos + 1)?]) as usize)
}

/// A Get-Printer-Attributes request for everything, including media-col-database.
pub fn get_printer_attributes_request(printer_uri: &str, request_id: u32) -> Vec<u8> {
    let mut msg = Message::new([1, 1], OP_GET_PRINTER_ATTRIBUTES, request_id);
    msg.groups.push(Group {
        tag: tag::OPERATION,
        attrs: vec![
            Attribute::new("attributes-charset", tag::CHARSET, ["utf-8"]),
            Attribute::new("attributes-natural-language", tag::LANGUAGE, ["en"]),
            Attribute::new("printer-uri", tag::URI, [printer_uri]),
            Attribute::new("requested-attributes", tag::KEYWORD, ["all", "media-col-database"]),
        ],
    });
    msg.encode()
}

/// Replaces scheme and authority of an `ipp://host:port/path` style URI, keeping the path.
pub fn replace_authority(uri: &str, scheme: &str, authority: &str) -> String {
    let rest = uri.split_once("://").map_or(uri, |(_, r)| r);
    let path = rest.find('/').map_or("", |i| &rest[i..]);
    format!("{scheme}://{authority}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Message {
        let mut msg = Message::new([2, 0], 0x0000, 7);
        msg.groups.push(Group {
            tag: tag::OPERATION,
            attrs: vec![Attribute::new("attributes-charset", tag::CHARSET, ["utf-8"])],
        });
        msg.groups.push(Group {
            tag: tag::PRINTER,
            attrs: vec![
                Attribute::new("document-format-supported", tag::MIME, ["application/PCL", "image/urf"]),
                Attribute::new("color-supported", tag::BOOLEAN, [vec![0u8]]),
                // media-col-default { media-size-name = "iso_a4_210x297mm" }
                Attribute {
                    name: "media-col-default".into(),
                    values: vec![
                        (0x34, vec![]),
                        (0x4A, b"media-size-name".to_vec()),
                        (tag::KEYWORD, b"iso_a4_210x297mm".to_vec()),
                        (0x37, vec![]),
                    ],
                },
            ],
        });
        msg
    }

    #[test]
    fn round_trip() {
        let msg = sample();
        let mut bytes = msg.encode();
        bytes.extend_from_slice(b"%PDF data");
        let (parsed, data) = parse(&bytes).unwrap();
        assert_eq!(parsed, msg);
        assert_eq!(data, b"%PDF data");
        assert_eq!(parsed.encode(), msg.encode());
    }

    #[test]
    fn incomplete_header() {
        let bytes = sample().encode();
        for len in 0..bytes.len() {
            assert!(parse_header(&bytes[..len]).unwrap().is_none(), "len {len}");
        }
        assert!(parse_header(&bytes).unwrap().is_some());
    }

    #[test]
    fn accessors() {
        let msg = sample();
        let formats = msg.find(tag::PRINTER, "document-format-supported").unwrap();
        assert_eq!(formats.strings(), ["application/PCL", "image/urf"]);
        assert_eq!(msg.find(tag::PRINTER, "color-supported").unwrap().boolean(), Some(false));
        assert!(msg.find(tag::OPERATION, "document-format-supported").is_none());
    }

    #[test]
    fn authority() {
        assert_eq!(
            replace_authority("ipp://192.168.178.200:631/ipp/print", "ipp", "ha.local:631"),
            "ipp://ha.local:631/ipp/print"
        );
        assert_eq!(replace_authority("ipps://printer", "ipp", "x:1"), "ipp://x:1");
    }
}
