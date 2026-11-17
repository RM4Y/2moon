//! Minimal XML helpers for the GameStream HTTP API.
//! Responses look like `<?xml ...?><root status_code="200"> <tag>value</tag> ... </root>`.

use std::collections::HashMap;

pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            _ => o.push(c),
        }
    }
    o
}

/// Builder for a flat `<root>` response document.
pub struct Response {
    status: u32,
    message: Option<String>,
    body: String,
}

impl Response {
    pub fn ok() -> Self {
        Self { status: 200, message: None, body: String::new() }
    }

    pub fn error(status: u32, message: &str) -> Self {
        Self { status, message: Some(message.to_string()), body: String::new() }
    }

    pub fn field(mut self, tag: &str, value: impl std::fmt::Display) -> Self {
        self.body.push_str(&format!("<{tag}>{}</{tag}>", escape(&value.to_string())));
        self
    }

    /// Append pre-formatted XML (caller must escape).
    pub fn raw(mut self, xml: &str) -> Self {
        self.body.push_str(xml);
        self
    }

    pub fn build(self) -> String {
        let msg = match &self.message {
            Some(m) => format!(" status_message=\"{}\"", escape(m)),
            None => String::new(),
        };
        format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><root status_code=\"{}\"{msg}>{}</root>", self.status, self.body)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum XmlError {
    #[error("xml parse: {0}")]
    Parse(#[from] roxmltree::Error),
    #[error("host returned status {code}: {message}")]
    Status { code: u32, message: String },
    #[error("missing field {0}")]
    Missing(&'static str),
}

/// A parsed flat response: status plus the text of every direct child of `<root>`.
/// Repeated tags keep the first occurrence in `fields` and all values in `multi`.
#[derive(Debug, Default, Clone)]
pub struct Parsed {
    pub status: u32,
    pub fields: HashMap<String, String>,
    pub multi: HashMap<String, Vec<String>>,
}

impl Parsed {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.fields.get(k).map(|s| s.as_str())
    }

    pub fn require(&self, k: &'static str) -> Result<&str, XmlError> {
        self.get(k).ok_or(XmlError::Missing(k))
    }
}

pub fn parse(doc: &str) -> Result<Parsed, XmlError> {
    let d = roxmltree::Document::parse(doc.trim_start_matches('\u{feff}'))?;
    let root = d.root_element();
    let status = root.attribute("status_code").and_then(|s| s.parse().ok()).unwrap_or(200);
    if status != 200 {
        return Err(XmlError::Status { code: status, message: root.attribute("status_message").unwrap_or("").to_string() });
    }
    let mut p = Parsed { status, ..Default::default() };
    for c in root.children().filter(|n| n.is_element()) {
        let name = c.tag_name().name().to_string();
        let text = c.text().unwrap_or("").trim().to_string();
        p.fields.entry(name.clone()).or_insert_with(|| text.clone());
        p.multi.entry(name).or_default().push(text);
    }
    Ok(p)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: u32,
    pub title: String,
    pub hdr_supported: bool,
}

pub fn apps_xml(apps: &[App]) -> String {
    apps.iter()
        .map(|a| {
            format!(
                "<App><IsHdrSupported>{}</IsHdrSupported><AppTitle>{}</AppTitle><ID>{}</ID></App>",
                a.hdr_supported as u8,
                escape(&a.title),
                a.id
            )
        })
        .collect()
}

pub fn parse_apps(doc: &str) -> Result<Vec<App>, XmlError> {
    let d = roxmltree::Document::parse(doc)?;
    let root = d.root_element();
    let status = root.attribute("status_code").and_then(|s| s.parse().ok()).unwrap_or(200);
    if status != 200 {
        return Err(XmlError::Status { code: status, message: root.attribute("status_message").unwrap_or("").to_string() });
    }
    let mut apps = Vec::new();
    for app in root.children().filter(|n| n.has_tag_name("App")) {
        let child = |name: &str| app.children().find(|n| n.has_tag_name(name)).and_then(|n| n.text()).unwrap_or("").trim().to_string();
        apps.push(App {
            id: child("ID").parse().unwrap_or(0),
            title: child("AppTitle"),
            hdr_supported: child("IsHdrSupported") == "1",
        });
    }
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let x = Response::ok().field("hostname", "a<b>&c").field("PairStatus", 1).build();
        let p = parse(&x).unwrap();
        assert_eq!(p.get("hostname"), Some("a<b>&c"));
        assert_eq!(p.get("PairStatus"), Some("1"));
        assert!(matches!(parse(&Response::error(401, "nope").build()), Err(XmlError::Status { code: 401, .. })));
    }

    #[test]
    fn apps() {
        let apps = vec![App { id: 1, title: "Desktop".into(), hdr_supported: false }, App { id: 2, title: "Steam & co".into(), hdr_supported: true }];
        let x = Response::ok().raw(&apps_xml(&apps)).build();
        assert_eq!(parse_apps(&x).unwrap(), apps);
    }
}
